use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use super::roots::{AuthorizedRoot, DirectoryEntryKind, RootError, WorkspaceRoot};
use crate::process::root_bound::RootBoundCommand;

const DEFAULT_MAX_FILES: usize = 20_000;
const DEFAULT_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_EXTERNAL_FILES: usize = 5_000;
const DEFAULT_MAX_EXTERNAL_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_GIT_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_WALK_DIRECTORIES: usize = 100_000;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("identity computation was cancelled")]
    Cancelled,
    #[error("identity computation exceeded the request deadline")]
    TimedOut,
    #[error("identity root operation failed: {0}")]
    Root(#[from] RootError),
    #[error("invalid identity input: {0}")]
    InvalidInput(String),
    #[error("git identity probe failed: {0}")]
    Git(String),
    #[error("git root binding failed: {0}")]
    RootBinding(String),
    #[error("identity I/O failed for {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IdentityIncompleteReason {
    GitTruncated,
    GitFailed,
    FileBudget,
    FileSizeBudget,
    ByteBudget,
    ExternalBudget,
    Symlink,
    Unreadable,
    NoGitBudget,
    Boundary,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct IdentityLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_external_files: usize,
    pub max_external_bytes: u64,
    pub max_git_output_bytes: usize,
}

impl Default for IdentityLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_external_files: DEFAULT_MAX_EXTERNAL_FILES,
            max_external_bytes: DEFAULT_MAX_EXTERNAL_BYTES,
            max_git_output_bytes: DEFAULT_MAX_GIT_OUTPUT_BYTES,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InputIdentity {
    pub hash: String,
    pub command_hash: String,
    pub environment_hash: String,
    pub head: String,
    pub changed_paths: Vec<PathBuf>,
    pub files_hashed: usize,
    pub bytes_hashed: u64,
    pub external_files_hashed: usize,
    pub external_bytes_hashed: u64,
    pub complete: bool,
    pub incomplete_reason: Option<IdentityIncompleteReason>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GitOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub truncated: bool,
}

