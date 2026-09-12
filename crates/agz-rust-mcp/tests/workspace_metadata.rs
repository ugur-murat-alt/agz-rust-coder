use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agz_rust_mcp::workspace::{
    CargoMetadataRunner, ClientRoots, MetadataCacheState, MetadataCommandSpec, MetadataError,
    MetadataRun, MetadataRunner, MetadataService, RootGuard, select_workspace,
};

#[derive(Debug)]
struct NoopRunner;

impl MetadataRunner for NoopRunner {
    fn run(&self, _command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError> {
        Err(MetadataError::Runner(
            "test runner was not expected to run".to_owned(),
        ))
    }
}

#[derive(Debug, Clone)]
struct CountingRunner {
    metadata: MetadataRun,
    calls: Arc<AtomicUsize>,
}

impl MetadataRunner for CountingRunner {
    fn run(&self, _command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.metadata.clone())
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!("agz-rust-mcp-metadata-{label}-{stamp}"));
        fs::create_dir(&path).expect("create temporary root");
        fs::write(path.join(".git"), b"fixture worktree marker").expect("write marker");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn package(&self, relative: &str, name: &str) -> PathBuf {
        let directory = self.path().join(relative);
        fs::create_dir_all(directory.join("src")).expect("create package");
        let manifest = directory.join("Cargo.toml");
        fs::write(
            &manifest,
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .expect("write package manifest");
        fs::write(
            directory.join("src/lib.rs"),
            b"pub fn value() -> u8 { 1 }\n",
        )
        .expect("write source");
        manifest
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn selection(
    root: &TestDir,
    dependencies: impl IntoIterator<Item = PathBuf>,
) -> (Arc<RootGuard>, agz_rust_mcp::workspace::WorkspaceSelection) {
    let guard =
        Arc::new(RootGuard::new([root.path().to_owned()], dependencies).expect("create guard"));
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot");
    let selected = select_workspace(&snapshot, Some(root.path())).expect("select workspace");
    (guard, selected)
}

#[test]
fn preflight_authorizes_only_declared_external_path_dependencies() {
    let root = TestDir::new("external");
    let dependency = TestDir::new("dependency");
    let manifest = root.path().join("Cargo.toml");
    let dependency_path = format!(
        "../{}",
        dependency.path().file_name().unwrap().to_string_lossy()
    );
    fs::write(
        &manifest,
        format!(
            "[package]\nname = \"root-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nexternal = {{ path = \"{dependency_path}\" }}\n"
        ),
    )
    .expect("write root manifest");
    dependency.package(".", "external");

    let (guard, selected) = selection(&root, [dependency.path().to_owned()]);
    let service = MetadataService::with_runner(guard, NoopRunner);
    let closure = service
        .preflight(&selected)
        .expect("preflight dependency closure");

    assert_eq!(closure.manifests.len(), 2);
    assert_eq!(closure.external_roots, vec![dependency.path().to_owned()]);
    assert!(closure.complete);
}

#[test]
fn preflight_ignores_non_dependency_manifest_paths() {
    let root = TestDir::new("non-dependency-path");
    fs::create_dir_all(root.path().join("src")).expect("create source directory");
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"root-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"root-package\"\npath = \"src/main.rs\"\n",
    )
    .expect("write manifest");
    fs::write(root.path().join("src/main.rs"), "fn main() {}\n").expect("write binary");
    let (guard, selected) = selection(&root, std::iter::empty());

    let closure = MetadataService::new(guard)
        .preflight(&selected)
        .expect("non-dependency paths are ignored");

    assert_eq!(closure.manifests, vec![root.path().join("Cargo.toml")]);
    assert!(closure.external_roots.is_empty());
}

#[test]
fn preflight_blocks_an_external_path_dependency_without_a_capability_root() {
    let root = TestDir::new("blocked");
    let dependency = TestDir::new("blocked-dependency");
    let dependency_path = format!(
        "../{}",
        dependency.path().file_name().unwrap().to_string_lossy()
    );
    fs::write(
        root.path().join("Cargo.toml"),
        format!(
            "[package]\nname = \"root-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nexternal = {{ path = \"{dependency_path}\" }}\n"
        ),
    )
    .expect("write root manifest");
    dependency.package(".", "external");

    let (guard, selected) = selection(&root, std::iter::empty());
    let service = MetadataService::with_runner(guard, NoopRunner);
    let error = service
        .preflight(&selected)
        .expect_err("blocked dependency");
    assert!(matches!(error, MetadataError::PathDependencyBlocked(_)));
}

#[test]
fn nested_worktree_keeps_shared_dependencies_external_under_a_broad_allowed_root() {
    let projects = TestDir::new("worktree-parent");
    let checkout = projects.path().join("main checkout/.worktrees/feature ü");
    let shared = projects.path().join("shared");
    projects.package("shared", "external");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join(".git"), "gitdir: fixture").unwrap();
    fs::write(checkout.join("Cargo.toml"), format!(
        "[package]\nname='worktree'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nexternal={{path={}}}\n",
        toml::Value::String(shared.to_string_lossy().into_owned())
    )).unwrap();
    let guard = Arc::new(RootGuard::new([projects.path().to_owned()], [shared.clone()]).unwrap());
    let snapshot = guard.snapshot(ClientRoots::unsupported()).unwrap();
    let selected = select_workspace(&snapshot, Some(&checkout)).unwrap();
    let closure = MetadataService::with_runner(guard, NoopRunner)
        .preflight(&selected)
        .unwrap();
    assert_eq!(selected.canonical_worktree(), checkout);
    assert_eq!(
        closure.external_roots,
        [shared],
        "a broad configured root must not hide a worktree's external source inputs"
    );
}

#[test]
fn member_subdirectory_preflight_includes_inherited_workspace_dependencies() {
    let root = TestDir::new("inherited");
    let dependency = TestDir::new("inherited-dependency");
    dependency.package(".", "external");
    root.package("members/app", "app");
    fs::write(root.path().join("Cargo.toml"), format!(
        "[workspace]\nmembers=['members/app']\nresolver='2'\n[workspace.dependencies]\nexternal={{path={}}}\n",
        toml::Value::String(dependency.path().to_string_lossy().into_owned())
    )).unwrap();
    fs::write(root.path().join("members/app/Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nexternal.workspace=true\n").unwrap();
    let guard =
        Arc::new(RootGuard::new([root.path().to_owned()], [dependency.path().to_owned()]).unwrap());
    let snapshot = guard.snapshot(ClientRoots::unsupported()).unwrap();
    let selected = select_workspace(&snapshot, Some(&root.path().join("members/app/src"))).unwrap();
    let closure = MetadataService::with_runner(guard, NoopRunner)
        .preflight(&selected)
        .unwrap();
    assert!(closure.manifests.contains(&root.path().join("Cargo.toml")));
    assert_eq!(closure.external_roots, [dependency.path().to_owned()]);
}

#[test]
fn workspace_member_globs_are_expanded_deterministically() {
    let root = TestDir::new("members");
    fs::write(
        root.path().join("Cargo.toml"),
        b"[workspace]\nmembers = [\"members/*\"]\nresolver = \"2\"\n",
    )
    .expect("write workspace manifest");
    let a = root.package("members/a", "member-a");
    let b = root.package("members/b", "member-b");
    let (guard, selected) = selection(&root, std::iter::empty());
    let service = MetadataService::with_runner(guard, NoopRunner);

    let closure = service.preflight(&selected).expect("expand members");
    assert_eq!(
        closure.package_roots,
        vec![
            root.path().to_owned(),
            a.parent().unwrap().to_owned(),
            b.parent().unwrap().to_owned(),
        ]
    );
}

#[test]
fn metadata_command_spec_is_locked_and_manifest_scoped() {
    let root = TestDir::new("command");
    let manifest = root.package("package", "command-package");
    let guard = Arc::new(
        RootGuard::new([root.path().to_owned()], std::iter::empty()).expect("create guard"),
    );
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot");
    let selected =
        select_workspace(&snapshot, Some(manifest.parent().unwrap())).expect("select package");

    let spec = MetadataCommandSpec::for_selection(
        &selected,
        root.path()
            .join(format!("cargo{}", std::env::consts::EXE_SUFFIX)),
    );
    assert!(spec.locked);
    assert!(spec.args.iter().any(|arg| arg == "--locked"));
    assert_eq!(spec.manifest_path, manifest);
    assert_eq!(spec.current_dir, manifest.parent().unwrap());
    let _ = CargoMetadataRunner;
}

#[test]
fn metadata_cache_tracks_target_layout_without_reloading_for_body_edits() {
    let root = TestDir::new("target-layout");
    root.package(".", "root-package");
    fs::write(
        root.path().join("Cargo.lock"),
        "version=4\n[[package]]\nname=\"root-package\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    let (guard, selected) = selection(&root, []);
    let metadata = CargoMetadataRunner
        .run(&MetadataCommandSpec::for_selection(&selected, "cargo"))
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let service = MetadataService::with_runner(
        guard,
        CountingRunner {
            metadata,
            calls: calls.clone(),
        },
    );
    assert_eq!(
        service.acquire(&selected, "cargo").unwrap().cache,
        MetadataCacheState::Miss
    );
    for relative in [
        "tests/new.rs",
        "tests/nested/main.rs",
        "src/bin/tool.rs",
        "src/bin/nested/main.rs",
        "src/main.rs",
        "examples/demo.rs",
        "benches/timing.rs",
        "build.rs",
    ] {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "fn main() {}\n").unwrap();
        assert_eq!(
            service.acquire(&selected, "cargo").unwrap().cache,
            MetadataCacheState::Miss,
            "{relative}"
        );
        let before = calls.load(Ordering::SeqCst);
        fs::write(&path, "fn main() { let _changed = 1; }\n").unwrap();
        assert_eq!(
            service.acquire(&selected, "cargo").unwrap().cache,
            MetadataCacheState::Hit,
            "{relative}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), before);
    }
    fs::remove_file(root.path().join("tests/new.rs")).unwrap();
    assert_eq!(
        service.acquire(&selected, "cargo").unwrap().cache,
        MetadataCacheState::Miss
    );
}

#[test]
fn metadata_changed_during_discovery_is_not_published_under_the_old_key() {
    #[derive(Debug)]
    struct MutatingRunner {
        root: PathBuf,
        calls: Arc<AtomicUsize>,
    }
    impl MetadataRunner for MutatingRunner {
        fn run(&self, command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                fs::create_dir_all(self.root.join("tests")).unwrap();
                fs::write(self.root.join("tests/late.rs"), "#[test] fn late() {}\n").unwrap();
            }
            CargoMetadataRunner.run(command)
        }
    }
    let root = TestDir::new("layout-during-discovery");
    root.package(".", "root-package");
    fs::write(
        root.path().join("Cargo.lock"),
        "version=4\n[[package]]\nname=\"root-package\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    let (guard, selected) = selection(&root, []);
    let calls = Arc::new(AtomicUsize::new(0));
    let service = MetadataService::with_runner(
        guard,
        MutatingRunner {
            root: root.path().to_owned(),
            calls: calls.clone(),
        },
    );
    assert!(matches!(
        service.acquire(&selected, "cargo"),
        Err(MetadataError::InputsChanged)
    ));
    let refreshed = service.acquire(&selected, "cargo").unwrap();
    assert_eq!(refreshed.cache, MetadataCacheState::Miss);
    assert!(
        refreshed
            .snapshot
            .metadata
            .packages
            .iter()
            .flat_map(|p| &p.targets)
            .any(|target| target.name == "late")
    );
    assert_eq!(
        service.acquire(&selected, "cargo").unwrap().cache,
        MetadataCacheState::Hit
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn metadata_cache_key_tracks_manifest_lock_and_local_config_changes() {
    let root = TestDir::new("cache-inputs");
    let manifest = root.package(".", "root-package");
    root.package("dep-a", "local-dependency");
    let dependency_b = root.package("dep-b", "local-dependency");
    fs::write(
        &manifest,
        b"[package]\nname = \"root-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nlocal-dependency = { path = \"dep-a\" }\n",
    )
    .expect("write path dependency manifest");
    fs::write(
        root.path().join("Cargo.lock"),
        b"version = 4\n\n[[package]]\nname = \"root-package\"\nversion = \"0.1.0\"\ndependencies = [\"local-dependency\"]\n\n[[package]]\nname = \"local-dependency\"\nversion = \"0.1.0\"\n",
    )
    .expect("write lockfile");

    let (guard, selected) = selection(&root, std::iter::empty());
    let initial = CargoMetadataRunner
        .run(&MetadataCommandSpec::for_selection(&selected, "cargo"))
        .expect("load initial metadata");
    let calls = Arc::new(AtomicUsize::new(0));
    let service = MetadataService::with_runner(
        guard,
        CountingRunner {
            metadata: initial,
            calls: Arc::clone(&calls),
        },
    );

    let first = service
        .acquire(&selected, "cargo")
        .expect("initial metadata acquire");
    assert_eq!(first.cache, MetadataCacheState::Miss);
    let second = service
        .acquire(&selected, "cargo")
        .expect("unchanged metadata acquire");
    assert_eq!(second.cache, MetadataCacheState::Hit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    fs::write(
        &manifest,
        b"[package]\nname = \"root-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nlocal-dependency = { path = \"dep-b\" }\n",
    )
    .expect("change path dependency");
    let path_changed = service
        .acquire(&selected, "cargo")
        .expect("path dependency metadata acquire");
    assert_eq!(path_changed.cache, MetadataCacheState::Miss);

    fs::write(
        root.path().join("Cargo.lock"),
        b"# changed\nversion = 4\n\n[[package]]\nname = \"root-package\"\nversion = \"0.1.0\"\ndependencies = [\"local-dependency\"]\n\n[[package]]\nname = \"local-dependency\"\nversion = \"0.1.0\"\n",
    )
    .expect("change lockfile");
    let lock_changed = service
        .acquire(&selected, "cargo")
        .expect("lockfile metadata acquire");
    assert_eq!(lock_changed.cache, MetadataCacheState::Miss);

    fs::create_dir_all(root.path().join(".cargo")).expect("create Cargo config directory");
    fs::write(
        root.path().join(".cargo/config.toml"),
        b"[net]\noffline = true\n",
    )
    .expect("write Cargo config");
    let config_changed = service
        .acquire(&selected, "cargo")
        .expect("config metadata acquire");
    assert_eq!(config_changed.cache, MetadataCacheState::Miss);

    fs::write(
        &dependency_b,
        b"[package]\nname = \"local-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n# changed\n",
    )
    .expect("change path dependency manifest");
    let dependency_changed = service
        .acquire(&selected, "cargo")
        .expect("path dependency manifest acquire");
    assert_eq!(dependency_changed.cache, MetadataCacheState::Miss);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

#[cfg(unix)]
#[test]
fn cargo_metadata_rejects_a_selected_child_replacement_before_cargo_runs() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = TestDir::new("root-replacement-runner");
    let root = sandbox.path().join("workspace");
    fs::create_dir(&root).expect("create original workspace root");
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"original\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write original manifest");

    let marker = sandbox.path().join("replacement-cargo-ran");
    let cargo = sandbox.path().join("fake-cargo");
    fs::write(
        &cargo,
        format!(
            "#!/bin/sh\ntouch '{}'\nprintf '%s' '{{}}'\n",
            marker.display(),
        ),
    )
    .expect("write fake cargo executable");
    let mut permissions = fs::metadata(&cargo)
        .expect("fake cargo metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&cargo, permissions).expect("make fake cargo executable");

    let guard = RootGuard::new([sandbox.path().to_owned()], std::iter::empty())
        .expect("authorize configured parent");
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot configured parent");
    let authority = snapshot
        .select(Some(&root))
        .expect("select child workspace")
        .requested_authority()
        .clone();
    let original = sandbox.path().join("workspace-original");
    fs::rename(&root, &original).expect("rename authorized root");
    fs::create_dir(&root).expect("create replacement workspace root");
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"replacement\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write replacement manifest");

    let manifest = root.join("Cargo.toml");
    let command = MetadataCommandSpec {
        cargo,
        manifest_path: manifest.clone(),
        current_dir: root,
        args: vec![
            "metadata".into(),
            "--format-version".into(),
            "1".into(),
            "--manifest-path".into(),
            manifest.into_os_string(),
            "--locked".into(),
        ],
        locked: true,
    };

    let result = CargoMetadataRunner.run_authorized(&command, authority);

    assert!(
        result.is_err(),
        "replacement root must fail before metadata runs: {result:?}"
    );
    assert!(!marker.exists(), "replacement cargo must not run");
    assert!(original.join("Cargo.toml").is_file());
}
