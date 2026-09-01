use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use agz_rust_coder::workspace::{
    ClientRoots, GitOutput, GitProbe, IdentityIncompleteReason, IdentityInput, IdentityLimits,
    RootGuard, compute_input_identity,
};

#[derive(Debug, Clone)]
struct FakeGit {
    status: i32,
    head: Vec<u8>,
    changed: Vec<u8>,
    truncated: bool,
}

impl GitProbe for FakeGit {
    fn run(
        &self,
        _cwd: &Path,
        args: &[OsString],
        _max_output_bytes: usize,
    ) -> Result<GitOutput, agz_rust_coder::workspace::IdentityError> {
        let is_head = args.iter().any(|arg| arg == "rev-parse");
        Ok(GitOutput {
            status: Some(self.status),
            stdout: if is_head {
                self.head.clone()
            } else {
                self.changed.clone()
            },
            truncated: self.truncated,
        })
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agz-rust-coder-identity-{label}-{stamp}"));
        fs::create_dir(&path).expect("create temporary root");
        fs::write(
            path.join("Cargo.toml"),
            b"[package]\nname = \"identity\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("write manifest");
        fs::create_dir(path.join("src")).expect("create source directory");
        fs::write(path.join("src/lib.rs"), b"pub fn identity() -> u8 { 1 }\n")
            .expect("write source");
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

fn workspace(root: &TestDir) -> agz_rust_coder::workspace::WorkspaceRoot {
    let guard = Arc::new(
        RootGuard::new([root.path().to_owned()], std::iter::empty()).expect("create guard"),
    );
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot");
    snapshot.select(None).expect("select root")
}

fn identity_input<'a>(
    root: &'a TestDir,
    workspace: &'a agz_rust_coder::workspace::WorkspaceRoot,
    git: &'a FakeGit,
    limits: IdentityLimits,
) -> IdentityInput<'a> {
    let manifest: &'a Path = Box::leak(root.path().join("Cargo.toml").into_boxed_path());
    let command = Box::leak(vec![OsString::from("check")].into_boxed_slice());
    let environment = Box::leak(Box::new(BTreeMap::from([(
        OsString::from("HOME"),
        root.path().as_os_str().to_owned(),
    )])));
    IdentityInput::new(
        workspace,
        manifest,
        Path::new("/usr/bin/cargo"),
        command,
        environment,
        git,
    )
    .with_limits(limits)
}

#[test]
fn identity_hashes_source_and_changed_paths_with_a_complete_git_probe() {
    let root = TestDir::new("complete");
    fs::write(root.path().join("changed.txt"), b"changed").expect("write changed file");
    let git = FakeGit {
        status: 0,
        head: b"0123456789abcdef\n".to_vec(),
        changed: b"changed.txt\0".to_vec(),
        truncated: false,
    };
    let workspace = workspace(&root);
    let identity_input = identity_input(&root, &workspace, &git, IdentityLimits::default());

    let first = compute_input_identity(&identity_input).expect("compute identity");
    let second = compute_input_identity(&identity_input).expect("compute identity twice");

    assert!(first.complete);
    assert_eq!(first.hash, second.hash);
    assert_eq!(first.command_hash, second.command_hash);
    assert_eq!(first.head, "0123456789abcdef");
    assert_eq!(first.changed_paths, vec![PathBuf::from("changed.txt")]);
    assert!(first.files_hashed >= 3);
    assert!(first.bytes_hashed > 0);
}

#[test]
fn identity_budget_exhaustion_is_incomplete_and_never_silent() {
    let root = TestDir::new("budget");
    fs::write(root.path().join("changed.txt"), b"changed").expect("write changed file");
    let git = FakeGit {
        status: 0,
        head: b"head\n".to_vec(),
        changed: b"changed.txt\0".to_vec(),
        truncated: false,
    };
    let workspace = workspace(&root);
    let identity_input = identity_input(
        &root,
        &workspace,
        &git,
        IdentityLimits {
            max_files: 1,
            ..IdentityLimits::default()
        },
    );

    let identity = compute_input_identity(&identity_input).expect("compute bounded identity");
    assert!(!identity.complete);
    assert_eq!(
        identity.incomplete_reason,
        Some(IdentityIncompleteReason::FileBudget)
    );
    assert!(identity.files_hashed <= 1);
}

#[test]
fn git_failure_falls_back_to_a_complete_bounded_walk() {
    let root = TestDir::new("git-failure");
    let git = FakeGit {
        status: 128,
        head: Vec::new(),
        changed: Vec::new(),
        truncated: false,
    };
    let workspace = workspace(&root);
    let identity_input = identity_input(&root, &workspace, &git, IdentityLimits::default());

    let identity = compute_input_identity(&identity_input).expect("compute fallback identity");
    assert!(identity.complete);
    assert_eq!(identity.head, "NO_GIT");
    assert_eq!(identity.incomplete_reason, None);
}

#[cfg(unix)]
#[test]
fn cargo_home_config_rejects_a_symlinked_parent_directory() {
    use std::os::unix::fs::symlink;

    let root = TestDir::new("cargo-home-symlink-workspace");
    let home = TestDir::new("cargo-home-symlink-home");
    let outside = TestDir::new("cargo-home-symlink-outside");
    fs::create_dir(outside.path().join("cargo-config"))
        .expect("create external Cargo config directory");
    fs::write(
        outside.path().join("cargo-config/config.toml"),
        "[build]\nrustflags = []\n",
    )
    .expect("write external Cargo config");
    symlink(
        outside.path().join("cargo-config"),
        home.path().join(".cargo"),
    )
    .expect("create Cargo home symlink");
    let git = FakeGit {
        status: 128,
        head: Vec::new(),
        changed: Vec::new(),
        truncated: false,
    };
    let workspace = workspace(&root);
    let command = [OsString::from("check")];
    let environment = BTreeMap::from([(
        OsString::from("CARGO_HOME"),
        home.path().join(".cargo").into_os_string(),
    )]);
    let manifest = root.path().join("Cargo.toml");
    let input = IdentityInput::new(
        &workspace,
        &manifest,
        Path::new("/usr/bin/cargo"),
        &command,
        &environment,
        &git,
    );

    let identity = compute_input_identity(&input).expect("compute identity");

    assert!(!identity.complete);
    assert_eq!(
        identity.incomplete_reason,
        Some(IdentityIncompleteReason::Symlink)
    );
}

#[cfg(unix)]
#[test]
fn symlink_in_the_identity_walk_is_incomplete() {
    use std::os::unix::fs::symlink;

    let root = TestDir::new("symlink");
    let outside = TestDir::new("symlink-outside");
    fs::write(outside.path().join("secret.rs"), b"pub fn secret() {}\n")
        .expect("write outside source");
    symlink(
        outside.path().join("secret.rs"),
        root.path().join("linked.rs"),
    )
    .expect("create source link");
    let git = FakeGit {
        status: 0,
        head: b"head\n".to_vec(),
        changed: Vec::new(),
        truncated: false,
    };
    let workspace = workspace(&root);
    let identity_input = identity_input(&root, &workspace, &git, IdentityLimits::default());

    let identity = compute_input_identity(&identity_input).expect("compute symlink identity");
    assert!(!identity.complete);
    assert_eq!(
        identity.incomplete_reason,
        Some(IdentityIncompleteReason::Symlink)
    );
    assert_eq!(
        fs::read(outside.path().join("secret.rs")).unwrap(),
        b"pub fn secret() {}\n"
    );
}
