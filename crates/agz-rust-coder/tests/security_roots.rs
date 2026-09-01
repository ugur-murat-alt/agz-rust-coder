use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use agz_rust_coder::lsp::NormalizeError;
use agz_rust_coder::lsp::path_to_file_uri;
use agz_rust_coder::workspace::{
    ClientRoots, RootError, RootGuard, WalkIssueKind, WalkLimits, WorkspaceRoot, parse_file_uri,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!("agz-rust-coder-roots-{label}-{stamp}"));
        fs::create_dir(&path).expect("create temporary root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn client_roots_only_narrow_configured_roots() {
    let root = TestDir::new("intersection");
    let child = root.path().join("child");
    let outside = TestDir::new("outside");
    fs::create_dir(&child).expect("create configured child");
    fs::write(child.join("source.rs"), b"fn main() {}\n").expect("write source");

    let guard = RootGuard::new([root.path().to_owned()], std::iter::empty()).expect("create guard");
    let snapshot = guard
        .snapshot(ClientRoots::available([child.clone()]))
        .expect("narrow roots");
    assert_eq!(snapshot.roots().len(), 1);
    assert_eq!(snapshot.roots()[0].path(), child);

    let selected = snapshot.select(None).expect("select narrowed root");
    assert_eq!(selected.path(), child);
    assert_eq!(
        selected.read_file(&child.join("source.rs"), 1024).unwrap(),
        b"fn main() {}\n"
    );
    assert!(matches!(
        guard.snapshot(ClientRoots::available([outside.path().to_owned()])),
        Err(RootError::NoRootIntersection)
    ));
    assert!(matches!(
        guard.snapshot(ClientRoots::available(Vec::<PathBuf>::new())),
        Err(RootError::ClientRootsEmpty)
    ));
}

#[test]
fn advertised_root_failure_is_fail_closed_and_invalidates_epoch() {
    let root = TestDir::new("failed-client");
    let guard = RootGuard::new([root.path().to_owned()], std::iter::empty()).expect("create guard");
    let first = guard
        .snapshot(ClientRoots::unsupported())
        .expect("unsupported roots use configured roots");
    let first_epoch = first.epoch();

    assert!(matches!(
        guard.snapshot(ClientRoots::failed()),
        Err(RootError::ClientRootsUnavailable)
    ));
    assert!(guard.current_epoch().expect("read epoch") > first_epoch);
    assert!(guard.current_snapshot().expect("read snapshot").is_none());
}

#[test]
fn file_uris_are_local_and_preserve_absolute_paths() {
    let root = TestDir::new("uri");
    let path = root.path().join("name with spaces");
    fs::create_dir(&path).expect("create URI fixture");
    let uri = path_to_file_uri(&path).expect("local file URI");
    let localhost_uri = uri.replacen("file://", "file://localhost", 1);

    assert_eq!(
        fs::canonicalize(parse_file_uri(&uri).unwrap()).unwrap(),
        path
    );
    assert_eq!(
        fs::canonicalize(parse_file_uri(&localhost_uri).unwrap()).unwrap(),
        path
    );
    assert!(matches!(
        parse_file_uri("https://localhost/tmp/project"),
        Err(RootError::InvalidFileUri(_))
    ));
    assert!(matches!(
        parse_file_uri("file://remotehost/tmp/project"),
        Err(RootError::InvalidFileUri(_))
    ));
    assert!(matches!(
        parse_file_uri("file:///tmp/project?query"),
        Err(RootError::InvalidFileUri(_))
    ));
}

#[cfg(windows)]
#[test]
fn file_uri_generation_rejects_windows_network_paths() {
    for path in [
        Path::new(r"\\server\share\source.rs"),
        Path::new("//server/share/source.rs"),
        Path::new(r"\\?\UNC\server\share\source.rs"),
        Path::new(r"\\?\unc\server\share\source.rs"),
    ] {
        assert!(matches!(
            path_to_file_uri(path),
            Err(NormalizeError::InvalidFileUri(_))
        ));
    }
    assert!(matches!(
        parse_file_uri("file:////server/share/source.rs"),
        Err(RootError::InvalidFileUri(_))
    ));
}

#[test]
fn bounded_reads_and_walks_report_unsafe_entries() {
    let root = TestDir::new("bounded");
    fs::write(root.path().join("large.txt"), b"123456").expect("write large file");
    fs::write(root.path().join("source.rs"), b"fn main() {}\n").expect("write Rust file");
    let guard = RootGuard::new([root.path().to_owned()], std::iter::empty()).expect("create guard");
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot");

    assert!(matches!(
        snapshot.read_file(&root.path().join("large.txt"), 3),
        Err(RootError::TooLarge { .. })
    ));
    let walked = snapshot.roots()[0]
        .walk_files_matching(WalkLimits::default(), |path| {
            path.extension().is_some_and(|ext| ext == "rs")
        })
        .expect("walk root");
    assert_eq!(walked.files.len(), 1);
    assert_eq!(walked.files[0].path, PathBuf::from("source.rs"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = TestDir::new("bounded-outside");
        fs::write(outside.path().join("secret"), b"secret").expect("write outside file");
        symlink(outside.path(), root.path().join("escape")).expect("create escape link");
        let walked = snapshot.roots()[0]
            .walk_files_matching(WalkLimits::default(), |_| true)
            .expect("walk symlink");
        assert!(
            walked
                .issues
                .iter()
                .any(|issue| issue.kind == WalkIssueKind::Symlink)
        );
        assert!(matches!(
            snapshot.resolve_existing(&root.path().join("escape/secret")),
            Err(RootError::Symlink(_))
        ));
        assert!(
            !outside.path().join("secret").is_file()
                || fs::read(outside.path().join("secret")).unwrap() == b"secret"
        );
    }
}

#[test]
fn dependency_roots_are_not_selectable_workspace_roots() {
    let workspace = TestDir::new("workspace");
    let dependency = TestDir::new("dependency");
    fs::write(
        dependency.path().join("Cargo.toml"),
        b"[package]\nname = \"dep\"\nversion = \"0.1.0\"\n",
    )
    .expect("write dependency manifest");

    let guard = RootGuard::new(
        [workspace.path().to_owned()],
        [dependency.path().to_owned()],
    )
    .expect("create guard");
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot");
    assert!(matches!(
        snapshot.resolve_existing(dependency.path()),
        Err(RootError::PathOutsideRoot(_))
    ));
    let resolved = guard
        .resolve_dependency(&dependency.path().join("Cargo.toml"))
        .expect("resolve authorized dependency");
    assert_eq!(
        resolved.root.kind(),
        agz_rust_coder::workspace::RootKind::Dependency
    );
}

#[allow(dead_code)]
fn _workspace_root_is_publicly_constructed(root: &WorkspaceRoot) -> &Path {
    root.path()
}
