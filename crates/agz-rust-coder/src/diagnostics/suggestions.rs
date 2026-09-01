use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Component, Path, PathBuf},
};

use super::model::{
    AdvisoryEdit, CompilerSuggestionEdit, Diagnostic, MAX_PATCH_CONTEXT_BYTES, ResolvedEdit,
    SkippedEdit, SourcePosition, SourceRange, WriteFreePackage, WriteFreePatch,
};

pub trait SnapshotLookup {
    fn get_snapshot(&self, file: &str) -> Option<&str>;
}

impl SnapshotLookup for BTreeMap<String, String> {
    fn get_snapshot(&self, file: &str) -> Option<&str> {
        self.get(file).map(String::as_str)
    }
}

impl SnapshotLookup for HashMap<String, String> {
    fn get_snapshot(&self, file: &str) -> Option<&str> {
        self.get(file).map(String::as_str)
    }
}

/// Package compiler suggestions without writing files.
pub fn machine_applicable_package(
    root: impl AsRef<Path>,
    diagnostics: &[Diagnostic],
) -> WriteFreePackage {
    package_internal(
        root.as_ref(),
        diagnostics,
        None::<&BTreeMap<String, String>>,
    )
}

/// Package suggestions against caller-provided pre-request source snapshots.
pub fn machine_applicable_package_with_snapshots<S: SnapshotLookup>(
    root: impl AsRef<Path>,
    diagnostics: &[Diagnostic],
    snapshots: &S,
) -> WriteFreePackage {
    package_internal(root.as_ref(), diagnostics, Some(snapshots))
}

pub fn advisory_edit(edit: &CompilerSuggestionEdit) -> AdvisoryEdit {
    AdvisoryEdit {
        file: edit.file.clone(),
        range: SourceRange {
            start: SourcePosition {
                line: edit.line_start.saturating_sub(1),
                character: edit.column_start.saturating_sub(1),
            },
            end: SourcePosition {
                line: edit.line_end.saturating_sub(1),
                character: edit.column_end.saturating_sub(1),
            },
        },
        new_text: edit.replacement.clone(),
    }
}

