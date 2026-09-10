//! Pre-validated, byte-exact patch application against the candidate copy.
//!
//! Every patch and new file is validated in memory before any byte is written.
//! Matching is performed on the exact candidate bytes (UTF-8 validated), so a
//! `\n`-only patch can never silently match a CRLF file.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::capture::relative_path_string;
use super::model::{NewFileInput, PatchInput};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub(crate) struct CandidateLimits {
    pub current_files: u64,
    pub current_bytes: u64,
    pub max_files: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedFile {
    pub relative: PathBuf,
    pub contents: Vec<u8>,
    pub existed: bool,
    pub previous_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedNewFile {
    pub relative: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PatchPlan {
    /// Patched existing files followed by new files, in deterministic order.
    pub files: Vec<PlannedFile>,
    pub patches: Vec<PatchInput>,
    pub new_files: Vec<PlannedNewFile>,
}

/// Validates every patch and new file without touching the candidate tree.
pub(crate) fn plan_patches(
    candidate_root: &Path,
    patches: &[PatchInput],
    new_files: &[NewFileInput],
    limits: CandidateLimits,
) -> Result<PatchPlan, String> {
    let mut planned: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut original: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut order: Vec<PathBuf> = Vec::new();
    let mut ranges: BTreeMap<PathBuf, Vec<(usize, usize)>> = BTreeMap::new();
    let mut normalized_patches = Vec::with_capacity(patches.len());

    for patch in patches {
        let relative = normalize_relative(&patch.file)?;
        if patch.old_string.is_empty() {
            return Err(format!("oldString cannot be empty (file {})", patch.file));
        }
        let current = match planned.get(&relative) {
            Some(contents) => contents.clone(),
            None => {
                let contents = read_candidate_file(candidate_root, &relative)?;
                original.insert(relative.clone(), contents.clone());
                order.push(relative.clone());
                contents
            }
        };
        let text = std::str::from_utf8(&current)
            .map_err(|_| format!("candidate file is not valid UTF-8: {}", patch.file))?
            .to_owned();
        let old = patch.old_string.as_str();
        let Some(start) = text.find(old) else {
            return Err(format!(
                "oldString does not occur in the candidate file: {}",
                patch.file
            ));
        };
        if text.rfind(old) != Some(start) {
            return Err(format!(
                "oldString occurs more than once in the candidate file: {}",
                patch.file
            ));
        }
        let end = start.saturating_add(old.len());
        let file_ranges = ranges.entry(relative.clone()).or_default();
        if file_ranges
            .iter()
            .any(|(existing_start, existing_end)| *existing_start < end && start < *existing_end)
        {
            return Err(format!(
                "overlapping edits in the same candidate file: {}",
                patch.file
            ));
        }
        let mut updated = text;
        updated.replace_range(start..end, &patch.new_string);
        let delta = isize::try_from(patch.new_string.len())
            .unwrap_or(isize::MAX)
            .saturating_sub(isize::try_from(old.len()).unwrap_or(isize::MAX));
        for (existing_start, existing_end) in file_ranges.iter_mut() {
            if *existing_start >= end {
                *existing_start = shift(*existing_start, delta);
                *existing_end = shift(*existing_end, delta);
            }
        }
        file_ranges.push((start, start.saturating_add(patch.new_string.len())));
        planned.insert(relative, updated.into_bytes());
        normalized_patches.push(PatchInput {
            file: patch.file.clone(),
            old_string: patch.old_string.clone(),
            new_string: patch.new_string.clone(),
        });
    }

    let mut new_planned = Vec::with_capacity(new_files.len());
    let mut new_relative = BTreeSet::new();
    for new_file in new_files {
        let relative = normalize_relative(&new_file.file)?;
        if !new_relative.insert(relative.clone()) || planned.contains_key(&relative) {
            return Err(format!(
                "duplicate new file or patch/new-file collision: {}",
                new_file.file
            ));
        }
        if let Ok(metadata) = fs::symlink_metadata(candidate_root.join(&relative)) {
            if metadata.file_type().is_symlink() {
                return Err(format!("new file target is a symlink: {}", new_file.file));
            }
            return Err(format!(
                "new file already exists in the candidate: {}",
                new_file.file
            ));
        }
        reject_symlinked_ancestors(candidate_root, &relative)?;
        new_planned.push(PlannedNewFile {
            relative,
            content: new_file.content.clone(),
        });
    }

    let added_files = new_planned.len() as u64;
    if limits.current_files.saturating_add(added_files) > limits.max_files {
        return Err(format!(
            "candidate file limit {} would be exceeded",
            limits.max_files
        ));
    }
    let mut delta: i128 = 0;
    for (relative, contents) in &planned {
        let before = original.get(relative).map_or(0usize, |bytes| bytes.len());
        delta += contents.len() as i128 - before as i128;
    }
    for file in &new_planned {
        delta += file.content.len() as i128;
    }
    let projected = i128::from(limits.current_bytes) + delta;
    if projected > i128::from(limits.max_bytes) {
        return Err(format!(
            "candidate byte limit {} would be exceeded",
            limits.max_bytes
        ));
    }
    if projected < 0 {
        return Err("candidate byte accounting is inconsistent".to_owned());
    }

    let mut files = Vec::with_capacity(planned.len() + new_planned.len());
    for relative in order {
        if let Some(contents) = planned.remove(&relative) {
            let previous_bytes = original
                .get(&relative)
                .map_or(0, |bytes| bytes.len().try_into().unwrap_or(u64::MAX));
            files.push(PlannedFile {
                relative,
                contents,
                existed: true,
                previous_bytes,
            });
        }
    }
    let mut new_files_planned = Vec::with_capacity(new_planned.len());
    for file in new_planned {
        files.push(PlannedFile {
            relative: file.relative.clone(),
            contents: file.content.as_bytes().to_vec(),
            existed: false,
            previous_bytes: 0,
        });
        new_files_planned.push(file);
    }
    Ok(PatchPlan {
        files,
        patches: normalized_patches,
        new_files: new_files_planned,
    })
}

fn shift(value: usize, delta: isize) -> usize {
    if delta >= 0 {
        value.saturating_add(delta.unsigned_abs())
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

/// Writes each planned file through a same-directory temporary and rename.
pub(crate) fn apply_plan(candidate_root: &Path, plan: &PatchPlan) -> Result<(), String> {
    for file in &plan.files {
        let target = candidate_root.join(&file.relative);
        // Never create a directory through a symlinked ancestor: check the
        // existing ancestry before `create_dir_all` can follow it, and check
        // again before the write can follow it.
        reject_symlinked_ancestors(candidate_root, &file.relative)?;
        let parent = target
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        reject_symlinked_ancestors(candidate_root, &file.relative)?;
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "candidate".to_owned());
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{name}.agz-change.{}.{sequence}.tmp",
            std::process::id()
        ));
        let write_result = (|| -> std::io::Result<()> {
            let mut handle = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            handle.write_all(&file.contents)?;
            handle.sync_all()?;
            drop(handle);
            if let Ok(metadata) = fs::symlink_metadata(&target)
                && metadata.file_type().is_symlink()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "candidate target became a symlink",
                ));
            }
            fs::rename(&temp, &target)
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp);
            return Err(format!(
                "could not apply patch to {}: {error}",
                relative_path_string(&file.relative)
            ));
        }
    }
    Ok(())
}

