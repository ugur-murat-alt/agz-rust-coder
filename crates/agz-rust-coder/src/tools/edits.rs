//! Source-write-free rust-analyzer rename and refactor edit planning.

use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lsp::{LspError, Position, Range, RustAnalyzerManager, value_range};

use super::symbol::{
    ToolError, bounded_chars, current_document_version, excerpt_from_content, file_path_from_uri,
    read_workspace_file, request_until, snapshot_rust_files, utf16_len, with_symbol_position,
};

const ADVISORY: &str =
    "Untrusted rust-analyzer edit suggestions follow; advisory only. No files were changed.";
const ACTION_LIMIT: usize = 30;
const MAX_NORMALIZED_EDITS: usize = 200;
const MAX_WORKSPACE_DOCUMENTS: usize = 256;
const MAX_WORKSPACE_EDITS_PER_DOCUMENT: usize = 256;
const MAX_UNSUPPORTED_OPERATIONS: usize = 128;
const MAX_EDIT_TEXT: usize = 8_000;
const MAX_OUTPUT_CHARS: usize = 120_000;
const MAX_CONTEXT_CHARS: usize = 8_000;
const MAX_CONTEXT_RADIUS: usize = 64;
const MAX_FULL_FILES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisoryEdit {
    pub file: PathBuf,
    pub range: Range,
    pub new_text: String,
    /// `None` means the version field was absent; `Some(None)` is LSP null.
    pub version: Option<Option<i64>>,
    pub annotation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFreePatch {
    pub file: PathBuf,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedEdit {
    pub edit: AdvisoryEdit,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WriteFreePackage {
    pub patches: Vec<WriteFreePatch>,
    pub skipped: Vec<SkippedEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NormalizedWorkspaceEdit {
    pub edits: Vec<AdvisoryEdit>,
    pub omitted: usize,
    pub unsupported_operations: Vec<String>,
}

/// Structured, write-free edit result returned to the MCP adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SemanticEditResult {
    pub reason: String,
    pub patches: Vec<WriteFreePatch>,
    pub skipped: Vec<String>,
    pub unsupported: Vec<String>,
}

impl SemanticEditResult {
    fn message(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            ..Self::default()
        }
    }

    fn merge(&mut self, other: Self) {
        self.patches.extend(
            other
                .patches
                .into_iter()
                .take(MAX_NORMALIZED_EDITS.saturating_sub(self.patches.len())),
        );
        self.skipped.extend(
            other
                .skipped
                .into_iter()
                .take(MAX_NORMALIZED_EDITS.saturating_sub(self.skipped.len())),
        );
        self.unsupported.extend(
            other
                .unsupported
                .into_iter()
                .take(MAX_UNSUPPORTED_OPERATIONS.saturating_sub(self.unsupported.len())),
        );
    }
}

pub fn normalize_workspace_edit(
    root: &Path,
    raw: &Value,
    max_edits: usize,
    document_versions: Option<&HashMap<PathBuf, i64>>,
) -> NormalizedWorkspaceEdit {
    let max_edits = max_edits.min(MAX_NORMALIZED_EDITS);
    let mut edits = Vec::new();
    let mut unsupported_operations = Vec::new();
    let mut discovered = 0usize;
    let mut remaining_documents = MAX_WORKSPACE_DOCUMENTS;

    let Some(workspace_edit) = raw.as_object() else {
        return NormalizedWorkspaceEdit::default();
    };
    let changes = workspace_edit.get("changes").and_then(Value::as_object);
    if let Some(changes) = changes {
        let allowed_documents = changes.len().min(remaining_documents);
        for (uri, raw_edits) in changes.iter().take(allowed_documents) {
            append_workspace_edits(
                root,
                Some(uri),
                Some(raw_edits),
                None,
                max_edits,
                document_versions,
                &mut edits,
                &mut unsupported_operations,
                &mut discovered,
            );
        }
        remaining_documents = remaining_documents.saturating_sub(allowed_documents);
        if changes.len() > allowed_documents {
            push_unsupported(
                &mut unsupported_operations,
                format!(
                    "{} additional changed documents omitted by limit",
                    changes.len() - allowed_documents
                ),
            );
        }
    }
    if let Some(document_changes) = workspace_edit
        .get("documentChanges")
        .and_then(Value::as_array)
    {
        if changes.is_some() {
            push_unsupported(
                &mut unsupported_operations,
                "ambiguous: both changes and documentChanges were returned".to_owned(),
            );
        }
        let allowed_documents = document_changes.len().min(remaining_documents);
        for change in document_changes.iter().take(allowed_documents) {
            let Some(change) = change.as_object() else {
                push_unsupported(
                    &mut unsupported_operations,
                    "documentChanges entry was not an object".to_owned(),
                );
                continue;
            };
            if let Some(document) = change.get("textDocument").and_then(Value::as_object) {
                let version = match document.get("version") {
                    None => None,
                    Some(value) if value.is_null() => Some(None),
                    Some(value) => value.as_i64().map(Some),
                };
                if document.contains_key("version") && version.is_none() {
                    push_unsupported(
                        &mut unsupported_operations,
                        "documentChanges entry has an invalid version".to_owned(),
                    );
                    continue;
                }
                append_workspace_edits(
                    root,
                    document.get("uri").and_then(Value::as_str),
                    change.get("edits"),
                    version,
                    max_edits,
                    document_versions,
                    &mut edits,
                    &mut unsupported_operations,
                    &mut discovered,
                );
            } else if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                push_unsupported(&mut unsupported_operations, kind.to_owned());
            } else {
                push_unsupported(
                    &mut unsupported_operations,
                    "documentChanges entry had no supported operation".to_owned(),
                );
            }
        }
        if document_changes.len() > allowed_documents {
            push_unsupported(
                &mut unsupported_operations,
                format!(
                    "{} additional documentChanges entries omitted by limit",
                    document_changes.len() - allowed_documents
                ),
            );
        }
    }

    let mut unique = BTreeSet::new();
    edits.retain(|edit| {
        unique.insert((
            edit.file.clone(),
            edit.range.start.line,
            edit.range.start.character,
            edit.range.end.line,
            edit.range.end.character,
            edit.new_text.clone(),
            edit.version,
            edit.annotation_id.clone(),
        ))
    });
    edits.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.range.start.line.cmp(&right.range.start.line))
            .then(left.range.start.character.cmp(&right.range.start.character))
            .then(left.range.end.line.cmp(&right.range.end.line))
            .then(left.range.end.character.cmp(&right.range.end.character))
    });
    NormalizedWorkspaceEdit {
        edits,
        omitted: discovered.saturating_sub(unique.len()),
        unsupported_operations,
    }
}