pub trait GitProbe: Send + Sync {
    /// Cooperatively stop identity collection when its owning request ends.
    fn checkpoint(&self) -> Result<(), IdentityError> {
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error when the bounded probe cannot be started or its output
    /// cannot be represented as an identity input.
    fn run(
        &self,
        cwd: &Path,
        args: &[OsString],
        max_output_bytes: usize,
    ) -> Result<GitOutput, IdentityError>;

    fn run_authorized(
        &self,
        cwd: &Path,
        args: &[OsString],
        max_output_bytes: usize,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<GitOutput, IdentityError> {
        let _ = authority;
        self.run(cwd, args, max_output_bytes)
    }
}

#[derive(Debug, Clone)]
pub struct StdGitProbe {
    executable: PathBuf,
}

impl StdGitProbe {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn fixed() -> Self {
        let executable = ["/usr/bin/git", "/bin/git"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("git"));
        Self::new(executable)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }
}

impl Default for StdGitProbe {
    fn default() -> Self {
        Self::fixed()
    }
}

impl GitProbe for StdGitProbe {
    fn run(
        &self,
        cwd: &Path,
        args: &[OsString],
        max_output_bytes: usize,
    ) -> Result<GitOutput, IdentityError> {
        if !cwd.is_absolute() {
            return Err(IdentityError::InvalidInput(
                "git probe cwd must be absolute".to_owned(),
            ));
        }
        for argument in args {
            validate_os_string(argument, "git argument")?;
        }
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.collect_output(command, max_output_bytes)
    }

    fn run_authorized(
        &self,
        cwd: &Path,
        args: &[OsString],
        max_output_bytes: usize,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<GitOutput, IdentityError> {
        if !cwd.is_absolute() {
            return Err(IdentityError::InvalidInput(
                "git probe cwd must be absolute".to_owned(),
            ));
        }
        for argument in args {
            validate_os_string(argument, "git argument")?;
        }
        let environment: BTreeMap<OsString, OsString> = [
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        ]
        .into_iter()
        .collect();
        let bound = RootBoundCommand::new(&authority, cwd, &self.executable, args, &environment)
            .map_err(|error| IdentityError::RootBinding(error.to_string()))?;
        let mut command = Command::new(&bound.executable);
        command
            .env_clear()
            .envs(bound.environment)
            .args(bound.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = self.collect_output(command, max_output_bytes)?;
        // Keep the exact directory handle live through child completion. This
        // is the Windows replacement barrier; Unix uses the verified cwd.
        drop(authority);
        Ok(output)
    }
}

impl StdGitProbe {
    fn collect_output(
        &self,
        mut command: Command,
        max_output_bytes: usize,
    ) -> Result<GitOutput, IdentityError> {
        let mut child = command.spawn().map_err(|source| IdentityError::Io {
            path: self.executable.clone(),
            source,
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_reader = thread::spawn(move || {
            stdout.map_or_else(BoundedRead::default, |reader| {
                read_bounded(reader, max_output_bytes)
            })
        });
        let stderr_reader = thread::spawn(move || {
            stderr.map_or_else(BoundedRead::default, |reader| {
                read_bounded(reader, max_output_bytes)
            })
        });
        let status = child
            .wait()
            .map_err(|source| IdentityError::Io {
                path: self.executable.clone(),
                source,
            })?
            .code();
        let stdout = stdout_reader
            .join()
            .map_err(|_| IdentityError::Git("git stdout reader panicked".to_owned()))?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| IdentityError::Git("git stderr reader panicked".to_owned()))?;
        Ok(GitOutput {
            status,
            stdout: stdout.bytes,
            truncated: stdout.truncated || stderr.truncated,
        })
    }
}

pub struct IdentityInput<'a> {
    pub root: &'a WorkspaceRoot,
    pub manifest_path: &'a Path,
    pub git_cwd: &'a Path,
    pub cargo: &'a Path,
    pub command: &'a [OsString],
    pub environment: &'a BTreeMap<OsString, OsString>,
    pub external_roots: &'a [Arc<AuthorizedRoot>],
    pub target_directory: Option<&'a Path>,
    pub limits: IdentityLimits,
    pub git: &'a dyn GitProbe,
}

impl<'a> IdentityInput<'a> {
    pub fn new(
        root: &'a WorkspaceRoot,
        manifest_path: &'a Path,
        cargo: &'a Path,
        command: &'a [OsString],
        environment: &'a BTreeMap<OsString, OsString>,
        git: &'a dyn GitProbe,
    ) -> Self {
        Self {
            root,
            manifest_path,
            git_cwd: root.path(),
            cargo,
            command,
            environment,
            external_roots: &[],
            target_directory: None,
            limits: IdentityLimits::default(),
            git,
        }
    }

    #[must_use]
    pub fn with_git_cwd(mut self, git_cwd: &'a Path) -> Self {
        self.git_cwd = git_cwd;
        self
    }

    #[must_use]
    pub fn with_external_roots(mut self, external_roots: &'a [Arc<AuthorizedRoot>]) -> Self {
        self.external_roots = external_roots;
        self
    }

    #[must_use]
    pub fn with_target_directory(mut self, target_directory: &'a Path) -> Self {
        self.target_directory = Some(target_directory);
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: IdentityLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// # Errors
///
/// Returns an error when an identity input is invalid, Git reports malformed
/// paths, or an authorized root operation fails.
#[allow(clippy::too_many_lines)]
pub fn compute_input_identity(input: &IdentityInput<'_>) -> Result<InputIdentity, IdentityError> {
    compute_input_identity_inner(input, None)
}

/// Computes an identity while binding Git child processes to the input root.
/// Internal callers with an existing root authority must use this variant.
pub fn compute_input_identity_authorized(
    input: &IdentityInput<'_>,
) -> Result<InputIdentity, IdentityError> {
    compute_input_identity_inner(input, Some(None))
}

/// Internal variant for callers that retained an exact Git/worktree capability
/// during workspace selection. The public authorized façade intentionally
/// keeps its existing input shape and derives its authority as before.
pub(crate) fn compute_input_identity_with_git_authority(
    input: &IdentityInput<'_>,
    authority: Arc<AuthorizedRoot>,
) -> Result<InputIdentity, IdentityError> {
    compute_input_identity_inner(input, Some(Some(authority)))
}

#[allow(clippy::too_many_lines)]
fn compute_input_identity_inner(
    input: &IdentityInput<'_>,
    authorized: Option<Option<Arc<AuthorizedRoot>>>,
) -> Result<InputIdentity, IdentityError> {
    input.git.checkpoint()?;
    validate_input(input)?;
    let command_hash = hash_command(input.cargo, input.command, input.environment);
    let environment_hash = hash_environment(input.environment);
    let mut hasher = IdentityHasher::new();
    hasher.write_label("agz-rust-coder-input-identity");
    hasher.write_str(&command_hash);
    hasher.write_str(&environment_hash);

    let head_args = git_args(["rev-parse", "--verify", "HEAD"]);
    let head_result = run_git(input, &head_args, &authorized);
    input.git.checkpoint()?;
    let mut incomplete_reason = None;
    let (head, use_git) = match head_result {
        Ok(output) if output.truncated => ("NO_GIT".to_owned(), false),
        Ok(output) if output.status == Some(0) => {
            let head = String::from_utf8(output.stdout)
                .ok()
                .map(|head| head.trim().to_owned())
                .filter(|head| !head.is_empty());
            if let Some(head) = head {
                (head, true)
            } else {
                ("NO_GIT".to_owned(), false)
            }
        }
        Ok(_) | Err(_) => ("NO_GIT".to_owned(), false),
    };
    hasher.write_str(&head);
    hasher.write_path(input.manifest_path);

    let git_paths = if use_git {
        collect_git_paths(input, &mut incomplete_reason, &authorized)?
    } else {
        Vec::new()
    };
    input.git.checkpoint()?;
    let mut files = BTreeSet::new();
    let mut changed_paths = Vec::new();
    let mut collector = FileCollector::new(input, &mut files, &mut incomplete_reason);

    if use_git {
        for path in git_paths {
            input.git.checkpoint()?;
            let absolute = path.absolute;
            changed_paths.push(path.relative);
            collector.collect_changed_path(&absolute)?;
        }
        collector.walk_workspace(false)?;
    } else {
        collector.walk_workspace(true)?;
    }
    collector.add_identity_config_paths();
    collector.walk_external_roots()?;
    changed_paths.sort();
    changed_paths.dedup();

    hasher.write_usize(changed_paths.len());
    for path in &changed_paths {
        input.git.checkpoint()?;
        hasher.write_path(path);
        let absolute = input.root.authority_path().join(path);
        if !path_exists_without_following(&absolute) {
            hasher.write_label("missing");
        }
    }

    let mut files_hashed = 0usize;
    let mut bytes_hashed = 0u64;
    let mut external_files_hashed = 0usize;
    let mut external_bytes_hashed = 0u64;
    for path in files {
        input.git.checkpoint()?;
        let external = external_root_for(&path, input.external_roots).is_some();
        if external {
            if external_files_hashed >= input.limits.max_external_files {
                set_reason(
                    &mut incomplete_reason,
                    IdentityIncompleteReason::ExternalBudget,
                );
                continue;
            }
        } else if files_hashed >= input.limits.max_files {
            set_reason(&mut incomplete_reason, IdentityIncompleteReason::FileBudget);
            continue;
        }

        hasher.write_path(&path);
        match read_identity_file(input, &path, input.limits.max_file_bytes) {
            Ok(bytes) => {
                if external {
                    if external_bytes_hashed.saturating_add(bytes.len() as u64)
                        > input.limits.max_external_bytes
                    {
                        set_reason(
                            &mut incomplete_reason,
                            IdentityIncompleteReason::ExternalBudget,
                        );
                        hasher.write_label("external-byte-budget");
                        continue;
                    }
                    external_bytes_hashed =
                        external_bytes_hashed.saturating_add(bytes.len() as u64);
                    external_files_hashed += 1;
                } else {
                    if bytes_hashed.saturating_add(bytes.len() as u64)
                        > input.limits.max_total_bytes
                    {
                        set_reason(&mut incomplete_reason, IdentityIncompleteReason::ByteBudget);
                        hasher.write_label("byte-budget");
                        continue;
                    }
                    bytes_hashed = bytes_hashed.saturating_add(bytes.len() as u64);
                    files_hashed += 1;
                }
                hasher.write_bytes(&bytes);
            }
            Err(ReadIdentityError::Missing) => {
                hasher.write_label("missing");
            }
            Err(ReadIdentityError::TooLarge(size)) => {
                set_reason(
                    &mut incomplete_reason,
                    IdentityIncompleteReason::FileSizeBudget,
                );
                hasher.write_label("file-size-budget");
                hasher.write_u64(size);
            }
            Err(ReadIdentityError::Symlink) => {
                set_reason(&mut incomplete_reason, IdentityIncompleteReason::Symlink);
                hasher.write_label("symlink");
            }
            Err(ReadIdentityError::Boundary) => {
                set_reason(&mut incomplete_reason, IdentityIncompleteReason::Boundary);
                hasher.write_label("boundary");
            }
            Err(ReadIdentityError::Unreadable) => {
                set_reason(&mut incomplete_reason, IdentityIncompleteReason::Unreadable);
                hasher.write_label("unreadable");
            }
        }
    }

    input.git.checkpoint()?;
    Ok(InputIdentity {
        hash: hasher.finish(),
        command_hash,
        environment_hash,
        head,
        changed_paths,
        files_hashed,
        bytes_hashed,
        external_files_hashed,
        external_bytes_hashed,
        complete: incomplete_reason.is_none(),
        incomplete_reason,
    })
}

fn validate_input(input: &IdentityInput<'_>) -> Result<(), IdentityError> {
    if !input.root.path().is_absolute()
        || !input.git_cwd.is_absolute()
        || !input.manifest_path.is_absolute()
        || !input.cargo.is_absolute()
    {
        return Err(IdentityError::InvalidInput(
            "identity paths must be absolute".to_owned(),
        ));
    }
    if !input.root.authority().contains(input.git_cwd) {
        return Err(IdentityError::InvalidInput(
            "git cwd is outside the authorized root".to_owned(),
        ));
    }
    if !input.root.authority().contains(input.manifest_path) {
        return Err(IdentityError::InvalidInput(
            "manifest is outside the authorized root".to_owned(),
        ));
    }
    for root in input.external_roots {
        if root.kind() != super::roots::RootKind::Dependency {
            return Err(IdentityError::InvalidInput(
                "external identity roots must be dependency roots".to_owned(),
            ));
        }
    }
    Ok(())
}

struct FileCollector<'input, 'scope, 'data> {
    input: &'input IdentityInput<'scope>,
    files: &'data mut BTreeSet<PathBuf>,
    incomplete_reason: &'data mut Option<IdentityIncompleteReason>,
    workspace_directories: usize,
}

impl<'input, 'scope, 'data> FileCollector<'input, 'scope, 'data> {
    fn new(
        input: &'input IdentityInput<'scope>,
        files: &'data mut BTreeSet<PathBuf>,
        incomplete_reason: &'data mut Option<IdentityIncompleteReason>,
    ) -> Self {
        Self {
            input,
            files,
            incomplete_reason,
            workspace_directories: 0,
        }
    }

    fn collect_changed_path(&mut self, path: &Path) -> Result<(), IdentityError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                set_reason(self.incomplete_reason, IdentityIncompleteReason::Symlink);
            }
            Ok(metadata) if metadata.is_dir() => {
                self.walk_tree(
                    self.input.root.authority(),
                    path,
                    true,
                    self.input.limits.max_files,
                    false,
                )?;
            }
            Ok(metadata) if metadata.is_file() => {
                self.insert(path.to_owned(), false);
            }
            Ok(_) => set_reason(self.incomplete_reason, IdentityIncompleteReason::Unreadable),
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    set_reason(self.incomplete_reason, IdentityIncompleteReason::Unreadable);
                }
            }
        }
        Ok(())
    }

