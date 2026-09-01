//! Bounded rust-analyzer symbol and location tools.

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    lsp::documents,
    lsp::{
        ClientRef, LspClientLike, LspError, ManagerError, Position, Range, RustAnalyzerManager,
        incremental_change, normalize, value_range,
    },
    workspace::{ClientRoots, RootError, RootGuard},
};

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_HOVER_CHARS: usize = 1_800;
const MAX_REFERENCE_COUNT: usize = 15;
const MAX_REFERENCE_SNIPPET_CHARS: usize = 220;
const MAX_DEFINITION_CHARS: usize = 6_000;
const MAX_DOCUMENTS: usize = 1_000;
const MAX_SYMBOL_ENTRIES: usize = 4_096;
const MAX_SYMBOL_DEPTH: usize = 128;
const RETRY_ATTEMPTS: usize = 21;
const RETRY_DELAY: Duration = Duration::from_millis(50);
const ADVISORY: &str = "Untrusted rust-analyzer output follows; advisory only. The compiler output remains ground truth.";

tokio::task_local! {
    static LSP_CANCELLATION: CancellationToken;
}

pub async fn with_lsp_cancellation<F>(cancellation: CancellationToken, future: F) -> F::Output
where
    F: Future,
{
    LSP_CANCELLATION.scope(cancellation, future).await
}

pub(crate) fn current_lsp_cancellation() -> Option<CancellationToken> {
    LSP_CANCELLATION.try_with(Clone::clone).ok()
}

/// Errors returned by the semantic helpers before MCP response conversion.
#[derive(Debug, Error, Clone)]
pub enum ToolError {
    #[error("invalid semantic-tool input: {0}")]
    InvalidInput(String),
    #[error("workspace boundary rejected the path: {0}")]
    Boundary(String),
    #[error("workspace file is not valid UTF-8: {0}")]
    InvalidUtf8(String),
    #[error("{0}")]
    Symbol(String),
    #[error(transparent)]
    Lsp(#[from] LspError),
    #[error("rust-analyzer manager error: {0}")]
    Manager(String),
}

impl From<ManagerError> for ToolError {
    fn from(error: ManagerError) -> Self {
        if matches!(error, ManagerError::Cancelled) {
            Self::Lsp(LspError::Cancelled)
        } else {
            Self::Manager(error.to_string())
        }
    }
}

impl From<RootError> for ToolError {
    fn from(error: RootError) -> Self {
        Self::Boundary(error.to_string())
    }
}

pub type LspPosition = Position;
pub type LspRange = Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolEntry {
    pub name: String,
    pub container_name: Option<String>,
    pub kind: Option<u32>,
    pub line: u32,
    pub character: u32,
}

pub const SYMBOL_LIMITS: (usize, usize, usize) =
    (MAX_HOVER_CHARS, MAX_REFERENCE_COUNT, MAX_DEFINITION_CHARS);

pub fn match_symbol<'a>(symbols: &'a [SymbolEntry], query: &str) -> Option<&'a SymbolEntry> {
    match_symbol_candidates(symbols, query).into_iter().next()
}

pub fn match_symbol_candidates<'a>(
    symbols: &'a [SymbolEntry],
    query: &str,
) -> Vec<&'a SymbolEntry> {
    let exact = symbols
        .iter()
        .filter(|entry| entry.name == query)
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return exact;
    }
    let folded = query.to_lowercase();
    let case_fold = symbols
        .iter()
        .filter(|entry| entry.name.to_lowercase() == folded)
        .collect::<Vec<_>>();
    if !case_fold.is_empty() {
        return case_fold;
    }
    let prefix = symbols
        .iter()
        .filter(|entry| entry.name.to_lowercase().starts_with(&folded))
        .collect::<Vec<_>>();
    if !prefix.is_empty() {
        return prefix;
    }
    symbols
        .iter()
        .filter(|entry| entry.name.to_lowercase().contains(&folded))
        .collect()
}