#[allow(clippy::too_many_arguments)]
fn append_workspace_edits(
    root: &Path,
    uri: Option<&str>,
    raw_edits: Option<&Value>,
    version: Option<Option<i64>>,
    max_edits: usize,
    document_versions: Option<&HashMap<PathBuf, i64>>,
    edits: &mut Vec<AdvisoryEdit>,
    unsupported_operations: &mut Vec<String>,
    discovered: &mut usize,
) {
    let Some(uri) = uri else {
        push_unsupported(
            unsupported_operations,
            "workspace edit is missing a document URI".to_owned(),
        );
        return;
    };
    let Some(raw_edits) = raw_edits.and_then(Value::as_array) else {
        push_unsupported(
            unsupported_operations,
            format!("workspace edits for {uri} were not an array"),
        );
        return;
    };
    let Some(file) =
        file_path_from_uri(uri).and_then(|path| super::symbol::resolve_asset_path(root, path))
    else {
        push_unsupported(
            unsupported_operations,
            format!("workspace edit outside the workspace: {uri}"),
        );
        return;
    };
    if let Some(Some(version)) = version
        && document_versions
            .and_then(|versions| versions.get(&file))
            .copied()
            != Some(version)
    {
        push_unsupported(
            unsupported_operations,
            format!(
                "versioned edits rejected for {}: expected current document version",
                file.display()
            ),
        );
        *discovered = discovered.saturating_add(raw_edits.len());
        return;
    }
    for raw_edit in raw_edits.iter().take(MAX_WORKSPACE_EDITS_PER_DOCUMENT) {
        let Some(object) = raw_edit.as_object() else {
            continue;
        };
        let Some(range) = object.get("range").and_then(value_range) else {
            continue;
        };
        let Some(new_text) = object.get("newText").and_then(Value::as_str) else {
            continue;
        };
        *discovered = discovered.saturating_add(1);
        if new_text.chars().count() > MAX_EDIT_TEXT {
            push_unsupported(
                unsupported_operations,
                format!("edit omitted: newText exceeds {MAX_EDIT_TEXT} characters"),
            );
            continue;
        }
        let edit = AdvisoryEdit {
            file: file.clone(),
            range,
            new_text: new_text.to_owned(),
            version,
            annotation_id: object
                .get("annotationId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        if edits.contains(&edit) {
            continue;
        }
        if edits.len() >= max_edits {
            continue;
        }
        edits.push(edit);
    }
    if raw_edits.len() > MAX_WORKSPACE_EDITS_PER_DOCUMENT {
        *discovered = discovered.saturating_add(
            raw_edits
                .len()
                .saturating_sub(MAX_WORKSPACE_EDITS_PER_DOCUMENT),
        );
        push_unsupported(
            unsupported_operations,
            format!(
                "{} additional edits for {} omitted by limit",
                raw_edits.len() - MAX_WORKSPACE_EDITS_PER_DOCUMENT,
                file.display()
            ),
        );
    }
}

fn push_unsupported(operations: &mut Vec<String>, operation: String) {
    match (operations.len() + 1).cmp(&MAX_UNSUPPORTED_OPERATIONS) {
        Ordering::Less => operations.push(operation),
        Ordering::Equal => operations
            .push("additional unsupported workspace operations omitted by limit".to_owned()),
        Ordering::Greater => {}
    }
}

pub fn build_write_free_package(
    root: &Path,
    edits: &[AdvisoryEdit],
    snapshots: Option<&HashMap<PathBuf, String>>,
) -> WriteFreePackage {
    let mut package = WriteFreePackage::default();
    let mut by_file: HashMap<&Path, Vec<&AdvisoryEdit>> = HashMap::new();
    for edit in edits.iter().take(MAX_NORMALIZED_EDITS) {
        by_file.entry(&edit.file).or_default().push(edit);
    }
    let mut files = by_file.keys().copied().collect::<Vec<_>>();
    files.sort();
    for file in files {
        let file = PathBuf::from(file);
        let current = read_workspace_file(root, &root.join(&file));
        let Some(current) = current else {
            if let Some(file_edits) = by_file.get(file.as_path()) {
                for edit in file_edits {
                    package.skipped.push(SkippedEdit {
                        edit: (*edit).clone(),
                        reason: "file is outside the workspace or unreadable".to_owned(),
                    });
                }
            }
            continue;
        };
        let snapshot = snapshots.and_then(|snapshots| snapshots.get(&file));
        if snapshots.is_some() && snapshot.is_none() {
            if let Some(file_edits) = by_file.get(file.as_path()) {
                for edit in file_edits {
                    package.skipped.push(SkippedEdit {
                        edit: (*edit).clone(),
                        reason: "no pre-request snapshot was captured for this file".to_owned(),
                    });
                }
            }
            continue;
        }
        if snapshot.is_some_and(|snapshot| snapshot != &current) {
            if let Some(file_edits) = by_file.get(file.as_path()) {
                for edit in file_edits {
                    package.skipped.push(SkippedEdit {
                        edit: (*edit).clone(),
                        reason: "file changed after the language-server request started".to_owned(),
                    });
                }
            }
            continue;
        }

        let mut content = snapshot.cloned().unwrap_or(current);
        let mut ordered = by_file.get(file.as_path()).cloned().unwrap_or_default();
        ordered.sort_by(|left, right| {
            right
                .range
                .start
                .line
                .cmp(&left.range.start.line)
                .then(right.range.start.character.cmp(&left.range.start.character))
        });
        let mut previous_start = usize::MAX;
        for edit in ordered {
            if edit.new_text.chars().count() > MAX_EDIT_TEXT {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: format!("newText exceeds {MAX_EDIT_TEXT} characters"),
                });
                continue;
            }
            let Some(start) = position_offset(&content, &edit.range.start) else {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: "invalid edit start position".to_owned(),
                });
                continue;
            };
            let Some(end) = position_offset(&content, &edit.range.end) else {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: "invalid edit end position".to_owned(),
                });
                continue;
            };
            if start > end {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: "invalid edit range".to_owned(),
                });
                continue;
            }
            if end > previous_start || (start == end && start == previous_start) {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: "overlapping edit range".to_owned(),
                });
                continue;
            }
            let Some(unique) = unique_context(&content, start, end) else {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: "could not derive a unique old_string".to_owned(),
                });
                continue;
            };
            let prefix_length = start.saturating_sub(unique.start);
            let suffix_start = end.saturating_sub(unique.start);
            let Some(prefix) = unique.text.get(..prefix_length) else {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: "edit range is not on a UTF-8 boundary".to_owned(),
                });
                continue;
            };
            let Some(suffix) = unique.text.get(suffix_start..) else {
                package.skipped.push(SkippedEdit {
                    edit: edit.clone(),
                    reason: "edit range is not on a UTF-8 boundary".to_owned(),
                });
                continue;
            };
            let new_string = format!("{prefix}{}{suffix}", edit.new_text);
            package.patches.push(WriteFreePatch {
                file: file.clone(),
                old_string: unique.text.clone(),
                new_string: new_string.clone(),
            });
            content = format!(
                "{}{}{}",
                &content[..unique.start],
                new_string,
                &content[unique.end..]
            );
            previous_start = start;
        }
    }
    package
}

