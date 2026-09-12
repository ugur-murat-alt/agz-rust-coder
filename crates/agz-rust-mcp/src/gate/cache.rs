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
    process::win32_spelling,
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
        return project_cache(&snapshot.workspace_root, &requested, environment);
    }
    if matches!(config.cache, GateCache::Auto) && project_safe {
        return project_cache(&snapshot.workspace_root, &requested, environment);
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
    // The isolated directory may live under a canonical (verbatim) path; Cargo
    // must receive an identity-preserving ordinary spelling so the linker can
    // open its build artifacts on Windows. `target_directory` stays canonical
    // for input-identity comparisons.
    environment.insert(
        OsString::from("CARGO_TARGET_DIR"),
        primary_spelling(&target_directory).into_os_string(),
    );
    Ok(CacheSelection {
        mode: CacheMode::Isolated,
        target_directory,
        environment,
        owned: true,
    })
}

fn project_cache(
    workspace_root: &Path,
    requested: &Path,
    mut environment: BTreeMap<OsString, OsString>,
) -> Result<CacheSelection, CacheError> {
    // Win32 aliases can only be checked for existing paths. Materialize the
    // authorized target before choosing Cargo's spelling so a cold build and
    // its final freshness check use the same environment identity.
    ensure_directory(requested)?;
    let target_directory = canonical_existing(requested)?;
    let workspace_root = canonical_existing(workspace_root)?;
    if !path_is_within(&workspace_root, &target_directory) || target_directory == workspace_root {
        return Err(CacheError::OutsideWorkspace(target_directory));
    }
    environment.insert(
        OsString::from("CARGO_TARGET_DIR"),
        primary_spelling(&target_directory).into_os_string(),
    );
    Ok(CacheSelection {
        mode: CacheMode::Project,
        target_directory,
        environment,
        owned: false,
    })
}

/// Spelling handed to Cargo: an identity-preserving ordinary Win32 form where
/// one exists, otherwise the canonical path.
fn primary_spelling(path: &Path) -> PathBuf {
    win32_spelling(path).unwrap_or_else(|| path.to_owned())
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

pub(crate) fn ensure_directory(path: &Path) -> Result<(), CacheError> {
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

/// Stable per-workspace cache subdirectory name.
///
/// The digest is deliberately truncated to 64 bits: it only has to separate
/// workspace roots that share one configured cache directory, and Windows
/// build tools (the MSVC linker in particular) still fail once an assembled
/// artifact path exceeds the legacy `MAX_PATH` limit. A 64-hex-character name
/// pushed isolated test binaries past that limit on CI runners, so the shorter
/// name keeps the deepest Cargo artifact comfortably bounded.
fn hash_path(path: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(b"agz-rust-mcp-gate-target\0");
    hash.update(path.as_os_str().to_string_lossy().as_bytes());
    let digest = hash.finalize();
    let mut name = String::with_capacity(16);
    for byte in &digest[..8] {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_cache_is_ready_before_cargo_and_keeps_its_spelling() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("agz-project-cache-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let requested = root.join("build output");
        let result = || {
            let first = project_cache(&root, &requested, BTreeMap::new()).unwrap();
            assert!(
                first.target_directory.is_dir(),
                "materialize the target before choosing the Win32 Cargo spelling"
            );
            let cargo_path = PathBuf::from(&first.environment[OsStr::new("CARGO_TARGET_DIR")]);
            assert_eq!(
                fs::canonicalize(cargo_path).unwrap(),
                first.target_directory
            );
            fs::write(first.target_directory.join("build-artifact"), b"output").unwrap();
            let second = project_cache(&root, &requested, BTreeMap::new()).unwrap();
            assert_eq!(first.target_directory, second.target_directory);
            assert_eq!(first.environment, second.environment);
            #[cfg(windows)]
            assert!(
                !first.environment[OsStr::new("CARGO_TARGET_DIR")]
                    .to_string_lossy()
                    .starts_with(r"\\?\")
            );
        };
        let outcome = std::panic::catch_unwind(result);
        fs::remove_dir_all(root).unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}