/// Find the first symbol occurrence and return an LSP UTF-16 character index.
pub fn find_symbol_column(line_text: &str, symbol: &str) -> Option<u32> {
    if symbol.is_empty() {
        return None;
    }
    let mut offset = 0usize;
    while let Some(found) = line_text[offset..].find(symbol) {
        let start = offset + found;
        let end = start.saturating_add(symbol.len());
        let before = line_text[..start].chars().next_back();
        let after = line_text[end..].chars().next();
        let is_word = |character: Option<char>| {
            character.is_none_or(|character| character.is_alphanumeric() || character == '_')
        };
        if !is_word(before) || !is_word(after) {
            return Some(utf16_len(&line_text[..start]));
        }
        offset = end;
        if offset >= line_text.len() {
            break;
        }
    }
    None
}

pub fn utf16_len(text: &str) -> u32 {
    text.chars()
        .map(|character| character.len_utf16() as u32)
        .sum()
}

pub async fn current_document_version(client: &dyn LspClientLike, uri: &str) -> Option<i64> {
    client.document_version(uri).await.ok().flatten()
}

/// Resolve an existing workspace file to a slash-separated workspace-relative
/// path. Symlink components and escapes are rejected by the capability root.
pub fn resolve_asset_path(root: &Path, raw_path: impl AsRef<Path>) -> Option<PathBuf> {
    let root = canonical_root(root).ok()?;
    let guard = RootGuard::new([root], std::iter::empty()).ok()?;
    let snapshot = guard.snapshot(ClientRoots::unsupported()).ok()?;
    let resolved = snapshot.resolve_existing(raw_path.as_ref()).ok()?;
    if resolved.relative.as_os_str().is_empty() {
        return None;
    }
    Some(resolved.relative)
}

/// Read through the capability-relative file descriptor. The optional hook is
/// invoked after opening and exists to exercise the symlink-swap invariant.
pub fn read_workspace_file_with_hook<F>(
    root: &Path,
    path: &Path,
    after_open: F,
) -> Result<String, ToolError>
where
    F: FnOnce(),
{
    let root = canonical_root(root).map_err(ToolError::Boundary)?;
    let guard = RootGuard::new([root], std::iter::empty())?;
    let snapshot = guard.snapshot(ClientRoots::unsupported())?;
    let authority = snapshot
        .roots()
        .first()
        .ok_or_else(|| ToolError::Boundary("workspace root is empty".to_owned()))?;
    let (_, _, bytes) =
        documents::read_authorized_file_with_hook(authority, path, MAX_FILE_BYTES, after_open)
            .map_err(|error| ToolError::Boundary(error.to_string()))?;
    String::from_utf8(bytes).map_err(|error| ToolError::InvalidUtf8(error.to_string()))
}

pub fn read_workspace_file(root: &Path, path: &Path) -> Option<String> {
    read_workspace_file_with_hook(root, path, || {}).ok()
}

pub fn display_path(root: &Path, path: &Path) -> String {
    if let Some(relative) = resolve_asset_path(root, path) {
        return path_to_slashes(&relative);
    }
    format!("[outside-workspace] {}", path.display())
}

pub fn file_path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    if rest.contains('?') || rest.contains('#') {
        return None;
    }
    let path = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let (host, path) = rest.split_once('/')?;
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        format!("/{path}")
    };
    let decoded = percent_decode(path.as_bytes())?;
    let decoded = String::from_utf8(decoded).ok()?;
    let path = PathBuf::from(decoded);
    path.is_absolute().then_some(path)
}

pub fn flatten_symbols(raw: &Value) -> Vec<SymbolEntry> {
    let mut entries = Vec::new();
    let mut pending = raw
        .as_array()
        .into_iter()
        .flat_map(|items| items.iter().rev())
        .map(|item| (item, None::<String>, 0usize))
        .collect::<Vec<_>>();
    while let Some((item, container, depth)) = pending.pop() {
        if entries.len() >= MAX_SYMBOL_ENTRIES {
            break;
        }
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            continue;
        };
        let start = object
            .get("selectionRange")
            .and_then(|range| range.get("start"))
            .or_else(|| {
                object
                    .get("location")
                    .and_then(|location| location.get("range"))
                    .and_then(|range| range.get("start"))
            })
            .or_else(|| object.get("range").and_then(|range| range.get("start")));
        if let Some(start) = start {
            if let (Some(line), Some(character)) = (
                start.get("line").and_then(Value::as_u64),
                start.get("character").and_then(Value::as_u64),
            ) {
                entries.push(SymbolEntry {
                    name: name.to_owned(),
                    container_name: object
                        .get("containerName")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| container.clone()),
                    kind: object
                        .get("kind")
                        .and_then(Value::as_u64)
                        .and_then(|kind| u32::try_from(kind).ok()),
                    line: u32::try_from(line).unwrap_or(u32::MAX),
                    character: u32::try_from(character).unwrap_or(u32::MAX),
                });
            }
        }
        if depth < MAX_SYMBOL_DEPTH {
            if let Some(children) = object.get("children").and_then(Value::as_array) {
                for child in children.iter().rev() {
                    pending.push((child, Some(name.to_owned()), depth + 1));
                }
            }
        }
    }
    entries
}