pub async fn semantic_rename(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
    new_name: &str,
    include_contents: bool,
    max_edits: usize,
    timeout: Duration,
) -> Result<SemanticEditResult, ToolError> {
    if new_name.trim().is_empty() {
        return Err(ToolError::InvalidInput(
            "newName cannot be empty".to_owned(),
        ));
    }
    let root = root.to_owned();
    let relative_path = relative_path.to_owned();
    let symbol = symbol.to_owned();
    let new_name = new_name.to_owned();
    let operation_root = root.clone();
    let operation_path = relative_path.clone();
    let operation_symbol = symbol.clone();
    with_symbol_position(
        manager,
        &root,
        &relative_path,
        &symbol,
        line,
        timeout,
        move |client, position, uri, text| {
            let root = operation_root.clone();
            let relative_path = operation_path.clone();
            let symbol = operation_symbol.clone();
            let new_name = new_name.clone();
            Box::pin(async move {
                let snapshots = snapshot_rust_files(&root, &relative_path, &text)
                    .map_err(tool_error_as_lsp)?;
                let params = serde_json::json!({
                    "textDocument": {"uri": uri},
                    "position": position_value(&position)
                });
                let prepared = request_until(
                    client.as_ref(),
                    "textDocument/prepareRename",
                    params.clone(),
                    timeout,
                    |value| prepare_range(value).is_some(),
                )
                .await?;
                if prepare_range(&prepared).is_none() {
                    return Ok(SemanticEditResult::message(format!(
                        "{ADVISORY}\nRename is not valid for '{symbol}'."
                    )));
                }
                let current_version = current_document_version(client.as_ref(), &uri).await;
                let workspace_edit = request_until(
                    client.as_ref(),
                    "textDocument/rename",
                    serde_json::json!({"textDocument": {"uri": params["textDocument"]["uri"].clone()}, "position": params["position"].clone(), "newName": new_name}),
                    timeout,
                    Value::is_object,
                )
                .await?;
                let mut versions = HashMap::new();
                let current = super::symbol::resolve_asset_path(&root, &relative_path)
                    .ok_or_else(|| LspError::InvalidInput("current file escaped the workspace".to_owned()))?;
                if let Some(version) = current_version {
                    versions.insert(current, version);
                }
                let normalized = normalize_workspace_edit(
                    &root,
                    &workspace_edit,
                    max_edits,
                    Some(&versions),
                );
                if normalized.edits.is_empty() {
                    if !normalized.unsupported_operations.is_empty() {
                        return Ok(format_edit_result(
                            &root,
                            &format!("rename '{symbol}' -> '{new_name}'"),
                            &normalized,
                            include_contents,
                            true,
                            Some(&snapshots),
                        ));
                    }
                    return Ok(SemanticEditResult::message(format!(
                        "{ADVISORY}\nRename produced no applicable workspace edits."
                    )));
                }
                Ok(format_edit_result(
                    &root,
                    &format!("rename '{symbol}' -> '{new_name}'"),
                    &normalized,
                    include_contents,
                    true,
                    Some(&snapshots),
                ))
            })
        },
    )
    .await
}

