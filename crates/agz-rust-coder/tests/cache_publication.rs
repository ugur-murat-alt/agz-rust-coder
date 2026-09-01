#![allow(dead_code)]

#[path = "../src/cache/atomic.rs"]
mod atomic;

use atomic::{
    LockOptions, PublishError, PublishOptions, PublishOutcome, complete_marker_path,
    has_complete_marker, lock_path, publish, temporary_path, write_complete_marker,
};
use fs4::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        for attempt in 0..100 {
            let path = base.join(format!(
                "agz-rust-coder-{label}-{}-{attempt}",
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

fn options() -> PublishOptions<'static> {
    PublishOptions::default()
}

fn options_with_prefix(prefix: &'static str, stale_temp_age: Duration) -> PublishOptions<'static> {
    PublishOptions {
        stale_temp_age,
        temp_prefix: Some(prefix),
        ..PublishOptions::default()
    }
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn publishes_after_flush_validation_and_parent_sync_in_the_same_directory() {
    let root = TestRoot::new("same-directory");
    let final_path = root.path().join("artifact.bin");
    let expected_parent = final_path.parent().unwrap().to_path_buf();
    let validated_parent = Arc::new(std::sync::Mutex::new(None));
    let validated_parent_for_call = Arc::clone(&validated_parent);

    let outcome = publish(&final_path, b"validated artifact", options(), |temporary| {
        *validated_parent_for_call.lock().unwrap() = temporary.parent().map(Path::to_path_buf);
        assert_eq!(read(temporary), b"validated artifact");
        Ok::<_, &'static str>(())
    })
    .unwrap();

    assert!(matches!(outcome, PublishOutcome::Published { .. }));
    assert_eq!(read(&final_path), b"validated artifact");
    assert_eq!(*validated_parent.lock().unwrap(), Some(expected_parent));
    assert!(
        fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp."))
    );
}

#[test]
fn complete_marker_is_published_once_and_is_not_overwritten() {
    let root = TestRoot::new("complete-marker");
    let entry = root.path().join("entry");
    fs::create_dir(&entry).unwrap();

    let first = write_complete_marker(&entry, br#"{"fingerprint":"one"}"#, options()).unwrap();
    let second = write_complete_marker(&entry, br#"{"fingerprint":"two"}"#, options()).unwrap();

    assert!(matches!(first, PublishOutcome::Published { .. }));
    assert!(matches!(second, PublishOutcome::PreservedExisting { .. }));
    assert!(has_complete_marker(&entry));
    assert_eq!(
        read(&complete_marker_path(&entry)),
        br#"{"fingerprint":"one"}"#
    );
}

#[test]
fn second_writer_preserves_the_first_final() {
    let root = TestRoot::new("second-writer");
    let final_path = root.path().join("artifact");

    let first = publish(
        &final_path,
        b"first",
        options(),
        atomic::validate_regular_file,
    )
    .unwrap();
    let second = publish(
        &final_path,
        b"second",
        options(),
        atomic::validate_regular_file,
    )
    .unwrap();

    assert!(matches!(first, PublishOutcome::Published { .. }));
    assert!(matches!(second, PublishOutcome::PreservedExisting { .. }));
    assert_eq!(read(&final_path), b"first");
}

#[test]
fn racing_writers_publish_one_complete_final_without_partial_contents() {
    let root = TestRoot::new("race");
    let final_path = Arc::new(root.path().join("artifact"));
    let barrier = Arc::new(Barrier::new(3));
    let mut writers = Vec::new();

    for payload in [b"writer-a".as_slice(), b"writer-b".as_slice()] {
        let final_path = Arc::clone(&final_path);
        let barrier = Arc::clone(&barrier);
        let payload = payload.to_vec();
        writers.push(thread::spawn(move || {
            barrier.wait();
            publish(
                &*final_path,
                &payload,
                options(),
                atomic::validate_regular_file,
            )
        }));
    }
    barrier.wait();

    let results: Vec<_> = writers
        .into_iter()
        .map(|writer| writer.join().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(PublishOutcome::Published { .. })))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(PublishOutcome::PreservedExisting { .. })))
            .count(),
        1
    );
    let final_bytes = read(&final_path);
    assert!(final_bytes == b"writer-a" || final_bytes == b"writer-b");
}

#[test]
fn create_new_collision_moves_to_the_next_candidate_without_overwriting_it() {
    let root = TestRoot::new("collision");
    let final_path = root.path().join("artifact");
    let collision = temporary_path(&final_path, "collision", 0);
    let mut existing = File::create(&collision).unwrap();
    existing.write_all(b"collision survives").unwrap();

    let result = publish(
        &final_path,
        b"published",
        options_with_prefix("collision", Duration::from_secs(u64::MAX)),
        atomic::validate_regular_file,
    )
    .unwrap();

    assert!(matches!(result, PublishOutcome::Published { .. }));
    assert_eq!(read(&collision), b"collision survives");
    assert_eq!(read(&final_path), b"published");
}

#[test]
fn stale_temporary_from_another_writer_is_cleaned_before_publish() {
    let root = TestRoot::new("stale-temp");
    let final_path = root.path().join("artifact");
    let stale_temp = temporary_path(&final_path, "other", 0);
    fs::write(&stale_temp, b"stale writer").unwrap();

    publish(
        &final_path,
        b"published",
        options_with_prefix("current", Duration::ZERO),
        atomic::validate_regular_file,
    )
    .unwrap();

    assert!(!stale_temp.exists());
    assert_eq!(read(&final_path), b"published");
}

#[test]
fn failed_validation_removes_only_the_current_temporary_file() {
    let root = TestRoot::new("validation");
    let final_path = root.path().join("artifact");
    let other_temp = temporary_path(&final_path, "other", 0);
    fs::write(&other_temp, b"other writer owns this").unwrap();

    let result = publish(
        &final_path,
        b"rejected",
        options_with_prefix("validation", Duration::from_secs(u64::MAX)),
        |_| Err::<(), _>("invalid artifact"),
    );

    assert!(matches!(result, Err(PublishError::Validation { .. })));
    assert!(!temporary_path(&final_path, "validation", 0).exists());
    assert_eq!(read(&other_temp), b"other writer owns this");
    assert!(!final_path.exists());
}

#[test]
fn cancellation_and_deadline_abort_contended_lock_without_creating_a_temp() {
    let root = TestRoot::new("abort");
    let final_path = root.path().join("artifact");
    let held_lock_path = lock_path(&final_path);
    let held_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&held_lock_path)
        .unwrap();
    FileExt::lock(&held_lock).unwrap();

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_for_thread = Arc::clone(&cancelled);
    let final_path_for_thread = final_path.clone();
    let cancellation_writer = thread::spawn(move || {
        let probe = move || cancelled_for_thread.load(Ordering::Acquire);
        publish(
            &final_path_for_thread,
            b"cancelled",
            PublishOptions {
                cancellation: Some(&probe),
                temp_prefix: Some("cancelled"),
                lock: LockOptions {
                    max_attempts: 1_000,
                    initial_backoff: Duration::from_millis(2),
                    max_backoff: Duration::from_millis(5),
                },
                ..PublishOptions::default()
            },
            atomic::validate_regular_file,
        )
    });
    thread::sleep(Duration::from_millis(20));
    cancelled.store(true, Ordering::Release);
    assert!(matches!(
        cancellation_writer.join().unwrap(),
        Err(PublishError::Cancelled)
    ));
    assert!(!temporary_path(&final_path, "cancelled", 0).exists());

    let deadline = Instant::now() + Duration::from_millis(20);
    let deadline_result = publish(
        &final_path,
        b"deadline",
        PublishOptions {
            deadline: Some(deadline),
            lock: LockOptions {
                max_attempts: 1_000,
                initial_backoff: Duration::from_millis(2),
                max_backoff: Duration::from_millis(5),
            },
            ..PublishOptions::default()
        },
        atomic::validate_regular_file,
    );
    assert!(matches!(
        deadline_result,
        Err(PublishError::DeadlineExceeded)
    ));
    FileExt::unlock(&held_lock).unwrap();
}

#[cfg(unix)]
#[test]
fn stale_final_symlink_is_unlinked_but_its_target_survives() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("final-symlink");
    let target = root.path().join("target");
    let final_path = root.path().join("artifact");
    fs::write(&target, b"keep target").unwrap();
    symlink(&target, &final_path).unwrap();

    publish(
        &final_path,
        b"new final",
        options(),
        atomic::validate_regular_file,
    )
    .unwrap();

    assert_eq!(read(&target), b"keep target");
    assert_eq!(read(&final_path), b"new final");
    assert!(fs::symlink_metadata(&final_path).unwrap().is_file());
}

