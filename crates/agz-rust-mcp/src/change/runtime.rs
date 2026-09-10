//! Server-owned baseline/candidate materialization for runtime comparison.
//!
//! `profile(action=runtime_compare)` never measures the original workspace.
//! Both sides are materialized copies of the change candidate: the candidate
//! side is the recorded revision bytes, and the baseline side is reconstructed
//! by reverse-applying the recorded patch log and re-verifying every file
//! against the server-owned capture manifest. Any hash mismatch, extra file,
//! or ambiguous reverse patch is reported as `INCOMPARABLE` instead of being
//! measured.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{capture::CaptureManifest, model::ChangeRecord, patch::normalize_relative};

/// Typed failure for snapshot materialization. The runtime service maps these
/// to terminal statuses without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// No server-owned record exists for the change id.
    NotFound,
    /// The request or record is malformed.
    Invalid(String),
    /// The requested revision is not the materializable one.
    Stale(String),
    /// Candidate/baseline bytes cannot be bound to the recorded revision.
    Incomparable(String),
    /// The materialization was cancelled.
    Cancelled,
    /// A bounded resource (scratch, manifest, copy) was unavailable.
    Unavailable(String),
}

impl SnapshotError {
    pub fn reason(&self) -> &str {
        match self {
            Self::NotFound => "the change id has no server-owned scratch",
            Self::Invalid(reason) | Self::Stale(reason) | Self::Incomparable(reason) => reason,
            Self::Cancelled => "the snapshot materialization was cancelled",
            Self::Unavailable(reason) => reason,
        }
    }
}

/// Two verified server-owned source snapshots plus the binding metadata needed
/// to keep a runtime comparison bound to exact source bytes.
#[derive(Debug, Clone)]
pub struct RuntimeSnapshotPair {
    pub change_id: String,
    pub baseline_revision: u64,
    pub candidate_revision: u64,
    pub base_identity: String,
    pub patch_hash: String,
    pub manifest_hash: String,
    pub workspace_root: PathBuf,
    pub workspace_epoch: u64,
    pub dependency_roots: Vec<PathBuf>,
    pub baseline_root: PathBuf,
    pub candidate_root: PathBuf,
    /// Combined digest over every baseline source file (path and hash).
    pub baseline_source_digest: String,
    /// Combined digest over every candidate source file (path and hash).
    pub candidate_source_digest: String,
    pub changed_files: Vec<String>,
    /// Artifact directories deliberately left out of both snapshots because the
    /// capture contract excludes them from the source identity.
    pub excluded: Vec<String>,
}

const COPY_SKIPPED: [&str; 2] = [".git", "target"];
const MAX_REPORTED_EXTRA_FILES: usize = 8;
const SOURCE_DIGEST_DOMAIN: &[u8] = b"agz-rust-mcp-runtime-source-v1\0";

/// Copy a bounded source tree without following symlinks, skipping the
/// capture-contract artifact directories. Returns the relative paths that were
/// skipped.
pub(crate) fn copy_tree_bounded(
    source: &Path,
    destination: &Path,
    max_files: u64,
    max_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<Vec<String>, String> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("could not create {}: {error}", destination.display()))?;
    let mut pending = vec![(source.to_owned(), PathBuf::new())];
    let mut skipped = BTreeSet::new();
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    while let Some((directory, relative)) = pending.pop() {
        if cancellation.is_cancelled() {
            return Err("cancelled during snapshot copy".to_owned());
        }
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("could not list {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "could not read an entry in {}: {error}",
                    directory.display()
                )
            })?;
            let name = entry.file_name();
            let child_relative = relative.join(&name);
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                format!("could not inspect {}: {error}", child_relative.display())
            })?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "snapshot input {} is a symlink; runtime comparison is fail-closed",
                    child_relative.display()
                ));
            }
            if metadata.is_dir() {
                if COPY_SKIPPED
                    .iter()
                    .any(|candidate| std::ffi::OsStr::new(candidate) == name)
                {
                    if skipped.len() < MAX_REPORTED_EXTRA_FILES {
                        skipped.insert(relative_display(&child_relative));
                    }
                    continue;
                }
                pending.push((entry.path(), child_relative));
                continue;
            }
            if !metadata.is_file() {
                return Err(format!(
                    "snapshot input {} is not a regular file",
                    child_relative.display()
                ));
            }
            files = files.saturating_add(1);
            if files > max_files {
                return Err(format!("snapshot copy exceeds the {max_files}-file bound"));
            }
            let size = metadata.len();
            bytes = bytes.saturating_add(size);
            if bytes > max_bytes {
                return Err(format!("snapshot copy exceeds the {max_bytes}-byte bound"));
            }
            let target = destination.join(&child_relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
            }
            let mut source_file = fs::File::open(entry.path())
                .map_err(|error| format!("could not open {}: {error}", child_relative.display()))?;
            let mut buffer = Vec::with_capacity(usize::try_from(size).unwrap_or(0).min(1 << 20));
            source_file
                .by_ref()
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut buffer)
                .map_err(|error| format!("could not read {}: {error}", child_relative.display()))?;
            if buffer.len() as u64 != size {
                return Err(format!(
                    "{} changed size while being copied",
                    child_relative.display()
                ));
            }
            fs::write(&target, &buffer)
                .map_err(|error| format!("could not write {}: {error}", target.display()))?;
        }
    }
    Ok(skipped.into_iter().collect())
}

