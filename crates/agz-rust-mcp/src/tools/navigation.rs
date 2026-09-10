//! Bounded rust-analyzer navigation and call-hierarchy tools.

use std::{collections::BTreeSet, future::Future, path::Path, pin::Pin, time::Duration};

use serde_json::Value;

use crate::lsp::{LspClientLike, LspError, Position, Range, RustAnalyzerManager, value_range};

use super::symbol::{
    ToolError, bounded_chars, display_path, excerpt_from_content, file_path_from_uri,
    read_workspace_file, request_until, with_rust_document, with_symbol_position,
};

const ADVISORY: &str = "Untrusted rust-analyzer output follows; advisory only. The compiler output remains ground truth.";
const MAX_SYMBOLS: usize = 200;
const MAX_IMPLEMENTATIONS: usize = 30;
const MAX_HIERARCHY_NODES: usize = 30;
const MAX_HIERARCHY_DEPTH: u32 = 2;
const MAX_DOCUMENT_SYMBOL_DEPTH: usize = 128;
const MAX_FULL_FILES: usize = 3;
const MAX_FULL_FILE_CHARS: usize = 8_000;
const MAX_SUMMARY_CHARS: usize = 6_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbolEntry {
    pub name: String,
    pub kind: String,
    pub container: Option<String>,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationLocation {
    pub uri: String,
    pub range: Range,
    pub excerpt_range: Range,
}

pub async fn document_symbols(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    timeout: Duration,
) -> Result<String, ToolError> {
    let requested = relative_path.to_string_lossy().replace('\\', "/");
    with_rust_document(manager, root, relative_path, move |client, uri, _text| {
        Box::pin(async move {
            let raw = request_until(
                client.as_ref(),
                "textDocument/documentSymbol",
                serde_json::json!({"textDocument": {"uri": uri}}),
                timeout,
                |value| value.is_array(),
            )
            .await?;
            let all = flatten_document_symbols(&raw);
            if all.is_empty() {
                return Ok(format!(
                    "{ADVISORY}\nNo document symbols found in {requested}."
                ));
            }
            let shown = all.iter().take(MAX_SYMBOLS);
            let mut lines = vec![ADVISORY.to_owned()];
            for symbol in shown {
                let container = symbol
                    .container
                    .as_deref()
                    .map_or_else(String::new, |container| format!(" in {container}"));
                lines.push(format!(
                    "{}-{}  {}  {}{}",
                    symbol.start.saturating_add(1),
                    symbol.end.saturating_add(1),
                    symbol.kind,
                    symbol.name,
                    container
                ));
            }
            lines.push(format!(
                "({} total, showing {})",
                all.len(),
                all.len().min(MAX_SYMBOLS)
            ));
            Ok(bounded(&lines.join("\n"), MAX_SUMMARY_CHARS))
        })
    })
    .await
}

pub async fn symbol_implementations(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
    include_contents: bool,
    timeout: Duration,
) -> Result<String, ToolError> {
    let root = root.to_owned();
    let symbol = symbol.to_owned();
    let operation_root = root.clone();
    let operation_symbol = symbol.clone();
    with_symbol_position(
        manager,
        &root,
        relative_path,
        &symbol,
        line,
        timeout,
        move |client, position, uri, _text| {
            let symbol = operation_symbol.clone();
            let root = operation_root.clone();
            Box::pin(async move {
                let raw = request_until(
                    client.as_ref(),
                    "textDocument/implementation",
                    serde_json::json!({
                        "textDocument": {"uri": uri},
                        "position": position_value(&position)
                    }),
                    timeout,
                    |value| value.is_array() || value.is_object(),
                )
                .await?;
                let all = all_locations(&raw);
                if all.is_empty() {
                    return Ok(format!(
                        "{ADVISORY}\nNo implementations found for '{symbol}'."
                    ));
                }
                Ok(format_locations(&root, &all, include_contents))
            })
        },
    )
    .await
}

pub async fn symbol_hierarchy(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
    direction: &str,
    depth: u32,
    timeout: Duration,
) -> Result<String, ToolError> {
    let root = root.to_owned();
    let direction = match direction {
        "incoming" | "outgoing" | "both" => direction.to_owned(),
        _ => {
            return Err(ToolError::InvalidInput(
                "hierarchy direction must be incoming, outgoing, or both".to_owned(),
            ));
        }
    };
    let symbol = symbol.to_owned();
    let operation_root = root.clone();
    let operation_symbol = symbol.clone();
    with_symbol_position(
        manager,
        &root,
        relative_path,
        &symbol,
        line,
        timeout,
        move |client, position, uri, _text| {
            let direction = direction.clone();
            let symbol = operation_symbol.clone();
            let root = operation_root.clone();
            Box::pin(async move {
                let prepared = request_until(
                    client.as_ref(),
                    "textDocument/prepareCallHierarchy",
                    serde_json::json!({
                        "textDocument": {"uri": uri},
                        "position": position_value(&position)
                    }),
                    timeout,
                    |value| value.as_array().is_some_and(|items| !items.is_empty()),
                )
                .await?;
                let Some(root_item) = first_hierarchy_item(&prepared, &symbol) else {
                    return Ok(format!(
                        "{ADVISORY}\nNo call hierarchy available for '{symbol}'."
                    ));
                };
                let max_depth = depth.clamp(1, MAX_HIERARCHY_DEPTH);
                let mut lines = vec![
                    ADVISORY.to_owned(),
                    format!(
                        "{}  {}",
                        root_item.name,
                        hierarchy_location(&root, &root_item)
                    ),
                ];
                let mut nodes = 1usize;
                if direction == "incoming" || direction == "both" {
                    walk_hierarchy(
                        client.as_ref(),
                        &root,
                        &root_item,
                        "incoming",
                        1,
                        max_depth,
                        timeout,
                        &mut nodes,
                        &mut lines,
                        &mut BTreeSet::from([hierarchy_key(&root_item)]),
                    )
                    .await?;
                }
                if direction == "outgoing" || direction == "both" {
                    walk_hierarchy(
                        client.as_ref(),
                        &root,
                        &root_item,
                        "outgoing",
                        1,
                        max_depth,
                        timeout,
                        &mut nodes,
                        &mut lines,
                        &mut BTreeSet::from([hierarchy_key(&root_item)]),
                    )
                    .await?;
                }
                lines.push(format!("({nodes} nodes, depth {max_depth})"));
                Ok(bounded(&lines.join("\n"), MAX_SUMMARY_CHARS))
            })
        },
    )
    .await
}

fn flatten_document_symbols(raw: &Value) -> Vec<DocumentSymbolEntry> {
    let mut output = Vec::new();
    let mut pending = raw
        .as_array()
        .into_iter()
        .flat_map(|items| items.iter().rev())
        .map(|item| (item, None::<String>, 0usize))
        .collect::<Vec<_>>();
    while let Some((item, container, depth)) = pending.pop() {
        if output.len() >= MAX_SYMBOLS {
            break;
        }
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            continue;
        };
        let range = object.get("range").and_then(value_range).or_else(|| {
            object
                .get("location")
                .and_then(|location| location.get("range"))
                .and_then(value_range)
        });
        let Some(range) = range else {
            continue;
        };
        output.push(DocumentSymbolEntry {
            name: name.to_owned(),
            kind: symbol_kind(object.get("kind").and_then(Value::as_u64)),
            container: object
                .get("containerName")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| container.clone()),
            start: range.start.line,
            end: range.end.line,
        });
        if depth < MAX_DOCUMENT_SYMBOL_DEPTH {
            if let Some(children) = object.get("children").and_then(Value::as_array) {
                for child in children.iter().rev() {
                    pending.push((child, Some(name.to_owned()), depth + 1));
                }
            }
        }
    }
    output
}