#[cfg(unix)]
#[test]
fn failed_validation_does_not_remove_a_stale_final_symlink() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("failed-final-symlink");
    let target = root.path().join("target");
    let final_path = root.path().join("artifact");
    fs::write(&target, b"keep target").unwrap();
    symlink(&target, &final_path).unwrap();

    let result = publish(
        &final_path,
        b"rejected",
        options_with_prefix("failed", Duration::from_secs(u64::MAX)),
        |_| Err::<(), _>("invalid artifact"),
    );

    assert!(matches!(result, Err(PublishError::Validation { .. })));
    assert!(
        fs::symlink_metadata(&final_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read(&target), b"keep target");
    assert!(!temporary_path(&final_path, "failed", 0).exists());
}

#[cfg(unix)]
#[test]
fn stale_temp_symlink_cleanup_unlinks_only_the_link_target_is_preserved() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("temp-symlink");
    let target = root.path().join("target");
    let final_path = root.path().join("artifact");
    let stale_temp = temporary_path(&final_path, "stale", 0);
    fs::write(&target, b"keep target").unwrap();
    symlink(&target, &stale_temp).unwrap();

    publish(
        &final_path,
        b"published",
        options_with_prefix("stale", Duration::ZERO),
        atomic::validate_regular_file,
    )
    .unwrap();

    assert_eq!(read(&target), b"keep target");
    assert_eq!(read(&final_path), b"published");
    assert!(!stale_temp.exists());
}

