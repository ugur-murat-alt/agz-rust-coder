#![allow(
    clippy::manual_let_else,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::single_match_else,
    clippy::too_many_lines
)]

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

#[cfg(windows)]
use std::path::Prefix;

use cap_std::fs::{Dir, File as CapabilityFile};

const DEFAULT_WALK_MAX_FILES: usize = 20_000;
const DEFAULT_WALK_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_WALK_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_WALK_MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RootKind {
    Workspace,
    Dependency,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RootError {
    EmptyConfiguredRoots,
    RelativePath,
    AbsolutePath,
    ParentComponent,
    InvalidPath(String),
    InvalidFileUri(String),
    ClientRootsEmpty,
    ClientRootsUnavailable,
    NoRootIntersection,
    MultipleRoots,
    PathOutsideRoot(PathBuf),
    PathNotFound(PathBuf),
    NotDirectory(PathBuf),
    NotRegularFile(PathBuf),
    Symlink(PathBuf),
    TooLarge {
        path: PathBuf,
        size: u64,
        max_bytes: u64,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
    Poisoned,
}

impl fmt::Display for RootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyConfiguredRoots => {
                formatter.write_str("no configured workspace root was supplied")
            }
            Self::RelativePath => formatter.write_str("relative path is not valid in this context"),
            Self::AbsolutePath => formatter.write_str("absolute path is not valid in this context"),
            Self::ParentComponent => formatter.write_str("parent path components are not allowed"),
            Self::InvalidPath(reason) => write!(formatter, "invalid path: {reason}"),
            Self::InvalidFileUri(uri) => write!(formatter, "invalid local file URI: {uri}"),
            Self::ClientRootsEmpty => {
                formatter.write_str("the advertised client root list was empty")
            }
            Self::ClientRootsUnavailable => {
                formatter.write_str("the advertised client roots could not be read")
            }
            Self::NoRootIntersection => {
                formatter.write_str("client roots do not intersect configured roots")
            }
            Self::MultipleRoots => {
                formatter.write_str("a directory is required when multiple roots are authorized")
            }
            Self::PathOutsideRoot(path) => write!(
                formatter,
                "path is outside the authorized root: {}",
                path.display()
            ),
            Self::PathNotFound(path) => write!(formatter, "path was not found: {}", path.display()),
            Self::NotDirectory(path) => {
                write!(formatter, "path is not a directory: {}", path.display())
            }
            Self::NotRegularFile(path) => {
                write!(formatter, "path is not a regular file: {}", path.display())
            }
            Self::Symlink(path) => write!(
                formatter,
                "symlink components are not allowed: {}",
                path.display()
            ),
            Self::TooLarge {
                path,
                size,
                max_bytes,
            } => write!(
                formatter,
                "file is too large: {} bytes > {} at {}",
                size,
                max_bytes,
                path.display()
            ),
            Self::Io {
                operation,
                path,
                message,
            } => write!(
                formatter,
                "{operation} failed for {}: {message}",
                path.display()
            ),
            Self::Poisoned => formatter.write_str("workspace root state was poisoned"),
        }
    }
}

impl std::error::Error for RootError {}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ClientRoots {
    Unsupported,
    Available(Vec<PathBuf>),
    Failed,
}

impl ClientRoots {
    pub fn unsupported() -> Self {
        Self::Unsupported
    }

    pub fn available(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self::Available(paths.into_iter().collect())
    }

    pub fn failed() -> Self {
        Self::Failed
    }

    pub fn from_file_uris<I, S>(uris: I) -> Result<Self, RootError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let paths = uris
            .into_iter()
            .map(|uri| parse_file_uri(uri.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::Available(paths))
    }
}

#[derive(Debug)]
pub struct AuthorizedRoot {
    canonical: PathBuf,
    dir: Dir,
    kind: RootKind,
}

