//! Crash-safe publication primitives for server-owned cache artifacts.
//!
//! The caller owns the cache root and is responsible for choosing a path under
//! that root. This module adds the last-mile checks needed by a cache writer:
//! no symlinked parent components, a per-artifact advisory lock, a unique
//! same-directory temporary file, validation before publication, and cleanup
//! of only the current writer's temporary file.

#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, OpenOptions as CapabilityOpenOptions};
use fs4::{FileExt, TryLockError};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

/// The marker written after an artifact has passed its caller-owned validation.
pub const COMPLETE_MARKER: &str = ".complete.json";

const DEFAULT_LOCK_ATTEMPTS: usize = 32;
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(2);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_millis(50);
const DEFAULT_STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_TEMP_ATTEMPTS: u64 = 64;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ACTIVE_LOCKS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// A cancellation source that can be checked without coupling this primitive
/// to a particular async runtime.
pub trait CancellationProbe {
    /// Returns `true` when the caller no longer wants the operation to run.
    fn is_cancelled(&self) -> bool;
}

impl CancellationProbe for std::sync::atomic::AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::Acquire)
    }
}

impl<F> CancellationProbe for F
where
    F: Fn() -> bool,
{
    fn is_cancelled(&self) -> bool {
        self()
    }
}

/// Bounded parameters for advisory lock acquisition.
#[derive(Debug, Clone, Copy)]
pub struct LockOptions {
    /// Maximum number of non-blocking lock attempts.
    pub max_attempts: usize,
    /// Delay before retrying a contended lock.
    pub initial_backoff: Duration,
    /// Upper bound for the exponential retry delay.
    pub max_backoff: Duration,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_LOCK_ATTEMPTS,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

/// Request-scoped limits and cancellation hooks for a publication.
#[derive(Clone, Copy)]
pub struct PublishOptions<'a> {
    /// Monotonic request deadline. No blocking lock acquisition is performed.
    pub deadline: Option<Instant>,
    /// Optional cooperative cancellation source.
    pub cancellation: Option<&'a dyn CancellationProbe>,
    /// Bounded lock retry policy.
    pub lock: LockOptions,
    /// Age after which matching temporary files are stale.
    pub stale_temp_age: Duration,
    /// Optional deterministic writer prefix, useful for recovery tests.
    pub temp_prefix: Option<&'a str>,
}

impl fmt::Debug for PublishOptions<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishOptions")
            .field("deadline", &self.deadline)
            .field("cancellation", &self.cancellation.is_some())
            .field("lock", &self.lock)
            .field("stale_temp_age", &self.stale_temp_age)
            .field("temp_prefix", &self.temp_prefix)
            .finish()
    }
}

impl Default for PublishOptions<'_> {
    fn default() -> Self {
        Self {
            deadline: None,
            cancellation: None,
            lock: LockOptions::default(),
            stale_temp_age: DEFAULT_STALE_TEMP_AGE,
            temp_prefix: None,
        }
    }
}

/// The result of a successful or deliberately non-destructive publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// This writer installed the validated artifact.
    Published { path: PathBuf },
    /// An existing regular artifact won the race and was left untouched.
    PreservedExisting { path: PathBuf },
}