    fn walk_workspace(&mut self, include_all: bool) -> Result<(), IdentityError> {
        self.walk_tree(
            self.input.root.authority(),
            self.input.root.path(),
            include_all,
            self.input.limits.max_files,
            false,
        )
    }

    fn walk_external_roots(&mut self) -> Result<(), IdentityError> {
        let roots: Vec<_> = self.input.external_roots.to_vec();
        for root in roots {
            self.walk_tree(
                &root,
                root.path(),
                true,
                self.input.limits.max_external_files,
                true,
            )?;
        }
        Ok(())
    }

    fn walk_tree(
        &mut self,
        root: &AuthorizedRoot,
        start: &Path,
        include_all: bool,
        max_files: usize,
        external: bool,
    ) -> Result<(), IdentityError> {
        let mut pending = vec![(start.to_owned(), 0usize)];
        while let Some((directory, depth)) = pending.pop() {
            self.input.git.checkpoint()?;
            if self.workspace_directories >= MAX_WALK_DIRECTORIES {
                set_reason(
                    self.incomplete_reason,
                    if external {
                        IdentityIncompleteReason::ExternalBudget
                    } else {
                        IdentityIncompleteReason::FileBudget
                    },
                );
                break;
            }
            self.workspace_directories += 1;
            let entries = match root.list_directory(&directory) {
                Ok(entries) => entries,
                Err(RootError::PathNotFound(_)) => {
                    set_reason(self.incomplete_reason, IdentityIncompleteReason::Unreadable);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                self.input.git.checkpoint()?;
                if should_skip_directory(&entry.name) {
                    continue;
                }
                let path = directory.join(&entry.name);
                if self
                    .input
                    .target_directory
                    .is_some_and(|target| path_is_within(target, &path))
                {
                    continue;
                }
                match entry.kind {
                    DirectoryEntryKind::Symlink => {
                        set_reason(self.incomplete_reason, IdentityIncompleteReason::Symlink);
                    }
                    DirectoryEntryKind::Directory => {
                        if depth >= 64 {
                            set_reason(
                                self.incomplete_reason,
                                IdentityIncompleteReason::FileBudget,
                            );
                        } else {
                            pending.push((path, depth + 1));
                        }
                    }
                    DirectoryEntryKind::RegularFile => {
                        if include_all || is_identity_candidate(&path) {
                            self.insert_with_limit(path, external, max_files);
                        }
                    }
                    DirectoryEntryKind::Other => {
                        set_reason(self.incomplete_reason, IdentityIncompleteReason::Unreadable);
                    }
                }
            }
        }
        Ok(())
    }

    fn insert(&mut self, path: PathBuf, external: bool) {
        self.insert_with_limit(
            path,
            external,
            if external {
                self.input.limits.max_external_files
            } else {
                self.input.limits.max_files
            },
        );
    }

    fn insert_with_limit(&mut self, path: PathBuf, external: bool, max_files: usize) {
        if self.files.contains(&path) {
            return;
        }
        let count = self
            .files
            .iter()
            .filter(|candidate| {
                external_root_for(candidate, self.input.external_roots).is_some() == external
            })
            .count();
        if count >= max_files {
            set_reason(
                self.incomplete_reason,
                if external {
                    IdentityIncompleteReason::ExternalBudget
                } else {
                    IdentityIncompleteReason::FileBudget
                },
            );
            return;
        }
        self.files.insert(path);
    }

    fn add_identity_config_paths(&mut self) {
        let paths = identity_config_paths(
            self.input.root.path(),
            self.input.git_cwd,
            self.input.manifest_path,
            self.input.environment,
        );
        for path in paths {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    set_reason(self.incomplete_reason, IdentityIncompleteReason::Symlink);
                }
                Ok(metadata) if metadata.is_file() => {
                    self.files.insert(path);
                }
                Ok(_) => set_reason(self.incomplete_reason, IdentityIncompleteReason::Unreadable),
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        set_reason(self.incomplete_reason, IdentityIncompleteReason::Unreadable);
                    }
                }
            }
        }
    }
}