impl AuthorizedRoot {
    fn open(path: &Path, kind: RootKind) -> Result<Arc<Self>, RootError> {
        let canonical = canonical_directory(path)?;
        let dir = Dir::open_ambient_dir(&canonical, cap_std::ambient_authority())
            .map_err(|error| io_error("open authorized directory", &canonical, &error))?;
        Ok(Arc::new(Self {
            canonical,
            dir,
            kind,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.canonical
    }

    pub fn kind(&self) -> RootKind {
        self.kind
    }

    pub fn dir(&self) -> &Dir {
        &self.dir
    }

    pub fn contains(&self, path: &Path) -> bool {
        path.is_absolute() && path_is_within(&self.canonical, path)
    }

    pub fn relative_path(&self, path: &Path) -> Result<PathBuf, RootError> {
        let relative = self.relative_input(path)?;
        let absolute = self.canonical.join(&relative);
        check_no_symlink_components(&absolute)?;
        let canonical =
            fs::canonicalize(&absolute).map_err(|error| map_path_error(&absolute, error))?;
        if !path_is_within(&self.canonical, &canonical) {
            return Err(RootError::PathOutsideRoot(canonical));
        }
        canonical
            .strip_prefix(&self.canonical)
            .map(normalize_relative)
            .map_err(|_| RootError::PathOutsideRoot(canonical))
    }

    pub fn authorize_dir(&self, path: &Path) -> Result<Arc<Self>, RootError> {
        let relative = self.relative_path(path)?;
        self.open_dir_relative(&relative)
    }

    pub fn open_file(&self, path: &Path, max_bytes: u64) -> Result<BoundedFile, RootError> {
        let relative = self.relative_input(path)?;
        if relative.as_os_str().is_empty() {
            return Err(RootError::NotRegularFile(self.canonical.clone()));
        }

        let (parent, file_name) = split_final_component(&relative)?;
        let parent_dir = self.open_dir_relative(&parent)?;
        let metadata = parent_dir
            .dir
            .symlink_metadata(&file_name)
            .map_err(|error| map_path_error(&self.canonical.join(&relative), error))?;
        if metadata.is_symlink() {
            return Err(RootError::Symlink(self.canonical.join(relative)));
        }
        if !metadata.is_file() {
            return Err(RootError::NotRegularFile(self.canonical.join(relative)));
        }
        let size = metadata.len();
        if size > max_bytes {
            return Err(RootError::TooLarge {
                path: self.canonical.join(&relative),
                size,
                max_bytes,
            });
        }
        let file = parent_dir
            .dir
            .open(&file_name)
            .map_err(|error| map_path_error(&self.canonical.join(&relative), error))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| map_path_error(&self.canonical.join(&relative), error))?;
        if !opened_metadata.is_file() {
            return Err(RootError::NotRegularFile(self.canonical.join(relative)));
        }
        if opened_metadata.len() > max_bytes {
            return Err(RootError::TooLarge {
                path: self.canonical.join(&relative),
                size: opened_metadata.len(),
                max_bytes,
            });
        }
        Ok(BoundedFile {
            file,
            relative_path: relative,
            size: opened_metadata.len(),
            max_bytes,
        })
    }

    pub fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, RootError> {
        self.open_file(path, max_bytes)?.read_to_end()
    }

    pub fn list_directory(&self, path: &Path) -> Result<Vec<DirectoryEntry>, RootError> {
        let relative = self.relative_input(path)?;
        let directory = self.open_dir_relative(&relative)?;
        let entries = directory
            .dir
            .entries()
            .map_err(|error| io_error("list authorized directory", &directory.canonical, &error))?;
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| io_error("read directory entry", &directory.canonical, &error))?;
            let name = entry.file_name();
            let kind = entry
                .file_type()
                .map(|file_type| {
                    if file_type.is_symlink() {
                        DirectoryEntryKind::Symlink
                    } else if file_type.is_dir() {
                        DirectoryEntryKind::Directory
                    } else if file_type.is_file() {
                        DirectoryEntryKind::RegularFile
                    } else {
                        DirectoryEntryKind::Other
                    }
                })
                .map_err(|error| {
                    io_error("inspect directory entry", &directory.canonical, &error)
                })?;
            result.push(DirectoryEntry { name, kind });
        }
        result.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(result)
    }