fn all_locations(raw: &Value) -> Vec<NavigationLocation> {
    let items = raw.as_array().map_or_else(
        || vec![raw],
        |items| items.iter().take(MAX_IMPLEMENTATIONS).collect(),
    );
    let mut output = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(uri) = object
            .get("uri")
            .and_then(Value::as_str)
            .or_else(|| object.get("targetUri").and_then(Value::as_str))
        else {
            continue;
        };
        let Some(range) = object
            .get("range")
            .and_then(value_range)
            .or_else(|| object.get("targetSelectionRange").and_then(value_range))
            .or_else(|| object.get("targetRange").and_then(value_range))
        else {
            continue;
        };
        let excerpt_range = object
            .get("range")
            .and_then(value_range)
            .or_else(|| object.get("targetRange").and_then(value_range))
            .unwrap_or_else(|| range.clone());
        let key = format!(
            "{uri}:{}:{}:{}:{}",
            range.start.line, range.start.character, range.end.line, range.end.character
        );
        if seen.insert(key) {
            output.push(NavigationLocation {
                uri: uri.to_owned(),
                range,
                excerpt_range,
            });
        }
    }
    output
}

fn format_locations(
    root: &Path,
    locations: &[NavigationLocation],
    include_contents: bool,
) -> String {
    let mut lines = vec![ADVISORY.to_owned()];
    let mut full_files = 0usize;
    for location in locations.iter().take(MAX_IMPLEMENTATIONS) {
        let Some(file) = file_path_from_uri(&location.uri) else {
            continue;
        };
        lines.push(format!(
            "{}:{}",
            display_path(root, &file),
            location.range.start.line.saturating_add(1)
        ));
        let Some(content) = read_workspace_file(root, &file) else {
            lines.push("(content omitted: location is outside the workspace)".to_owned());
            continue;
        };
        if include_contents && full_files < MAX_FULL_FILES {
            let clipped = bounded_chars(&content, MAX_FULL_FILE_CHARS);
            lines.push(clipped);
            full_files = full_files.saturating_add(1);
        } else {
            lines.push(excerpt_from_content(
                &content,
                location.excerpt_range.start.line as usize,
                1_500,
            ));
        }
    }
    lines.push(format!(
        "({} total, showing {}{})",
        locations.len(),
        locations.len().min(MAX_IMPLEMENTATIONS),
        if include_contents {
            format!("; full contents for {full_files} files")
        } else {
            String::new()
        }
    ));
    bounded(
        &lines.join("\n"),
        MAX_SUMMARY_CHARS.max(MAX_FULL_FILES * MAX_FULL_FILE_CHARS),
    )
}

