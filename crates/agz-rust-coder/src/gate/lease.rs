use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const STALE_RECORD_AGE: Duration = Duration::from_secs(10);
const MAX_RECORD_BYTES: u64 = 8 * 1024;
static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LeaseError {
    #[error("gate lease acquisition was cancelled")]
    Cancelled,
    #[error("timed out waiting for gate lease {key}")]
    TimedOut { key: String },
    #[error("gate lease directory is not safe: {0}")]
    UnsafeDirectory(PathBuf),
    #[error("gate lease I/O failed for {path}: {message}")]
    Io { path: PathBuf, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRecord {
    pid: u32,
    token: String,
    created_at_ms: u128,
}

/// A process-safe, idempotently releasable gate lease.
#[derive(Debug)]
pub struct Lease {
    pub path: PathBuf,
    token: String,
    released: bool,
}

impl Lease {
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let Ok(bytes) = fs::read(&self.path) else {
            return;
        };
        let Ok(record) = serde_json::from_slice::<LeaseRecord>(&bytes) else {
            return;
        };
        if record.pid == std::process::id() && record.token == self.token {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}

pub async fn acquire_lease(
    root: impl AsRef<Path>,
    key: &str,
    deadline: Option<Instant>,
    cancellation: Option<&CancellationToken>,
) -> Result<Lease, LeaseError> {
    let root = root.as_ref().to_owned();
    ensure_directory(&root)?;
    let path = root.join(format!("{}.lock", safe_key(key)));
    let token = format!(
        "{}-{}-{}",
        std::process::id(),
        timestamp_millis(),
        TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let deadline = deadline.unwrap_or_else(|| Instant::now() + DEFAULT_TIMEOUT);

    loop {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(LeaseError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(LeaseError::TimedOut {
                key: key.to_owned(),
            });
        }
        match create_lease_file(&path, &token) {
            Ok(()) => {
                return Ok(Lease {
                    path,
                    token,
                    released: false,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                reclaim_dead_lease(&path);
            }
            Err(error) => {
                return Err(LeaseError::Io {
                    path,
                    message: error.to_string(),
                });
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL.min(remaining)) => {}
            () = async {
                if let Some(cancellation) = cancellation {
                    cancellation.cancelled().await;
                }
            }, if cancellation.is_some() => return Err(LeaseError::Cancelled),
        }
    }
}

pub async fn acquire_lease_with_timeout(
    root: impl AsRef<Path>,
    key: &str,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
) -> Result<Lease, LeaseError> {
    acquire_lease(root, key, Some(Instant::now() + timeout), cancellation).await
}

pub async fn acquire_host_slot(
    root: impl AsRef<Path>,
    slots: usize,
    deadline: Option<Instant>,
    cancellation: Option<&CancellationToken>,
) -> Result<Lease, LeaseError> {
    if slots == 0 {
        return Err(LeaseError::TimedOut {
            key: "host slot".to_owned(),
        });
    }
    let root = root.as_ref().to_owned();
    ensure_directory(&root)?;
    let deadline = deadline.unwrap_or_else(|| Instant::now() + DEFAULT_TIMEOUT);
    loop {
        for index in 0..slots {
            match try_acquire_lease(&root, &format!("host-{index}")) {
                Ok(lease) => return Ok(lease),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    reclaim_dead_lease(
                        &root.join(format!("{}.lock", safe_key(&format!("host-{index}")))),
                    );
                }
                Err(error) => {
                    return Err(LeaseError::Io {
                        path: root.clone(),
                        message: error.to_string(),
                    });
                }
            }
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(LeaseError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(LeaseError::TimedOut {
                key: "host slot".to_owned(),
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL.min(remaining)) => {}
            () = async {
                if let Some(cancellation) = cancellation {
                    cancellation.cancelled().await;
                }
            }, if cancellation.is_some() => return Err(LeaseError::Cancelled),
        }
    }
}

fn try_acquire_lease(root: &Path, key: &str) -> io::Result<Lease> {
    let path = root.join(format!("{}.lock", safe_key(key)));
    let token = format!(
        "{}-{}-{}",
        std::process::id(),
        timestamp_millis(),
        TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    create_lease_file(&path, &token)?;
    Ok(Lease {
        path,
        token,
        released: false,
    })
}

fn create_lease_file(path: &Path, token: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let record = LeaseRecord {
        pid: std::process::id(),
        token: token.to_owned(),
        created_at_ms: timestamp_millis(),
    };
    let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

fn reclaim_dead_lease(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return;
    }
    let mut bytes = Vec::new();
    let Ok(file) = File::open(path) else {
        return;
    };
    if file
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_RECORD_BYTES
    {
        return;
    }
    let Ok(record) = serde_json::from_slice::<LeaseRecord>(&bytes) else {
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_RECORD_AGE);
        if old_enough {
            let _ = fs::remove_file(path);
        }
        return;
    };
    if !process_is_alive(record.pid) {
        let _ = fs::remove_file(path);
    }
}

fn ensure_directory(path: &Path) -> Result<(), LeaseError> {
    if !path.is_absolute() || !no_symlink_components(path) {
        return Err(LeaseError::UnsafeDirectory(path.to_owned()));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(LeaseError::UnsafeDirectory(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| LeaseError::Io {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
            if no_symlink_components(path) {
                Ok(())
            } else {
                Err(LeaseError::UnsafeDirectory(path.to_owned()))
            }
        }
        Err(error) => Err(LeaseError::Io {
            path: path.to_owned(),
            message: error.to_string(),
        }),
    }
}

fn no_symlink_components(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(_) => return false,
        }
    }
    true
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        PathBuf::from(format!("/proc/{pid}/stat")).is_file()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn safe_key(key: &str) -> String {
    let label = key
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                byte as char
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    let digest = format!("{:x}", hash.finalize());
    format!(
        "{}-{}",
        if label.is_empty() { "lease" } else { &label },
        &digest[..16]
    )
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}