/// Errors returned before a cache artifact can be published.
#[derive(Debug)]
pub enum PublishError {
    /// The path is not a single cache artifact path.
    InvalidPath { path: PathBuf, reason: &'static str },
    /// A required parent component does not exist.
    MissingParent { path: PathBuf },
    /// A path component is a symlink and therefore not an authorized cache path.
    Symlink { path: PathBuf },
    /// A required directory component is not a directory.
    NotDirectory { path: PathBuf },
    /// The final or temporary artifact was not a regular file.
    NotRegularFile { path: PathBuf },
    /// The lock could not be obtained within the bounded retry policy.
    LockContended { path: PathBuf },
    /// The request was cancelled while waiting or before publication.
    Cancelled,
    /// The monotonic request deadline elapsed while waiting or before publication.
    DeadlineExceeded,
    /// An operating-system operation failed.
    Io {
        path: Option<PathBuf>,
        source: io::Error,
    },
    /// Caller validation rejected the fully flushed temporary artifact.
    Validation { path: PathBuf, reason: String },
    /// Every bounded temporary-file candidate already existed.
    TempExhausted { parent: PathBuf },
}

impl PublishError {
    fn io(path: impl Into<Option<PathBuf>>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, reason } => {
                write!(formatter, "invalid cache path {}: {reason}", path.display())
            }
            Self::MissingParent { path } => {
                write!(formatter, "cache parent does not exist: {}", path.display())
            }
            Self::Symlink { path } => {
                write!(
                    formatter,
                    "symlink cache component rejected: {}",
                    path.display()
                )
            }
            Self::NotDirectory { path } => {
                write!(
                    formatter,
                    "cache component is not a directory: {}",
                    path.display()
                )
            }
            Self::NotRegularFile { path } => {
                write!(
                    formatter,
                    "cache artifact is not a regular file: {}",
                    path.display()
                )
            }
            Self::LockContended { path } => {
                write!(
                    formatter,
                    "cache lock remained contended: {}",
                    path.display()
                )
            }
            Self::Cancelled => formatter.write_str("cache publication was cancelled"),
            Self::DeadlineExceeded => formatter.write_str("cache publication deadline elapsed"),
            Self::Io { path, source } => match path {
                Some(path) => write!(
                    formatter,
                    "cache I/O failed for {}: {source}",
                    path.display()
                ),
                None => write!(formatter, "cache I/O failed: {source}"),
            },
            Self::Validation { path, reason } => {
                write!(
                    formatter,
                    "cache validation failed for {}: {reason}",
                    path.display()
                )
            }
            Self::TempExhausted { parent } => write!(
                formatter,
                "no unique cache temporary file was available in {}",
                parent.display()
            ),
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Returns the per-artifact lock path used by [`publish`].
pub fn lock_path(final_path: impl AsRef<Path>) -> PathBuf {
    let final_path = final_path.as_ref();
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .map_or(std::borrow::Cow::Borrowed("artifact"), |value| {
            value.to_string_lossy()
        });
    parent.join(format!(".{name}.lock"))
}

/// Returns a deterministic temporary candidate for a given writer prefix.
///
/// Production callers normally leave the prefix unset. A prefix is exposed so
/// recovery tests can pre-create candidate zero and prove `create_new` moves to
/// the next candidate without opening or replacing the collision.
pub fn temporary_path(final_path: impl AsRef<Path>, prefix: &str, attempt: u64) -> PathBuf {
    let final_path = final_path.as_ref();
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .map_or(std::borrow::Cow::Borrowed("artifact"), |value| {
            value.to_string_lossy()
        });
    let prefix = safe_component(prefix);
    parent.join(format!(".{name}.tmp.{prefix}-{attempt}"))
}

/// Returns the complete-marker path for a cache entry directory.
pub fn complete_marker_path(entry: impl AsRef<Path>) -> PathBuf {
    entry.as_ref().join(COMPLETE_MARKER)
}

/// Checks a marker without following a symlink.
pub fn has_complete_marker(entry: impl AsRef<Path>) -> bool {
    let Ok(entry) = absolute_path(entry.as_ref()) else {
        return false;
    };
    let Ok(directory) = open_directory(&entry) else {
        return false;
    };
    match directory.symlink_metadata(COMPLETE_MARKER) {
        Ok(metadata) => metadata.is_file() && !metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Atomically publishes a complete marker after the caller has produced its
/// identity/fingerprint payload.
///
/// # Errors
///
/// Returns an error when the entry path is unsafe or unavailable, the marker
/// cannot be written or validated, or the request is cancelled or expires.
pub fn write_complete_marker(
    entry: impl AsRef<Path>,
    contents: &[u8],
    options: PublishOptions<'_>,
) -> Result<PublishOutcome, PublishError> {
    publish(
        complete_marker_path(entry),
        contents,
        options,
        validate_regular_file,
    )
}

/// Writes, flushes, validates, and publishes one cache artifact.
///
/// # Errors
///
/// Returns an error when the path is unsafe or unavailable, lock acquisition
/// cannot complete, the request is cancelled or expires, or validation fails.
pub fn publish<F, E>(
    final_path: impl AsRef<Path>,
    contents: &[u8],
    options: PublishOptions<'_>,
    validate: F,
) -> Result<PublishOutcome, PublishError>
where
    F: FnOnce(&Path) -> Result<(), E>,
    E: fmt::Display,
{
    let final_path = absolute_path(final_path.as_ref())?;
    let directory = checked_parent(&final_path)?;
    let final_name = final_path
        .file_name()
        .map(OsStr::to_os_string)
        .ok_or_else(|| PublishError::InvalidPath {
            path: final_path.clone(),
            reason: "artifact has no file name",
        })?;
    let lock_file_path = lock_path(&final_path);
    let lock_name = lock_file_path
        .file_name()
        .map(OsStr::to_os_string)
        .ok_or_else(|| PublishError::InvalidPath {
            path: lock_file_path.clone(),
            reason: "lock has no file name",
        })?;
    let _lock = acquire_lock(&directory.dir, &lock_name, &lock_file_path, options)?;

    check_abort(&options)?;
    cleanup_stale_temps(
        &directory.dir,
        &final_name,
        options.stale_temp_age,
        &options,
    )?;

    if matches!(
        inspect_final(&directory.dir, &final_name, &final_path)?,
        FinalState::Existing
    ) {
        return Ok(PublishOutcome::PreservedExisting { path: final_path });
    }

    check_abort(&options)?;
    let mut temporary = create_temporary(&final_path, &directory.dir, options.temp_prefix)?;
    let temporary_path = temporary.path.clone();

    let write_result = (|| {
        let file = temporary.file.as_mut().ok_or_else(|| {
            PublishError::io(
                Some(temporary_path.clone()),
                io::Error::other("temporary file handle was already closed"),
            )
        })?;
        file.write_all(contents)
            .map_err(|error| PublishError::io(Some(temporary_path.clone()), error))?;
        file.flush()
            .map_err(|error| PublishError::io(Some(temporary_path.clone()), error))?;
        file.sync_all()
            .map_err(|error| PublishError::io(Some(temporary_path.clone()), error))?;
        check_abort(&options)?;
        validate(&temporary_path).map_err(|error| PublishError::Validation {
            path: temporary_path.clone(),
            reason: error.to_string(),
        })?;
        validate_regular_entry(&directory.dir, &temporary.name).map_err(|reason| {
            PublishError::Validation {
                path: temporary_path.clone(),
                reason,
            }
        })?;
        check_abort(&options)
    })();

    write_result?;

    // Closing before rename is required on platforms that do not permit a
    // directory entry to be renamed while a writer handle is open.
    drop(temporary.file.take());
    check_abort(&options)?;
    ensure_parent_unchanged(&directory)?;

    let outcome = match inspect_final(&directory.dir, &final_name, &final_path)? {
        FinalState::Existing => PublishOutcome::PreservedExisting { path: final_path },
        FinalState::Absent | FinalState::StaleSymlink => {
            if matches!(
                inspect_final(&directory.dir, &final_name, &final_path)?,
                FinalState::StaleSymlink
            ) {
                directory
                    .dir
                    .remove_file(&final_name)
                    .map_err(|error| PublishError::io(Some(final_path.clone()), error))?;
            }
            match inspect_final(&directory.dir, &final_name, &final_path)? {
                FinalState::Existing => {
                    return Ok(PublishOutcome::PreservedExisting { path: final_path });
                }
                FinalState::StaleSymlink => {
                    return Err(PublishError::Symlink { path: final_path });
                }
                FinalState::Absent => {}
            }
            ensure_parent_unchanged(&directory)?;
            directory
                .dir
                .rename(&temporary.name, &directory.dir, &final_name)
                .map_err(|error| PublishError::io(Some(final_path.clone()), error))?;
            temporary.disarm();
            sync_parent_directory(&directory.dir);
            PublishOutcome::Published { path: final_path }
        }
    };

    Ok(outcome)
}

/// A caller validation helper for the common regular-file requirement.
///
/// # Errors
///
/// Returns a description when the path is missing, is a symlink, or is not a
/// regular file.
pub fn validate_regular_file(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err("temporary artifact is not a regular file".to_owned()),
        Err(error) => Err(format!(
            "temporary artifact could not be inspected: {error}"
        )),
    }
}

fn validate_regular_entry(parent: &Dir, name: &OsStr) -> Result<(), String> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err("temporary artifact is not a regular file".to_owned()),
        Err(error) => Err(format!(
            "temporary artifact could not be inspected: {error}"
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalState {
    Absent,
    Existing,
    StaleSymlink,
}

struct CacheDirectory {
    path: PathBuf,
    dir: Dir,
}

struct AdvisoryLock {
    file: File,
    _process: ProcessLock,
}

struct ProcessLock {
    path: PathBuf,
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        let mut active = ACTIVE_LOCKS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.remove(&self.path);
    }
}

struct TemporaryFile<'a> {
    parent: &'a Dir,
    name: OsString,
    path: PathBuf,
    file: Option<File>,
}

impl TemporaryFile<'_> {
    fn disarm(&mut self) {
        self.name = OsString::new();
        self.path = PathBuf::new();
    }
}

impl Drop for TemporaryFile<'_> {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.name.as_os_str().is_empty() {
            return;
        }
        let Ok(metadata) = self.parent.symlink_metadata(&self.name) else {
            return;
        };
        if metadata.is_file() || metadata.file_type().is_symlink() {
            let _ = self.parent.remove_file(&self.name);
        }
    }
}

