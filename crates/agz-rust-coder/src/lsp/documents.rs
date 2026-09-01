//! Workspace-bound document and URI normalization.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::normalize::{self, NormalizeError};
use crate::workspace::{AuthorizedRoot, ClientRoots, RootError, RootGuard, WorkspaceRoot};

pub const DEFAULT_MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum DocumentError {
    #[error("document size limit must be greater than zero")]
    InvalidLimit,
    #[error("document path is not valid UTF-8: {0}")]
    InvalidUtf8(PathBuf),
    #[error("document content is not valid UTF-8: {0}")]
    ContentNotUtf8(PathBuf),
    #[error("document path is not a regular file: {0}")]
    NotRegularFile(PathBuf),
    #[error("document I/O failed for {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("document normalization failed: {0}")]
    Normalize(#[from] NormalizeError),
    #[error("workspace root error: {0}")]
    Root(#[from] RootError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedDocument {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub uri: String,
    pub text: String,
}

impl NormalizedDocument {
    pub fn file_name(&self) -> Option<&std::ffi::OsStr> {
        self.path.file_name()
    }
}

/// Resolve and read a file URI through the authorized workspace capability.
pub fn normalize_uri(
    root: &WorkspaceRoot,
    uri: &str,
    max_bytes: u64,
) -> Result<NormalizedDocument, DocumentError> {
    if max_bytes == 0 {
        return Err(DocumentError::InvalidLimit);
    }
    let path = normalize::uri_path(uri)?;
    let (path, relative_path, bytes) =
        read_authorized_file_with_hook(root.authority(), &path, max_bytes, || {})?;
    let canonical_uri = normalize::path_to_file_uri(&path)?;
    let text = String::from_utf8(bytes).map_err(|_| DocumentError::ContentNotUtf8(path.clone()))?;
    Ok(NormalizedDocument {
        path,
        relative_path,
        uri: canonical_uri,
        text,
    })
}

/// Resolve and read an absolute or workspace-relative path.  Relative paths
/// are interpreted by the already selected `WorkspaceRoot`, never by process
/// CWD.
pub fn normalize_path(
    root: &WorkspaceRoot,
    path: &Path,
    max_bytes: u64,
) -> Result<NormalizedDocument, DocumentError> {
    if max_bytes == 0 {
        return Err(DocumentError::InvalidLimit);
    }
    let (canonical, relative, bytes) =
        read_authorized_file_with_hook(root.authority(), path, max_bytes, || {})?;
    let text =
        String::from_utf8(bytes).map_err(|_| DocumentError::ContentNotUtf8(canonical.clone()))?;
    let uri = normalize::path_to_file_uri(&canonical)?;
    Ok(NormalizedDocument {
        path: canonical,
        relative_path: relative,
        uri,
        text,
    })
}

/// Open through the authorized directory capability before reading.
/// This keeps a final symlink swap from changing the bytes returned to callers.
pub(crate) fn read_authorized_file_with_hook<F>(
    authority: &AuthorizedRoot,
    path: &Path,
    max_bytes: u64,
    after_open: F,
) -> Result<(PathBuf, PathBuf, Vec<u8>), DocumentError>
where
    F: FnOnce(),
{
    if max_bytes == 0 {
        return Err(DocumentError::InvalidLimit);
    }
    let relative = relative_path(authority.path(), path)?;
    if relative.as_os_str().is_empty() {
        return Err(DocumentError::NotRegularFile(authority.path().to_owned()));
    }
    let file = authority
        .open_file(&relative, max_bytes)
        .map_err(DocumentError::Root)?;
    let relative = file.relative_path().to_owned();
    let canonical = authority.path().join(&relative);
    after_open();
    let bytes = file.read_to_end().map_err(DocumentError::Root)?;
    Ok((canonical, relative, bytes))
}

fn relative_path(root: &Path, path: &Path) -> Result<PathBuf, DocumentError> {
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .map_err(|_| DocumentError::Root(RootError::PathOutsideRoot(path.to_owned())))?
    } else {
        path
    };
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => normalized.push(name),
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(DocumentError::Root(RootError::ParentComponent));
            }
        }
    }
    Ok(normalized)
}

pub fn normalize_uri_under_guard(
    guard: &RootGuard,
    workspace: &Path,
    uri: &str,
    max_bytes: u64,
) -> Result<NormalizedDocument, DocumentError> {
    let root = select_workspace(guard, workspace)?;
    normalize_uri(&root, uri, max_bytes)
}

pub fn normalize_path_under_guard(
    guard: &RootGuard,
    workspace: &Path,
    path: &Path,
    max_bytes: u64,
) -> Result<NormalizedDocument, DocumentError> {
    let root = select_workspace(guard, workspace)?;
    normalize_path(&root, path, max_bytes)
}

pub fn select_workspace(
    guard: &RootGuard,
    workspace: &Path,
) -> Result<WorkspaceRoot, DocumentError> {
    let snapshot = guard.snapshot(ClientRoots::unsupported())?;
    Ok(snapshot.select(Some(workspace))?)
}

pub fn display_relative_path(document: &NormalizedDocument) -> Result<&str, DocumentError> {
    document
        .relative_path
        .to_str()
        .ok_or_else(|| DocumentError::InvalidUtf8(document.relative_path.clone()))
}

impl fmt::Display for NormalizedDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.relative_path.display())
    }
}
