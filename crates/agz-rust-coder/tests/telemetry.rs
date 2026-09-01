use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
    time::SystemTime,
};

use agz_rust_coder::{config::TelemetryConfig, telemetry::ActivityLog};
use fs4::FileExt;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "agz-rust-coder-telemetry-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create telemetry temp root");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config(path: &Path) -> TelemetryConfig {
    TelemetryConfig {
        enabled: true,
        path: path.to_owned(),
        retention_bytes: 1,
        retention_days: 7,
        max_archives: 4,
    }
}

fn hold_lock(path: &Path) -> File {
    let lock_path = path.with_file_name("activity.jsonl.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open telemetry lock");
    FileExt::lock(&lock).expect("hold telemetry lock");
    lock
}

fn all_activity(path: &Path) -> String {
    let mut contents = String::new();
    for candidate in [
        path.to_owned(),
        path.with_file_name("activity.jsonl.1"),
        path.with_file_name("activity.jsonl.2"),
        path.with_file_name("activity.jsonl.3"),
        path.with_file_name("activity.jsonl.4"),
    ] {
        if let Ok(value) = fs::read_to_string(candidate) {
            contents.push_str(&value);
        }
    }
    contents
}

#[test]
fn independent_activity_logs_retry_contended_rotation() {
    let root = TempRoot::new();
    let path = root.0.join("activity.jsonl");
    let first = ActivityLog::new(&config(&path)).expect("create first log");
    let second = ActivityLog::new(&config(&path)).expect("create second log");
    first
        .record("seed", None, None, None, Some("seed-request"))
        .expect("seed activity");
    let held_lock = hold_lock(&path);
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = Arc::clone(&barrier);
    let first_thread = thread::spawn(move || {
        first_barrier.wait();
        first.record("rotate-first", None, None, None, Some("first-request"))
    });
    let second_barrier = Arc::clone(&barrier);
    let second_thread = thread::spawn(move || {
        second_barrier.wait();
        second.record("rotate-second", None, None, None, Some("second-request"))
    });
    barrier.wait();
    thread::sleep(std::time::Duration::from_millis(100));
    FileExt::unlock(&held_lock).expect("release telemetry lock");

    first_thread
        .join()
        .expect("join first telemetry writer")
        .expect("first rotation record");
    second_thread
        .join()
        .expect("join second telemetry writer")
        .expect("second rotation record");

    let contents = all_activity(&path);
    assert!(contents.contains("rotate-first"), "{contents}");
    assert!(contents.contains("rotate-second"), "{contents}");
}

#[test]
fn independent_activity_logs_retry_contended_flush() {
    let root = TempRoot::new();
    let path = root.0.join("activity.jsonl");
    let first = ActivityLog::new(&config(&path)).expect("create first log");
    let second = ActivityLog::new(&config(&path)).expect("create second log");
    first
        .record("seed", None, None, None, Some("seed-request"))
        .expect("seed activity");
    let held_lock = hold_lock(&path);
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = Arc::clone(&barrier);
    let first_thread = thread::spawn(move || {
        first_barrier.wait();
        first.flush()
    });
    let second_barrier = Arc::clone(&barrier);
    let second_thread = thread::spawn(move || {
        second_barrier.wait();
        second.flush()
    });
    barrier.wait();
    thread::sleep(std::time::Duration::from_millis(100));
    FileExt::unlock(&held_lock).expect("release telemetry lock");

    first_thread
        .join()
        .expect("join first telemetry flusher")
        .expect("first flush");
    second_thread
        .join()
        .expect("join second telemetry flusher")
        .expect("second flush");
}