fn package_internal<S: SnapshotLookup>(
    root: &Path,
    diagnostics: &[Diagnostic],
    snapshots: Option<&S>,
) -> WriteFreePackage {
    let canonical_root = match fs::canonicalize(root) {
        Ok(root) if root.is_dir() => root,
        _ => {
            return WriteFreePackage {
                patches: Vec::new(),
                skipped: Vec::new(),
            };
        }
    };
    let mut skipped = Vec::new();
    let mut groups = Vec::new();

    for diagnostic in diagnostics {
        for suggestion in &diagnostic.suggestions {
            if !matches!(
                suggestion.applicability,
                super::model::SuggestionApplicability::MachineApplicable
            ) {
                continue;
            }
            if suggestion.edits.is_empty() {
                continue;
            }
            let group_index = groups.len();
            let mut edits = Vec::new();
            let mut failure = None;
            for edit in &suggestion.edits {
                match resolve_edit(&canonical_root, group_index, edit) {
                    Ok(resolved) => edits.push(resolved),
                    Err(reason) => {
                        failure = Some(reason);
                        break;
                    }
                }
            }
            if let Some(reason) = failure {
                for edit in &suggestion.edits {
                    skipped.push(SkippedEdit {
                        edit: advisory_edit(edit),
                        reason: format!("compiler suggestion rejected atomically: {reason}"),
                    });
                }
                continue;
            }
            if let Some((left, right)) = first_internal_overlap(&edits) {
                let reason = format!(
                    "compiler suggestion contains overlapping edits in {}",
                    left.edit.file
                );
                skipped.push(SkippedEdit {
                    edit: left.edit.clone(),
                    reason: reason.clone(),
                });
                skipped.push(SkippedEdit {
                    edit: right.edit.clone(),
                    reason,
                });
                continue;
            }
            groups.push(edits);
        }
    }

    let mut valid_groups = Vec::new();
    let mut accepted = Vec::new();
    for group in groups {
        if let Some(conflict) = accepted.iter().find_map(|existing: &ResolvedEdit| {
            group.iter().find_map(|candidate| {
                ranges_overlap(existing, candidate).then_some((existing, candidate))
            })
        }) {
            let reason = format!(
                "compiler suggestion overlaps another accepted suggestion in {}",
                conflict.1.edit.file
            );
            for edit in &group {
                skipped.push(SkippedEdit {
                    edit: edit.edit.clone(),
                    reason: reason.clone(),
                });
            }
            continue;
        }
        accepted.extend(group.iter().cloned());
        valid_groups.push(group);
    }

    let valid_groups = reindex_groups(valid_groups);
    let mut snapshots_by_file = BTreeMap::new();
    let mut snapshot_valid_groups = Vec::new();
    for group in valid_groups {
        let mut complete = true;
        for edit in &group {
            if snapshots_by_file.contains_key(&edit.edit.file) {
                continue;
            }
            let expected = snapshots
                .and_then(|snapshots| snapshots.get_snapshot(&edit.edit.file).map(str::to_owned));
            if snapshots.is_some() && expected.is_none() {
                complete = false;
                continue;
            }
            let current = match read_regular_source(&edit.file) {
                Ok(source) => source,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if current != edit.source {
                complete = false;
                continue;
            }
            if let Some(expected) = expected {
                if edit.source != expected {
                    complete = false;
                    continue;
                }
                snapshots_by_file.insert(edit.edit.file.clone(), expected);
            } else {
                snapshots_by_file.insert(edit.edit.file.clone(), edit.source.clone());
            }
        }
        if complete {
            snapshot_valid_groups.push(group);
        } else {
            for edit in &group {
                skipped.push(SkippedEdit {
                    edit: edit.edit.clone(),
                    reason: "compiler suggestion rejected atomically because the source snapshot is missing or changed".to_owned(),
                });
            }
        }
    }

    // Re-read every source after validation so a concurrent replacement cannot be
    // silently converted into a write-free patch.
    let mut stable_groups = Vec::new();
    for group in snapshot_valid_groups {
        let stable = group.iter().all(|edit| {
            read_regular_source(&edit.file)
                .ok()
                .is_some_and(|current| snapshots_by_file.get(&edit.edit.file) == Some(&current))
        });
        if stable {
            stable_groups.push(group);
        } else {
            for edit in &group {
                skipped.push(SkippedEdit {
                    edit: edit.edit.clone(),
                    reason: "compiler suggestion rejected atomically because the source changed after snapshot validation".to_owned(),
                });
            }
        }
    }

    let stable_groups = reindex_groups(stable_groups);
    let mut build = build_patches(&stable_groups, &snapshots_by_file);
    if !build.invalid_groups.is_empty() {
        let invalid = build.invalid_groups;
        let mut retained = Vec::new();
        for (index, group) in stable_groups.into_iter().enumerate() {
            if invalid.contains(&index) {
                for edit in &group {
                    skipped.push(SkippedEdit {
                        edit: edit.edit.clone(),
                        reason: "compiler suggestion rejected atomically because a unique bounded patch context could not be derived".to_owned(),
                    });
                }
            } else {
                retained.push(group);
            }
        }
        let retained = reindex_groups(retained);
        build = build_patches(&retained, &snapshots_by_file);
        if !build.invalid_groups.is_empty() {
            for group_index in build.invalid_groups {
                if let Some(group) = retained.get(group_index) {
                    for edit in group {
                        skipped.push(SkippedEdit {
                            edit: edit.edit.clone(),
                            reason: "compiler suggestion package was incomplete".to_owned(),
                        });
                    }
                }
            }
            return WriteFreePackage {
                patches: Vec::new(),
                skipped,
            };
        }
    }

    WriteFreePackage {
        patches: build.patches,
        skipped,
    }
}

fn resolve_edit(
    root: &Path,
    group: usize,
    edit: &CompilerSuggestionEdit,
) -> Result<ResolvedEdit, String> {
    if !edit.range_complete {
        return Err("an edit did not include a complete source range".to_owned());
    }
    if edit.line_start == 0 || edit.line_end == 0 || edit.column_start == 0 || edit.column_end == 0
    {
        return Err("an edit contains a zero or missing source coordinate".to_owned());
    }
    let (file, relative) = resolve_regular_source(root, &edit.file)?;
    let source = read_regular_source(&file)?;
    let start = offset_for_position(&source, edit.line_start, edit.column_start)
        .ok_or_else(|| "an edit start range is invalid for the UTF-8 source snapshot".to_owned())?;
    let end = offset_for_position(&source, edit.line_end, edit.column_end)
        .ok_or_else(|| "an edit end range is invalid for the UTF-8 source snapshot".to_owned())?;
    if start > end {
        return Err("an edit range is reversed".to_owned());
    }
    if edit.byte_start.is_some() != edit.byte_end.is_some() {
        return Err("the compiler supplied only half of a byte range".to_owned());
    }
    if let (Some(byte_start), Some(byte_end)) = (edit.byte_start, edit.byte_end) {
        if usize::try_from(byte_start).ok() != Some(start)
            || usize::try_from(byte_end).ok() != Some(end)
        {
            return Err("the compiler byte range does not match the source snapshot".to_owned());
        }
    }
    Ok(ResolvedEdit {
        group,
        edit: AdvisoryEdit {
            file: relative,
            range: SourceRange {
                start: SourcePosition {
                    line: edit.line_start - 1,
                    character: edit.column_start - 1,
                },
                end: SourcePosition {
                    line: edit.line_end - 1,
                    character: edit.column_end - 1,
                },
            },
            new_text: edit.replacement.clone(),
        },
        file,
        source,
        start,
        end,
        replacement: edit.replacement.clone(),
    })
}

fn reindex_groups(groups: Vec<Vec<ResolvedEdit>>) -> Vec<Vec<ResolvedEdit>> {
    groups
        .into_iter()
        .enumerate()
        .map(|(group, edits)| {
            edits
                .into_iter()
                .map(|mut edit| {
                    edit.group = group;
                    edit
                })
                .collect()
        })
        .collect()
}

fn resolve_regular_source(root: &Path, value: &str) -> Result<(PathBuf, String), String> {
    let input = Path::new(value);
    if value.is_empty() {
        return Err("the suggestion named an empty path".to_owned());
    }
    if input
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("the suggestion path contains a parent component".to_owned());
    }
    let candidate = if input.is_absolute() {
        input.to_owned()
    } else {
        root.join(input)
    };
    let relative_candidate = candidate
        .strip_prefix(root)
        .map_err(|_| "the compiler suggestion crosses the workspace boundary".to_owned())?;
    if relative_candidate.as_os_str().is_empty() {
        return Err("the suggestion names the workspace directory, not a source file".to_owned());
    }
    let mut current = root.to_owned();
    let components = relative_candidate.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            return Err("the suggestion path is not a normal workspace path".to_owned());
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| "the suggested source file could not be inspected".to_owned())?;
        if metadata.file_type().is_symlink() {
            return Err("the suggestion targets a symlink, not a regular source file".to_owned());
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            return Err("the suggestion path contains a non-directory component".to_owned());
        }
    }
    let canonical = fs::canonicalize(&candidate)
        .map_err(|_| "the suggested source file could not be canonicalized".to_owned())?;
    if !canonical.starts_with(root) {
        return Err("the compiler suggestion crosses the workspace boundary".to_owned());
    }
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|_| "the suggested source file could not be inspected".to_owned())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("the suggestion does not target a regular source file".to_owned());
    }
    if canonical
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("rs")
    {
        return Err("the suggestion does not target a Rust source file".to_owned());
    }
    let relative = canonical
        .strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .map(|path| path.replace('\\', "/"))
        .ok_or_else(|| "the suggested source path is not valid UTF-8".to_owned())?;
    Ok((canonical, relative))
}