pub async fn with_rust_document<T, F>(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    operation: F,
) -> Result<T, ToolError>
where
    F: FnOnce(
            ClientRef,
            String,
            String,
        ) -> Pin<Box<dyn Future<Output = Result<T, LspError>> + Send>>
        + Send
        + 'static,
    T: Send,
{
    let root = canonical_root(root).map_err(ToolError::Boundary)?;
    let relative = resolve_asset_path(&root, relative_path)
        .ok_or_else(|| ToolError::Boundary("path is outside the workspace".to_owned()))?;
    let file = root.join(&relative);
    let text = read_workspace_file(&root, &file)
        .ok_or_else(|| ToolError::Boundary("workspace file is unreadable".to_owned()))?;
    let uri = normalize::path_to_file_uri(&file)
        .map_err(|error| ToolError::Boundary(error.to_string()))?;
    let cancellation = current_lsp_cancellation();
    let operation_cancellation = cancellation.clone();
    let client_operation = move |client: ClientRef| {
        let uri = uri.clone();
        let text = text.clone();
        let cancellation = operation_cancellation.clone();
        async move {
            let _document = client
                .begin_document_with_cancellation(&uri, "rust", &text, cancellation)
                .await?;
            operation(client, uri, text).await
        }
    };
    let result = match cancellation {
        Some(cancellation) => {
            manager
                .with_client_with_cancellation(&root, cancellation, client_operation)
                .await
        }
        None => manager.with_client(&root, client_operation).await,
    };
    result.map_err(ToolError::from)
}

pub async fn with_symbol_position<T, F>(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
    timeout: Duration,
    operation: F,
) -> Result<T, ToolError>
where
    F: FnOnce(
            ClientRef,
            Position,
            String,
            String,
        ) -> Pin<Box<dyn Future<Output = Result<T, LspError>> + Send>>
        + Send
        + 'static,
    T: Send,
{
    if symbol.trim().is_empty() {
        return Err(ToolError::InvalidInput("symbol cannot be empty".to_owned()));
    }
    let symbol = symbol.to_owned();
    with_rust_document(manager, root, relative_path, move |client, uri, text| {
        let symbol = symbol.clone();
        Box::pin(async move {
            let position = if let Some(line) = line {
                let line = line.saturating_sub(1);
                let line_text = text.lines().nth(line as usize).unwrap_or("");
                let character = find_symbol_column(line_text, &symbol).ok_or_else(|| {
                    LspError::NotFound(format!(
                        "symbol '{symbol}' was not found on line {}",
                        line.saturating_add(1)
                    ))
                })?;
                Position { line, character }
            } else {
                let raw = request_until(
                    client.as_ref(),
                    "textDocument/documentSymbol",
                    serde_json::json!({"textDocument": {"uri": uri}}),
                    timeout,
                    |value| value.as_array().is_some_and(|items| !items.is_empty()),
                )
                .await?;
                let entries = flatten_symbols(&raw);
                let matches = match_symbol_candidates(&entries, &symbol);
                if matches.is_empty() {
                    return Err(LspError::NotFound(format!(
                        "symbol '{symbol}' was not found"
                    )));
                }
                if matches.len() > 1 {
                    return Err(LspError::Ambiguous(format!(
                        "symbol '{symbol}' is ambiguous; pass line to select one occurrence"
                    )));
                }
                let entry = matches[0];
                Position {
                    line: entry.line,
                    character: entry.character,
                }
            };
            operation(client, position, uri, text).await
        })
    })
    .await
}

