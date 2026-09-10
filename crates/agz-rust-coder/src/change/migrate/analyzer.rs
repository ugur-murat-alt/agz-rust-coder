//! Advisory rust-analyzer analysis used by `change(action=migrate)`.
//!
//! The analyzer resolves an anchor definition, its references, and its
//! implementations through the configured rust-analyzer manager. Every
//! reference is checked back through `textDocument/definition` so a same-named
//! but unrelated symbol is separated by definition identity rather than by
//! text. Identity checks beyond the configured budget stay `None` and become
//! typed obligations instead of silent trust.

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use serde_json::Value;

use crate::{
    lsp::{LspClientLike, LspError, Position, Range, RustAnalyzerManager, value_range},
    tools::symbol::{
        ToolError, file_path_from_uri, first_location, request_until, with_symbol_position,
    },
};

/// Bounded transport-level facts about one analyzed location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedSite {
    /// Candidate-relative path with `/` separators.
    pub file: String,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    /// `Some(true)` when the site resolves back to the anchor definition,
    /// `Some(false)` when it resolves elsewhere, `None` when the identity
    /// check could not run within the budget.
    pub identity: Option<bool>,
}

/// Complete bounded analysis of one anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorAnalysis {
    pub definition: AnalyzedSite,
    pub implementations: Vec<AnalyzedSite>,
    pub references: Vec<AnalyzedSite>,
    pub references_total: u64,
    pub implementations_total: u64,
    /// Number of `textDocument/definition` identity checks actually issued.
    pub identity_checks: u64,
    pub signature: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalyzerError {
    Unavailable(String),
    NotFound(String),
    Ambiguous(String),
    Invalid(String),
    Unsupported(String),
}

impl std::fmt::Display for AnalyzerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => {
                write!(formatter, "rust-analyzer is unavailable: {reason}")
            }
            Self::NotFound(reason) => write!(formatter, "anchor not found: {reason}"),
            Self::Ambiguous(reason) => write!(formatter, "anchor is ambiguous: {reason}"),
            Self::Invalid(reason) => write!(formatter, "analysis input is invalid: {reason}"),
            Self::Unsupported(reason) => write!(formatter, "analysis is unsupported: {reason}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalyzeRequest {
    /// Authorized candidate root the analysis runs against.
    pub root: PathBuf,
    pub anchor_file: String,
    pub anchor_symbol: String,
    /// 1-based line, matching the semantic tools.
    pub anchor_line: Option<u32>,
    pub max_references: u64,
    pub max_identity_checks: u64,
    pub timeout: Duration,
}

pub type AnalyzerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Injectible semantic analyzer. The default implementation wraps
/// rust-analyzer; an embedding or test host may supply another implementation.
pub trait MigrationAnalyzer: Send + Sync {
    fn analyze<'a>(
        &'a self,
        request: AnalyzeRequest,
    ) -> AnalyzerFuture<'a, Result<AnchorAnalysis, AnalyzerError>>;
}

/// rust-analyzer backed analyzer. The service runs it against the
/// server-owned candidate copy, never the original workspace.
pub struct RustAnalyzerMigrationAnalyzer {
    manager: Arc<RustAnalyzerManager>,
    timeout: Duration,
}

impl std::fmt::Debug for RustAnalyzerMigrationAnalyzer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RustAnalyzerMigrationAnalyzer")
            .finish_non_exhaustive()
    }
}

impl RustAnalyzerMigrationAnalyzer {
    pub fn new(manager: Arc<RustAnalyzerManager>, timeout: Duration) -> Self {
        Self { manager, timeout }
    }
}