/// Expected candidate files: captured manifest entries overridden by the
/// recorded post-patch hashes and recorded new files.
pub(crate) fn expected_candidate_files(
    record: &ChangeRecord,
    manifest: &CaptureManifest,
) -> Result<BTreeMap<String, (String, u64)>, SnapshotError> {
    let mut expected = manifest_files(manifest);
    for (file, hash) in &record.source_hashes {
        expected.insert(record_relative(file)?, (hash.sha256.clone(), hash.bytes));
    }
    for file in &record.new_files {
        expected.insert(
            record_relative(&file.file)?,
            (file.sha256.clone(), file.bytes),
        );
    }
    Ok(expected)
}

/// Expected baseline files: the capture manifest without any recorded new
/// files. A new file that shadows a captured file cannot be reversed.
pub(crate) fn expected_baseline_files(
    record: &ChangeRecord,
    manifest: &CaptureManifest,
) -> Result<BTreeMap<String, (String, u64)>, SnapshotError> {
    let mut expected = manifest_files(manifest);
    for file in &record.new_files {
        let file = record_relative(&file.file)?;
        if expected.remove(&file).is_some() {
            return Err(SnapshotError::Incomparable(format!(
                "cannot reconstruct the baseline: new file {file} replaced a captured file"
            )));
        }
    }
    Ok(expected)
}

fn manifest_files(manifest: &CaptureManifest) -> BTreeMap<String, (String, u64)> {
    manifest
        .files
        .iter()
        .map(|entry| (entry.path.clone(), (entry.sha256.clone(), entry.bytes)))
        .collect()
}

fn record_relative(file: &str) -> Result<String, SnapshotError> {
    normalize_relative(file)
        .map(|path| relative_display(&path))
        .map_err(|error| {
            SnapshotError::Invalid(format!("recorded path {file} is invalid: {error}"))
        })
}