pub async fn semantic_refactor(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
    only: Option<&[String]>,
    include_contents: bool,
    max_edits: usize,
    timeout: Duration,
) -> Result<SemanticEditResult, ToolError> {
    let root = root.to_owned();
    let relative_path = relative_path.to_owned();
    let symbol = symbol.to_owned();
    let only = only.map(|only| only.iter().take(10).cloned().collect::<Vec<_>>());
    let operation_root = root.clone();
    let operation_path = relative_path.clone();
    let operation_symbol = symbol.clone();
    with_symbol_position(
        manager,
        &root,
        &relative_path,
        &symbol,
        line,
        timeout,
        move |client, position, uri, text| {
            let root = operation_root.clone();
            let relative_path = operation_path.clone();
            let symbol = operation_symbol.clone();
            let only = only.clone();
            Box::pin(async move {
                let snapshots = snapshot_rust_files(&root, &relative_path, &text)
                    .map_err(tool_error_as_lsp)?;
                let current_version = current_document_version(client.as_ref(), &uri).await;
                let current = super::symbol::resolve_asset_path(&root, &relative_path)
                    .ok_or_else(|| LspError::InvalidInput("current file escaped the workspace".to_owned()))?;
                let mut document_versions = HashMap::new();
                if let Some(version) = current_version {
                    document_versions.insert(current, version);
                }
                let end_character = position.character.saturating_add(utf16_len(&symbol).max(1));
                let raw = request_until(
                    client.as_ref(),
                    "textDocument/codeAction",
                    serde_json::json!({
                        "textDocument": {"uri": uri},
                        "range": {"start": position_value(&position), "end": {"line": position.line, "character": end_character}},
                        "context": {"diagnostics": [], "only": only}
                    }),
                    timeout,
                    Value::is_array,
                )
                .await?;
                let actions = raw.as_array().map_or(&[][..], |actions| &actions[..actions.len().min(ACTION_LIMIT)]);
                if actions.is_empty() {
                    return Ok(SemanticEditResult::message(format!(
                        "{ADVISORY}\nNo refactor actions available for '{symbol}'."
                    )));
                }
                let mut lines = vec![ADVISORY.to_owned()];
                let mut result = SemanticEditResult::default();
                for (index, action) in actions.iter().enumerate() {
                    let Some(action) = action.as_object() else {
                        continue;
                    };
                    let title = action
                        .get("title")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("Action {}", index.saturating_add(1)), str::to_owned);
                    let kind = action
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("command");
                    if let Some(only) = &only {
                        if !only.is_empty() && !only.iter().any(|prefix| kind == prefix || kind.starts_with(&format!("{prefix}."))) {
                            continue;
                        }
                    }
                    lines.push(format!("\n[{}] {title} ({kind})", index.saturating_add(1)));
                    if let Some(diagnostics) = action.get("diagnostics").and_then(Value::as_array) {
                        if !diagnostics.is_empty() {
                            lines.push(format!("diagnostics: {}", diagnostics.len()));
                        }
                    }
                    if let Some(disabled) = action.get("disabled") {
                        lines.push(format!(
                            "disabled: {}",
                            disabled
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("server disabled this action")
                        ));
                    } else if let Some(edit) = action.get("edit") {
                        let normalized = normalize_workspace_edit(
                            &root,
                            edit,
                            max_edits,
                            Some(&document_versions),
                        );
                        let edit_result = format_edit_result(
                            &root,
                            &title,
                            &normalized,
                            include_contents,
                            false,
                            Some(&snapshots),
                        );
                        lines.push(edit_result.reason.clone());
                        result.merge(edit_result);
                        if let Some(command) = action.get("command") {
                            let command = command_name(command);
                            lines.push(format!("follow-up command not executed: {command}"));
                            result
                                .unsupported
                                .push(format!("command not executed: {command}"));
                        }
                    } else if let Some(command) = action.get("command") {
                        let command = command_name(command);
                        lines.push(format!("command suggestion only (not executed): {command}"));
                        result
                            .unsupported
                            .push(format!("command not executed: {command}"));
                    } else {
                        lines.push("suggestion only; no edit or executable command returned".to_owned());
                    }
                }
                result.reason = bounded_chars(&lines.join("\n"), MAX_OUTPUT_CHARS);
                Ok(result)
            })
        },
    )
    .await
}