fn acquire_lock(
    parent: &Dir,
    name: &OsStr,
    path: &Path,
    options: PublishOptions<'_>,
) -> Result<AdvisoryLock, PublishError> {
    let process_lock = acquire_process_lock(path, options)?;
    let file = open_lock(parent, name, path)?;
    let attempts = options.lock.max_attempts.max(1);
    let mut backoff = options.lock.initial_backoff;
    let maximum_backoff = options.lock.max_backoff.max(backoff);

    for attempt in 0..attempts {
        check_abort(&options)?;
        match FileExt::try_lock(&file) {
            Ok(()) => {
                return Ok(AdvisoryLock {
                    file,
                    _process: process_lock,
                });
            }
            Err(TryLockError::WouldBlock) if attempt + 1 < attempts => {
                let mut delay = backoff;
                if let Some(deadline) = options.deadline {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(PublishError::DeadlineExceeded);
                    }
                    if delay > remaining {
                        delay = remaining;
                    }
                }
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                backoff = backoff
                    .checked_mul(2)
                    .unwrap_or(maximum_backoff)
                    .min(maximum_backoff);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(PublishError::LockContended {
                    path: path.to_path_buf(),
                });
            }
            Err(TryLockError::Error(error)) => {
                return Err(PublishError::io(Some(path.to_path_buf()), error));
            }
        }
    }

    Err(PublishError::LockContended {
        path: path.to_path_buf(),
    })
}

