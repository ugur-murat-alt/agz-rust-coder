//! Byte-exact bounded capture of a workspace tree into server-owned scratch.
//!
//! The walk never follows symlinks, never creates hard links, and fails closed
//! when any input (symlink, special file, unreadable entry, or configured
//! limit) makes the copy incomplete. File contents are copied byte-for-byte so
//! CRLF is preserved.

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use cap_std::fs::Dir;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::workspace::AuthorizedRoot;

pub(crate) const CAPTURE_MAX_DEPTH: usize = 128;
const SKIPPED_DIRECTORIES: [&str; 2] = [".git", "target"];
/// Upper bound on the reported exclusion list so a hostile tree cannot grow
/// the capture summary without bound.
pub(crate) const MAX_REPORTED_EXCLUSIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManifestEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureManifest {
    pub files: Vec<ManifestEntry>,
    pub total_bytes: u64,
    /// Bounded relative paths deliberately excluded from this capture.
    #[serde(default)]
    pub excluded: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CaptureLimits {
    pub max_files: u64,
    pub max_bytes: u64,
}

#[derive(Debug)]
pub(crate) enum CaptureError {
    /// Capture could not prove a complete byte-exact input set; no change may
    /// be created from it.
    Incomplete(String),
    Cancelled,
}

/// Copies `authority` into `destination` and returns the sorted manifest.
///
/// `excluded` holds relative paths (for example the Cargo target directory)
/// that must not be captured even when they are regular directories.
pub(crate) fn capture_tree(
    authority: &AuthorizedRoot,
    destination: &Path,
    excluded: &BTreeSet<PathBuf>,
    limits: CaptureLimits,
    cancellation: &CancellationToken,
) -> Result<CaptureManifest, CaptureError> {
    if cancellation.is_cancelled() {
        return Err(CaptureError::Cancelled);
    }
    fs::create_dir_all(destination).map_err(|error| {
        CaptureError::Incomplete(format!(
            "could not create candidate directory {}: {error}",
            destination.display()
        ))
    })?;

    let root = authority.dir();
    let destination_relative = destination
        .strip_prefix(authority.path())
        .ok()
        .map(Path::to_owned);
    let mut pending = vec![Frame {
        dir: root.try_clone().map_err(|error| {
            CaptureError::Incomplete(format!("could not open capture root: {error}"))
        })?,
        relative: PathBuf::new(),
        depth: 0,
    }];
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    let mut excluded_report = BTreeSet::new();

    while let Some(frame) = pending.pop() {
        if cancellation.is_cancelled() {
            return Err(CaptureError::Cancelled);
        }
        let entries = frame.dir.entries().map_err(|error| {
            CaptureError::Incomplete(format!(
                "could not list {}: {error}",
                display_relative(&frame.relative)
            ))
        })?;
        for entry in entries {
            if cancellation.is_cancelled() {
                return Err(CaptureError::Cancelled);
            }
            let entry = entry.map_err(|error| {
                CaptureError::Incomplete(format!(
                    "could not read an entry in {}: {error}",
                    display_relative(&frame.relative)
                ))
            })?;
            let name = entry.file_name();
            let relative = frame.relative.join(&name);
            if SKIPPED_DIRECTORIES
                .iter()
                .any(|skipped| OsStr::new(skipped) == name)
                || excluded.contains(&relative)
            {
                if excluded_report.len() < MAX_REPORTED_EXCLUSIONS {
                    excluded_report.insert(display_relative(&relative));
                }
                continue;
            }
            if let Some(destination_relative) = &destination_relative
                && (!relative.as_os_str().is_empty()
                    && (destination_relative == &relative
                        || destination_relative.starts_with(&relative)))
            {
                // Never walk into the destination subtree being written.
                continue;
            }
            let file_type = entry.file_type().map_err(|error| {
                CaptureError::Incomplete(format!(
                    "could not inspect {}: {error}",
                    display_relative(&relative)
                ))
            })?;
            if file_type.is_symlink() {
                return Err(CaptureError::Incomplete(format!(
                    "symlink input is not capture-safe: {}",
                    display_relative(&relative)
                )));
            }
            if file_type.is_dir() {
                if frame.depth >= CAPTURE_MAX_DEPTH {
                    return Err(CaptureError::Incomplete(format!(
                        "capture depth limit reached at {}",
                        display_relative(&relative)
                    )));
                }
                let child = entry.open_dir().map_err(|error| {
                    CaptureError::Incomplete(format!(
                        "could not open {}: {error}",
                        display_relative(&relative)
                    ))
                })?;
                pending.push(Frame {
                    dir: child,
                    relative,
                    depth: frame.depth + 1,
                });
                continue;
            }
            if !file_type.is_file() {
                return Err(CaptureError::Incomplete(format!(
                    "unsupported file type: {}",
                    display_relative(&relative)
                )));
            }
            if files.len() as u64 >= limits.max_files {
                return Err(CaptureError::Incomplete(format!(
                    "capture file limit {} exceeded",
                    limits.max_files
                )));
            }
            let metadata = entry.metadata().map_err(|error| {
                CaptureError::Incomplete(format!(
                    "could not stat {}: {error}",
                    display_relative(&relative)
                ))
            })?;
            let size = metadata.len();
            if total_bytes.saturating_add(size) > limits.max_bytes {
                return Err(CaptureError::Incomplete(format!(
                    "capture byte limit {} exceeded at {}",
                    limits.max_bytes,
                    display_relative(&relative)
                )));
            }
            let bytes = read_bounded(&entry, &relative, limits.max_bytes)?;
            if bytes.len() as u64 != size {
                return Err(CaptureError::Incomplete(format!(
                    "input changed size while being read: {}",
                    display_relative(&relative)
                )));
            }
            let target = destination.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    CaptureError::Incomplete(format!(
                        "could not create {}: {error}",
                        parent.display()
                    ))
                })?;
            }
            fs::write(&target, &bytes).map_err(|error| {
                CaptureError::Incomplete(format!("could not write {}: {error}", target.display()))
            })?;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            files.push(ManifestEntry {
                path: relative_path_string(&relative),
                sha256: format!("{:x}", hasher.finalize()),
                bytes: bytes.len() as u64,
            });
            total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(CaptureManifest {
        files,
        total_bytes,
        excluded: excluded_report.into_iter().collect(),
    })
}