fn format_edit_result(
    root: &Path,
    label: &str,
    normalized: &NormalizedWorkspaceEdit,
    include_contents: bool,
    include_header: bool,
    snapshots: Option<&HashMap<PathBuf, String>>,
) -> SemanticEditResult {
    let mut lines = if include_header {
        vec![ADVISORY.to_owned(), label.to_owned()]
    } else {
        vec![label.to_owned()]
    };
    for edit in &normalized.edits {
        lines.push(format!(
            "{}:{}:{}-{}:{}:{}",
            edit.file.display(),
            edit.range.start.line.saturating_add(1),
            edit.range.start.character.saturating_add(1),
            edit.range.end.line.saturating_add(1),
            edit.range.end.character.saturating_add(1),
            ""
        ));
        lines.push(format!("newText: {:?}", edit.new_text));
        if let Some(version) = edit.version {
            lines.push(format!(
                "documentVersion: {}",
                version.map_or_else(|| "disk".to_owned(), |version| version.to_string())
            ));
        }
        if let Some(annotation) = &edit.annotation_id {
            lines.push(format!("annotationId: {annotation}"));
        }
    }
    if normalized.omitted > 0 {
        lines.push(format!(
            "... {} edits omitted by limit or deduplication",
            normalized.omitted
        ));
    }
    if !normalized.unsupported_operations.is_empty() {
        lines.push(format!(
            "resource operations not applied: {}",
            normalized.unsupported_operations.join(", ")
        ));
    }
    let package = build_write_free_package(root, &normalized.edits, snapshots);
    lines.push(format!(
        "write-free package: {} patch(es)",
        package.patches.len()
    ));
    if !package.skipped.is_empty() {
        lines.push(format!("unverified patches: {}", package.skipped.len()));
    }
    if include_contents {
        let files = normalized
            .edits
            .iter()
            .map(|edit| edit.file.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(MAX_FULL_FILES)
            .collect::<Vec<_>>();
        if !files.is_empty() {
            lines.push(format!(
                "affected workspace files: {}",
                files
                    .iter()
                    .map(|file| file.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for file in files {
            let content = snapshots
                .and_then(|snapshots| snapshots.get(&file).cloned())
                .or_else(|| read_workspace_file(root, &root.join(&file)));
            if let Some(content) = content {
                let line = normalized
                    .edits
                    .iter()
                    .find(|edit| edit.file == file)
                    .map_or(0, |edit| edit.range.start.line);
                lines.push(format!(
                    "context {}:\n{}",
                    file.display(),
                    excerpt_from_content(&content, line as usize, 2_000)
                ));
            }
        }
    }
    SemanticEditResult {
        reason: bounded_chars(&lines.join("\n"), MAX_OUTPUT_CHARS),
        patches: package.patches,
        skipped: package
            .skipped
            .into_iter()
            .map(|skipped| {
                format!(
                    "{}:{}:{}: {}",
                    skipped.edit.file.display(),
                    skipped.edit.range.start.line.saturating_add(1),
                    skipped.edit.range.start.character.saturating_add(1),
                    skipped.reason
                )
            })
            .collect(),
        unsupported: normalized.unsupported_operations.clone(),
    }
}

fn position_offset(content: &str, position: &Position) -> Option<usize> {
    let starts = line_starts(content);
    let line_start = *starts.get(position.line as usize)?;
    let line_end = starts
        .get(position.line as usize + 1)
        .copied()
        .unwrap_or(content.len());
    let line = content
        .get(line_start..line_end)?
        .strip_suffix('\n')
        .unwrap_or_else(|| content.get(line_start..line_end).unwrap_or_default());
    let line = line.strip_suffix('\r').unwrap_or(line);
    let mut utf16 = 0u32;
    for (offset, character) in line.char_indices() {
        if utf16 == position.character {
            return Some(line_start + offset);
        }
        utf16 = utf16.saturating_add(character.len_utf16() as u32);
        if utf16 > position.character {
            return None;
        }
    }
    (utf16 == position.character).then_some(line_start + line.len())
}

fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (offset, character) in content.char_indices() {
        if character == '\n' {
            starts.push(offset.saturating_add(1));
        }
    }
    starts
}

fn unique_context(content: &str, start: usize, end: usize) -> Option<Context> {
    let starts = line_starts(content);
    let start_line = line_at_offset(&starts, start);
    let end_line = line_at_offset(&starts, end);
    for radius in 0..=starts.len().min(MAX_CONTEXT_RADIUS) {
        let from_line = start_line.saturating_sub(radius);
        let to_line = (end_line.saturating_add(radius)).min(starts.len().saturating_sub(1));
        let from = *starts.get(from_line)?;
        let to = starts
            .get(to_line.saturating_add(1))
            .copied()
            .unwrap_or(content.len());
        let text = content.get(from..to)?.to_owned();
        if text.chars().count() > MAX_CONTEXT_CHARS {
            return None;
        }
        if !text.is_empty() && count_occurrences(content, &text) == 1 {
            return Some(Context {
                start: from,
                end: to,
                text,
            });
        }
    }
    None
}

#[derive(Debug)]
struct Context {
    start: usize,
    end: usize,
    text: String,
}

fn line_at_offset(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1)
}

fn count_occurrences(content: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut offset = 0usize;
    while let Some(found) = content
        .get(offset..)
        .and_then(|content| content.find(needle))
    {
        count = count.saturating_add(1);
        if count >= 2 {
            return count;
        }
        let start = offset.saturating_add(found);
        let step = content
            .get(start..)
            .and_then(|content| content.chars().next())
            .map_or(needle.len(), char::len_utf8);
        offset = start.saturating_add(step);
        if offset >= content.len() {
            break;
        }
    }
    count
}

fn prepare_range(raw: &Value) -> Option<Range> {
    value_range(raw)
        .or_else(|| raw.get("range").and_then(value_range))
        .or_else(|| {
            (raw.get("defaultBehavior").and_then(Value::as_bool) == Some(true)).then_some(Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            })
        })
}

fn command_name(command: &Value) -> String {
    command.as_str().map_or_else(
        || {
            command
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned()
        },
        str::to_owned,
    )
}

fn position_value(position: &Position) -> Value {
    serde_json::json!({"line": position.line, "character": position.character})
}

fn tool_error_as_lsp(error: ToolError) -> LspError {
    LspError::InvalidInput(error.to_string())
}
