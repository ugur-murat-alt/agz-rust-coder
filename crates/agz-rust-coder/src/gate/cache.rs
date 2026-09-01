use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    config::{GateCache, GateConfig},
    workspace::WorkspaceSnapshot,
};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CacheError {
    #[error("gate cache path is not absolute: {0}")]
    Relative(PathBuf),
    #[error("gate cache path contains a symlink: {0}")]
    Symlink(PathBuf),
    #[error("gate cache path is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("gate cache path is outside the workspace target boundary: {0}")]
    OutsideWorkspace(PathBuf),
    #[error("gate cache I/O failed for {path}: {message}")]
    Io { path: PathBuf, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Project,
    Isolated,
}

impl CacheMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Isolated => "isolated",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheSelection {
    pub mode: CacheMode,
    pub target_directory: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub owned: bool,
}

pub fn select_gate_cache(
    snapshot: &WorkspaceSnapshot,
    config: &GateConfig,
    mode: crate::gate::types::GateMode,
) -> Result<CacheSelection, CacheError> {
    let mut environment = env::vars_os().collect::<BTreeMap<_, _>>();
    environment.insert(OsString::from("CARGO_TERM_COLOR"), OsString::from("never"));

    let requested = environment
        .get(OsStr::new("CARGO_TARGET_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshot.target_directory.clone());
    let requested = if requested.is_absolute() {
        requested
    } else {
        snapshot.workspace_root.join(requested)
    };
    let requested = lexical_absolute(&requested)?;

    let project_safe = is_safe_project_target(&snapshot.workspace_root, &requested);
    if matches!(config.cache, GateCache::Project) {
        if !project_safe {
            return Err(CacheError::OutsideWorkspace(requested));
        }
        environment.insert(
            OsString::from("CARGO_TARGET_DIR"),
            requested.clone().into_os_string(),
        );
        return Ok(CacheSelection {
            mode: CacheMode::Project,
            target_directory: requested,
            environment,
            owned: false,
        });
    }
    if matches!(config.cache, GateCache::Auto) && project_safe {
        environment.insert(
            OsString::from("CARGO_TARGET_DIR"),
            requested.clone().into_os_string(),
        );
        return Ok(CacheSelection {
            mode: CacheMode::Project,
            target_directory: requested,
            environment,
            owned: false,
        });
    }

    let cache_root = lexical_absolute(&config.cache_dir)?;
    ensure_directory(&cache_root)?;
    let workspace_hash = hash_path(&snapshot.workspace_root);
    let mode_name = match mode {
        crate::gate::types::GateMode::Fast => "fast",
        crate::gate::types::GateMode::Full => "full",
    };
    let target_directory = cache_root.join(workspace_hash).join(mode_name);
    ensure_directory(&target_directory)?;
    environment.insert(
        OsString::from("CARGO_TARGET_DIR"),
        target_directory.clone().into_os_string(),
    );
    Ok(CacheSelection {
        mode: CacheMode::Isolated,
        target_directory,
        environment,
        owned: true,
    })
}

fn is_safe_project_target(workspace_root: &Path, target: &Path) -> bool {
    let Ok(workspace_root) = canonical_existing(workspace_root) else {
        return false;
    };
    let Ok(target) = prospective_canonical(target) else {
        return false;
    };
    path_is_within(&workspace_root, &target)
        && target != workspace_root
        && no_symlink_components(&target)
}

fn ensure_directory(path: &Path) -> Result<(), CacheError> {
    if !path.is_absolute() {
        return Err(CacheError::Relative(path.to_owned()));
    }
    if no_symlink_components(path) {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() {
                return Err(CacheError::Symlink(path.to_owned()));
            }
            if !metadata.is_dir() {
                return Err(CacheError::NotDirectory(path.to_owned()));
            }
            return Ok(());
        }
    } else {
        return Err(CacheError::Symlink(path.to_owned()));
    }
    fs::create_dir_all(path).map_err(|error| CacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if !no_symlink_components(path) {
        return Err(CacheError::Symlink(path.to_owned()));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| CacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(CacheError::NotDirectory(path.to_owned()));
    }
    Ok(())
}

fn canonical_existing(path: &Path) -> Result<PathBuf, CacheError> {
    if !path.is_absolute() {
        return Err(CacheError::Relative(path.to_owned()));
    }
    fs::canonicalize(path).map_err(|error| CacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })
}

fn prospective_canonical(path: &Path) -> Result<PathBuf, CacheError> {
    if !path.is_absolute() {
        return Err(CacheError::Relative(path.to_owned()));
    }
    let mut current = path.to_owned();
    let mut missing = Vec::new();
    while !current.exists() {
        let Some(parent) = current.parent() else {
            return Err(CacheError::Io {
                path: path.to_owned(),
                message: "path has no existing ancestor".to_owned(),
            });
        };
        missing.push(current.file_name().unwrap_or_default().to_owned());
        current = parent.to_owned();
    }
    let mut result = canonical_existing(&current)?;
    for component in missing.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}

fn no_symlink_components(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => return false,
            Component::Normal(name) => {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => return false,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(_) => return false,
                }
            }
        }
    }
    true
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, CacheError> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Err(CacheError::Relative(path.to_owned()))
    }
}

fn path_is_within(root: &Path, candidate: &Path) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_ok_and(|relative| !relative.is_absolute())
}

fn hash_path(path: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(b"agz-rust-coder-gate-target\0");
    hash.update(path.as_os_str().to_string_lossy().as_bytes());
    format!("{:x}", hash.finalize())
}