pub async fn symbol_hover(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
    timeout: Duration,
) -> Result<String, ToolError> {
    let symbol = symbol.to_owned();
    let operation_symbol = symbol.clone();
    with_symbol_position(
        manager,
        root,
        relative_path,
        &symbol,
        line,
        timeout,
        move |client, position, uri, _text| {
            let symbol = operation_symbol.clone();
            Box::pin(async move {
                let value = request_until(
                    client.as_ref(),
                    "textDocument/hover",
                    serde_json::json!({"textDocument": {"uri": uri}, "position": position_value(&position)}),
                    timeout,
                    |value| !value.is_null(),
                )
                .await?;
                let output = value.get("contents").and_then(hover_text);
                Ok(output.map_or_else(
                    || format!("{ADVISORY}\nNo hover information for '{symbol}'."),
                    |output| format!("{ADVISORY}\n{}", bounded_chars(&output, MAX_HOVER_CHARS)),
                ))
            })
        },
    )
    .await
}

pub async fn symbol_hover_at_position(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    line: u32,
    character: u32,
    timeout: Duration,
) -> Result<String, ToolError> {
    with_rust_document(manager, root, relative_path, move |client, uri, _text| {
        Box::pin(async move {
            let value = request_until(
                client.as_ref(),
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": character}
                }),
                timeout,
                |value| !value.is_null(),
            )
            .await?;
            let output = value
                .get("contents")
                .and_then(hover_text)
                .map(|output| bounded_chars(&output, MAX_HOVER_CHARS));
            Ok(output.map_or_else(
                || format!("{ADVISORY}\nNo hover information at the requested position."),
                |output| format!("{ADVISORY}\n{output}"),
            ))
        })
    })
    .await
}

pub async fn symbol_references(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
    timeout: Duration,
) -> Result<String, ToolError> {
    let root = root.to_owned();
    let symbol = symbol.to_owned();
    let operation_root = root.clone();
    let operation_symbol = symbol.clone();
    let requested_path = path_to_slashes(relative_path);
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
            let requested_path = requested_path.clone();
            Box::pin(async move {
                let value = request_until(
                    client.as_ref(),
                    "textDocument/references",
                    serde_json::json!({
                        "textDocument": {"uri": uri},
                        "position": position_value(&position),
                        "context": {"includeDeclaration": true}
                    }),
                    timeout,
                    |value| value.is_array(),
                )
                .await?;
                let references = value.as_array().cloned().unwrap_or_default();
                if references.is_empty() {
                    return Ok(format!(
                        "{ADVISORY}\nNo references found for '{symbol}' ({requested_path})."
                    ));
                }
                let mut lines = vec![ADVISORY.to_owned()];
                let mut shown = 0usize;
                let mut seen = std::collections::BTreeSet::new();
                for reference in &references {
                    if shown >= MAX_REFERENCE_COUNT {
                        break;
                    }
                    let Some(uri) = reference.get("uri").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(file) = file_path_from_uri(uri) else {
                        continue;
                    };
                    let Some(start) = reference.get("range").and_then(|range| range.get("start"))
                    else {
                        continue;
                    };
                    let Some(line) = start.get("line").and_then(Value::as_u64) else {
                        continue;
                    };
                    let end = reference.get("range").and_then(|range| range.get("end"));
                    let end_line = end
                        .and_then(|end| end.get("line"))
                        .and_then(Value::as_u64)
                        .unwrap_or(line);
                    let end_character = end
                        .and_then(|end| end.get("character"))
                        .and_then(Value::as_u64)
                        .unwrap_or_default();
                    let start_character = start
                        .get("character")
                        .and_then(Value::as_u64)
                        .unwrap_or_default();
                    let key = format!("{uri}:{line}:{start_character}:{end_line}:{end_character}");
                    if !seen.insert(key) {
                        continue;
                    }
                    let preview = read_workspace_file(&root, &file).map_or_else(
                        || "(content omitted: location is outside the workspace)".to_owned(),
                        |content| snippet_from_content(&content, line as usize),
                    );
                    lines.push(format!(
                        "{}:{}  {}",
                        display_path(&root, &file),
                        line.saturating_add(1),
                        bounded_chars(&preview, MAX_REFERENCE_SNIPPET_CHARS)
                    ));
                    shown = shown.saturating_add(1);
                }
                lines.push(format!("({} total, showing {shown})", references.len()));
                Ok(lines.join("\n"))
            })
        },
    )
    .await
}