    pub fn walk_files_matching<F>(
        &self,
        limits: WalkLimits,
        mut include: F,
    ) -> Result<WalkResult, RootError>
    where
        F: FnMut(&Path) -> bool,
    {
        let mut pending = vec![(
            self.dir
                .try_clone()
                .map_err(|error| io_error("clone root directory", &self.canonical, &error))?,
            PathBuf::new(),
            0usize,
        )];
        let mut files = Vec::new();
        let mut issues = Vec::new();
        let mut total_bytes = 0u64;

        while let Some((directory, relative, depth)) = pending.pop() {
            let entries = match directory.entries() {
                Ok(entries) => entries,
                Err(error) => {
                    issues.push(WalkIssue {
                        path: relative,
                        kind: WalkIssueKind::Unreadable,
                    });
                    let _ = error;
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        issues.push(WalkIssue {
                            path: relative.clone(),
                            kind: WalkIssueKind::Unreadable,
                        });
                        continue;
                    }
                };
                let name = entry.file_name();
                if should_skip_directory(&name, &limits.skip_directories) {
                    continue;
                }
                let child = relative.join(&name);
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(_) => {
                        issues.push(WalkIssue {
                            path: child,
                            kind: WalkIssueKind::Unreadable,
                        });
                        continue;
                    }
                };
                if file_type.is_symlink() {
                    issues.push(WalkIssue {
                        path: child,
                        kind: WalkIssueKind::Symlink,
                    });
                    continue;
                }
                if file_type.is_dir() {
                    if depth >= limits.max_depth {
                        issues.push(WalkIssue {
                            path: child,
                            kind: WalkIssueKind::DepthLimit,
                        });
                        continue;
                    }
                    match entry.open_dir() {
                        Ok(child_dir) => pending.push((child_dir, child, depth + 1)),
                        Err(_) => issues.push(WalkIssue {
                            path: child,
                            kind: WalkIssueKind::Unreadable,
                        }),
                    }
                    continue;
                }
                if !file_type.is_file() || !include(&child) {
                    continue;
                }
                if files.len() >= limits.max_files {
                    issues.push(WalkIssue {
                        path: child,
                        kind: WalkIssueKind::FileLimit,
                    });
                    continue;
                }
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) => {
                        issues.push(WalkIssue {
                            path: child,
                            kind: WalkIssueKind::Unreadable,
                        });
                        continue;
                    }
                };
                let size = metadata.len();
                if size > limits.max_file_bytes {
                    issues.push(WalkIssue {
                        path: child,
                        kind: WalkIssueKind::FileTooLarge,
                    });
                    continue;
                }
                if total_bytes.saturating_add(size) > limits.max_total_bytes {
                    issues.push(WalkIssue {
                        path: child,
                        kind: WalkIssueKind::ByteLimit,
                    });
                    continue;
                }
                total_bytes = total_bytes.saturating_add(size);
                files.push(WalkFile {
                    path: child,
                    bytes: size,
                });
            }
        }

        files.sort_by(|left, right| left.path.cmp(&right.path));
        issues.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(WalkResult {
            files,
            issues,
            total_bytes,
        })
    }

    fn relative_input(&self, path: &Path) -> Result<PathBuf, RootError> {
        let normalized = normalize_path(path)?;
        if path.is_absolute() {
            let relative = normalized
                .strip_prefix(&self.canonical)
                .map_err(|_| RootError::PathOutsideRoot(normalized.clone()))?;
            Ok(normalize_relative(relative))
        } else {
            Ok(normalized)
        }
    }

    fn open_dir_relative(&self, relative: &Path) -> Result<Arc<Self>, RootError> {
        let relative = normalize_relative(relative);
        let mut directory = self
            .dir
            .try_clone()
            .map_err(|error| io_error("clone authorized directory", &self.canonical, &error))?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(RootError::InvalidPath(relative.display().to_string()));
            };
            let metadata = directory
                .symlink_metadata(name)
                .map_err(|error| map_path_error(&self.canonical.join(&relative), error))?;
            if metadata.is_symlink() {
                return Err(RootError::Symlink(self.canonical.join(&relative)));
            }
            if !metadata.is_dir() {
                return Err(RootError::NotDirectory(self.canonical.join(&relative)));
            }
            directory = directory
                .open_dir(name)
                .map_err(|error| map_path_error(&self.canonical.join(&relative), error))?;
        }
        Ok(Arc::new(Self {
            canonical: self.canonical.join(&relative),
            dir: directory,
            kind: self.kind,
        }))
    }
}

#[derive(Debug)]
pub struct BoundedFile {
    file: CapabilityFile,
    relative_path: PathBuf,
    size: u64,
    max_bytes: u64,
}

impl BoundedFile {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn into_inner(self) -> CapabilityFile {
        self.file
    }