/// Reverse-apply the recorded patch log so the copy matches the captured
/// baseline. Patches are applied in reverse order; each `newString` must occur
/// exactly once, otherwise the reconstruction is ambiguous and refused.
pub(crate) fn reverse_apply(
    root: &Path,
    record: &ChangeRecord,
    cancellation: &CancellationToken,
) -> Result<(), SnapshotError> {
    for patch in record.patches.iter().rev() {
        if cancellation.is_cancelled() {
            return Err(SnapshotError::Cancelled);
        }
        let relative = normalize_relative(&patch.file)
            .map_err(|error| SnapshotError::Invalid(format!("patch path is invalid: {error}")))?;
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            SnapshotError::Incomparable(format!(
                "baseline file {} is unavailable while reversing a patch: {error}",
                relative.display()
            ))
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(SnapshotError::Incomparable(format!(
                "baseline file {} is not a regular file",
                relative.display()
            )));
        }
        let text = fs::read_to_string(&path).map_err(|error| {
            SnapshotError::Incomparable(format!(
                "baseline file {} could not be read as UTF-8: {error}",
                relative.display()
            ))
        })?;
        let matches = text.matches(&patch.new_string).count();
        if matches != 1 {
            return Err(SnapshotError::Incomparable(format!(
                "baseline reconstruction is ambiguous: the staged bytes of {} match the \
                 recorded newString {matches} times",
                relative.display()
            )));
        }
        let restored = text.replacen(&patch.new_string, &patch.old_string, 1);
        fs::write(&path, restored.as_bytes()).map_err(|error| {
            SnapshotError::Incomparable(format!(
                "baseline file {} could not be written: {error}",
                relative.display()
            ))
        })?;
    }
    for file in &record.new_files {
        if cancellation.is_cancelled() {
            return Err(SnapshotError::Cancelled);
        }
        let relative = normalize_relative(&file.file).map_err(|error| {
            SnapshotError::Invalid(format!("new file path is invalid: {error}"))
        })?;
        let path = root.join(&relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                fs::remove_file(&path).map_err(|error| {
                    SnapshotError::Incomparable(format!(
                        "baseline new file {} could not be removed: {error}",
                        relative.display()
                    ))
                })?;
            }
            Ok(_) => {
                return Err(SnapshotError::Incomparable(format!(
                    "baseline new file {} is not a regular file",
                    relative.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SnapshotError::Incomparable(format!(
                    "baseline new file {} could not be inspected: {error}",
                    relative.display()
                )));
            }
        }
    }
    Ok(())
}

/// Hash every file under `root`, require exact equality with `expected`, refuse
/// any extra file, and return a combined source digest. Missing or extra files
/// make the experiment incomparable.
pub(crate) fn verify_and_digest(
    root: &Path,
    expected: &BTreeMap<String, (String, u64)>,
) -> Result<String, SnapshotError> {
    let mut seen = BTreeSet::new();
    let mut extras = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| {
            SnapshotError::Unavailable(format!("could not list {}: {error}", directory.display()))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                SnapshotError::Unavailable(format!(
                    "could not read an entry in {}: {error}",
                    directory.display()
                ))
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                SnapshotError::Unavailable(format!("could not inspect {}: {error}", path.display()))
            })?;
            if metadata.file_type().is_symlink() {
                return Err(SnapshotError::Incomparable(format!(
                    "snapshot file {} is a symlink",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map(relative_display)
                .map_err(|error| {
                    SnapshotError::Unavailable(format!(
                        "could not relativize {}: {error}",
                        path.display()
                    ))
                })?;
            let Some((expected_hash, expected_bytes)) = expected.get(&relative) else {
                if extras.len() < MAX_REPORTED_EXTRA_FILES {
                    extras.push(relative);
                }
                continue;
            };
            if metadata.len() != *expected_bytes {
                return Err(SnapshotError::Incomparable(format!(
                    "snapshot file {relative} is {} bytes but the recorded revision has {expected_bytes}",
                    metadata.len()
                )));
            }
            let mut file = fs::File::open(&path).map_err(|error| {
                SnapshotError::Unavailable(format!("could not open {relative}: {error}"))
            })?;
            let mut hasher = Sha256::new();
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).map_err(|error| {
                    SnapshotError::Unavailable(format!("could not read {relative}: {error}"))
                })?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            let digest = format!("{:x}", hasher.finalize());
            if digest != *expected_hash {
                return Err(SnapshotError::Incomparable(format!(
                    "snapshot file {relative} does not match the recorded revision hash"
                )));
            }
            seen.insert(relative);
        }
    }
    if !extras.is_empty() {
        return Err(SnapshotError::Incomparable(format!(
            "snapshot contains files that are not part of the captured revision: {}",
            extras.join(", ")
        )));
    }
    let missing = expected
        .keys()
        .filter(|path| !seen.contains(*path))
        .take(MAX_REPORTED_EXTRA_FILES)
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(SnapshotError::Incomparable(format!(
            "snapshot is missing recorded revision files: {}",
            missing.join(", ")
        )));
    }
    let mut hasher = Sha256::new();
    hasher.update(SOURCE_DIGEST_DOMAIN);
    for (path, (digest, bytes)) in expected {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update([0]);
        hasher.update(bytes.to_string().as_bytes());
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn relative_display(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