struct ChangedPath {
    relative: PathBuf,
    absolute: PathBuf,
}

fn run_git(
    input: &IdentityInput<'_>,
    args: &[OsString],
    authorized: &Option<Option<Arc<AuthorizedRoot>>>,
) -> Result<GitOutput, IdentityError> {
    input.git.checkpoint()?;
    if let Some(authority) = authorized {
        let authority = match authority {
            Some(authority) => authority.clone(),
            None => input
                .root
                .authority()
                .authorize_dir(input.git_cwd)
                .map_err(IdentityError::Root)?,
        };
        input.git.run_authorized(
            input.git_cwd,
            args,
            input.limits.max_git_output_bytes,
            authority,
        )
    } else {
        input
            .git
            .run(input.git_cwd, args, input.limits.max_git_output_bytes)
    }
}

fn collect_git_paths(
    input: &IdentityInput<'_>,
    incomplete_reason: &mut Option<IdentityIncompleteReason>,
    authorized: &Option<Option<Arc<AuthorizedRoot>>>,
) -> Result<Vec<ChangedPath>, IdentityError> {
    let mut paths = Vec::new();
    let probes = [
        git_args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "-z",
            "HEAD",
            "--",
            ".",
        ]),
        git_args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ]),
    ];
    for args in probes {
        let Ok(output) = run_git(input, &args, authorized) else {
            set_reason(incomplete_reason, IdentityIncompleteReason::GitFailed);
            return Ok(paths);
        };
        if output.truncated {
            set_reason(incomplete_reason, IdentityIncompleteReason::GitTruncated);
            return Ok(paths);
        }
        if output.status != Some(0) {
            set_reason(incomplete_reason, IdentityIncompleteReason::GitFailed);
            return Ok(paths);
        }
        for relative in parse_git_paths(&output.stdout)? {
            let absolute = lexical_join(input.git_cwd, &relative).ok_or_else(|| {
                IdentityError::InvalidInput(format!(
                    "git reported a path outside the lexical workspace boundary: {}",
                    relative.display()
                ))
            })?;
            if !input.root.authority().contains(&absolute) {
                set_reason(incomplete_reason, IdentityIncompleteReason::Boundary);
                continue;
            }
            if input
                .target_directory
                .is_some_and(|target| path_is_within(target, &absolute))
            {
                continue;
            }
            let relative_to_authority = absolute
                .strip_prefix(input.root.authority_path())
                .map(normalize_relative)
                .map_err(|_| {
                    IdentityError::InvalidInput("git path escaped authority".to_owned())
                })?;
            paths.push(ChangedPath {
                relative: relative_to_authority,
                absolute,
            });
        }
    }
    paths.sort_by(|left, right| left.relative.cmp(&right.relative));
    paths.dedup_by(|left, right| left.relative == right.relative);
    if paths.len() > input.limits.max_files {
        paths.truncate(input.limits.max_files);
        set_reason(incomplete_reason, IdentityIncompleteReason::FileBudget);
    }
    Ok(paths)
}