pub async fn symbol_definition(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: &str,
    line: Option<u32>,
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
                let value = request_until(
                    client.as_ref(),
                    "textDocument/definition",
                    serde_json::json!({
                        "textDocument": {"uri": uri},
                        "position": position_value(&position)
                    }),
                    timeout,
                    |value| !is_empty_definition(value),
                )
                .await?;
                let Some(location) = first_location(&value) else {
                    return Ok(format!("{ADVISORY}\nNo definition found for '{symbol}'."));
                };
                let Some(file) = file_path_from_uri(&location.uri) else {
                    return Ok(format!(
                        "{ADVISORY}\nDefinition uses an unsupported URI: {}",
                        location.uri
                    ));
                };
                let line = location.range.as_ref().map_or(0, |range| range.start.line);
                let excerpt = read_workspace_file(&root, &file).map_or_else(
                    || "(content omitted: definition is outside the workspace)".to_owned(),
                    |content| excerpt_from_content(&content, line as usize, MAX_DEFINITION_CHARS),
                );
                Ok(format!(
                    "{ADVISORY}\n{}:{}\n{excerpt}",
                    display_path(&root, &file),
                    line.saturating_add(1)
                ))
            })
        },
    )
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionLocation {
    pub uri: String,
    pub range: Option<Range>,
}

pub fn first_location(raw: &Value) -> Option<DefinitionLocation> {
    if let Some(items) = raw.as_array() {
        return items.iter().find_map(first_location);
    }
    let object = raw.as_object()?;
    if let Some(uri) = object.get("uri").and_then(Value::as_str) {
        return Some(DefinitionLocation {
            uri: uri.to_owned(),
            range: object.get("range").and_then(value_range),
        });
    }
    let uri = object.get("targetUri").and_then(Value::as_str)?;
    Some(DefinitionLocation {
        uri: uri.to_owned(),
        range: object
            .get("targetSelectionRange")
            .and_then(value_range)
            .or_else(|| object.get("targetRange").and_then(value_range)),
    })
}

pub fn excerpt_from_content(content: &str, start_line: usize, max_chars: usize) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    let from = start_line.saturating_sub(6);
    let to = lines.len().min(from.saturating_add(80));
    bounded_chars(&lines[from.min(lines.len())..to].join("\n"), max_chars)
}

pub fn snapshot_rust_files(
    root: &Path,
    current_file: &Path,
    current_text: &str,
) -> Result<HashMap<PathBuf, String>, ToolError> {
    let root = canonical_root(root).map_err(ToolError::Boundary)?;
    let current = resolve_asset_path(&root, current_file)
        .ok_or_else(|| ToolError::Boundary("current file is outside the workspace".to_owned()))?;
    let mut snapshots = HashMap::new();
    snapshots.insert(current, current_text.to_owned());
    let guard = RootGuard::new([root], std::iter::empty())?;
    let snapshot = guard.snapshot(ClientRoots::unsupported())?;
    let authority = snapshot
        .roots()
        .first()
        .ok_or_else(|| ToolError::Boundary("workspace root is empty".to_owned()))?;
    let walked = authority.walk_files_matching(Default::default(), |path| {
        path.extension().is_some_and(|extension| extension == "rs")
    })?;
    for file in walked.files.into_iter().take(MAX_DOCUMENTS) {
        if snapshots.contains_key(&file.path) {
            continue;
        }
        let absolute = authority.path().join(&file.path);
        if let Some(content) = read_workspace_file(authority.path(), &absolute) {
            snapshots.insert(file.path, content);
        }
    }
    Ok(snapshots)
}

pub fn incremental_change_for_tools(previous: &str, next: &str) -> Value {
    let change = incremental_change(previous, next);
    serde_json::json!({"range": {"start": {"line": change.range.start.line, "character": change.range.start.character}, "end": {"line": change.range.end.line, "character": change.range.end.character}}, "text": change.text})
}

fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    normalize::canonical_workspace_path(root).map_err(|error| error.to_string())
}

