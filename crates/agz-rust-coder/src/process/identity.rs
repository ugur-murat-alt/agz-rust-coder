use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum IdentityError {
    #[cfg(not(target_os = "linux"))]
    #[error("process identity is not available on this platform")]
    Unsupported,
    #[error("malformed process identity: {0}")]
    Malformed(&'static str),
    #[error("process identity I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub(crate) struct LiveProcessIdentity {
    pub(crate) start_time: u64,
    pub(crate) executable: PathBuf,
    pub(crate) group_id: u32,
    pub(crate) argv: Vec<Vec<u8>>,
}

#[cfg(target_os = "linux")]
pub(crate) fn read_process_identity(
    pid: u32,
) -> Result<Option<LiveProcessIdentity>, IdentityError> {
    let stat_path = proc_path(pid, "stat");
    let stat = match fs::read_to_string(&stat_path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let (start_time, group_id) = parse_stat(&stat)?;

    let executable = match fs::read_link(proc_path(pid, "exe")) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let cmdline = match fs::read(proc_path(pid, "cmdline")) {
        Ok(cmdline) => cmdline,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let argv = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(<[u8]>::to_vec)
        .collect();

    Ok(Some(LiveProcessIdentity {
        start_time,
        executable,
        group_id,
        argv,
    }))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_process_identity(
    _pid: u32,
) -> Result<Option<LiveProcessIdentity>, IdentityError> {
    Err(IdentityError::Unsupported)
}

#[cfg(target_os = "linux")]
fn proc_path(pid: u32, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/{name}"))
}

#[cfg(target_os = "linux")]
fn parse_stat(stat: &str) -> Result<(u64, u32), IdentityError> {
    let end_of_name = stat
        .rfind(')')
        .ok_or(IdentityError::Malformed("missing process name"))?;
    let fields: Vec<&str> = stat[end_of_name + 1..].split_whitespace().collect();
    let group_id = fields
        .get(2)
        .ok_or(IdentityError::Malformed("missing process group"))?
        .parse()
        .map_err(|_| IdentityError::Malformed("invalid process group"))?;
    let start_time = fields
        .get(19)
        .ok_or(IdentityError::Malformed("missing process start time"))?
        .parse()
        .map_err(|_| IdentityError::Malformed("invalid process start time"))?;
    Ok((start_time, group_id))
}

pub(crate) fn current_start_time() -> Option<u64> {
    read_process_identity(std::process::id())
        .ok()
        .flatten()
        .map(|identity| identity.start_time)
}

pub(crate) fn current_group_id() -> Option<u32> {
    read_process_identity(std::process::id())
        .ok()
        .flatten()
        .map(|identity| identity.group_id)
}

pub(crate) fn command_hash(executable: &Path, args: &[OsString]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"agz-rust-coder-command\0");
    update_os(&mut hash, executable.as_os_str());
    for argument in args {
        hash.update([0]);
        update_os(&mut hash, argument.as_os_str());
    }
    format!("{:x}", hash.finalize())
}

pub(crate) fn command_hash_bytes(executable: &Path, args: &[Vec<u8>]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"agz-rust-coder-command\0");
    update_os(&mut hash, executable.as_os_str());
    for argument in args {
        hash.update([0]);
        hash.update(argument);
    }
    format!("{:x}", hash.finalize())
}

fn update_os(hash: &mut Sha256, value: &OsStr) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hash.update(value.as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in value.encode_wide() {
            hash.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hash.update(value.to_string_lossy().as_bytes());
}