    pub fn read_to_end(mut self) -> Result<Vec<u8>, RootError> {
        let capacity = self.size.min(self.max_bytes).try_into().unwrap_or(0usize);
        let mut bytes = Vec::with_capacity(capacity);
        let read_limit = self.max_bytes.saturating_add(1);
        self.file
            .by_ref()
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|error| RootError::Io {
                operation: "read bounded file",
                path: self.relative_path.clone(),
                message: error.to_string(),
            })?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(RootError::TooLarge {
                path: self.relative_path,
                size: bytes.len() as u64,
                max_bytes: self.max_bytes,
            });
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone)]
pub struct WalkLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_depth: usize,
    pub skip_directories: Vec<OsString>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DirectoryEntryKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub name: OsString,
    pub kind: DirectoryEntryKind,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_WALK_MAX_FILES,
            max_file_bytes: DEFAULT_WALK_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_WALK_MAX_TOTAL_BYTES,
            max_depth: DEFAULT_WALK_MAX_DEPTH,
            skip_directories: [".git", "target", "vendor"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WalkFile {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum WalkIssueKind {
    Symlink,
    Unreadable,
    NonRegular,
    FileTooLarge,
    FileLimit,
    ByteLimit,
    DepthLimit,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WalkIssue {
    pub path: PathBuf,
    pub kind: WalkIssueKind,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WalkResult {
    pub files: Vec<WalkFile>,
    pub issues: Vec<WalkIssue>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub root: Arc<AuthorizedRoot>,
    pub canonical: PathBuf,
    pub relative: PathBuf,
}

#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    root: Arc<AuthorizedRoot>,
    requested: PathBuf,
    epoch: u64,
}

impl WorkspaceRoot {
    pub(crate) fn from_parts(root: Arc<AuthorizedRoot>, requested: PathBuf, epoch: u64) -> Self {
        Self {
            root,
            requested,
            epoch,
        }
    }

    pub fn root(&self) -> &Arc<AuthorizedRoot> {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.requested
    }

    pub fn authority(&self) -> &Arc<AuthorizedRoot> {
        &self.root
    }

    pub fn authority_path(&self) -> &Path {
        self.root.path()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, RootError> {
        self.root.read_file(path, max_bytes)
    }

    pub fn list_directory(&self, path: &Path) -> Result<Vec<DirectoryEntry>, RootError> {
        self.root.list_directory(path)
    }

    pub fn resolve_existing(&self, path: &Path) -> Result<ResolvedPath, RootError> {
        self.root
            .resolve_existing(path)
            .map(|(canonical, relative)| ResolvedPath {
                root: self.root.clone(),
                canonical,
                relative,
            })
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.root.contains(path)
    }

    pub fn walk_files_matching<F>(
        &self,
        limits: WalkLimits,
        include: F,
    ) -> Result<WalkResult, RootError>
    where
        F: FnMut(&Path) -> bool,
    {
        self.root.walk_files_matching(limits, include)
    }
}

#[derive(Debug, Clone)]
pub struct RootSnapshot {
    epoch: u64,
    roots: Arc<[Arc<AuthorizedRoot>]>,
}

impl RootSnapshot {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn roots(&self) -> &[Arc<AuthorizedRoot>] {
        &self.roots
    }

    pub fn select(&self, directory: Option<&Path>) -> Result<WorkspaceRoot, RootError> {
        let (root, requested) = match directory {
            None if self.roots.len() == 1 => {
                (self.roots[0].clone(), self.roots[0].path().to_owned())
            }
            None => return Err(RootError::MultipleRoots),
            Some(directory) if !directory.is_absolute() => return Err(RootError::RelativePath),
            Some(directory) => {
                let resolved = self.resolve_existing(directory)?;
                resolved.root.authorize_dir(&resolved.canonical)?;
                (resolved.root, resolved.canonical)
            }
        };
        Ok(WorkspaceRoot {
            root,
            requested,
            epoch: self.epoch,
        })
    }

    pub fn resolve_existing(&self, path: &Path) -> Result<ResolvedPath, RootError> {
        if !path.is_absolute() && self.roots.len() != 1 {
            return Err(RootError::MultipleRoots);
        }
        let mut last_error = None;
        for root in self.roots.iter() {
            match root.resolve_existing(path) {
                Ok((canonical, relative)) => {
                    return Ok(ResolvedPath {
                        root: root.clone(),
                        canonical,
                        relative,
                    });
                }
                Err(error @ (RootError::PathOutsideRoot(_) | RootError::PathNotFound(_))) => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or(RootError::NoRootIntersection))
    }

    pub fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, RootError> {
        self.resolve_existing(path)?.root.read_file(path, max_bytes)
    }
}

#[derive(Debug)]
pub struct RootGuard {
    configured: Vec<Arc<AuthorizedRoot>>,
    dependencies: Vec<Arc<AuthorizedRoot>>,
    client: RwLock<ClientEpoch>,
}

#[derive(Debug)]
struct ClientEpoch {
    epoch: u64,
    state: Option<NormalizedClientRoots>,
    snapshot: Option<RootSnapshot>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum NormalizedClientRoots {
    Unsupported,
    Available(Vec<PathBuf>),
    Failed,
}

impl RootGuard {
    pub fn new<I, J>(configured: I, dependencies: J) -> Result<Self, RootError>
    where
        I: IntoIterator<Item = PathBuf>,
        J: IntoIterator<Item = PathBuf>,
    {
        let configured = open_roots(configured, RootKind::Workspace)?;
        if configured.is_empty() {
            return Err(RootError::EmptyConfiguredRoots);
        }
        Ok(Self {
            configured,
            dependencies: open_roots(dependencies, RootKind::Dependency)?,
            client: RwLock::new(ClientEpoch {
                epoch: 0,
                state: None,
                snapshot: None,
            }),
        })
    }

    pub fn from_current_dir(
        dependencies: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, RootError> {
        let current = std::env::current_dir().map_err(|error| RootError::Io {
            operation: "read current directory",
            path: PathBuf::new(),
            message: error.to_string(),
        })?;
        Self::new([current], dependencies)
    }

    pub fn configured_roots(&self) -> &[Arc<AuthorizedRoot>] {
        &self.configured
    }

    pub fn dependency_roots(&self) -> &[Arc<AuthorizedRoot>] {
        &self.dependencies
    }

    pub fn snapshot(&self, client_roots: ClientRoots) -> Result<RootSnapshot, RootError> {
        let normalized = normalize_client_roots(&client_roots)?;
        if let NormalizedClientRoots::Failed = normalized {
            let mut state = self.client.write().map_err(|_| RootError::Poisoned)?;
            state.epoch = state.epoch.saturating_add(1);
            state.state = Some(normalized);
            state.snapshot = None;
            return Err(RootError::ClientRootsUnavailable);
        }

        {
            let state = self.client.read().map_err(|_| RootError::Poisoned)?;
            if state.state.as_ref() == Some(&normalized)
                && let Some(snapshot) = &state.snapshot
            {
                return Ok(snapshot.clone());
            }
        }

        let roots = match &normalized {
            NormalizedClientRoots::Unsupported => self.configured.clone(),
            NormalizedClientRoots::Available(client) => self.intersect_client_roots(client)?,
            NormalizedClientRoots::Failed => unreachable!(),
        };
        if roots.is_empty() {
            return Err(RootError::NoRootIntersection);
        }

        let mut state = self.client.write().map_err(|_| RootError::Poisoned)?;
        if state.state.as_ref() == Some(&normalized)
            && let Some(snapshot) = &state.snapshot
        {
            return Ok(snapshot.clone());
        }
        state.epoch = state.epoch.saturating_add(1);
        let snapshot = RootSnapshot {
            epoch: state.epoch,
            roots: Arc::from(roots.into_boxed_slice()),
        };
        state.state = Some(normalized);
        state.snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    pub fn current_snapshot(&self) -> Result<Option<RootSnapshot>, RootError> {
        Ok(self
            .client
            .read()
            .map_err(|_| RootError::Poisoned)?
            .snapshot
            .clone())
    }

    pub fn current_epoch(&self) -> Result<u64, RootError> {
        Ok(self.client.read().map_err(|_| RootError::Poisoned)?.epoch)
    }

    pub fn invalidate_client_roots(&self) -> Result<u64, RootError> {
        let mut state = self.client.write().map_err(|_| RootError::Poisoned)?;
        state.epoch = state.epoch.saturating_add(1);
        state.state = None;
        state.snapshot = None;
        Ok(state.epoch)
    }

    pub fn authorize_dependency(&self, path: &Path) -> Result<Arc<AuthorizedRoot>, RootError> {
        for root in &self.dependencies {
            if root.contains(path) {
                return root.authorize_dir(path);
            }
        }
        Err(RootError::PathOutsideRoot(path.to_path_buf()))
    }

    pub fn resolve_dependency(&self, path: &Path) -> Result<ResolvedPath, RootError> {
        for root in &self.dependencies {
            match root.resolve_existing(path) {
                Ok((canonical, relative)) => {
                    return Ok(ResolvedPath {
                        root: root.clone(),
                        canonical,
                        relative,
                    });
                }
                Err(RootError::PathOutsideRoot(_) | RootError::PathNotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Err(RootError::PathOutsideRoot(path.to_path_buf()))
    }

    fn intersect_client_roots(
        &self,
        client_roots: &[PathBuf],
    ) -> Result<Vec<Arc<AuthorizedRoot>>, RootError> {
        let mut roots = Vec::new();
        let mut seen = BTreeSet::new();
        for client in client_roots {
            let client = canonical_directory(client)?;
            for configured in &self.configured {
                let candidate = if path_is_within(&configured.canonical, &client) {
                    configured.authorize_dir(&client)?
                } else if path_is_within(&client, &configured.canonical) {
                    configured.clone()
                } else {
                    continue;
                };
                if seen.insert(candidate.canonical.clone()) {
                    roots.push(candidate);
                }
            }
        }
        roots.sort_by(|left, right| left.canonical.cmp(&right.canonical));
        Ok(roots)
    }
}

impl AuthorizedRoot {
    fn resolve_existing(&self, path: &Path) -> Result<(PathBuf, PathBuf), RootError> {
        let relative = self.relative_input(path)?;
        let absolute = self.canonical.join(&relative);
        check_no_symlink_components(&absolute)?;
        let canonical =
            fs::canonicalize(&absolute).map_err(|error| map_path_error(&absolute, error))?;
        if !path_is_within(&self.canonical, &canonical) {
            return Err(RootError::PathOutsideRoot(canonical));
        }
        Ok((
            canonical.clone(),
            normalize_relative(
                canonical
                    .strip_prefix(&self.canonical)
                    .map_err(|_| RootError::PathOutsideRoot(canonical.clone()))?,
            ),
        ))
    }
}

fn open_roots(
    paths: impl IntoIterator<Item = PathBuf>,
    kind: RootKind,
) -> Result<Vec<Arc<AuthorizedRoot>>, RootError> {
    let mut roots = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        let root = AuthorizedRoot::open(&path, kind)?;
        if seen.insert(root.canonical.clone()) {
            roots.push(root);
        }
    }
    roots.sort_by(|left, right| left.canonical.cmp(&right.canonical));
    Ok(roots)
}

fn normalize_client_roots(client_roots: &ClientRoots) -> Result<NormalizedClientRoots, RootError> {
    match client_roots {
        ClientRoots::Unsupported => Ok(NormalizedClientRoots::Unsupported),
        ClientRoots::Failed => Ok(NormalizedClientRoots::Failed),
        ClientRoots::Available(paths) if paths.is_empty() => Err(RootError::ClientRootsEmpty),
        ClientRoots::Available(paths) => {
            let mut normalized = BTreeSet::new();
            for path in paths {
                normalized.insert(canonical_directory(path)?);
            }
            if normalized.is_empty() {
                return Err(RootError::ClientRootsEmpty);
            }
            Ok(NormalizedClientRoots::Available(
                normalized.into_iter().collect(),
            ))
        }
    }
}

fn split_final_component(path: &Path) -> Result<(PathBuf, OsString), RootError> {
    let mut components = path.components().peekable();
    let mut parent = PathBuf::new();
    let mut final_name = None;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(RootError::InvalidPath(path.display().to_string()));
        };
        if components.peek().is_none() {
            final_name = Some(name.to_os_string());
        } else {
            parent.push(name);
        }
    }
    final_name
        .map(|name| (parent, name))
        .ok_or_else(|| RootError::InvalidPath(path.display().to_string()))
}

fn normalize_path(path: &Path) -> Result<PathBuf, RootError> {
    if path.as_os_str().is_empty() {
        return Ok(PathBuf::new());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => return Err(RootError::ParentComponent),
            Component::Normal(name) => normalized.push(name),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    if path.is_absolute() && !normalized.is_absolute() {
        return Err(RootError::InvalidPath(path.display().to_string()));
    }
    Ok(normalized)
}

fn normalize_relative(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(name) = component {
            normalized.push(name);
        }
    }
    normalized
}

fn canonical_directory(path: &Path) -> Result<PathBuf, RootError> {
    let absolute = absolute_path(path)?;
    check_no_symlink_components(&absolute)?;
    let canonical =
        fs::canonicalize(&absolute).map_err(|error| map_path_error(&absolute, error))?;
    let metadata = fs::metadata(&canonical).map_err(|error| map_path_error(&canonical, error))?;
    if !metadata.is_dir() {
        return Err(RootError::NotDirectory(canonical));
    }
    Ok(canonical)
}

fn absolute_path(path: &Path) -> Result<PathBuf, RootError> {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        let current = std::env::current_dir().map_err(|error| RootError::Io {
            operation: "read current directory",
            path: PathBuf::new(),
            message: error.to_string(),
        })?;
        normalize_path(&current.join(path))
    }
}

fn check_no_symlink_components(path: &Path) -> Result<(), RootError> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => return Err(RootError::ParentComponent),
            Component::Normal(name) => {
                current.push(name);
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|error| map_path_error(&current, error))?;
                if metadata.file_type().is_symlink() {
                    return Err(RootError::Symlink(current));
                }
            }
        }
    }
    Ok(())
}

fn path_is_within(root: &Path, candidate: &Path) -> bool {
    let Ok(root) = normalize_path(root) else {
        return false;
    };
    let Ok(candidate) = normalize_path(candidate) else {
        return false;
    };
    candidate == root
        || candidate
            .strip_prefix(&root)
            .is_ok_and(|relative| !relative.is_absolute())
}

fn should_skip_directory(name: &OsStr, skipped: &[OsString]) -> bool {
    skipped.iter().any(|candidate| candidate == name)
}

fn map_path_error(path: &Path, error: io::Error) -> RootError {
    if error.kind() == io::ErrorKind::NotFound {
        RootError::PathNotFound(path.to_path_buf())
    } else {
        io_error("filesystem operation", path, &error)
    }
}

fn io_error(operation: &'static str, path: &Path, error: &io::Error) -> RootError {
    RootError::Io {
        operation,
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

pub fn parse_file_uri(uri: &str) -> Result<PathBuf, RootError> {
    let Some(rest) = uri.strip_prefix("file://") else {
        return Err(RootError::InvalidFileUri(uri.to_owned()));
    };
    if rest.contains('?') || rest.contains('#') {
        return Err(RootError::InvalidFileUri(uri.to_owned()));
    }
    let (authority, path) = if rest.starts_with('/') {
        ("", rest)
    } else {
        let Some(separator) = rest.find('/') else {
            return Err(RootError::InvalidFileUri(uri.to_owned()));
        };
        (&rest[..separator], &rest[separator..])
    };
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return Err(RootError::InvalidFileUri(uri.to_owned()));
    }
    let decoded =
        percent_decode(path.as_bytes()).ok_or_else(|| RootError::InvalidFileUri(uri.to_owned()))?;
    let decoded =
        String::from_utf8(decoded).map_err(|_| RootError::InvalidFileUri(uri.to_owned()))?;
    #[cfg(windows)]
    let path = if decoded.starts_with('/') && decoded.as_bytes().get(2) == Some(&b':') {
        PathBuf::from(format!(r"\\?\{}", decoded[1..].replace('/', r"\")))
    } else {
        PathBuf::from(decoded)
    };
    #[cfg(windows)]
    match path.components().next() {
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => {}
        _ => return Err(RootError::InvalidFileUri(uri.to_owned())),
    }
    #[cfg(not(windows))]
    let path = PathBuf::from(decoded);
    if !path.is_absolute() {
        return Err(RootError::InvalidFileUri(uri.to_owned()));
    }
    normalize_path(&path).map_err(|_| RootError::InvalidFileUri(uri.to_owned()))
}

fn percent_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'%' {
            output.push(input[index]);
            index += 1;
            continue;
        }
        if index + 2 >= input.len() {
            return None;
        }
        let high = hex_value(input[index + 1])?;
        let low = hex_value(input[index + 2])?;
        output.push((high << 4) | low);
        index += 3;
    }
    Some(output)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
