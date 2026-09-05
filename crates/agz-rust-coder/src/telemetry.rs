use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs4::{FileExt, TryLockError};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::TelemetryConfig;

const MAX_EVENT_BYTES: usize = 4 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_INITIAL_BACKOFF: Duration = Duration::from_millis(2);
const LOCK_MAX_BACKOFF: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct ActivityLog {
    enabled: bool,
    path: PathBuf,
    lock_path: PathBuf,
    retention_bytes: u64,
    retention_age: Duration,
    max_archives: u64,
    salt: [u8; 32],
    sequence: AtomicU64,
    writer: Mutex<()>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivityEvent<'a> {
    version: u8,
    timestamp_ms: u128,
    event: &'a str,
    tool: Option<&'a str>,
    status: Option<&'a str>,
    request_hash: String,
    root_hash: Option<String>,
}

impl ActivityLog {
    pub fn new(config: &TelemetryConfig) -> io::Result<Self> {
        let path = absolute_path(&config.path)?;
        let lock_path = sibling_path(&path, "lock")?;
        if config.enabled {
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "telemetry path has no parent")
            })?;
            ensure_directory_no_symlink(parent)?;
            reject_symlink_or_nonfile(&path)?;
            reject_symlink_or_nonfile(&lock_path)?;
        }
        let mut hasher = Sha256::new();
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
        hasher.update(path.as_os_str().as_encoded_bytes());
        let salt: [u8; 32] = hasher.finalize().into();
        Ok(Self {
            enabled: config.enabled,
            path,
            lock_path,
            retention_bytes: config.retention_bytes,
            retention_age: Duration::from_secs(config.retention_days.saturating_mul(24 * 60 * 60)),
            max_archives: config.max_archives,
            salt,
            sequence: AtomicU64::new(0),
            writer: Mutex::new(()),
        })
    }

    pub fn record(
        &self,
        event: &str,
        tool: Option<&str>,
        status: Option<&str>,
        root: Option<&Path>,
        request: Option<&str>,
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let _writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("telemetry writer lock was poisoned"))?;
        let lock = open_private(&self.lock_path, true)?;
        lock_with_deadline(&lock, Instant::now() + LOCK_TIMEOUT)?;
        let result = self.record_locked(event, tool, status, root, request);
        let unlock = FileExt::unlock(&lock);
        result.and(unlock)
    }

    pub fn flush(&self) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let _writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("telemetry writer lock was poisoned"))?;
        let lock = open_private(&self.lock_path, true)?;
        lock_with_deadline(&lock, Instant::now() + LOCK_TIMEOUT)?;
        let result = match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "telemetry path is not a regular file",
                ))
            }
            Ok(_) => OpenOptions::new().write(true).open(&self.path)?.sync_data(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        let unlock = FileExt::unlock(&lock);
        result.and(unlock)
    }

    fn record_locked(
        &self,
        event: &str,
        tool: Option<&str>,
        status: Option<&str>,
        root: Option<&Path>,
        request: Option<&str>,
    ) -> io::Result<()> {
        reject_symlink_or_nonfile(&self.path)?;
        self.remove_expired_archives()?;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let fallback_request;
        let request = if let Some(request) = request {
            request
        } else {
            fallback_request = format!("event:{timestamp_ms}:{sequence}");
            &fallback_request
        };
        let event = ActivityEvent {
            version: 1,
            timestamp_ms,
            event: bounded_token(event),
            tool: tool.map(bounded_token),
            status: status.map(bounded_token),
            request_hash: self.hash(request),
            root_hash: root.map(|root| self.hash(&root.to_string_lossy())),
        };
        let mut line = serde_json::to_vec(&event).map_err(io::Error::other)?;
        if line.len() >= MAX_EVENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "telemetry event exceeded its byte limit",
            ));
        }
        line.push(b'\n');
        let current_size = fs::metadata(&self.path).map_or(0, |metadata| metadata.len());
        if current_size > 0 && current_size.saturating_add(line.len() as u64) > self.retention_bytes
        {
            self.rotate()?;
        }
        let mut file = open_private(&self.path, true)?;
        file.write_all(&line)?;
        file.flush()
    }

    fn rotate(&self) -> io::Result<()> {
        if self.max_archives == 0 {
            return remove_if_file(&self.path);
        }
        remove_if_file(&archive_path(&self.path, self.max_archives)?)?;
        for index in (1..self.max_archives).rev() {
            let source = archive_path(&self.path, index)?;
            let destination = archive_path(&self.path, index + 1)?;
            rename_if_file(&source, &destination)?;
        }
        let first = archive_path(&self.path, 1)?;
        rename_if_file(&self.path, &first)
    }

    fn remove_expired_archives(&self) -> io::Result<()> {
        let now = SystemTime::now();
        for index in 1..=self.max_archives {
            let path = archive_path(&self.path, index)?;
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "telemetry archive is not a regular file: {}",
                            path.display()
                        ),
                    ));
                }
                Ok(metadata) => {
                    let expired = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| now.duration_since(modified).ok())
                        .is_some_and(|age| age > self.retention_age);
                    if expired {
                        fs::remove_file(path)?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn hash(&self, value: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(value.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

fn bounded_token(value: &str) -> &str {
    let value = value.trim();
    if value.len() <= 64 {
        value
    } else {
        "oversized"
    }
}

fn lock_with_deadline(file: &File, deadline: Instant) -> io::Result<()> {
    let mut backoff = LOCK_INITIAL_BACKOFF;
    loop {
        match FileExt::try_lock(file) {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "telemetry file lock deadline elapsed",
                    ));
                }
                std::thread::sleep(backoff.min(remaining));
                backoff = backoff
                    .checked_mul(2)
                    .unwrap_or(LOCK_MAX_BACKOFF)
                    .min(LOCK_MAX_BACKOFF);
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn ensure_directory_no_symlink(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "telemetry path contains a parent component",
                ));
            }
            Component::Normal(part) => {
                current.push(part);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("telemetry directory is a symlink: {}", current.display()),
                        ));
                    }
                    Ok(metadata) if !metadata.is_dir() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("telemetry parent is not a directory: {}", current.display()),
                        ));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&current)?;
                        set_private_directory_permissions(&current)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(())
}

