//! Durable scratch store for revision-bound changesets.
//!
//! Layout (all under the configured `change.scratch_dir`):
//!
//! ```text
//! <scratch>/<change-id>/record.json      bounded change record
//! <scratch>/<change-id>/manifest.json    base capture manifest (path/sha/bytes)
//! <scratch>/<change-id>/candidate/       the only tree any patch touches
//! <scratch>/<change-id>/cache/           optional per-change validation cache
//! <scratch>/<change-id>/lock             cross-process advisory lock
//! ```
//!
//! The store never follows symlinks while removing scratch entries and removes
//! partial or expired changes during service construction.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fs4::{FileExt, TryLockError};

use crate::config::ChangeConfig;

use super::model::{CHANGE_SCHEMA_VERSION, ChangeRecord, RecordState};

pub(crate) const RECORD_FILE: &str = "record.json";
pub(crate) const MANIFEST_FILE: &str = "manifest.json";
pub(crate) const CANDIDATE_DIR: &str = "candidate";
pub(crate) const CACHE_DIR: &str = "cache";
pub(crate) const LOCK_FILE: &str = "lock";
pub(crate) const MAX_CLEANUP_WARNINGS: usize = 32;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct ChangeStore {
    root: PathBuf,
    max_active: u64,
    ttl_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct StoreSweep {
    pub removed: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct StoreRecovery {
    /// Orphan `Applying` records marked `FailedInconsistent`.
    pub failed: u64,
    /// Unpublished `Capturing` scratch directories removed.
    pub removed: u64,
    pub warnings: Vec<String>,
}

impl ChangeStore {
    pub(crate) fn new(config: &ChangeConfig) -> Result<Self, String> {
        let root = absolute_scratch_root(&config.scratch_dir)?;
        fs::create_dir_all(&root).map_err(|error| {
            format!(
                "could not create change scratch {}: {error}",
                root.display()
            )
        })?;
        if fs::symlink_metadata(&root)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(true)
        {
            return Err(format!(
                "change scratch {} is a symlink or unreadable",
                root.display()
            ));
        }
        Ok(Self {
            root,
            max_active: config.max_active,
            ttl_ms: config.ttl_ms,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn max_active(&self) -> u64 {
        self.max_active
    }

    pub(crate) fn change_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub(crate) fn candidate_dir(&self, id: &str) -> PathBuf {
        self.change_dir(id).join(CANDIDATE_DIR)
    }

    pub(crate) fn cache_dir(&self, id: &str) -> PathBuf {
        self.change_dir(id).join(CACHE_DIR)
    }

    pub(crate) fn manifest_path(&self, id: &str) -> PathBuf {
        self.change_dir(id).join(MANIFEST_FILE)
    }

    pub(crate) fn record_path(&self, id: &str) -> PathBuf {
        self.change_dir(id).join(RECORD_FILE)
    }

    pub(crate) fn lock_path(&self, id: &str) -> PathBuf {
        self.change_dir(id).join(LOCK_FILE)
    }

    pub(crate) fn load(&self, id: &str) -> Result<Option<ChangeRecord>, String> {
        if !is_valid_change_id(id) {
            return Err("invalid change id".to_owned());
        }
        let path = self.record_path(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let record: ChangeRecord = serde_json::from_slice(&bytes)
            .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
        if record.schema_version != CHANGE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported change record schema {}",
                record.schema_version
            ));
        }
        Ok(Some(record))
    }

    pub(crate) fn save(&self, record: &mut ChangeRecord) -> Result<(), String> {
        if !is_valid_change_id(&record.id) {
            return Err("invalid change id".to_owned());
        }
        let dir = self.change_dir(&record.id);
        fs::create_dir_all(&dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        record.updated_at_ms = now_ms();
        let bytes = serde_json::to_vec(record)
            .map_err(|error| format!("could not serialize change record: {error}"))?;
        write_atomic(&self.record_path(&record.id), &bytes)
    }

    pub(crate) fn write_manifest(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        write_atomic(&self.manifest_path(id), bytes)
    }

    /// Removes one scratch subtree without ever following a symlink. A top-level
    /// symlink is unlinked, not traversed.
    pub(crate) fn remove_path(&self, path: &Path) -> Result<(), String> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("could not stat {}: {error}", path.display())),
        };
        let result = if metadata.file_type().is_symlink() {
            fs::remove_file(path)
        } else if metadata.is_dir() {
            // std does not follow symlinked entries while removing a directory.
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        result.map_err(|error| format!("could not remove {}: {error}", path.display()))
    }

    /// Removes the candidate/cache copy while keeping the bounded record for
    /// inspect/idempotent discard until the TTL sweep removes it.
    pub(crate) fn discard_worktree(&self, id: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        for path in [self.candidate_dir(id), self.cache_dir(id)] {
            if let Err(error) = self.remove_path(&path) {
                warnings.push(truncate(&error, 512));
            }
        }
        warnings
    }

    pub(crate) fn active_count(&self) -> u64 {
        self.scan()
            .into_iter()
            .filter(|(_, record)| {
                record
                    .as_ref()
                    .is_some_and(|record| !matches!(record.state, RecordState::Discarded))
            })
            .count()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    /// Removes orphan, partial, and expired change directories.
    ///
    /// Every candidate entry is probed with `try_lock` first: a change whose
    /// per-change lock is held by a live writer is never removed, even when its
    /// recorded timestamp looks expired. Age and state are re-read under the
    /// lock so an in-flight capture or stage cannot be swept mid-write.
    pub(crate) fn sweep(&self) -> StoreSweep {
        let mut sweep = StoreSweep::default();
        for (path, _) in self.scan_paths() {
            let Some(id) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            // Read the on-disk age before `try_lock` creates/reopens the lock
            // file, which can itself refresh the directory mtime.
            let stale_directory = self.directory_expired(&path);
            let lock = match self.try_lock(&id) {
                Ok(Some(lock)) => lock,
                // A live process owns this change; never sweep it.
                Ok(None) => continue,
                Err(error) => {
                    self.push_warning(&mut sweep.warnings, &error);
                    continue;
                }
            };
            let record = match self.load(&id) {
                Ok(record) => record,
                Err(error) => {
                    self.push_warning(&mut sweep.warnings, &error);
                    continue;
                }
            };
            let expired = match &record {
                Some(record) => record
                    .updated_at_ms
                    .saturating_add(self.ttl_ms)
                    .le(&now_ms()),
                // A directory without any record is only removed once its
                // directory mtime shows it predates the TTL; a fresh directory
                // may still be a capture that has not published its marker.
                None => stale_directory,
            };
            if !expired {
                continue;
            }
            match self.remove_change_locked(&id, lock) {
                Ok(()) => sweep.removed = sweep.removed.saturating_add(1),
                Err(error) => self.push_warning(&mut sweep.warnings, &error),
            }
        }
        sweep
    }

    /// Recovers scratch entries left behind by an interrupted server process.
    ///
    /// An orphan `Applying` record is never silently returned to `Ready`:
    /// candidate bytes may already differ from the recorded revision, so it is
    /// marked `FailedInconsistent`. An unpublished `Capturing` directory is an
    /// incomplete capture with no user-visible id and is removed.
    pub(crate) fn recover_interrupted(&self) -> StoreRecovery {
        let mut recovery = StoreRecovery::default();
        for (path, _) in self.scan_paths() {
            let Some(id) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let lock = match self.try_lock(&id) {
                Ok(Some(lock)) => lock,
                Ok(None) => continue,
                Err(error) => {
                    self.push_warning(&mut recovery.warnings, &error);
                    continue;
                }
            };
            let record = match self.load(&id) {
                Ok(record) => record,
                Err(error) => {
                    self.push_warning(&mut recovery.warnings, &error);
                    continue;
                }
            };
            match record {
                Some(mut record) if record.state == RecordState::Applying => {
                    let target = record
                        .applying_revision
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
                    record.state = RecordState::FailedInconsistent;
                    record.applying_revision = None;
                    record.evidence.iter_mut().for_each(|evidence| {
                        evidence.fresh = false;
                        evidence.authoritative = false;
                    });
                    if record.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
                        record.cleanup_warnings.push(format!(
                            "an interrupted stage (target revision {target}) was recovered as failed_inconsistent"
                        ));
                    }
                    match self.save(&mut record) {
                        Ok(()) => recovery.failed = recovery.failed.saturating_add(1),
                        Err(error) => self.push_warning(&mut recovery.warnings, &error),
                    }
                }
                Some(record) if record.state == RecordState::Capturing => {
                    drop(lock);
                    match self.remove_path(&path) {
                        Ok(()) => recovery.removed = recovery.removed.saturating_add(1),
                        Err(error) => self.push_warning(&mut recovery.warnings, &error),
                    }
                }
                _ => {}
            }
        }
        recovery
    }

    fn push_warning(&self, warnings: &mut Vec<String>, error: &str) {
        if warnings.len() < MAX_CLEANUP_WARNINGS {
            warnings.push(truncate(error, 512));
        }
    }

    fn directory_expired(&self, path: &Path) -> bool {
        let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
            // Unverifiable age must never authorize deletion.
            return false;
        };
        modified
            .elapsed()
            .is_ok_and(|elapsed| elapsed.as_millis() >= u128::from(self.ttl_ms))
    }

    /// Removes a change subtree while its lock is held: everything except the
    /// open lock file first, then the lock is released and the directory (with
    /// the lock file) is removed. Reporting a failure still leaves the record
    /// in place for a later sweep.
    fn remove_change_locked(&self, id: &str, lock: ChangeLock) -> Result<(), String> {
        let dir = self.change_dir(id);
        let entries = fs::read_dir(&dir)
            .map_err(|error| format!("could not read {}: {error}", dir.display()))?;
        for entry in entries.flatten() {
            if entry.path() == dir.join(LOCK_FILE) {
                continue;
            }
            self.remove_path(&entry.path())?;
        }
        drop(lock);
        self.remove_path(&dir)
    }

    fn scan(&self) -> Vec<(PathBuf, Option<ChangeRecord>)> {
        self.scan_paths()
            .into_iter()
            .map(|(path, record)| {
                let id = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (path, record.or_else(|| self.load(&id).ok().flatten()))
            })
            .collect()
    }

    fn scan_paths(&self) -> Vec<(PathBuf, Option<ChangeRecord>)> {
        let mut entries = Vec::new();
        let Ok(directory) = fs::read_dir(&self.root) else {
            return entries;
        };
        for entry in directory.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if !is_valid_change_id(&name) {
                continue;
            }
            let record = self.load(&name).ok().flatten();
            entries.push((path, record));
        }
        entries
    }

    /// Opens and exclusively locks the per-change lock file. Contention from a
    /// second server process returns `Ok(None)` instead of blocking forever.
    pub(crate) fn try_lock(&self, id: &str) -> Result<Option<ChangeLock>, String> {
        let path = self.lock_path(id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Some(ChangeLock { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => {
                Err(format!("could not lock {}: {error}", path.display()))
            }
        }
    }
}

/// Held for the duration of one mutating change action.
#[derive(Debug)]
pub(crate) struct ChangeLock {
    file: File,
}

impl Drop for ChangeLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub(crate) fn is_valid_change_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.starts_with(super::model::CHANGE_ID_PREFIX)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "artifact".to_owned());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|error| format!("could not create {}: {error}", temp.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(format!("could not write {}: {error}", temp.display()));
    }
    drop(file);
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("could not publish {}: {error}", path.display())
    })
}