fn parse_git_paths(bytes: &[u8]) -> Result<Vec<PathBuf>, IdentityError> {
    let mut paths = Vec::new();
    for value in bytes
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
    {
        let value = std::str::from_utf8(value)
            .map_err(|_| IdentityError::Git("git returned a non-UTF-8 path".to_owned()))?;
        let path = PathBuf::from(value);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
        {
            return Err(IdentityError::Git(
                "git returned an absolute path".to_owned(),
            ));
        }
        paths.push(path);
    }
    Ok(paths)
}

fn read_identity_file(
    input: &IdentityInput<'_>,
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, ReadIdentityError> {
    if let Some(root) = external_root_for(path, input.external_roots) {
        return root
            .read_file(path, max_bytes)
            .map_err(|error| map_root_read_error(&error));
    }
    if input.root.authority().contains(path) {
        return input
            .root
            .authority()
            .read_file(path, max_bytes)
            .map_err(|error| map_root_read_error(&error));
    }
    read_exact_file(path, max_bytes)
}

fn map_root_read_error(error: &RootError) -> ReadIdentityError {
    match error {
        RootError::PathNotFound(_) => ReadIdentityError::Missing,
        RootError::TooLarge { size, .. } => ReadIdentityError::TooLarge(*size),
        RootError::Symlink(_) => ReadIdentityError::Symlink,
        RootError::PathOutsideRoot(_) => ReadIdentityError::Boundary,
        _ => ReadIdentityError::Unreadable,
    }
}

#[derive(Debug)]
enum ReadIdentityError {
    Missing,
    TooLarge(u64),
    Symlink,
    Boundary,
    Unreadable,
}

fn external_root_for<'a>(
    path: &Path,
    roots: &'a [Arc<AuthorizedRoot>],
) -> Option<&'a Arc<AuthorizedRoot>> {
    roots.iter().find(|root| root.contains(path))
}