fn open_private(path: &Path, append: bool) -> io::Result<File> {
    reject_symlink_or_nonfile(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    validate_open_file_identity(path, &file)?;
    set_private_file_permissions(&file)?;
    Ok(file)
}

fn validate_open_file_identity(path: &Path, file: &File) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !file_metadata.is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("telemetry path is not a regular file: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("telemetry path changed while opening: {}", path.display()),
            ));
        }
    }
    Ok(())
}

fn reject_symlink_or_nonfile(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("telemetry path is not a regular file: {}", path.display()),
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn archive_path(path: &Path, index: u64) -> io::Result<PathBuf> {
    sibling_path(path, &index.to_string())
}

fn sibling_path(path: &Path, suffix: &str) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "telemetry path has no file name",
        )
    })?;
    let mut sibling = name.to_os_string();
    sibling.push(".");
    sibling.push(suffix);
    Ok(path.with_file_name(sibling))
}

fn remove_if_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "telemetry archive is not a regular file: {}",
                    path.display()
                ),
            ))
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn rename_if_file(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "telemetry archive is not a regular file: {}",
                    source.display()
                ),
            ))
        }
        Ok(_) => fs::rename(source, destination),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
// Preserve the shared fallible interface on platforms with a no-op implementation.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
// Preserve the shared fallible interface on platforms with a no-op implementation.
#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-coder-telemetry-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ))
    }

    #[test]
    fn activity_log_is_private_bounded_and_content_free() {
        let root = temp_path("privacy");
        let path = root.join("activity.jsonl");
        let config = TelemetryConfig {
            enabled: true,
            path: path.clone(),
            retention_bytes: 4 * 1024,
            retention_days: 7,
            max_archives: 2,
        };
        let log = ActivityLog::new(&config).expect("create activity log");
        let raw_root = Path::new("/private/workspace/name");
        for _ in 0..40 {
            log.record(
                "tool_completed",
                Some("check"),
                Some("FAST_PASS"),
                Some(raw_root),
                Some("request-42"),
            )
            .expect("record activity");
        }
        log.flush().expect("flush activity");
        let current = fs::read_to_string(&path).expect("read current activity");
        assert!(!current.contains("/private/workspace/name"));
        assert!(!current.contains("prompt"));
        assert!(!current.contains("request-42"));
        assert!(archive_path(&path, 1).expect("archive path").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("activity metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(root);
    }
}
