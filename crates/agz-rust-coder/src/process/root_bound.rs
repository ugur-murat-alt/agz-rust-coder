//! Root-bound process launch.  The outer process never resolves the requested
//! directory as its cwd: a small instance of this binary first binds its cwd
//! and proves that it is still the directory authorized by `RootGuard`.

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use crate::workspace::AuthorizedRoot;

pub const GUARD_ARGUMENT: &str = "--agz-internal-root-guard";
const TARGET_ENV: &str = "AGZ_RUST_CODER_GUARD_TARGET";
#[cfg(unix)]
const DEVICE_ENV: &str = "AGZ_RUST_CODER_GUARD_DEVICE";
#[cfg(unix)]
const INODE_ENV: &str = "AGZ_RUST_CODER_GUARD_INODE";
/// Enables the LSP-only stable root descriptor in the guarded child.
const LSP_ROOT_DESCRIPTOR_ENV: &str = "AGZ_RUST_CODER_GUARD_LSP_ROOT_DESCRIPTOR";
#[cfg(unix)]
const LSP_ROOT_DESCRIPTOR_FD: i32 = 198;
const TEST_READY_ENV: &str = "AGZ_RUST_CODER_GUARD_TEST_READY";
const TEST_CONTINUE_ENV: &str = "AGZ_RUST_CODER_GUARD_TEST_CONTINUE";

#[derive(Debug, thiserror::Error)]
pub enum RootBindingError {
    #[error("cannot resolve the agz-rust-coder root guard executable: {0}")]
    GuardExecutable(String),
    #[error("cannot inspect authorized directory: {0}")]
    Identity(String),
    #[error("authorized directory changed before the child process started")]
    Mismatch,
    #[error("root guard input is invalid")]
    InvalidInput,
    #[error("root guard failed: {0}")]
    Io(#[from] io::Error),
}

/// A command line for the outer guard process.  `authority` is deliberately
/// retained by its caller for the complete child lifetime.
#[derive(Debug, Clone)]
pub(crate) struct RootBoundCommand {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    /// Arguments the guard passes to the logical child after its identity check.
    /// They are distinct from `args`, which are the outer guard's argv.
    pub handoff_args: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
}

impl RootBoundCommand {
    pub(crate) fn new(
        authority: &Arc<AuthorizedRoot>,
        target: &Path,
        executable: &Path,
        args: &[OsString],
        environment: &BTreeMap<OsString, OsString>,
    ) -> Result<Self, RootBindingError> {
        Self::new_inner(authority, target, executable, args, environment, false)
    }

    /// Builds the guard command used only for a language server.  Its guarded
    /// child receives a stable descriptor alias for the authorized root.
    pub(crate) fn for_lsp(
        authority: &Arc<AuthorizedRoot>,
        target: &Path,
        executable: &Path,
        args: &[OsString],
        environment: &BTreeMap<OsString, OsString>,
    ) -> Result<Self, RootBindingError> {
        Self::new_inner(authority, target, executable, args, environment, true)
    }