#[cfg(unix)]
#[test]
fn complete_marker_reader_rejects_symlinked_entries() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("marker-entry-symlink");
    let outside = TestRoot::new("marker-entry-symlink-outside");
    let outside_entry = outside.path().join("entry");
    fs::create_dir(&outside_entry).unwrap();
    fs::write(complete_marker_path(&outside_entry), b"complete").unwrap();

    let linked_entry = root.path().join("linked-entry");
    symlink(&outside_entry, &linked_entry).unwrap();

    assert!(has_complete_marker(&outside_entry));
    assert!(!has_complete_marker(&linked_entry));
}

#[cfg(unix)]
#[test]
fn parent_swap_after_validation_fails_closed_without_writing_outside() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("parent-swap");
    let outside = TestRoot::new("parent-swap-outside");
    let moved = root.path().with_extension("moved");
    let final_path = root.path().join("artifact");
    let old_parent = root.path().to_path_buf();
    let moved_for_validation = moved.clone();
    let outside_for_validation = outside.path().to_path_buf();

    let result = publish(&final_path, b"must not escape", options(), move |_| {
        fs::rename(&old_parent, &moved_for_validation)?;
        symlink(&outside_for_validation, &old_parent)?;
        Ok::<_, std::io::Error>(())
    });

    assert!(matches!(result, Err(PublishError::Symlink { .. })));
    assert!(!outside.path().join("artifact").exists());
    assert!(!moved.join("artifact").exists());
    assert!(
        fs::read_dir(&moved)
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp."))
    );

    fs::remove_file(root.path()).unwrap();
    fs::remove_dir_all(moved).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinked_parent_and_lock_are_rejected_without_following_external_paths() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("path-check");
    let outside = TestRoot::new("path-check-outside");
    let linked_parent = root.path().join("linked");
    symlink(outside.path(), &linked_parent).unwrap();
    let escaped = linked_parent.join("artifact");
    let escaped_result = publish(
        &escaped,
        b"escape",
        options(),
        atomic::validate_regular_file,
    );
    assert!(matches!(escaped_result, Err(PublishError::Symlink { .. })));
    assert!(!outside.path().join("artifact").exists());

    let final_path = root.path().join("locked-artifact");
    let external_lock = outside.path().join("external-lock");
    fs::write(&external_lock, b"lock target").unwrap();
    symlink(&external_lock, lock_path(&final_path)).unwrap();
    let lock_result = publish(
        &final_path,
        b"blocked",
        options(),
        atomic::validate_regular_file,
    );
    assert!(matches!(lock_result, Err(PublishError::Symlink { .. })));
    assert_eq!(read(&external_lock), b"lock target");
}

#[test]
fn non_regular_final_is_rejected_before_a_temporary_is_created() {
    let root = TestRoot::new("non-regular");
    let final_path = root.path().join("artifact");
    fs::create_dir(&final_path).unwrap();

    let result = publish(
        &final_path,
        b"data",
        options(),
        atomic::validate_regular_file,
    );

    assert!(matches!(result, Err(PublishError::NotRegularFile { .. })));
    assert!(final_path.is_dir());
    assert!(
        fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp."))
    );
}
