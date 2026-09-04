#![allow(clippy::missing_errors_doc)]

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::roots::{AuthorizedRoot, DirectoryEntryKind, RootError, RootSnapshot, WorkspaceRoot};

const MAX_SELECTION_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SelectionError {
    Root(RootError),
    ManifestNotFound {
        requested_dir: PathBuf,
        worktree: PathBuf,
    },
    Ambiguous {
        candidates: Vec<PathBuf>,
    },
    InvalidManifest(PathBuf),
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(error) => error.fmt(formatter),
            Self::ManifestNotFound {
                requested_dir,
                worktree,
            } => write!(
                formatter,
                "Cargo.toml was not found from {} within worktree {}",
                requested_dir.display(),
                worktree.display()
            ),
            Self::Ambiguous { candidates } => write!(
                formatter,
                "Cargo workspace selection is ambiguous at the same depth: {}",
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::InvalidManifest(path) => write!(
                formatter,
                "Cargo.toml is not valid UTF-8: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SelectionError {}

impl From<RootError> for SelectionError {
    fn from(error: RootError) -> Self {
        Self::Root(error)
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceSelection {
    requested_dir: PathBuf,
    canonical_worktree: PathBuf,
    package_root: PathBuf,
    manifest_path: PathBuf,
    authority: Arc<AuthorizedRoot>,
    requested_authority: Arc<AuthorizedRoot>,
    package_authority: Arc<AuthorizedRoot>,
    worktree_authority: Arc<AuthorizedRoot>,
    epoch: u64,
}

impl WorkspaceSelection {
    pub fn requested_dir(&self) -> &Path {
        &self.requested_dir
    }

    pub fn canonical_worktree(&self) -> &Path {
        &self.canonical_worktree
    }

    pub fn package_root(&self) -> &Path {
        &self.package_root
    }

    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn authority(&self) -> &Arc<AuthorizedRoot> {
        &self.authority
    }

    pub fn requested_authority(&self) -> &Arc<AuthorizedRoot> {
        &self.requested_authority
    }

    /// Exact capability captured for the package selected during workspace
    /// discovery. Internal process owners must retain this instead of opening
    /// the package again through the configured parent root.
    pub(crate) fn package_authority(&self) -> &Arc<AuthorizedRoot> {
        &self.package_authority
    }

    /// Exact capability captured for the worktree selected during workspace
    /// discovery. It is used for Git and workspace-root descendants.
    pub(crate) fn worktree_authority(&self) -> &Arc<AuthorizedRoot> {
        &self.worktree_authority
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn requested_root(&self) -> WorkspaceRoot {
        WorkspaceRoot::from_parts(
            self.authority.clone(),
            self.requested_authority.clone(),
            self.requested_dir.clone(),
            self.epoch,
        )
    }
}

pub fn select_workspace(
    snapshot: &RootSnapshot,
    requested_dir: Option<&Path>,
) -> Result<WorkspaceSelection, SelectionError> {
    let selected = snapshot.select(requested_dir)?;
    select_in_root(&selected)
}

pub fn select_in_root(root: &WorkspaceRoot) -> Result<WorkspaceSelection, SelectionError> {
    let requested_dir = root.path().to_owned();
    let canonical_worktree = find_worktree_boundary(root, &requested_dir)?;

    if let Some(package_root) = find_ancestor_manifest(root, &requested_dir, &canonical_worktree)? {
        return make_selection(root, requested_dir, canonical_worktree, package_root);
    }

    let direct = child_manifest_candidates(root, &requested_dir)?;
    if direct.len() == 1 {
        return make_selection(root, requested_dir, canonical_worktree, direct[0].clone());
    }
    if direct.len() > 1 {
        return Err(SelectionError::Ambiguous { candidates: direct });
    }

    let mut nested = Vec::new();
    for child in directory_children(root, &requested_dir)? {
        if has_git_marker(root, &child)? {
            continue;
        }
        nested.extend(child_manifest_candidates(root, &child)?);
    }
    nested.sort();
    nested.dedup();
    match nested.as_slice() {
        [package_root] => make_selection(
            root,
            requested_dir,
            canonical_worktree,
            package_root.clone(),
        ),
        [] => Err(SelectionError::ManifestNotFound {
            requested_dir,
            worktree: canonical_worktree,
        }),
        _ => Err(SelectionError::Ambiguous { candidates: nested }),
    }
}

fn make_selection(
    root: &WorkspaceRoot,
    requested_dir: PathBuf,
    canonical_worktree: PathBuf,
    package_root: PathBuf,
) -> Result<WorkspaceSelection, SelectionError> {
    let manifest_path = package_root.join("Cargo.toml");
    let package_authority = capture_exact_authority(root, &package_root)?;
    let worktree_authority = capture_exact_authority(root, &canonical_worktree)?;
    Ok(WorkspaceSelection {
        requested_dir,
        canonical_worktree,
        package_root,
        manifest_path,
        authority: root.authority().clone(),
        requested_authority: root.requested_authority().clone(),
        package_authority,
        worktree_authority,
        epoch: root.epoch(),
    })
}

fn capture_exact_authority(
    root: &WorkspaceRoot,
    path: &Path,
) -> Result<Arc<AuthorizedRoot>, SelectionError> {
    if root.requested_authority().path() == path {
        return Ok(root.requested_authority().clone());
    }
    if root.authority().path() == path {
        return Ok(root.authority().clone());
    }
    Ok(root.authority().authorize_dir(path)?)
}

fn find_worktree_boundary(
    root: &WorkspaceRoot,
    requested: &Path,
) -> Result<PathBuf, SelectionError> {
    let mut current = requested.to_owned();
    loop {
        if has_git_marker(root, &current)? {
            return Ok(current);
        }
        if current == root.authority_path() {
            return Ok(current);
        }
        let Some(parent) = current.parent() else {
            return Ok(root.authority_path().to_owned());
        };
        if !root.contains(parent) {
            return Ok(root.authority_path().to_owned());
        }
        let parent = parent.to_owned();
        current = parent;
    }
}

fn find_ancestor_manifest(
    root: &WorkspaceRoot,
    requested: &Path,
    boundary: &Path,
) -> Result<Option<PathBuf>, SelectionError> {
    let mut current = requested.to_owned();
    loop {
        if let Some(manifest) = manifest_in(root, &current)? {
            return Ok(Some(manifest));
        }
        if current == boundary || current == root.authority_path() {
            return Ok(None);
        }
        let Some(parent) = current.parent() else {
            return Ok(None);
        };
        if !root.contains(parent) {
            return Ok(None);
        }
        let parent = parent.to_owned();
        current = parent;
    }
}

fn child_manifest_candidates(
    root: &WorkspaceRoot,
    parent: &Path,
) -> Result<Vec<PathBuf>, SelectionError> {
    let mut candidates = Vec::new();
    for child in directory_children(root, parent)? {
        if has_git_marker(root, &child)? {
            continue;
        }
        if let Some(package_root) = manifest_in(root, &child)? {
            candidates.push(package_root);
        }
    }
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

fn directory_children(root: &WorkspaceRoot, parent: &Path) -> Result<Vec<PathBuf>, SelectionError> {
    let mut children = Vec::new();
    for entry in root.list_directory(parent)? {
        if entry.name.to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.kind == DirectoryEntryKind::Directory {
            children.push(parent.join(entry.name));
        }
    }
    children.sort();
    Ok(children)
}

fn manifest_in(root: &WorkspaceRoot, directory: &Path) -> Result<Option<PathBuf>, SelectionError> {
    let manifest = directory.join("Cargo.toml");
    let Some(kind) = root
        .list_directory(directory)?
        .into_iter()
        .find(|entry| entry.name == OsStr::new("Cargo.toml"))
        .map(|entry| entry.kind)
    else {
        return Ok(None);
    };
    match kind {
        DirectoryEntryKind::RegularFile => {
            let bytes = root.read_file(&manifest, MAX_SELECTION_MANIFEST_BYTES)?;
            if std::str::from_utf8(&bytes).is_err() {
                return Err(SelectionError::InvalidManifest(manifest));
            }
            Ok(Some(directory.to_owned()))
        }
        DirectoryEntryKind::Symlink => Err(SelectionError::Root(RootError::Symlink(manifest))),
        _ => Err(SelectionError::Root(RootError::NotRegularFile(manifest))),
    }
}

fn has_git_marker(root: &WorkspaceRoot, directory: &Path) -> Result<bool, SelectionError> {
    Ok(root
        .list_directory(directory)?
        .into_iter()
        .any(|entry| entry.name == OsStr::new(".git")))
}