pub(crate) fn normalize_relative(value: &str) -> Result<PathBuf, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("file cannot be empty".to_owned());
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(format!("file must be relative to the candidate: {value}"));
    }
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                if name == ".git" || name == "target" {
                    return Err(format!("file targets a server-excluded directory: {value}"));
                }
                relative.push(name);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("file escapes the candidate root: {value}"));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(format!(
            "file must name a file inside the candidate: {value}"
        ));
    }
    Ok(relative)
}

fn read_candidate_file(candidate_root: &Path, relative: &Path) -> Result<Vec<u8>, String> {
    reject_symlinked_ancestors(candidate_root, relative)?;
    let path = candidate_root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "candidate file is unavailable {}: {error}",
            relative_path_string(relative)
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "candidate file is a symlink: {}",
            relative_path_string(relative)
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "candidate path is not a regular file: {}",
            relative_path_string(relative)
        ));
    }
    fs::read(&path).map_err(|error| {
        format!(
            "could not read candidate file {}: {error}",
            relative_path_string(relative)
        )
    })
}

fn reject_symlinked_ancestors(candidate_root: &Path, relative: &Path) -> Result<(), String> {
    let mut current = candidate_root.to_owned();
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    for component in parent.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "candidate path contains a symlinked directory: {}",
                        relative_path_string(relative)
                    ));
                }
                if !metadata.is_dir() {
                    return Err(format!(
                        "candidate path component is not a directory: {}",
                        relative_path_string(relative)
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "could not inspect candidate path {}: {error}",
                    relative_path_string(relative)
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agz-change-patch-{label}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("src")).expect("create candidate root");
        path
    }

    fn limits() -> CandidateLimits {
        CandidateLimits {
            current_files: 10,
            current_bytes: 1_000_000,
            max_files: 100,
            max_bytes: 1_000_000,
        }
    }

    fn patch(file: &str, old: &str, new: &str) -> PatchInput {
        PatchInput {
            file: file.to_owned(),
            old_string: old.to_owned(),
            new_string: new.to_owned(),
        }
    }

    #[test]
    fn plan_applies_sequential_non_overlapping_patches() {
        let root = base("sequential");
        fs::write(root.join("src/lib.rs"), b"one two").expect("write file");
        let plan = plan_patches(
            &root,
            &[
                patch("src/lib.rs", "one", "1"),
                patch("src/lib.rs", "two", "2"),
            ],
            &[],
            limits(),
        )
        .expect("plan");
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].contents, b"1 2");
        apply_plan(&root, &plan).expect("apply");
        assert_eq!(fs::read(root.join("src/lib.rs")).expect("read"), b"1 2");
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn plan_rejects_stale_missing_and_overlapping_edits() {
        let root = base("reject");
        fs::write(root.join("file.txt"), b"abcdef").expect("write file");
        assert!(plan_patches(&root, &[patch("file.txt", "zzz", "1")], &[], limits()).is_err());
        assert!(
            plan_patches(
                &root,
                &[
                    patch("file.txt", "abc", "abcd"),
                    patch("file.txt", "bc", "x")
                ],
                &[],
                limits()
            )
            .is_err()
        );
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn plan_requires_crlf_to_match_crlf_bytes() {
        let root = base("crlf");
        fs::write(root.join("file.txt"), b"line one\r\nline two\r\n").expect("write file");
        assert!(
            plan_patches(&root, &[patch("file.txt", "one\nline", "1")], &[], limits()).is_err()
        );
        let plan = plan_patches(
            &root,
            &[patch("file.txt", "one\r\nline", "1")],
            &[],
            limits(),
        )
        .expect("crlf patch");
        assert_eq!(plan.files[0].contents, b"line 1 two\r\n");
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn plan_rejects_path_escape_duplicate_new_files_and_invalid_utf8() {
        let root = base("escape");
        fs::write(root.join("file.txt"), b"abc").expect("write file");
        fs::write(root.join("binary.dat"), [0xff, 0xfe, 0xfd]).expect("write binary");
        for escaped in ["../escape.txt", "/etc/passwd", "src/../../escape"] {
            assert!(
                plan_patches(&root, &[patch(escaped, "a", "b")], &[], limits()).is_err(),
                "{escaped}"
            );
        }
        assert!(plan_patches(&root, &[patch("binary.dat", "a", "b")], &[], limits()).is_err());
        assert!(
            plan_patches(
                &root,
                &[],
                &[
                    NewFileInput {
                        file: "new.txt".to_owned(),
                        content: "a".to_owned(),
                    },
                    NewFileInput {
                        file: "new.txt".to_owned(),
                        content: "b".to_owned(),
                    },
                ],
                limits()
            )
            .is_err()
        );
        assert!(
            plan_patches(
                &root,
                &[],
                &[NewFileInput {
                    file: "file.txt".to_owned(),
                    content: "a".to_owned(),
                }],
                limits()
            )
            .is_err()
        );
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn apply_rejects_tampered_symlink_ancestor_before_creating_directories() {
        use std::os::unix::fs::symlink;

        let root = base("symlink-ancestor");
        let victim = root.parent().expect("candidate parent").join(format!(
            "agz-change-victim-{}",
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&victim).expect("create victim");
        let plan = plan_patches(
            &root,
            &[],
            &[NewFileInput {
                file: "src/nested/new.txt".to_owned(),
                content: "payload".to_owned(),
            }],
            CandidateLimits {
                current_files: 0,
                current_bytes: 0,
                max_files: 100,
                max_bytes: 1_000_000,
            },
        )
        .expect("plan new nested file");
        // Tamper with the candidate after planning: `src` becomes a symlink to
        // an outside directory.
        fs::remove_dir_all(root.join("src")).expect("remove src dir");
        symlink(&victim, root.join("src")).expect("plant symlink ancestor");

        let error = apply_plan(&root, &plan).expect_err("symlinked ancestor must fail");
        assert!(error.contains("symlinked"), "{error}");
        assert!(
            !victim.join("nested").exists(),
            "no directory may be created through a symlinked ancestor"
        );
        fs::remove_dir_all(&root).expect("cleanup root");
        fs::remove_dir_all(&victim).expect("cleanup victim");
    }

    #[test]
    fn plan_enforces_file_and_byte_limits() {
        let root = base("limits");
        fs::write(root.join("file.txt"), b"abc").expect("write file");
        assert!(
            plan_patches(
                &root,
                &[],
                &[NewFileInput {
                    file: "new.txt".to_owned(),
                    content: "x".to_owned(),
                }],
                CandidateLimits {
                    current_files: 1,
                    current_bytes: 3,
                    max_files: 1,
                    max_bytes: 1_000,
                }
            )
            .is_err()
        );
        assert!(
            plan_patches(
                &root,
                &[],
                &[NewFileInput {
                    file: "new.txt".to_owned(),
                    content: "x".repeat(100),
                }],
                CandidateLimits {
                    current_files: 1,
                    current_bytes: 3,
                    max_files: 10,
                    max_bytes: 50,
                }
            )
            .is_err()
        );
        fs::remove_dir_all(&root).expect("cleanup");
    }
}