fn read_regular_source(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "the source snapshot could not be read".to_owned())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("the source snapshot is not a regular file".to_owned());
    }
    let bytes = fs::read(path).map_err(|_| "the source snapshot could not be read".to_owned())?;
    String::from_utf8(bytes).map_err(|_| "the source snapshot is not valid UTF-8".to_owned())
}

fn offset_for_position(source: &str, line: usize, column: usize) -> Option<usize> {
    if line == 0 || column == 0 {
        return None;
    }
    let starts = line_starts(source);
    let line_start = *starts.get(line - 1)?;
    let line_end = starts
        .get(line)
        .copied()
        .map_or(source.len(), |next_start| {
            next_start.saturating_sub(
                if source.as_bytes().get(next_start.saturating_sub(2)) == Some(&b'\r') {
                    2
                } else {
                    1
                },
            )
        });
    let offset = line_start.checked_add(column - 1)?;
    (offset <= line_end && source.is_char_boundary(offset)).then_some(offset)
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, character) in source.char_indices() {
        if character == '\n' {
            starts.push(index + character.len_utf8());
        }
    }
    starts
}

fn first_internal_overlap(edits: &[ResolvedEdit]) -> Option<(&ResolvedEdit, &ResolvedEdit)> {
    for (index, left) in edits.iter().enumerate() {
        for right in &edits[index + 1..] {
            if ranges_overlap(left, right) {
                return Some((left, right));
            }
        }
    }
    None
}