impl MigrationAnalyzer for RustAnalyzerMigrationAnalyzer {
    fn analyze<'a>(
        &'a self,
        request: AnalyzeRequest,
    ) -> AnalyzerFuture<'a, Result<AnchorAnalysis, AnalyzerError>> {
        Box::pin(async move {
            let timeout = self.timeout;
            let root = request.root.clone();
            let anchor_file = PathBuf::from(&request.anchor_file);
            let symbol = request.anchor_symbol.clone();
            let max_references = request.max_references;
            let max_identity_checks = request.max_identity_checks;
            let operation_root = root.clone();
            let operation_symbol = symbol.clone();
            let result = with_symbol_position(
                &self.manager,
                &root,
                &anchor_file,
                &symbol,
                request.anchor_line,
                timeout,
                move |client, position, uri, _text| {
                    let root = operation_root.clone();
                    let symbol = operation_symbol.clone();
                    Box::pin(async move {
                        let definition_raw = request_until(
                            client.as_ref(),
                            "textDocument/definition",
                            serde_json::json!({
                                "textDocument": {"uri": uri},
                                "position": position_json(&position)
                            }),
                            timeout,
                            |value| !is_empty_locations(value),
                        )
                        .await?;
                        let Some(definition_location) = first_location(&definition_raw) else {
                            return Err(LspError::NotFound(format!(
                                "no definition was resolved for '{symbol}'"
                            )));
                        };
                        let Some(definition_file) = relative_file(&root, &definition_location.uri)
                        else {
                            return Err(LspError::InvalidInput(format!(
                                "definition of '{symbol}' is outside the analyzed root"
                            )));
                        };
                        let definition_range = definition_location.range.clone();
                        let definition = site_from_parts(
                            definition_file.clone(),
                            definition_range
                                .as_ref()
                                .map_or(0, |range| range.start.line),
                            definition_range
                                .as_ref()
                                .map_or(0, |range| range.start.character),
                            definition_range
                                .as_ref()
                                .map_or(0, |range| range.end.line),
                            definition_range
                                .as_ref()
                                .map_or(0, |range| range.end.character),
                            Some(true),
                        );

                        let mut notes = Vec::new();
                        let signature =
                            hover_signature(client.as_ref(), &uri, &position, timeout).await;

                        let references_raw = request_until(
                            client.as_ref(),
                            "textDocument/references",
                            serde_json::json!({
                                "textDocument": {"uri": uri},
                                "position": position_json(&position),
                                "context": {"includeDeclaration": false}
                            }),
                            timeout,
                            |value| value.is_array(),
                        )
                        .await?;
                        let mut references = Vec::new();
                        let mut references_total = 0u64;
                        let mut identity_checks = 0u64;
                        for item in references_raw.as_array().map_or(&[][..], Vec::as_slice) {
                            let Some(uri_raw) = item.get("uri").and_then(Value::as_str) else {
                                continue;
                            };
                            let Some(range) = item.get("range").and_then(value_range) else {
                                continue;
                            };
                            references_total = references_total.saturating_add(1);
                            if u64::try_from(references.len()).unwrap_or(u64::MAX) >= max_references {
                                continue;
                            }
                            let Some(file) = relative_file(&root, uri_raw) else {
                                notes.push(format!(
                                    "reference outside the analyzed root was not migrated: {uri_raw}"
                                ));
                                continue;
                            };
                            let mut identity = None;
                            if identity_checks < max_identity_checks {
                                identity_checks = identity_checks.saturating_add(1);
                                identity = definition_identity(
                                    client.as_ref(),
                                    &root,
                                    uri_raw,
                                    &range.start,
                                    &definition_file,
                                    definition_range.as_ref(),
                                    timeout,
                                )
                                .await;
                                if identity.is_none() {
                                    notes.push(format!(
                                        "identity check was inconclusive for {file}:{}",
                                        range.start.line.saturating_add(1)
                                    ));
                                }
                            }
                            references.push(site_from_parts(
                                file,
                                range.start.line,
                                range.start.character,
                                range.end.line,
                                range.end.character,
                                identity,
                            ));
                        }
                        if references_total > u64::try_from(references.len()).unwrap_or(u64::MAX) {
                            notes.push(
                                "reference budget truncated the impact map; remaining sites were not migrated"
                                    .to_owned(),
                            );
                        }
                        if identity_checks >= max_identity_checks
                            && references_total > identity_checks
                        {
                            notes.push(
                                "identity check budget was exhausted; some references were treated as unverified"
                                    .to_owned(),
                            );
                        }

                        let implementations_raw = request_until(
                            client.as_ref(),
                            "textDocument/implementation",
                            serde_json::json!({
                                "textDocument": {"uri": uri},
                                "position": position_json(&position)
                            }),
                            timeout,
                            |value| value.is_array() || value.is_object(),
                        )
                        .await
                        .unwrap_or(Value::Array(Vec::new()));
                        let mut implementations = Vec::new();
                        let mut implementations_total = 0u64;
                        for location in location_items(&implementations_raw) {
                            implementations_total = implementations_total.saturating_add(1);
                            let Some(file) = relative_file(&root, &location.0) else {
                                continue;
                            };
                            let (start, end) = location.1;
                            if file == definition.file
                                && start == (definition.start_line, definition.start_character)
                            {
                                continue;
                            }
                            implementations.push(site_from_parts(
                                file,
                                start.0,
                                start.1,
                                end.0,
                                end.1,
                                Some(true),
                            ));
                        }

                        Ok(AnchorAnalysis {
                            definition,
                            implementations,
                            references,
                            references_total,
                            implementations_total,
                            identity_checks,
                            signature,
                            notes,
                        })
                    })
                },
            )
            .await;
            result.map_err(map_tool_error)
        })
    }
}

