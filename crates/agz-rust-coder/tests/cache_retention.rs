#![allow(dead_code)]

#[path = "../src/cache/atomic.rs"]
mod atomic;
#[path = "../src/cache/retention.rs"]
mod retention;

use retention::{OwnedCacheRetention, RetentionLimits, RetentionReport};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir();
        for attempt in 0..100 {
            let path = base.join(format!(
                "agz-rust-coder-retention-{label}-{}-{attempt}",
                std::process::id()
            ));
            if fs::create_dir(&path).is_ok() {
                return Self(path);
            }
        }
        panic!("could not create temporary test directory");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn limits(
    max_age: Duration,
    max_bytes: u64,
    max_entries: usize,
    max_nodes: usize,
) -> RetentionLimits {
    RetentionLimits {
        max_age,
        max_bytes,
        max_entries,
        max_nodes,
    }
}

fn file_entry(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let entry = root.join(name);
    fs::create_dir(&entry).unwrap();
    fs::write(entry.join("data"), bytes).unwrap();
    fs::write(entry.join(atomic::COMPLETE_MARKER), b"complete").unwrap();
    entry
}

#[test]
fn age_pruning_keeps_active_leases() {
    let root = TestRoot::new("age-active");
    let active = file_entry(root.path(), "active", b"active");
    let stale = file_entry(root.path(), "stale", b"stale");
    let retention =
        OwnedCacheRetention::with_limits(root.path(), limits(Duration::ZERO, u64::MAX, 10, 100));
    let _lease = retention.lease(&active);

    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert!(active.exists());
    assert!(!stale.exists());
    assert_eq!(report.removed, 1);
    assert_eq!(report.retained, 1);
}

#[test]
fn lru_entry_budget_removes_oldest_and_touch_makes_newest() {
    let root = TestRoot::new("lru");
    let old = file_entry(root.path(), "old", b"old");
    let new = file_entry(root.path(), "new", b"new");
    let retention =
        OwnedCacheRetention::with_limits(root.path(), limits(Duration::MAX, u64::MAX, 1, 100));
    assert!(retention.touch(&new));

    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert!(!old.exists());
    assert!(new.exists());
    assert_eq!(report.removed, 1);
    assert_eq!(report.retained, 1);
}

#[test]
fn byte_and_node_budgets_account_for_nested_regular_nodes() {
    let root = TestRoot::new("budgets");
    let old = file_entry(root.path(), "old", b"1234");
    let new = file_entry(root.path(), "new", b"5678");
    let nested = new.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("data"), b"nested").unwrap();
    let retention = OwnedCacheRetention::with_limits(root.path(), limits(Duration::MAX, 18, 10, 5));
    assert!(retention.touch(&new));

    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert!(!old.exists());
    assert!(new.exists());
    assert_eq!(report.bytes, 18);
    assert_eq!(report.nodes, 5);
    assert_eq!(report.removed, 1);
}

#[cfg(unix)]
#[test]
fn direct_symlinks_are_ignored_and_nested_symlinks_keep_their_entry() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("symlinks");
    let outside = TestRoot::new("symlinks-outside");
    let outside_file = outside.path().join("outside");
    fs::write(&outside_file, b"outside data").unwrap();

    let direct_link = root.path().join("direct-link");
    symlink(&outside_file, &direct_link).unwrap();
    let nested_entry = file_entry(root.path(), "nested", b"cache");
    let nested_link = nested_entry.join("linked");
    symlink(&outside_file, &nested_link).unwrap();

    let retention = OwnedCacheRetention::with_limits(root.path(), limits(Duration::ZERO, 0, 0, 0));
    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert!(direct_link.is_symlink());
    assert!(nested_link.is_symlink());
    assert!(nested_entry.exists());
    assert_eq!(fs::read(&outside_file).unwrap(), b"outside data");
    assert_eq!(report.removed, 0);
    assert_eq!(report.retained, 1);
}

#[test]
fn only_plugin_owned_direct_children_are_considered() {
    let root = TestRoot::new("scope");
    let outside = TestRoot::new("scope-outside");
    let owned = file_entry(root.path(), "owned", b"owned");
    let external = file_entry(outside.path(), "external", b"external");
    let retention =
        OwnedCacheRetention::with_limits(root.path(), limits(Duration::ZERO, u64::MAX, 10, 100));
    let _invalid_lease = retention.lease(outside.path().join("external"));
    let _owned_lease = retention.lease(&owned);

    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert!(owned.exists());
    assert!(external.exists());
    assert_eq!(report.removed, 0);
}

#[test]
fn reserved_lock_and_temp_names_are_not_retention_candidates() {
    let root = TestRoot::new("reserved");
    let lock = root.path().join(".artifact.lock");
    let temporary = root.path().join(".artifact.tmp.writer-0");
    let marker = root.path().join(".complete.json");
    fs::write(&lock, b"lock").unwrap();
    fs::write(&temporary, b"temp").unwrap();
    fs::write(&marker, b"marker").unwrap();
    let retention = OwnedCacheRetention::with_limits(root.path(), limits(Duration::ZERO, 0, 0, 0));

    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert_eq!(report, RetentionReport::default());
    assert!(lock.exists());
    assert!(temporary.exists());
    assert!(marker.exists());
}

#[test]
fn incomplete_entries_are_ignored_by_retention() {
    let root = TestRoot::new("incomplete");
    let incomplete = root.path().join("incomplete");
    fs::create_dir(&incomplete).unwrap();
    fs::write(incomplete.join("data"), b"partial").unwrap();
    let complete = file_entry(root.path(), "complete", b"ready");
    let retention = OwnedCacheRetention::with_limits(root.path(), limits(Duration::ZERO, 0, 0, 0));

    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert!(incomplete.exists());
    assert!(!complete.exists());
    assert_eq!(report.removed, 1);
    assert_eq!(report.retained, 0);
}

#[cfg(unix)]
#[test]
fn symlinked_root_is_not_followed_or_pruned() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("root-link");
    let actual = TestRoot::new("root-link-actual");
    let entry = file_entry(actual.path(), "entry", b"keep");
    let linked_root = root.path().join("linked-root");
    symlink(actual.path(), &linked_root).unwrap();
    let retention = OwnedCacheRetention::with_limits(&linked_root, limits(Duration::ZERO, 0, 0, 0));

    let report = retention.prune_at(SystemTime::now() + Duration::from_secs(1));

    assert_eq!(report, RetentionReport::default());
    assert!(entry.exists());
}