    fn new_inner(
        authority: &Arc<AuthorizedRoot>,
        target: &Path,
        executable: &Path,
        args: &[OsString],
        environment: &BTreeMap<OsString, OsString>,
        lsp_root_descriptor: bool,
    ) -> Result<Self, RootBindingError> {
        if !target.is_absolute() || target != authority.path() {
            return Err(RootBindingError::InvalidInput);
        }
        let guard = std::fs::canonicalize(guard_executable()?).map_err(RootBindingError::Io)?;
        // The outer guard is still launched by pathname.  If that pathname is
        // inside the mutable authorized root, a root replacement could change
        // the guard before it reaches the descriptor-backed identity check.
        // Portable descriptor execution is unavailable, so fail closed.
        if guard.starts_with(target) {
            return Err(RootBindingError::GuardExecutable(format!(
                "root guard executable {} is inside the authorized root {}",
                guard.display(),
                target.display()
            )));
        }
        let executable = resolve_executable(executable)?;
        // Executing a root-contained absolute pathname would resolve it again
        // after the guard's identity check.  The explicit `./` form instead
        // resolves from the already-verified cwd, which remains attached to
        // the original directory after a lexical root replacement.
        let guarded_executable = executable.strip_prefix(target).map_or_else(
            |_| executable.clone(),
            |relative| PathBuf::from(".").join(relative),
        );
        // This metadata is obtained through the live capability, not from the
        // ambient path.  Unix guard verification closes the pathname race after
        // chdir; Windows keeps this handle live, preventing replacement.
        #[cfg(unix)]
        let metadata = authority
            .dir()
            .dir_metadata()
            .map_err(|error| RootBindingError::Identity(error.to_string()))?;
        let handoff_args = args
            .iter()
            .map(|argument| {
                let path = Path::new(argument);
                path.strip_prefix(target).map_or_else(
                    |_| argument.clone(),
                    |relative| {
                        if relative.as_os_str().is_empty() {
                            OsString::from(".")
                        } else {
                            relative.as_os_str().to_owned()
                        }
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut guard_args = Vec::with_capacity(handoff_args.len().saturating_add(4));
        guard_args.push(OsString::from(GUARD_ARGUMENT));
        guard_args.push(target.as_os_str().to_owned());
        guard_args.push(guarded_executable.as_os_str().to_owned());
        guard_args.push(OsString::from("--"));
        guard_args.extend(handoff_args.iter().cloned());

        let mut guard_environment = environment.clone();
        // This flag is internal-only.  Do not allow arbitrary subprocesses to
        // request a descriptor just by supplying an environment variable.
        guard_environment.remove(OsStr::new(LSP_ROOT_DESCRIPTOR_ENV));
        if lsp_root_descriptor {
            guard_environment.insert(OsString::from(LSP_ROOT_DESCRIPTOR_ENV), OsString::from("1"));
        }
        guard_environment.insert(OsString::from(TARGET_ENV), target.as_os_str().to_owned());
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            guard_environment.insert(
                OsString::from(DEVICE_ENV),
                metadata.dev().to_string().into(),
            );
            guard_environment.insert(OsString::from(INODE_ENV), metadata.ino().to_string().into());
        }
        Ok(Self {
            executable: guard,
            args: guard_args,
            handoff_args,
            environment: guard_environment,
        })
    }
}

/// Returns the protocol-visible alias for the LSP's retained root descriptor.
/// Only the guarded LSP constructor creates the descriptor on Unix.
pub(crate) fn lsp_protocol_root(lexical: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let _ = lexical;
        PathBuf::from(format!("/proc/self/fd/{LSP_ROOT_DESCRIPTOR_FD}"))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = lexical;
        PathBuf::from(format!("/dev/fd/{LSP_ROOT_DESCRIPTOR_FD}"))
    }
    #[cfg(not(unix))]
    {
        lexical.to_owned()
    }
}

fn resolve_executable(path: &Path) -> Result<PathBuf, RootBindingError> {
    if path.is_absolute() {
        return std::fs::canonicalize(path).map_err(RootBindingError::Io);
    }
    let Some(paths) = env::var_os("PATH") else {
        return Err(RootBindingError::GuardExecutable(
            "PATH is unavailable for executable resolution".to_owned(),
        ));
    };
    env::split_paths(&paths)
        .map(|directory| directory.join(path))
        .find(|candidate| candidate.is_file())
        .map(|candidate| std::fs::canonicalize(candidate).map_err(RootBindingError::Io))
        .transpose()?
        .ok_or_else(|| {
            RootBindingError::GuardExecutable(format!(
                "executable {} was not found",
                path.display()
            ))
        })
}

fn guard_executable() -> Result<PathBuf, RootBindingError> {
    let current =
        env::current_exe().map_err(|error| RootBindingError::GuardExecutable(error.to_string()))?;
    if current
        .file_stem()
        .is_some_and(|name| name == "agz-rust-coder")
    {
        return std::fs::canonicalize(current).map_err(RootBindingError::Io);
    }
    let Some(parent) = current.parent().and_then(Path::parent) else {
        return Err(RootBindingError::GuardExecutable(
            "test executable has no target directory".to_owned(),
        ));
    };
    let sibling = parent.join(format!("agz-rust-coder{}", env::consts::EXE_SUFFIX));
    if sibling.is_file() {
        std::fs::canonicalize(sibling).map_err(RootBindingError::Io)
    } else {
        Err(RootBindingError::GuardExecutable(format!(
            "expected sibling binary {} is unavailable",
            sibling.display()
        )))
    }
}

/// Runs only in the executable, before Clap or MCP stdio initialization.
///
/// The marker and original argv are separate OS strings, so no lossy quoting or
/// shell parsing is introduced.
pub fn maybe_run_root_guard() -> Result<Option<()>, RootBindingError> {
    let mut argv = env::args_os();
    let _program = argv.next();
    if argv.next().as_deref() != Some(OsStr::new(GUARD_ARGUMENT)) {
        return Ok(None);
    }
    let target = PathBuf::from(argv.next().ok_or(RootBindingError::InvalidInput)?);
    let executable = PathBuf::from(argv.next().ok_or(RootBindingError::InvalidInput)?);
    if argv.next().as_deref() != Some(OsStr::new("--")) {
        return Err(RootBindingError::InvalidInput);
    }
    let arguments = argv.collect::<Vec<_>>();
    let expected_target = env::var_os(TARGET_ENV).ok_or(RootBindingError::InvalidInput)?;
    if expected_target != target.as_os_str()
        || !target.is_absolute()
        || !valid_guard_executable(&executable)
    {
        return Err(RootBindingError::InvalidInput);
    }
    #[cfg(unix)]
    {
        use std::os::unix::{fs::MetadataExt, process::CommandExt};
        let device = env::var(DEVICE_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let inode = env::var(INODE_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let (Some(device), Some(inode)) = (device, inode) else {
            return Err(RootBindingError::InvalidInput);
        };
        env::set_current_dir(&target)?;
        let current = std::fs::metadata(".")?;
        if current.dev() != device || current.ino() != inode {
            return Err(RootBindingError::Mismatch);
        }
        #[cfg(debug_assertions)]
        test_barrier_after_identity_check()?;
        let lsp_root_descriptor =
            env::var_os(LSP_ROOT_DESCRIPTOR_ENV).is_some_and(|value| value == OsStr::new("1"));
        let mut command = Command::new(executable);
        command
            .args(arguments)
            .env_remove(TARGET_ENV)
            .env_remove(DEVICE_ENV)
            .env_remove(INODE_ENV)
            .env_remove(LSP_ROOT_DESCRIPTOR_ENV)
            .env_remove(TEST_READY_ENV)
            .env_remove(TEST_CONTINUE_ENV);
        if lsp_root_descriptor {
            use command_fds::{CommandFdExt, FdMapping};
            use std::os::fd::OwnedFd;

            let root: OwnedFd = std::fs::File::open(".")?.into();
            command
                .fd_mappings(vec![FdMapping {
                    parent_fd: root,
                    child_fd: LSP_ROOT_DESCRIPTOR_FD,
                }])
                .map_err(|error| RootBindingError::Identity(error.to_string()))?;
        }
        let error = command.exec();
        Err(RootBindingError::Io(error))
    }
    #[cfg(windows)]
    {
        env::set_current_dir(&target)?;
        let status = Command::new(executable)
            .args(arguments)
            .env_remove(TARGET_ENV)
            .env_remove(LSP_ROOT_DESCRIPTOR_ENV)
            .env_remove(TEST_READY_ENV)
            .env_remove(TEST_CONTINUE_ENV)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;
        exit_with_status(status);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, executable, arguments);
        Err(RootBindingError::InvalidInput)
    }
}

fn valid_guard_executable(executable: &Path) -> bool {
    if executable.is_absolute() {
        return true;
    }
    let mut components = executable.components();
    if !matches!(components.next(), Some(std::path::Component::CurDir)) {
        return false;
    }
    let mut has_name = false;
    for component in components {
        if !matches!(component, std::path::Component::Normal(_)) {
            return false;
        }
        has_name = true;
    }
    has_name
}

#[cfg(all(unix, debug_assertions))]
fn test_barrier_after_identity_check() -> Result<(), RootBindingError> {
    use std::time::{Duration, Instant};

    let (Some(ready), Some(continue_file)) = (
        env::var_os(TEST_READY_ENV).map(PathBuf::from),
        env::var_os(TEST_CONTINUE_ENV).map(PathBuf::from),
    ) else {
        return Ok(());
    };
    std::fs::write(ready, b"ready")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !continue_file.exists() {
        if Instant::now() >= deadline {
            return Err(RootBindingError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "root guard test barrier timed out",
            )));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

#[cfg(windows)]
fn exit_with_status(status: std::process::ExitStatus) -> ! {
    std::process::exit(status.code().unwrap_or(1));
}