fn identity_config_paths(
    workspace_root: &Path,
    git_cwd: &Path,
    manifest_path: &Path,
    environment: &BTreeMap<OsString, OsString>,
) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    paths.insert(manifest_path.to_owned());
    for root in [
        workspace_root,
        git_cwd,
        manifest_path.parent().unwrap_or(workspace_root),
    ] {
        paths.insert(root.join("Cargo.toml"));
        paths.insert(root.join("Cargo.lock"));
        paths.insert(root.join("rust-toolchain"));
        paths.insert(root.join("rust-toolchain.toml"));
        let mut current = root.to_owned();
        loop {
            paths.insert(current.join(".cargo/config"));
            paths.insert(current.join(".cargo/config.toml"));
            let Some(parent) = current.parent() else {
                break;
            };
            if parent == current {
                break;
            }
            current = parent.to_owned();
        }
    }
    let cargo_home = environment
        .get(OsStr::new("CARGO_HOME"))
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .get(OsStr::new("HOME"))
                .map(|home| PathBuf::from(home).join(".cargo"))
        })
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        paths.insert(cargo_home.join("config"));
        paths.insert(cargo_home.join("config.toml"));
    }
    paths.into_iter().collect()
}

fn should_skip_directory(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".worktrees" | "node_modules" | "target")
    )
}

