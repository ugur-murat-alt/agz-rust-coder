use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use agz_rust_coder::workspace::{
    ClientRoots, RootError, RootGuard, SelectionError, select_workspace,
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agz-rust-coder-selection-{label}-{stamp}"));
        fs::create_dir(&path).expect("create temporary root");
        fs::write(path.join(".git"), b"fixture worktree marker").expect("write worktree marker");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn manifest(&self, relative: &str, name: &str) -> PathBuf {
        let directory = self.path().join(relative);
        fs::create_dir_all(directory.join("src")).expect("create package directories");
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

fn snapshot(root: &TestDir) -> agz_rust_coder::workspace::RootSnapshot {
    RootGuard::new([root.path().to_owned()], std::iter::empty())
        .expect("create guard")
        .snapshot(ClientRoots::unsupported())
        .expect("create root snapshot")
}

#[test]
fn selects_the_nearest_ancestor_manifest() {
    let root = TestDir::new("ancestor");
    let manifest = root.manifest("package", "ancestor-package");
    let source_dir = root.path().join("package/src");

    let selection = select_workspace(&snapshot(&root), Some(&source_dir)).expect("select package");

    assert_eq!(selection.package_root(), manifest.parent().unwrap());
    assert_eq!(selection.manifest_path(), manifest);
    assert_eq!(selection.canonical_worktree(), root.path());
}

#[test]
fn selects_one_nested_project_when_it_is_unambiguous() {
    let root = TestDir::new("nested");
    let manifest = root.manifest("nested/project", "nested-project");

    let selection =
        select_workspace(&snapshot(&root), Some(root.path())).expect("select nested project");

    assert_eq!(selection.package_root(), manifest.parent().unwrap());
}

#[test]
fn rejects_same_depth_nested_projects_as_ambiguous() {
    let root = TestDir::new("ambiguous");
    root.manifest("left", "left-package");
    root.manifest("right", "right-package");

    let error =
        select_workspace(&snapshot(&root), Some(root.path())).expect_err("ambiguous selection");
    match error {
        SelectionError::Ambiguous { candidates } => {
            assert_eq!(candidates.len(), 2);
            assert!(candidates[0] < candidates[1]);
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
}

#[test]
fn does_not_cross_a_nested_git_worktree_boundary() {
    let root = TestDir::new("boundary");
    let linked = root.path().join("linked");
    fs::create_dir_all(&linked).expect("create linked worktree");
    fs::write(linked.join(".git"), b"linked worktree").expect("write linked marker");
    root.manifest("linked", "linked-package");

    let error =
        select_workspace(&snapshot(&root), Some(root.path())).expect_err("boundary selection");
    assert!(matches!(error, SelectionError::ManifestNotFound { .. }));
}

#[test]
fn rejects_relative_selection_and_parent_escape() {
    let root = TestDir::new("path-safety");
    root.manifest("package", "safe-package");
    let root_snapshot = snapshot(&root);

    assert!(matches!(
        root_snapshot.select(Some(Path::new("package"))),
        Err(RootError::RelativePath)
    ));
    assert!(matches!(
        root_snapshot.select(Some(&root.path().join("../outside"))),
        Err(RootError::ParentComponent)
    ));
}

#[test]
fn invalid_manifest_bytes_are_rejected_before_selection() {
    let root = TestDir::new("invalid-manifest");
    fs::write(root.path().join("Cargo.toml"), [0xff, 0xfe]).expect("write invalid manifest");

    let error =
        select_workspace(&snapshot(&root), Some(root.path())).expect_err("invalid manifest");
    assert!(matches!(error, SelectionError::InvalidManifest(_)));
}