fn ranges_overlap(left: &ResolvedEdit, right: &ResolvedEdit) -> bool {
    if left.file != right.file {
        return false;
    }
    if left.start == left.end {
        return right.start <= left.start && left.start <= right.end;
    }
    if right.start == right.end {
        return left.start <= right.start && right.start <= left.end;
    }
    left.start < right.end && right.start < left.end
}

struct PatchBuild {
    patches: Vec<WriteFreePatch>,
    invalid_groups: Vec<usize>,
}

fn build_patches(groups: &[Vec<ResolvedEdit>], snapshots: &BTreeMap<String, String>) -> PatchBuild {
    let mut by_file: BTreeMap<String, Vec<&ResolvedEdit>> = BTreeMap::new();
    for group in groups {
        for edit in group {
            by_file
                .entry(edit.edit.file.clone())
                .or_default()
                .push(edit);
        }
    }
    let mut patches = Vec::new();
    let mut invalid_groups = Vec::new();
    for (file, mut edits) in by_file {
        let Some(mut content) = snapshots.get(&file).cloned() else {
            invalid_groups.extend(edits.iter().map(|edit| edit.group));
            continue;
        };
        edits.sort_by(|left, right| {
            right
                .start
                .cmp(&left.start)
                .then_with(|| right.end.cmp(&left.end))
                .then_with(|| right.group.cmp(&left.group))
        });
        for edit in edits {
            let Some((context_start, context_end, old_string)) =
                unique_context(&content, edit.start, edit.end)
            else {
                invalid_groups.push(edit.group);
                continue;
            };
            let prefix_length = edit.start - context_start;
            let suffix_start = edit.end - context_start;
            let new_string = format!(
                "{}{}{}",
                &old_string[..prefix_length],
                edit.replacement,
                &old_string[suffix_start..]
            );
            patches.push(WriteFreePatch {
                file: file.clone(),
                old_string: old_string.clone(),
                new_string: new_string.clone(),
            });
            content.replace_range(context_start..context_end, &new_string);
        }
    }
    invalid_groups.sort_unstable();
    invalid_groups.dedup();
    PatchBuild {
        patches,
        invalid_groups,
    }
}

fn unique_context(source: &str, start: usize, end: usize) -> Option<(usize, usize, String)> {
    if start > end
        || end > source.len()
        || !source.is_char_boundary(start)
        || !source.is_char_boundary(end)
    {
        return None;
    }
    let starts = line_starts(source);
    let start_line = line_at_offset(&starts, start);
    let end_line = line_at_offset(&starts, end);
    for radius in 0..=starts.len() {
        let from_line = start_line.saturating_sub(radius);
        let to_line = (end_line + radius).min(starts.len().saturating_sub(1));
        let context_start = starts[from_line];
        let context_end = if to_line + 1 < starts.len() {
            starts[to_line + 1]
        } else {
            source.len()
        };
        let context = &source[context_start..context_end];
        if context.len() > MAX_PATCH_CONTEXT_BYTES {
            return None;
        }
        if !context.is_empty() && occurrence_count(source, context) == 1 {
            return Some((context_start, context_end, context.to_owned()));
        }
    }
    None
}

fn line_at_offset(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1)
}

fn occurrence_count(source: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    source.match_indices(needle).count()
}