fn is_identity_candidate(path: &Path) -> bool {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    name == "Cargo.toml"
        || name == "Cargo.lock"
        || name == "rust-toolchain"
        || name == "rust-toolchain.toml"
        || name == "config"
        || name == "config.toml"
        || path.extension().is_some_and(|extension| extension == "rs")
}

fn path_exists_without_following(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.file_type().is_symlink())
}

fn path_is_within(root: &Path, candidate: &Path) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_ok_and(|relative| !relative.is_absolute())
}

fn lexical_join(base: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.is_absolute() {
        return None;
    }
    let mut result = base.to_owned();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::Normal(name) => result.push(name),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(result)
}

fn normalize_relative(path: &Path) -> PathBuf {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            _ => None,
        })
        .collect()
}

fn git_args<const N: usize>(arguments: [&str; N]) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from("diff.external="),
        OsString::from("-c"),
        OsString::from("pager.diff=false"),
        OsString::from("-c"),
        OsString::from("pager.status=false"),
    ];
    args.extend(arguments.into_iter().map(OsString::from));
    args
}

fn validate_os_string(value: &OsStr, name: &str) -> Result<(), IdentityError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if value.as_bytes().contains(&0) {
            return Err(IdentityError::InvalidInput(format!(
                "{name} contains a NUL byte"
            )));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if value.encode_wide().any(|unit| unit == 0) {
            return Err(IdentityError::InvalidInput(format!(
                "{name} contains a NUL byte"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct BoundedRead {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded<R: Read>(mut reader: R, max_bytes: usize) -> BoundedRead {
    let mut output = BoundedRead {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut buffer = [0_u8; 16 * 1024];
    while let Ok(read) = reader.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let before_len = output.bytes.len();
        if before_len < max_bytes {
            let remaining = max_bytes - before_len;
            output
                .bytes
                .extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if before_len.saturating_add(read) > max_bytes {
            output.truncated = true;
        }
    }
    output
}

fn read_exact_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ReadIdentityError> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ReadIdentityError::Missing
        } else {
            ReadIdentityError::Unreadable
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ReadIdentityError::Symlink);
    }
    if !metadata.is_file() {
        return Err(ReadIdentityError::Unreadable);
    }
    if metadata.len() > max_bytes {
        return Err(ReadIdentityError::TooLarge(metadata.len()));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|_| ReadIdentityError::Unreadable)?;
    let opened = file.metadata().map_err(|_| ReadIdentityError::Unreadable)?;
    if !opened.is_file() || opened.len() > max_bytes {
        return if opened.len() > max_bytes {
            Err(ReadIdentityError::TooLarge(opened.len()))
        } else {
            Err(ReadIdentityError::Unreadable)
        };
    }
    let mut bytes = Vec::with_capacity(opened.len().try_into().unwrap_or(0));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ReadIdentityError::Unreadable)?;
    if bytes.len() as u64 > max_bytes {
        return Err(ReadIdentityError::TooLarge(bytes.len() as u64));
    }
    Ok(bytes)
}

fn reject_symlink_components(path: &Path) -> Result<(), ReadIdentityError> {
    let mut current = PathBuf::new();
    let components = path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => continue,
            Component::ParentDir => return Err(ReadIdentityError::Boundary),
            Component::Normal(name) => current.push(name),
        }
        if current.parent().is_none() || index + 1 == components.len() {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ReadIdentityError::Symlink);
            }
            Ok(metadata) if !metadata.is_dir() => return Err(ReadIdentityError::Unreadable),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ReadIdentityError::Missing);
            }
            Err(_) => return Err(ReadIdentityError::Unreadable),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(open_no_follow_flag());
}