fn acquire_process_lock(
    path: &Path,
    options: PublishOptions<'_>,
) -> Result<ProcessLock, PublishError> {
    let attempts = options.lock.max_attempts.max(1);
    let mut backoff = options.lock.initial_backoff;
    let maximum_backoff = options.lock.max_backoff.max(backoff);

    for attempt in 0..attempts {
        check_abort(&options)?;
        {
            let mut active = ACTIVE_LOCKS
                .get_or_init(|| Mutex::new(HashSet::new()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !active.contains(path) {
                let path = path.to_path_buf();
                active.insert(path.clone());
                return Ok(ProcessLock { path });
            }
        }
        if attempt + 1 == attempts {
            break;
        }

        let mut delay = backoff;
        if let Some(deadline) = options.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PublishError::DeadlineExceeded);
            }
            if delay > remaining {
                delay = remaining;
            }
        }
        if !delay.is_zero() {
            thread::sleep(delay);
        }
        backoff = backoff
            .checked_mul(2)
            .unwrap_or(maximum_backoff)
            .min(maximum_backoff);
    }

    Err(PublishError::LockContended {
        path: path.to_path_buf(),
    })
}

fn open_lock(parent: &Dir, name: &OsStr, path: &Path) -> Result<File, PublishError> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(PublishError::Symlink {
                path: path.to_path_buf(),
            });
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(PublishError::NotRegularFile {
                path: path.to_path_buf(),
            });
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(PublishError::io(Some(path.to_path_buf()), error));
        }
        _ => {}
    }

    let mut options = CapabilityOpenOptions::new();
    options.read(true).write(true).create(true);
    configure_open_options(&mut options);
    let file = match parent.open_with(name, &options) {
        Ok(file) => file,
        Err(error) => {
            return match parent.symlink_metadata(name) {
                Ok(metadata) if metadata.file_type().is_symlink() => Err(PublishError::Symlink {
                    path: path.to_path_buf(),
                }),
                Ok(metadata) if !metadata.is_file() => Err(PublishError::NotRegularFile {
                    path: path.to_path_buf(),
                }),
                _ => Err(PublishError::io(Some(path.to_path_buf()), error)),
            };
        }
    };
    let metadata = parent
        .symlink_metadata(name)
        .map_err(|error| PublishError::io(Some(path.to_path_buf()), error))?;
    if metadata.file_type().is_symlink() {
        return Err(PublishError::Symlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file()
        || !file
            .metadata()
            .map(|value| value.is_file())
            .unwrap_or(false)
    {
        return Err(PublishError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    Ok(file.into_std())
}

fn inspect_final(parent: &Dir, name: &OsStr, path: &Path) -> Result<FinalState, PublishError> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(FinalState::StaleSymlink),
        Ok(metadata) if metadata.is_file() => Ok(FinalState::Existing),
        Ok(_) => Err(PublishError::NotRegularFile {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(FinalState::Absent),
        Err(error) => Err(PublishError::io(Some(path.to_path_buf()), error)),
    }
}

fn create_temporary<'a>(
    final_path: &Path,
    parent: &'a Dir,
    requested_prefix: Option<&str>,
) -> Result<TemporaryFile<'a>, PublishError> {
    let parent_path = final_path
        .parent()
        .ok_or_else(|| PublishError::InvalidPath {
            path: final_path.to_path_buf(),
            reason: "artifact has no parent directory",
        })?;
    let prefix = requested_prefix.map_or_else(
        || {
            format!(
                "{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )
        },
        safe_component,
    );

    for attempt in 0..MAX_TEMP_ATTEMPTS {
        let candidate = temporary_path(final_path, &prefix, attempt);
        let candidate_name = candidate
            .file_name()
            .map(OsStr::to_os_string)
            .ok_or_else(|| PublishError::InvalidPath {
                path: candidate.clone(),
                reason: "temporary file has no file name",
            })?;
        let mut options = CapabilityOpenOptions::new();
        options.read(true).write(true).create_new(true);
        configure_open_options(&mut options);
        match parent.open_with(&candidate_name, &options) {
            Ok(file) => {
                return Ok(TemporaryFile {
                    parent,
                    name: candidate_name,
                    path: candidate,
                    file: Some(file.into_std()),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(PublishError::io(Some(candidate), error)),
        }
    }

    Err(PublishError::TempExhausted {
        parent: parent_path.to_path_buf(),
    })
}

fn cleanup_stale_temps(
    parent: &Dir,
    final_name: &OsStr,
    stale_age: Duration,
    options: &PublishOptions<'_>,
) -> Result<(), PublishError> {
    check_abort(options)?;
    let name = final_name.to_string_lossy();
    let prefix = format!(".{name}.tmp.");
    let now = SystemTime::now();
    let Ok(entries) = parent.entries() else {
        return Ok(());
    };

    for entry in entries.flatten() {
        check_abort(options)?;
        let candidate_name = entry.file_name();
        let candidate_name_string = candidate_name.to_string_lossy();
        if !candidate_name_string.starts_with(&prefix) {
            continue;
        }
        let Ok(metadata) = parent.symlink_metadata(&candidate_name) else {
            continue;
        };
        if !metadata.is_file() && !metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified.into_std()) else {
            continue;
        };
        if age >= stale_age {
            // A symlink is removed as a directory entry only. The target is
            // never opened, traversed, or recursively removed.
            let _ = parent.remove_file(&candidate_name);
        }
    }
    Ok(())
}

fn checked_parent(final_path: &Path) -> Result<CacheDirectory, PublishError> {
    let parent = final_path
        .parent()
        .ok_or_else(|| PublishError::InvalidPath {
            path: final_path.to_path_buf(),
            reason: "artifact has no parent directory",
        })?;
    if final_path.file_name().is_none() {
        return Err(PublishError::InvalidPath {
            path: final_path.to_path_buf(),
            reason: "artifact has no file name",
        });
    }
    Ok(CacheDirectory {
        path: parent.to_path_buf(),
        dir: open_directory(parent)?,
    })
}

fn ensure_parent_unchanged(directory: &CacheDirectory) -> Result<(), PublishError> {
    let current = open_directory(&directory.path)?;
    #[cfg(not(unix))]
    let _ = &current;
    #[cfg(unix)]
    {
        let expected = directory
            .dir
            .dir_metadata()
            .map_err(|error| PublishError::io(Some(directory.path.clone()), error))?;
        let actual = current
            .dir_metadata()
            .map_err(|error| PublishError::io(Some(directory.path.clone()), error))?;
        if cap_std::fs::MetadataExt::dev(&expected) != cap_std::fs::MetadataExt::dev(&actual)
            || cap_std::fs::MetadataExt::ino(&expected) != cap_std::fs::MetadataExt::ino(&actual)
        {
            return Err(PublishError::InvalidPath {
                path: directory.path.clone(),
                reason: "cache parent changed during publication",
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory(path: &Path) -> Result<Dir, PublishError> {
    if !path.is_absolute() {
        return Err(PublishError::InvalidPath {
            path: path.to_path_buf(),
            reason: "directory path must be absolute",
        });
    }

    let mut directory = Dir::open_ambient_dir(Path::new("/"), cap_std::ambient_authority())
        .map_err(|error| PublishError::io(Some(PathBuf::from("/")), error))?;
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                current.push(name);
                let next = open_directory_component(&directory, name, &current)?;
                directory = next;
            }
            Component::Prefix(_) | Component::ParentDir => {
                return Err(PublishError::InvalidPath {
                    path: path.to_path_buf(),
                    reason: "parent path contains an unsupported component",
                });
            }
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_directory_component(parent: &Dir, name: &OsStr, path: &Path) -> Result<Dir, PublishError> {
    let metadata = parent.symlink_metadata(name).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            PublishError::MissingParent {
                path: path.to_path_buf(),
            }
        } else {
            PublishError::io(Some(path.to_path_buf()), error)
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(PublishError::Symlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_dir() {
        return Err(PublishError::NotDirectory {
            path: path.to_path_buf(),
        });
    }

    let mut options = CapabilityOpenOptions::new();
    options.read(true);
    configure_open_options(&mut options);
    let file = match parent.open_with(name, &options) {
        Ok(file) => file,
        Err(error) => {
            return match parent.symlink_metadata(name) {
                Ok(metadata) if metadata.file_type().is_symlink() => Err(PublishError::Symlink {
                    path: path.to_path_buf(),
                }),
                Ok(metadata) if !metadata.is_dir() => Err(PublishError::NotDirectory {
                    path: path.to_path_buf(),
                }),
                _ => Err(PublishError::io(Some(path.to_path_buf()), error)),
            };
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| PublishError::io(Some(path.to_path_buf()), error))?;
    if !metadata.is_dir() {
        return Err(PublishError::NotDirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(Dir::from_std_file(file.into_std()))
}

#[cfg(not(unix))]
fn open_directory(path: &Path) -> Result<Dir, PublishError> {
    verify_directory_path(path)?;
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
        .map_err(|error| PublishError::io(Some(path.to_path_buf()), error))
}

#[cfg(not(unix))]
fn verify_directory_path(path: &Path) -> Result<(), PublishError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                // A drive prefix is not a complete directory until RootDir.
                continue;
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(PublishError::InvalidPath {
                    path: path.to_path_buf(),
                    reason: "parent traversal is not allowed",
                });
            }
            Component::Normal(name) => current.push(name),
        }

        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                PublishError::MissingParent {
                    path: current.clone(),
                }
            } else {
                PublishError::io(Some(current.clone()), error)
            }
        })?;
        if metadata.file_type().is_symlink() {
            return Err(PublishError::Symlink { path: current });
        }
        if !metadata.is_dir() {
            return Err(PublishError::NotDirectory { path: current });
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, PublishError> {
    if path.as_os_str().is_empty() {
        return Err(PublishError::InvalidPath {
            path: path.to_path_buf(),
            reason: "empty path",
        });
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|error| PublishError::io(None::<PathBuf>, error))
}

fn check_abort(options: &PublishOptions<'_>) -> Result<(), PublishError> {
    if options
        .cancellation
        .is_some_and(CancellationProbe::is_cancelled)
    {
        return Err(PublishError::Cancelled);
    }
    if options
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(PublishError::DeadlineExceeded);
    }
    Ok(())
}

fn safe_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "writer".to_owned()
    } else {
        sanitized
    }
}

fn sync_parent_directory(parent: &Dir) {
    // Directory fsync is supported on Unix-like systems but is not uniformly
    // available on Windows. Publication remains valid when this best-effort
    // durability hint is rejected by the platform/filesystem.
    if let Ok(directory) = parent.try_clone().map(Dir::into_std_file) {
        let _ = directory.sync_all();
    }
}

#[cfg(unix)]
fn configure_open_options(options: &mut CapabilityOpenOptions) {
    options.mode(0o600);
    options.custom_flags(open_no_follow_flag());
}

#[cfg(not(unix))]
fn configure_open_options(_options: &mut CapabilityOpenOptions) {}

#[cfg(target_os = "linux")]
const OPEN_NO_FOLLOW: i32 = 0x20000;

#[cfg(target_os = "android")]
const OPEN_NO_FOLLOW: i32 = 0x20000;

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "ios",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd"
))]
const OPEN_NO_FOLLOW: i32 = 0x100;

#[cfg(unix)]
fn open_no_follow_flag() -> i32 {
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        OPEN_NO_FOLLOW
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        0
    }
}