pub(crate) fn absolute_scratch_root(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("could not read current directory: {error}"))?
            .join(path)
    };
    Ok(normalize_lexical(&absolute))
}

pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::model::{CaptureSummary, RecordState};
    use crate::config::ChangeConfig;

    fn test_config(root: &Path) -> ChangeConfig {
        ChangeConfig {
            scratch_dir: root.to_owned(),
            max_active: 4,
            max_files: 1_000,
            max_bytes: 1_048_576,
            ttl_ms: 60_000,
            max_revisions: 8,
        }
    }

    fn record(id: &str, state: RecordState, updated_at_ms: u64) -> ChangeRecord {
        ChangeRecord {
            schema_version: CHANGE_SCHEMA_VERSION,
            id: id.to_owned(),
            created_at_ms: updated_at_ms,
            updated_at_ms,
            workspace_root: PathBuf::from("/workspace"),
            capture_root: PathBuf::from("/workspace"),
            workspace_epoch: 1,
            candidate_epoch: 1,
            base_identity: "base".to_owned(),
            state,
            capture: CaptureSummary {
                files: 0,
                bytes: 0,
                manifest_hash: "hash".to_owned(),
                complete: true,
                excluded: Vec::new(),
            },
            external_paths: Vec::new(),
            dependency_roots: Vec::new(),
            revision: 0,
            applying_revision: None,
            patches: Vec::new(),
            new_files: Vec::new(),
            changed_files: Vec::new(),
            candidate_files: 0,
            candidate_bytes: 0,
            patch_hash: String::new(),
            source_hashes: std::collections::BTreeMap::new(),
            evidence: Vec::new(),
            migration: None,
            cleanup_warnings: Vec::new(),
        }
    }

    #[test]
    fn store_round_trips_records_and_rejects_invalid_ids() {
        let base = std::env::temp_dir().join(format!(
            "agz-change-store-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = ChangeStore::new(&test_config(&base)).expect("create store");
        assert!(store.load("../../escape").is_err());
        assert!(store.load("not-a-change").is_err());

        let mut stored = record("ch-1-1", RecordState::Ready, now_ms());
        fs::create_dir_all(store.change_dir(&stored.id)).expect("create change dir");
        store.save(&mut stored).expect("save record");
        let loaded = store.load("ch-1-1").expect("load").expect("present");
        assert_eq!(loaded.id, "ch-1-1");
        assert_eq!(store.active_count(), 1);
        fs::remove_dir_all(&base).expect("cleanup");
    }

    fn write_record(store: &ChangeStore, record: &ChangeRecord) {
        fs::create_dir_all(store.change_dir(&record.id)).expect("create change dir");
        write_atomic(
            &store.record_path(&record.id),
            &serde_json::to_vec(record).expect("serialize record"),
        )
        .expect("write record");
    }

    fn make_stale(path: &Path) {
        let file = File::open(path).expect("open directory for mtime");
        file.set_modified(std::time::UNIX_EPOCH)
            .expect("set stale mtime");
    }

    #[test]
    fn sweep_removes_only_unlocked_expired_or_stale_scratch() {
        let base = std::env::temp_dir().join(format!(
            "agz-change-sweep-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let config = ChangeConfig {
            ttl_ms: 1_000,
            ..test_config(&base)
        };
        let store = ChangeStore::new(&config).expect("create store");

        // Stale recordless orphan: swept.
        fs::create_dir_all(store.change_dir("ch-1-1")).expect("orphan dir");
        make_stale(&store.change_dir("ch-1-1"));
        // Fresh recordless directory: kept (may be a capture about to publish).
        fs::create_dir_all(store.change_dir("ch-1-2")).expect("fresh dir");
        // Expired record: swept.
        write_record(&store, &record("ch-2-2", RecordState::Ready, 1));
        // Current record: kept.
        let mut current = record("ch-3-3", RecordState::Ready, now_ms());
        store.save(&mut current).expect("save current");
        // Capturing placeholder with a current timestamp: kept.
        write_record(&store, &record("ch-4-4", RecordState::Capturing, now_ms()));
        // Expired record held by a live independent store handle (the same
        // flock contention a second server process would cause): never swept.
        write_record(&store, &record("ch-5-5", RecordState::Ready, 1));
        let second_store = ChangeStore::new(&config).expect("second store handle");
        let live = second_store
            .try_lock("ch-5-5")
            .expect("open live lock")
            .expect("acquire live lock");

        let sweep = store.sweep();
        assert_eq!(sweep.removed, 2, "{sweep:?}");
        assert!(!store.change_dir("ch-1-1").exists());
        assert!(!store.change_dir("ch-2-2").exists());
        assert!(store.change_dir("ch-1-2").exists(), "fresh dir swept");
        assert!(store.change_dir("ch-3-3").exists(), "current record swept");
        assert!(
            store.change_dir("ch-4-4").exists(),
            "capturing placeholder swept"
        );
        assert!(store.change_dir("ch-5-5").exists(), "live lock swept");

        drop(live);
        let sweep = store.sweep();
        assert_eq!(sweep.removed, 1, "{sweep:?}");
        assert!(!store.change_dir("ch-5-5").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn try_lock_reports_contention_from_an_independent_handle() {
        let base = std::env::temp_dir().join(format!(
            "agz-change-lock-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = ChangeStore::new(&test_config(&base)).expect("create store");
        let second = ChangeStore::new(&test_config(&base)).expect("create second store");
        let first = store
            .try_lock("ch-1-1")
            .expect("open first")
            .expect("acquire first");
        assert!(
            second.try_lock("ch-1-1").expect("open second").is_none(),
            "an independent handle must observe contention"
        );
        drop(first);
        assert!(
            second.try_lock("ch-1-1").expect("open second").is_some(),
            "lock must be reusable after release"
        );
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn recover_marks_orphan_applying_failed_inconsistent() {
        let base = std::env::temp_dir().join(format!(
            "agz-change-recover-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = ChangeStore::new(&test_config(&base)).expect("create store");
        let mut applying = record("ch-1-1", RecordState::Applying, now_ms());
        applying.applying_revision = Some(2);
        write_record(&store, &applying);

        let recovery = store.recover_interrupted();
        assert_eq!(recovery.failed, 1, "{recovery:?}");
        let loaded = store.load("ch-1-1").expect("load").expect("present");
        assert_eq!(loaded.state, RecordState::FailedInconsistent);
        assert_eq!(loaded.applying_revision, None);
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn recover_removes_unpublished_capturing_scratch() {
        let base = std::env::temp_dir().join(format!(
            "agz-change-capturing-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = ChangeStore::new(&test_config(&base)).expect("create store");
        write_record(&store, &record("ch-1-1", RecordState::Capturing, now_ms()));
        fs::create_dir_all(store.candidate_dir("ch-1-1")).expect("partial candidate");

        let recovery = store.recover_interrupted();
        assert_eq!(recovery.removed, 1, "{recovery:?}");
        assert!(!store.change_dir("ch-1-1").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn discard_does_not_follow_symlinked_entries() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "agz-change-symlink-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let victim = base.join("victim");
        fs::create_dir_all(&victim).expect("create victim");
        fs::write(victim.join("keep.txt"), b"keep").expect("write victim file");
        let config = test_config(&base.join("scratch"));
        let store = ChangeStore::new(&config).expect("create store");
        let candidate = store.candidate_dir("ch-1-1");
        fs::create_dir_all(store.change_dir("ch-1-1")).expect("create change dir");
        symlink(&victim, &candidate).expect("create candidate symlink");

        let warnings = store.discard_worktree("ch-1-1");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(victim.join("keep.txt").is_file(), "victim must survive");
        assert!(!candidate.exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