struct Frame {
    dir: Dir,
    relative: PathBuf,
    depth: usize,
}

fn read_bounded(
    entry: &cap_std::fs::DirEntry,
    relative: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, CaptureError> {
    let file = entry.open().map_err(|error| {
        CaptureError::Incomplete(format!(
            "could not open {}: {error}",
            display_relative(relative)
        ))
    })?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CaptureError::Incomplete(format!(
                "could not read {}: {error}",
                display_relative(relative)
            ))
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(CaptureError::Incomplete(format!(
            "input exceeds the capture byte limit: {}",
            display_relative(relative)
        )));
    }
    Ok(bytes)
}

/// Stable digest binding a manifest to its exact sorted content.
pub(crate) fn manifest_hash(manifest: &CaptureManifest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-mcp-change-manifest\0");
    hasher.update(manifest.files.len().to_string().as_bytes());
    hasher.update(b"\n");
    for entry in &manifest.files {
        hasher.update(entry.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.sha256.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.bytes.to_string().as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn relative_path_string(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        return String::new();
    }
    let mut result = String::new();
    for component in path.components() {
        if let Component::Normal(name) = component {
            if !result.is_empty() {
                result.push('/');
            }
            result.push_str(&name.to_string_lossy());
        }
    }
    result
}

fn display_relative(path: &Path) -> String {
    let display = relative_path_string(path);
    if display.is_empty() {
        "<root>".to_owned()
    } else {
        display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{ClientRoots, RootGuard};
    use std::sync::Arc;

    fn scratch(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agz-change-capture-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ))
    }

    fn open_root(path: &Path) -> Arc<AuthorizedRoot> {
        let guard = RootGuard::new([path.to_owned()], std::iter::empty()).expect("root guard");
        let snapshot = guard
            .snapshot(ClientRoots::unsupported())
            .expect("root snapshot");
        snapshot.roots()[0].clone()
    }

    fn capture(base: &Path, limits: CaptureLimits) -> Result<CaptureManifest, CaptureError> {
        let root = open_root(base);
        capture_tree(
            &root,
            &base.join("copy"),
            &BTreeSet::new(),
            limits,
            &CancellationToken::new(),
        )
    }

    #[test]
    fn capture_is_byte_exact_preserves_crlf_and_excludes_target_and_git() {
        let base = scratch("byte-exact");
        fs::create_dir_all(base.join("src")).expect("create src");
        fs::create_dir_all(base.join("target/debug")).expect("create target");
        fs::create_dir_all(base.join(".git")).expect("create git");
        fs::write(base.join("src/lib.rs"), b"pub fn x() {}\r\n").expect("write source");
        fs::write(base.join("target/debug/artifact"), b"binary").expect("write target file");
        fs::write(base.join(".git/config"), b"git").expect("write git file");

        let manifest = capture(
            &base,
            CaptureLimits {
                max_files: 100,
                max_bytes: 1_000_000,
            },
        )
        .expect("capture");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "src/lib.rs");
        assert!(
            manifest.excluded.iter().any(|path| path == ".git"),
            "exclusions must be reported: {:?}",
            manifest.excluded
        );
        assert!(
            manifest.excluded.iter().any(|path| path == "target"),
            "exclusions must be reported: {:?}",
            manifest.excluded
        );
        let copied = fs::read(base.join("copy/src/lib.rs")).expect("read copy");
        assert_eq!(copied, b"pub fn x() {}\r\n");
        assert!(!base.join("copy/target").exists());
        assert!(!base.join("copy/.git").exists());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn capture_fails_closed_on_symlinks() {
        use std::os::unix::fs::symlink;

        let base = scratch("symlink");
        fs::create_dir_all(base.join("src")).expect("create src");
        fs::write(base.join("real.rs"), b"pub fn x() {}\n").expect("write real");
        symlink(base.join("real.rs"), base.join("src/lib.rs")).expect("create symlink");
        let error = capture(
            &base,
            CaptureLimits {
                max_files: 100,
                max_bytes: 1_000_000,
            },
        )
        .expect_err("symlink must fail closed");
        assert!(matches!(error, CaptureError::Incomplete(_)));
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn capture_fails_closed_when_limits_are_exceeded() {
        let base = scratch("limits");
        fs::create_dir_all(&base).expect("create base");
        fs::write(base.join("a.txt"), b"1234567890").expect("write file");
        let error = capture(
            &base,
            CaptureLimits {
                max_files: 1,
                max_bytes: 4,
            },
        )
        .expect_err("byte limit");
        assert!(matches!(error, CaptureError::Incomplete(_)));
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