fn path_to_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn percent_decode(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(bytes.get(index + 1).copied()?)?;
            let low = hex(bytes.get(index + 2).copied()?)?;
            output.push((high << 4) | low);
            index = index.saturating_add(3);
        } else {
            output.push(bytes[index]);
            index = index.saturating_add(1);
        }
    }
    Some(output)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn position_value(position: &Position) -> Value {
    serde_json::json!({"line": position.line, "character": position.character})
}

pub(crate) async fn request_until<F>(
    client: &dyn LspClientLike,
    method: &str,
    params: Value,
    timeout: Duration,
    ready: F,
) -> Result<Value, LspError>
where
    F: Fn(&Value) -> bool,
{
    let mut last = Value::Null;
    for attempt in 0..RETRY_ATTEMPTS {
        let cancellation = LSP_CANCELLATION.try_with(Clone::clone).ok();
        match client
            .request_with_cancellation(method, params.clone(), timeout, cancellation.clone())
            .await
        {
            Ok(value) if ready(&value) => return Ok(value),
            Ok(value) => {
                last = value;
                if attempt + 1 < RETRY_ATTEMPTS {
                    retry_delay(cancellation.as_ref()).await?;
                }
            }
            Err(error) if error.is_content_modified() && attempt + 1 < RETRY_ATTEMPTS => {
                retry_delay(cancellation.as_ref()).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(last)
}

async fn retry_delay(cancellation: Option<&CancellationToken>) -> Result<(), LspError> {
    if let Some(cancellation) = cancellation {
        tokio::select! {
            () = tokio::time::sleep(RETRY_DELAY) => Ok(()),
            () = cancellation.cancelled() => Err(LspError::Cancelled),
        }
    } else {
        tokio::time::sleep(RETRY_DELAY).await;
        Ok(())
    }
}

fn hover_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    if let Some(items) = value.as_array() {
        let text = items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .or_else(|| item.get("value").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n");
        return (!text.trim().is_empty()).then_some(text);
    }
    value
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub(crate) fn bounded_chars(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if output.chars().count() < value.chars().count() {
        output.push_str("\n... (truncated)");
    }
    output
}

fn snippet_from_content(content: &str, line: usize) -> String {
    content.lines().nth(line).unwrap_or("").trim().to_owned()
}

fn is_empty_definition(value: &Value) -> bool {
    value.is_null() || value.as_array().is_some_and(Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;
    use crate::lsp::{ClientCallbacks, ClientFuture, DocumentSyncOptions};

    struct CancellationClient {
        observed: Arc<AtomicBool>,
    }

    impl LspClientLike for CancellationClient {
        fn set_callbacks(&self, _callbacks: ClientCallbacks) -> ClientFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn set_document_sync(&self, _options: DocumentSyncOptions) -> ClientFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn request(
            &self,
            _method: &str,
            _params: Value,
            _timeout: Duration,
        ) -> ClientFuture<'_, Value> {
            Box::pin(async { Err(LspError::Cancelled) })
        }

        fn request_with_cancellation(
            &self,
            _method: &str,
            _params: Value,
            _timeout: Duration,
            cancellation: Option<CancellationToken>,
        ) -> ClientFuture<'_, Value> {
            Box::pin(async move {
                let cancellation = cancellation.expect("scoped cancellation token");
                cancellation.cancelled().await;
                self.observed.store(true, Ordering::SeqCst);
                Err(LspError::Cancelled)
            })
        }

        fn notify(&self, _method: &str, _params: Value) -> ClientFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn shutdown(&self, _timeout: Duration) -> ClientFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn is_closed(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn scoped_cancellation_reaches_the_lsp_request() {
        let observed = Arc::new(AtomicBool::new(false));
        let client = CancellationClient {
            observed: Arc::clone(&observed),
        };
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            cancel.cancel();
        });

        let result = with_lsp_cancellation(
            cancellation,
            request_until(
                &client,
                "textDocument/hover",
                serde_json::json!({}),
                Duration::from_secs(5),
                |_| true,
            ),
        )
        .await;

        assert!(matches!(result, Err(LspError::Cancelled)));
        assert!(observed.load(Ordering::SeqCst));
    }
}