#[derive(Debug, Clone)]
struct HierarchyItem {
    value: Value,
    name: String,
    uri: String,
    range: Range,
}

fn walk_hierarchy<'a>(
    client: &'a dyn LspClientLike,
    root: &'a Path,
    item: &'a HierarchyItem,
    direction: &'a str,
    level: u32,
    max_depth: u32,
    timeout: Duration,
    nodes: &'a mut usize,
    lines: &'a mut Vec<String>,
    seen: &'a mut BTreeSet<String>,
) -> Pin<Box<dyn Future<Output = Result<(), LspError>> + Send + 'a>> {
    Box::pin(async move {
        if level > max_depth || *nodes >= MAX_HIERARCHY_NODES {
            return Ok(());
        }
        let method = if direction == "incoming" {
            "callHierarchy/incomingCalls"
        } else {
            "callHierarchy/outgoingCalls"
        };
        let raw = request_until(
            client,
            method,
            serde_json::json!({"item": item.value}),
            timeout,
            |value| value.is_array(),
        )
        .await?;
        let Some(entries) = raw.as_array() else {
            return Ok(());
        };
        let field = if direction == "incoming" {
            "from"
        } else {
            "to"
        };
        for entry in entries {
            if *nodes >= MAX_HIERARCHY_NODES {
                break;
            }
            let Some(child) = entry.get(field).and_then(parse_hierarchy_item) else {
                continue;
            };
            if !seen.insert(hierarchy_key(&child)) {
                continue;
            }
            *nodes = nodes.saturating_add(1);
            let marker = if direction == "incoming" { "<-" } else { "->" };
            lines.push(format!(
                "{}{} {}  {}",
                "  ".repeat(level as usize),
                marker,
                child.name,
                hierarchy_location(root, &child)
            ));
            walk_hierarchy(
                client,
                root,
                &child,
                direction,
                level.saturating_add(1),
                max_depth,
                timeout,
                nodes,
                lines,
                seen,
            )
            .await?;
        }
        Ok(())
    })
}

fn first_hierarchy_item(raw: &Value, symbol: &str) -> Option<HierarchyItem> {
    let items = raw.as_array()?;
    items
        .iter()
        .take(MAX_HIERARCHY_NODES)
        .find(|item| item.get("name").and_then(Value::as_str) == Some(symbol))
        .or_else(|| items.first())
        .and_then(parse_hierarchy_item)
}

fn parse_hierarchy_item(value: &Value) -> Option<HierarchyItem> {
    Some(HierarchyItem {
        value: value.clone(),
        name: value.get("name")?.as_str()?.to_owned(),
        uri: value.get("uri")?.as_str()?.to_owned(),
        range: value.get("range").and_then(value_range)?,
    })
}

fn hierarchy_key(item: &HierarchyItem) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        item.uri,
        item.range.start.line,
        item.range.start.character,
        item.range.end.line,
        item.range.end.character,
        item.name
    )
}

fn hierarchy_location(root: &Path, item: &HierarchyItem) -> String {
    file_path_from_uri(&item.uri).map_or_else(
        || item.uri.clone(),
        |file| {
            format!(
                "{}:{}",
                display_path(root, &file),
                item.range.start.line.saturating_add(1)
            )
        },
    )
}

fn position_value(position: &Position) -> Value {
    serde_json::json!({"line": position.line, "character": position.character})
}

fn symbol_kind(kind: Option<u64>) -> String {
    match kind {
        Some(2) => "module",
        Some(5) => "class",
        Some(6) => "method",
        Some(10) => "enum",
        Some(11) => "interface",
        Some(12) => "function",
        Some(13) => "variable",
        Some(14) => "constant",
        Some(22) => "enum-member",
        Some(23) => "struct",
        Some(26) => "type-parameter",
        Some(kind) => return format!("kind-{kind}"),
        None => "kind-unknown",
    }
    .to_owned()
}

fn bounded(value: &str, max_chars: usize) -> String {
    bounded_chars(value, max_chars)
}