fn map_tool_error(error: ToolError) -> AnalyzerError {
    match error {
        ToolError::InvalidInput(reason) => AnalyzerError::Invalid(reason),
        ToolError::Boundary(reason) => AnalyzerError::Invalid(reason),
        ToolError::InvalidUtf8(reason) => AnalyzerError::Invalid(reason),
        ToolError::Symbol(reason) => AnalyzerError::NotFound(reason),
        ToolError::Manager(reason) => AnalyzerError::Unavailable(reason),
        ToolError::Lsp(LspError::NotFound(reason)) => AnalyzerError::NotFound(reason),
        ToolError::Lsp(LspError::Ambiguous(reason)) => AnalyzerError::Ambiguous(reason),
        ToolError::Lsp(LspError::InvalidInput(reason)) => AnalyzerError::Invalid(reason),
        ToolError::Lsp(error) => AnalyzerError::Unavailable(error.to_string()),
    }
}

async fn hover_signature(
    client: &dyn LspClientLike,
    uri: &str,
    position: &Position,
    timeout: Duration,
) -> Option<String> {
    let value = request_until(
        client,
        "textDocument/hover",
        serde_json::json!({"textDocument": {"uri": uri}, "position": position_json(position)}),
        timeout,
        |value| !value.is_null(),
    )
    .await
    .ok()?;
    let text = value.get("contents").and_then(|contents| {
        if let Some(text) = contents.as_str() {
            return Some(text.to_owned());
        }
        contents
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })?;
    let signature = text
        .lines()
        .find(|line| line.contains("fn "))
        .unwrap_or(text.as_str())
        .trim();
    (!signature.is_empty()).then(|| signature.to_owned())
}

async fn definition_identity(
    client: &dyn LspClientLike,
    root: &Path,
    uri: &str,
    position: &Position,
    definition_file: &str,
    definition_range: Option<&Range>,
    timeout: Duration,
) -> Option<bool> {
    let value = request_until(
        client,
        "textDocument/definition",
        serde_json::json!({"textDocument": {"uri": uri}, "position": position_json(position)}),
        timeout,
        |value| !is_empty_locations(value),
    )
    .await
    .ok()?;
    let location = first_location(&value)?;
    let file = relative_file(root, &location.uri)?;
    if file != definition_file {
        return Some(false);
    }
    let Some(range) = location.range else {
        return Some(false);
    };
    let Some(expected) = definition_range else {
        return Some(true);
    };
    Some(
        range.start.line == expected.start.line
            && range.start.character == expected.start.character,
    )
}

/// One resolved implementation/definition location: URI plus UTF-16 positions.
type LocatedImplementation = (String, ((u32, u32), (u32, u32)));

fn location_items(raw: &Value) -> Vec<LocatedImplementation> {
    let items: Vec<&Value> = match raw {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![raw],
        _ => Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|item| {
            let uri = item
                .get("uri")
                .and_then(Value::as_str)
                .or_else(|| item.get("targetUri").and_then(Value::as_str))?
                .to_owned();
            let range = item
                .get("range")
                .and_then(value_range)
                .or_else(|| item.get("targetSelectionRange").and_then(value_range))
                .or_else(|| item.get("targetRange").and_then(value_range))
                .unwrap_or(Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                });
            Some((
                uri,
                (
                    (range.start.line, range.start.character),
                    (range.end.line, range.end.character),
                ),
            ))
        })
        .collect()
}

fn is_empty_locations(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(object) => object.is_empty(),
        _ => false,
    }
}

fn relative_file(root: &Path, uri: &str) -> Option<String> {
    let path = file_path_from_uri(uri)?;
    let relative = path.strip_prefix(root).ok()?;
    let text = relative.to_string_lossy().replace('\\', "/");
    (!text.is_empty()).then_some(text)
}

fn site_from_parts(
    file: String,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    identity: Option<bool>,
) -> AnalyzedSite {
    AnalyzedSite {
        file,
        start_line,
        start_character,
        end_line,
        end_character,
        identity,
    }
}

fn position_json(position: &Position) -> Value {
    serde_json::json!({"line": position.line, "character": position.character})
}
