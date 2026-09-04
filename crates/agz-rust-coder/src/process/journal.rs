use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(not(target_os = "linux"))]
use super::identity::IdentityError;
use super::identity::{
    command_hash_bytes, current_group_id, current_start_time, read_process_identity,
};

const JOURNAL_VERSION: u8 = 1;
const MAX_RECORD_BYTES: u64 = 128 * 1024;
const DEFAULT_RECOVERY_RECORDS: usize = 256;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal path is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("journal I/O failed for {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("journal serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid journal token: {0}")]
    InvalidToken(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProcessGroupIdentity {
    Unix { pgid: u32 },
    WindowsJob { process_id: u32 },
    Unmanaged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalRecord {
    pub version: u8,
    pub token: String,
    pub owner_pid: u32,
    pub owner_start_time: Option<u64>,
    pub child_pid: u32,
    pub start_time: Option<u64>,
    pub executable: PathBuf,
    pub group: ProcessGroupIdentity,
    pub command_hash: String,
}

impl JournalRecord {
    pub fn new(
        token: impl Into<String>,
        child_pid: u32,
        start_time: Option<u64>,
        executable: impl Into<PathBuf>,
        group: ProcessGroupIdentity,
        command_hash: impl Into<String>,
    ) -> Self {
        Self {
            version: JOURNAL_VERSION,
            token: token.into(),
            owner_pid: std::process::id(),
            owner_start_time: current_start_time(),
            child_pid,
            start_time,
            executable: executable.into(),
            group,
            command_hash: command_hash.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredJournalRecord {
    #[serde(flatten)]
    record: JournalRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alternate_command_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDisposition {
    OwnerAlive,
    AlreadyExited,
    IdentityMismatch,
    Unverifiable,
    Killed,
    KillFailed,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEntry {
    pub token: String,
    pub disposition: RecoveryDisposition,
    pub reason: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    pub inspected: usize,
    pub killed: usize,
    pub skipped: usize,
    pub truncated: bool,
    pub entries: Vec<RecoveryEntry>,
}

#[derive(Debug)]
pub struct ProcessJournal {
    jobs_dir: PathBuf,
    owner_pid: u32,
    owner_start_time: Option<u64>,
    sequence: AtomicU64,
}

impl ProcessJournal {
    /// # Errors
    ///
    /// Returns an error when the journal root or its jobs directory cannot be
    /// created or is not a directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, JournalError> {
        let root = root.into();
        ensure_directory(&root)?;
        let jobs_dir = root.join("jobs");
        ensure_directory(&jobs_dir)?;
        Ok(Self {
            jobs_dir,
            owner_pid: std::process::id(),
            owner_start_time: current_start_time(),
            sequence: AtomicU64::new(0),
        })
    }

    /// # Errors
    ///
    /// Returns an error when the record is invalid or cannot be atomically
    /// written to the journal directory.
    pub fn record(&self, record: &JournalRecord) -> Result<(), JournalError> {
        self.record_with_alternate(record, None)
    }

    pub(crate) fn record_with_alternate(
        &self,
        record: &JournalRecord,
        alternate_command_hash: Option<&str>,
    ) -> Result<(), JournalError> {
        validate_token(&record.token)?;
        if record.version != JOURNAL_VERSION {
            return Err(JournalError::InvalidToken(format!(
                "unsupported journal version {}",
                record.version
            )));
        }
        if record.owner_pid != self.owner_pid {
            return Err(JournalError::InvalidToken(
                "journal owner does not match this process".to_owned(),
            ));
        }
        let path = self.path_for(&record.token)?;
        let temp = self.temp_path(&record.token);
        let data = serde_json::to_vec(&StoredJournalRecord {
            record: record.clone(),
            alternate_command_hash: alternate_command_hash.map(str::to_owned),
        })?;
        let result = Self::write_atomic(&temp, &path, &data);
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// # Errors
    ///
    /// Returns an error when the token is invalid or the journal entry cannot
    /// be removed.
    pub fn remove(&self, token: &str) -> Result<(), JournalError> {
        let path = self.path_for(token)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(JournalError::Io { path, source }),
        }
    }

    pub fn recover_orphans(&self) -> RecoveryReport {
        self.recover_orphans_with_limit(DEFAULT_RECOVERY_RECORDS)
    }

    pub fn recover_orphans_with_limit(&self, limit: usize) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        let entries = match fs::read_dir(&self.jobs_dir) {
            Ok(entries) => entries,
            Err(error) => {
                push_entry(
                    &mut report,
                    RecoveryEntry {
                        token: String::new(),
                        disposition: RecoveryDisposition::Unverifiable,
                        reason: format!("cannot inspect journal directory: {error}"),
                    },
                    limit,
                );
                return report;
            }
        };

        for entry in entries {
            if report.inspected >= limit {
                report.truncated = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.inspected += 1;
                    push_entry(
                        &mut report,
                        RecoveryEntry {
                            token: String::new(),
                            disposition: RecoveryDisposition::Unverifiable,
                            reason: format!("cannot inspect journal entry: {error}"),
                        },
                        limit,
                    );
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            report.inspected += 1;
            let result = self.recover_entry(&path);
            let recovery = match result {
                Ok(recovery) => recovery,
                Err(error) => RecoveryEntry {
                    token: path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_default()
                        .to_owned(),
                    disposition: RecoveryDisposition::Unverifiable,
                    reason: error,
                },
            };
            if recovery.disposition == RecoveryDisposition::Killed {
                report.killed += 1;
            } else {
                report.skipped += 1;
            }
            push_entry(&mut report, recovery, limit);
        }
        report
    }

    fn recover_entry(&self, path: &Path) -> Result<RecoveryEntry, String> {
        let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if !metadata.file_type().is_file() {
            return Ok(RecoveryEntry {
                token: path_token(path),
                disposition: RecoveryDisposition::Unverifiable,
                reason: "journal entry is not a regular file".to_owned(),
            });
        }
        let stored = read_record(path)?;
        let record = stored.record;
        if record.version != JOURNAL_VERSION {
            return Ok(invalid_entry(&record.token, "unsupported journal version"));
        }
        if validate_token(&record.token).is_err() || path_token(path) != record.token {
            return Ok(invalid_entry(
                &record.token,
                "journal token does not match its file",
            ));
        }
        if let Some(entry) = self.owner_state(&record) {
            return Ok(entry);
        }

        let group = match verify_child(&record, stored.alternate_command_hash.as_deref()) {
            Ok(group) => group,
            Err(VerifyFailure::AlreadyExited(reason)) => {
                let _ = fs::remove_file(path);
                return Ok(RecoveryEntry {
                    token: record.token,
                    disposition: RecoveryDisposition::AlreadyExited,
                    reason,
                });
            }
            Err(VerifyFailure::Mismatch(reason)) => {
                return Ok(RecoveryEntry {
                    token: record.token,
                    disposition: RecoveryDisposition::IdentityMismatch,
                    reason,
                });
            }
            Err(VerifyFailure::Unverifiable(reason)) => {
                return Ok(RecoveryEntry {
                    token: record.token,
                    disposition: RecoveryDisposition::Unverifiable,
                    reason,
                });
            }
        };

        match force_kill_group(group) {
            Ok(()) => {
                let _ = fs::remove_file(path);
                Ok(RecoveryEntry {
                    token: record.token,
                    disposition: RecoveryDisposition::Killed,
                    reason: "all recorded process identities matched".to_owned(),
                })
            }
            Err(reason) => Ok(RecoveryEntry {
                token: record.token,
                disposition: RecoveryDisposition::KillFailed,
                reason,
            }),
        }
    }

    fn owner_state(&self, record: &JournalRecord) -> Option<RecoveryEntry> {
        if record.owner_pid == self.owner_pid && record.owner_start_time == self.owner_start_time {
            return Some(RecoveryEntry {
                token: record.token.clone(),
                disposition: RecoveryDisposition::OwnerAlive,
                reason: "journal owner is this live process".to_owned(),
            });
        }
        match read_process_identity(record.owner_pid) {
            Ok(Some(identity)) => {
                if record.owner_start_time == Some(identity.start_time) {
                    Some(RecoveryEntry {
                        token: record.token.clone(),
                        disposition: RecoveryDisposition::OwnerAlive,
                        reason: "journal owner is still alive".to_owned(),
                    })
                } else {
                    Some(RecoveryEntry {
                        token: record.token.clone(),
                        disposition: RecoveryDisposition::Unverifiable,
                        reason: "owner PID was reused with a different start time".to_owned(),
                    })
                }
            }
            Ok(None) => None,
            #[cfg(not(target_os = "linux"))]
            Err(IdentityError::Unsupported) => Some(RecoveryEntry {
                token: record.token.clone(),
                disposition: RecoveryDisposition::Unverifiable,
                reason: "owner liveness cannot be verified on this platform".to_owned(),
            }),
            Err(error) => Some(RecoveryEntry {
                token: record.token.clone(),
                disposition: RecoveryDisposition::Unverifiable,
                reason: format!("owner identity cannot be verified: {error}"),
            }),
        }
    }

    fn path_for(&self, token: &str) -> Result<PathBuf, JournalError> {
        validate_token(token)?;
        Ok(self.jobs_dir.join(format!("{token}.json")))
    }

    fn temp_path(&self, token: &str) -> PathBuf {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        self.jobs_dir
            .join(format!(".{token}.{}.{}.tmp", self.owner_pid, sequence))
    }

    fn write_atomic(temp: &Path, final_path: &Path, data: &[u8]) -> Result<(), JournalError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(temp)
            .map_err(|source| JournalError::Io {
                path: temp.to_owned(),
                source,
            })?;
        set_private_file_mode(&file, temp)?;
        file.write_all(data).map_err(|source| JournalError::Io {
            path: temp.to_owned(),
            source,
        })?;
        file.sync_all().map_err(|source| JournalError::Io {
            path: temp.to_owned(),
            source,
        })?;
        drop(file);
        fs::rename(temp, final_path).map_err(|source| JournalError::Io {
            path: final_path.to_owned(),
            source,
        })
    }
}

fn ensure_directory(path: &Path) -> Result<(), JournalError> {
    let mut current = PathBuf::new();
    let mut checked_component = false;
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        checked_component = true;
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(JournalError::NotDirectory(current));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(JournalError::NotDirectory(current)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // The preceding component was checked without following symlinks.
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(source) => {
                        return Err(JournalError::Io {
                            path: current,
                            source,
                        });
                    }
                }
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Ok(_) | Err(_) => return Err(JournalError::NotDirectory(current)),
                }
            }
            Err(source) => {
                return Err(JournalError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    if checked_component {
        Ok(())
    } else {
        Err(JournalError::NotDirectory(path.to_owned()))
    }
}

fn set_private_file_mode(file: &File, path: &Path) -> Result<(), JournalError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| JournalError::Io {
                path: path.to_owned(),
                source,
            })?;
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), JournalError> {
    if token.is_empty()
        || token.len() > 128
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(JournalError::InvalidToken(token.to_owned()));
    }
    Ok(())
}

fn read_record(path: &Path) -> Result<StoredJournalRecord, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut data = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|error| error.to_string())?;
    if data.len() as u64 > MAX_RECORD_BYTES {
        return Err("journal record exceeds its size bound".to_owned());
    }
    serde_json::from_slice(&data).map_err(|error| format!("invalid journal record: {error}"))
}

enum VerifyFailure {
    AlreadyExited(String),
    Mismatch(String),
    Unverifiable(String),
}

fn verify_child(
    record: &JournalRecord,
    alternate_command_hash: Option<&str>,
) -> Result<u32, VerifyFailure> {
    let identity = match read_process_identity(record.child_pid) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            return Err(VerifyFailure::AlreadyExited(
                "recorded child is no longer present".to_owned(),
            ));
        }
        #[cfg(not(target_os = "linux"))]
        Err(IdentityError::Unsupported) => {
            return Err(VerifyFailure::Unverifiable(
                "child identity cannot be verified on this platform".to_owned(),
            ));
        }
        Err(error) => {
            return Err(VerifyFailure::Unverifiable(format!(
                "child identity cannot be read: {error}"
            )));
        }
    };
    let Some(start_time) = record.start_time else {
        return Err(VerifyFailure::Unverifiable(
            "journal has no child start time".to_owned(),
        ));
    };
    if identity.start_time != start_time {
        return Err(VerifyFailure::Mismatch(
            "child PID was reused with a different start time".to_owned(),
        ));
    }
    let ProcessGroupIdentity::Unix { pgid } = &record.group else {
        return Err(VerifyFailure::Unverifiable(
            "the recorded process group cannot be verified on this platform".to_owned(),
        ));
    };
    let pgid = *pgid;
    if pgid <= 1 || pgid != record.child_pid || identity.group_id != pgid {
        return Err(VerifyFailure::Mismatch(
            "child process group does not match the journal".to_owned(),
        ));
    }
    if current_group_id() == Some(pgid) {
        return Err(VerifyFailure::Mismatch(
            "refusing to signal the current process group".to_owned(),
        ));
    }
    let expected_executable = match fs::canonicalize(&record.executable) {
        Ok(path) => path,
        Err(error) => {
            return Err(VerifyFailure::Unverifiable(format!(
                "recorded executable cannot be resolved: {error}"
            )));
        }
    };
    let actual_executable = match fs::canonicalize(&identity.executable) {
        Ok(path) => path,
        Err(error) => {
            return Err(VerifyFailure::Unverifiable(format!(
                "live executable cannot be resolved: {error}"
            )));
        }
    };
    if expected_executable != actual_executable {
        return Err(VerifyFailure::Mismatch(
            "child executable does not match the journal".to_owned(),
        ));
    }
    if identity.argv.is_empty() {
        return Err(VerifyFailure::Unverifiable(
            "live command line is unavailable".to_owned(),
        ));
    }
    let actual_args = identity.argv[1..].to_vec();
    let actual_hash = command_hash_bytes(&record.executable, &actual_args);
    if actual_hash != record.command_hash && alternate_command_hash != Some(actual_hash.as_str()) {
        return Err(VerifyFailure::Mismatch(
            "child command hash does not match the journal".to_owned(),
        ));
    }
    Ok(pgid)
}

fn force_kill_group(pgid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        let group = format!("-{pgid}");
        let mut command = None;
        for executable in ["/bin/kill", "/usr/bin/kill"] {
            if Path::new(executable).is_file() {
                command = Some(executable);
                break;
            }
        }
        let Some(executable) = command else {
            return Err("no fixed kill executable is available".to_owned());
        };
        let status = std::process::Command::new(executable)
            .env_clear()
            .arg("-KILL")
            .arg("--")
            .arg(group)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|error| format!("failed to invoke fixed kill executable: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("fixed kill executable returned {status}"))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        Err("process-group recovery is not available on this platform".to_owned())
    }
}

fn path_token(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn invalid_entry(token: &str, reason: &str) -> RecoveryEntry {
    RecoveryEntry {
        token: token.to_owned(),
        disposition: RecoveryDisposition::Invalid,
        reason: reason.to_owned(),
    }
}

fn push_entry(report: &mut RecoveryReport, entry: RecoveryEntry, limit: usize) {
    if report.entries.len() < limit {
        report.entries.push(entry);
    } else {
        report.truncated = true;
    }
}