#[cfg(not(unix))]
fn configure_no_follow(_options: &mut OpenOptions) {}

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

fn hash_command(
    cargo: &Path,
    command: &[OsString],
    environment: &BTreeMap<OsString, OsString>,
) -> String {
    let mut hasher = IdentityHasher::new();
    hasher.write_label("agz-rust-coder-command");
    hasher.write_path(cargo);
    for argument in command {
        hasher.write_os(argument);
    }
    for (key, value) in environment {
        hasher.write_os(key);
        hasher.write_os(value);
    }
    hasher.finish()
}

fn hash_environment(environment: &BTreeMap<OsString, OsString>) -> String {
    let mut hasher = IdentityHasher::new();
    hasher.write_label("agz-rust-coder-environment");
    for (key, value) in environment {
        hasher.write_os(key);
        hasher.write_os(value);
    }
    hasher.finish()
}

fn set_reason(reason: &mut Option<IdentityIncompleteReason>, next: IdentityIncompleteReason) {
    if reason.is_none() {
        *reason = Some(next);
    }
}

struct IdentityHasher {
    hasher: Sha256,
}

impl IdentityHasher {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    fn write_label(&mut self, value: &str) {
        self.write_str(value);
    }

    fn write_str(&mut self, value: &str) {
        self.write_usize(value.len());
        self.hasher.update(value.as_bytes());
    }

    fn write_os(&mut self, value: &OsStr) {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.write_usize(value.as_bytes().len());
            self.hasher.update(value.as_bytes());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let value = value.encode_wide().collect::<Vec<_>>();
            self.write_usize(value.len());
            for unit in value {
                self.hasher.update(unit.to_le_bytes());
            }
        }
        #[cfg(not(any(unix, windows)))]
        self.write_str(&value.to_string_lossy());
    }

    fn write_path(&mut self, value: &Path) {
        self.write_os(value.as_os_str());
    }

    fn write_bytes(&mut self, value: &[u8]) {
        self.write_usize(value.len());
        self.hasher.update(value);
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_u64(&mut self, value: u64) {
        self.hasher.update(value.to_le_bytes());
    }

    fn finish(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

impl fmt::Debug for IdentityInput<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityInput")
            .field("root", &self.root.path())
            .field("manifest_path", &self.manifest_path)
            .field("git_cwd", &self.git_cwd)
            .field("cargo", &self.cargo)
            .field("command_arguments", &self.command.len())
            .field("environment_entries", &self.environment.len())
            .field("external_roots", &self.external_roots.len())
            .field("target_directory", &self.target_directory)
            .field("limits", &self.limits)
            .finish()
    }
}
