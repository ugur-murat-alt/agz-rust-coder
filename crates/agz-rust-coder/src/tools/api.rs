//! Bounded API signature resolution and compile-only probes.
//!
//! `resolve` composes existing Rust Analyzer hover, definition, completion, and
//! reference evidence with cargo metadata feature state; it never starts a new
//! semantic engine. `probe` stages a temporary harness module into a
//! server-owned change candidate, type-checks it with the same dependency
//! graph, locked manifest, and feature selection as the authorized workspace,
//! and then discards its ephemeral change. No workspace source is ever written,
//! no lockfile or dependency is ever changed, and a successful probe reports
//! only `COMPILES_IN_CONFIGURATION` — never runtime correctness.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    change::{
        ChangeAction, ChangeData, ChangeDiagnosticData, ChangeRequest, ChangeService, NewFileInput,
        PatchInput,
    },
    config::ApiConfig,
    gate::{GateDetail, GateTargetId, ValidationOptions},
    lsp::{LspError, Position, RustAnalyzerManager},
    workspace::{PackageNode, WorkspaceRoot, WorkspaceSnapshot, graph::PackageGraph},
};

use super::symbol::{
    ToolError, bounded_chars, excerpt_from_content, file_path_from_uri, find_symbol_column,
    request_until, resolve_asset_path, with_rust_document,
};

const MAX_SOURCE_BYTES: u64 = 1_048_576;
const MAX_EXCERPT_CHARS: usize = 2_400;
const MAX_HOVER_CHARS: usize = 1_800;
const MAX_EXPECTED_CHARS: usize = 512;
const MAX_SNIPPET_CHARS: usize = 4_096;
const MAX_LOCATIONS: usize = 8;
const MAX_IMPORTS: usize = 8;
const MAX_TRAIT_LINES: usize = 6;
const MAX_FEATURES: usize = 24;
const MAX_SOURCE_GATES: usize = 12;
const MAX_DIAGNOSTICS: usize = 32;
const MAX_DIAGNOSTIC_CHARS: usize = 512;
const MAX_PROPOSALS: usize = 8;
const MAX_HARNESS_BYTES: usize = 262_144;
const MAX_SOURCE_HASHES: usize = 8;
const MAX_MODULES: usize = 64;
const MAX_OMISSIONS: usize = 12;

/// Supported `api` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApiAction {
    #[default]
    Resolve,
    Probe,
}

impl ApiAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Probe => "probe",
        }
    }
}

/// One resolved anchor: a workspace-relative path plus an optional symbol,
/// line, and UTF-16 character position.
#[derive(Debug, Clone, Default)]
pub struct ApiAnchor {
    pub path: String,
    pub symbol: Option<String>,
    pub line: Option<u32>,
    pub character: Option<u32>,
}

/// Compile configuration for a probe. Only typed Cargo feature selection is
/// accepted; free-form flags and toolchain switches are never forwarded.
#[derive(Debug, Clone, Default)]
pub struct ApiConfiguration {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
}

impl ApiConfiguration {
    /// Typed feature selection for the gate request. Kept separate from
    /// [`ValidationOptions::default`] so unrelated check options stay off.
    pub fn gate_options(&self) -> ValidationOptions {
        ValidationOptions {
            features: self.features.clone(),
            all_features: self.all_features,
            no_default_features: self.no_default_features,
            ..ValidationOptions::default()
        }
    }
}

/// Domain request assembled by the MCP adapter.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub action: ApiAction,
    pub anchor: ApiAnchor,
    pub expected_signature: Option<String>,
    pub snippets: Vec<String>,
    pub change_id: Option<String>,
    pub configuration: ApiConfiguration,
}

/// Request-scoped services and limits for one `api` call.
pub struct ApiEnvironment<'a> {
    pub manager: Option<&'a RustAnalyzerManager>,
    pub root: &'a WorkspaceRoot,
    pub snapshot: Option<&'a WorkspaceSnapshot>,
    pub snapshot_error: Option<String>,
    pub change: Option<&'a Arc<ChangeService>>,
    pub limits: &'a ApiConfig,
    pub timeout: Duration,
    pub tool_output_bytes: u64,
    pub cancellation: CancellationToken,
}

impl std::fmt::Debug for ApiEnvironment<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiEnvironment")
            .field("has_manager", &self.manager.is_some())
            .field("root", &self.root.path())
            .field("has_snapshot", &self.snapshot.is_some())
            .field("has_snapshot_error", &self.snapshot_error.is_some())
            .field("has_change", &self.change.is_some())
            .field("timeout", &self.timeout)
            .field("tool_output_bytes", &self.tool_output_bytes)
            .finish_non_exhaustive()
    }
}

/// Anchor echo carried by every `api` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiAnchorData {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiLocationData {
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiExampleData {
    pub file: String,
    pub line: u32,
    pub excerpt: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiFeatureData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    pub enabled: Vec<String>,
    /// `feature = "..."` occurrences visible in bounded anchor/definition
    /// source text (heuristic, advisory).
    pub source_gates: Vec<String>,
}

/// `resolve` evidence. Every textual field is untrusted analyzer/source text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiResolveData {
    pub analyzer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<ApiLocationData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<ApiLocationData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_excerpt: Option<String>,
    /// Bounded source excerpt around the anchor itself (used when
    /// rust-analyzer is unavailable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_excerpt: Option<String>,
    pub imports: Vec<String>,
    pub trait_requirements: Vec<String>,
    pub features: ApiFeatureData,
    pub examples: Vec<ApiExampleData>,
    pub omissions: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiProbeConfigurationData {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiHarnessFileData {
    pub file: String,
    /// `patched` for the anchor module file, `created` for the harness module.
    pub kind: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiSnippetData {
    pub index: u32,
    pub sha256: String,
    pub bytes: u64,
    pub text: String,
    /// Bounded lexical divergence findings (`todo!`, `unimplemented!`,
    /// `panic!`, `unreachable!`, bare `loop`, process exit, or a trailing
    /// `return`/`break`/`continue`). A non-empty list means a type-check pass
    /// is not reported as a completed implementation.
    pub diverging: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiCompileData {
    pub target: String,
    pub command: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub total_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiDiagnosticData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub message: String,
}

/// Advisory suggestion returned when a probe needs a dependency or feature
/// that the host has not selected. Nothing is applied automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiProposalData {
    /// `dependency`, `dependency_version`, or `feature`.
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    pub reason: String,
    pub requires_explicit_host_change: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiSourceHashData {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiProbeBudgetData {
    pub max_snippets: u64,
    pub max_snippet_bytes: u64,
    pub compile_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiProbeData {
    /// Terminal probe result. On success this is always
    /// `COMPILES_IN_CONFIGURATION`; runtime correctness is never claimed.
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Always true in this version: the probe owns and discards its ephemeral
    /// change, and `changeId` is only a binding label.
    pub ephemeral: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_identity: Option<String>,
    pub configuration: ApiProbeConfigurationData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_signature: Option<String>,
    /// `expected-type-return` when the expected signature is asserted, or
    /// `discard-value` when no constraint was requested.
    pub assertion: String,
    pub input_hash: String,
    pub harness: Vec<ApiHarnessFileData>,
    pub snippets: Vec<ApiSnippetData>,
    pub implementation_complete: bool,
    pub compile: ApiCompileData,
    pub diagnostics: Vec<ApiDiagnosticData>,
    pub proposals: Vec<ApiProposalData>,
    pub original_source: Vec<ApiSourceHashData>,
    pub original_unchanged: bool,
    pub budget: ApiProbeBudgetData,
    pub notes: Vec<String>,
}

/// Bounded payload returned by every `api` action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiData {
    pub status: String,
    pub action: String,
    pub anchor: ApiAnchorData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve: Option<ApiResolveData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<ApiProbeData>,
    pub reason: String,
}

impl ApiData {
    pub fn failure(
        action: ApiAction,
        anchor: &ApiAnchor,
        status: &str,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status: status.to_owned(),
            action: action.as_str().to_owned(),
            anchor: anchor_data(anchor),
            change_id: None,
            resolve: None,
            probe: None,
            reason: reason.into(),
        }
    }
}

fn anchor_data(anchor: &ApiAnchor) -> ApiAnchorData {
    ApiAnchorData {
        path: anchor.path.clone(),
        symbol: anchor.symbol.clone(),
        line: anchor.line,
        character: anchor.character,
    }
}

/// Execute one bounded `api` action. Workspace resolution and admission are
/// owned by the caller.
pub async fn execute_api(request: ApiRequest, env: ApiEnvironment<'_>) -> ApiData {
    if env.cancellation.is_cancelled() {
        return ApiData::failure(
            request.action,
            &request.anchor,
            "CANCELLED",
            "the api request was cancelled before it started",
        );
    }
    match request.action {
        ApiAction::Resolve => resolve(&request, &env).await,
        ApiAction::Probe => probe(request, &env).await,
    }
}

#[derive(Debug, Clone)]
struct RawLocation {
    file: Option<String>,
    line: u32,
}

#[derive(Debug, Default)]
struct LspEvidence {
    hover: Option<String>,
    definition: Option<RawLocation>,
    references: Vec<RawLocation>,
    imports: Vec<String>,
    failures: Vec<String>,
    responded: bool,
}

async fn resolve(request: &ApiRequest, env: &ApiEnvironment<'_>) -> ApiData {
    let anchor = &request.anchor;
    let Some(relative) = resolve_anchor_path(env.root, &anchor.path) else {
        return ApiData::failure(
            ApiAction::Resolve,
            anchor,
            "INVALID",
            "the anchor path is outside the authorized workspace or does not exist",
        );
    };
    let source = match read_source(env.root, &relative, anchor.line.unwrap_or(1)) {
        Ok(source) => source,
        Err(reason) => {
            return ApiData::failure(ApiAction::Resolve, anchor, "INVALID", reason);
        }
    };
    let mut data = ApiData::failure(ApiAction::Resolve, anchor, "OK", "");
    data.change_id = request.change_id.clone();
    let features = feature_data(env, &relative, &source.content);
    let (mut resolve, position) = match env.manager {
        Some(manager) => match collect_lsp_evidence(manager, env, anchor, &relative).await {
            Ok((evidence, position)) => (evidence, position),
            Err(error) => {
                let mut evidence = LspEvidence::default();
                evidence
                    .failures
                    .push(format!("rust-analyzer resolution failed: {error}"));
                (evidence, None)
            }
        },
        None => {
            let mut evidence = LspEvidence::default();
            evidence.failures.push(
                "rust-analyzer manager is unavailable; only a bounded source excerpt is reported"
                    .to_owned(),
            );
            (evidence, None)
        }
    };

    let mut omissions = Vec::new();
    for failure in &resolve.failures {
        if omissions.len() < MAX_OMISSIONS {
            omissions.push(bounded_chars(failure, MAX_DIAGNOSTIC_CHARS));
        }
    }
    let signature = resolve
        .hover
        .as_deref()
        .filter(|hover| !hover.trim().is_empty())
        .map(|hover| bounded_chars(hover, MAX_HOVER_CHARS));
    let definition = resolve.definition.as_ref().and_then(|location| {
        location.file.as_ref().map(|file| ApiLocationData {
            file: file.clone(),
            line: location.line,
        })
    });
    let definition_excerpt = definition.as_ref().and_then(|location| {
        read_source(env.root, Path::new(&location.file), location.line)
            .ok()
            .map(|excerpt| excerpt.excerpt)
    });
    let trait_requirements = extract_trait_requirements(
        signature.as_deref(),
        definition_excerpt.as_deref(),
        MAX_TRAIT_LINES,
    );
    let examples = collect_examples(env, &resolve.references);
    let analyzer_responded = resolve.responded;
    if resolve.references.len() > examples.len() + usize::from(definition.is_some()) {
        omissions.push("reference examples were capped to the bounded per-call limit".to_owned());
    }
    if signature.is_none() && analyzer_responded {
        omissions.push("rust-analyzer returned no hover signature for the anchor".to_owned());
    }
    let anchor_excerpt = if signature.is_none() {
        Some(source.excerpt.clone())
    } else {
        None
    };
    let mut notes = vec![
        "Resolve evidence is advisory; rust-analyzer hover, completion, and references are not a compiler verdict.".to_owned(),
        "Feature state comes from cargo metadata for the package owning the anchor; feature gates in source are textual heuristics.".to_owned(),
        "changeId is recorded as a binding label; no change candidate is read or modified by resolve.".to_owned(),
    ];
    if env.snapshot.is_none() {
        notes.push(match &env.snapshot_error {
            Some(error) => {
                format!("cargo metadata is unavailable ({error}); enabled features are omitted.")
            }
            None => "cargo metadata is unavailable; enabled features are omitted.".to_owned(),
        });
    }
    let resolved = ApiResolveData {
        analyzer: if analyzer_responded {
            "rust-analyzer".to_owned()
        } else {
            "unavailable".to_owned()
        },
        position,
        signature,
        definition,
        definition_excerpt,
        anchor_excerpt,
        imports: std::mem::take(&mut resolve.imports),
        trait_requirements,
        features,
        examples,
        omissions,
        notes,
    };
    data.resolve = Some(resolved);
    data
}

fn resolve_anchor_path(root: &WorkspaceRoot, requested: &str) -> Option<PathBuf> {
    let relative = resolve_asset_path(root.path(), Path::new(requested))?;
    (!relative.as_os_str().is_empty()).then_some(relative)
}

/// Bounded position selection for hover/completion/definition requests.
fn anchor_position(anchor: &ApiAnchor, text: &str) -> Result<(Position, u32), String> {
    if let Some(line) = anchor.line {
        let index = line.saturating_sub(1) as usize;
        let line_text = text.lines().nth(index).unwrap_or("");
        let character = match (anchor.character, anchor.symbol.as_deref()) {
            (Some(character), _) => character,
            (None, Some(symbol)) => find_symbol_column(line_text, symbol)
                .ok_or_else(|| format!("symbol '{symbol}' was not found on line {line}"))?,
            (None, None) => first_non_whitespace_column(line_text),
        };
        return Ok((
            Position {
                line: index as u32,
                character,
            },
            line,
        ));
    }
    let symbol = anchor
        .symbol
        .as_deref()
        .ok_or_else(|| "an anchor line or symbol is required".to_owned())?;
    for (index, line_text) in text.lines().enumerate() {
        if let Some(character) = find_symbol_column(line_text, symbol) {
            let line = u32::try_from(index + 1).unwrap_or(u32::MAX);
            return Ok((
                Position {
                    line: u32::try_from(index).unwrap_or(u32::MAX),
                    character,
                },
                line,
            ));
        }
    }
    Err(format!(
        "symbol '{symbol}' was not found in the anchor file"
    ))
}

fn first_non_whitespace_column(line: &str) -> u32 {
    let prefix = line.len() - line.trim_start().len();
    u32::try_from(line[..prefix].chars().map(char::len_utf16).sum::<usize>()).unwrap_or(0)
}

async fn collect_lsp_evidence(
    manager: &RustAnalyzerManager,
    env: &ApiEnvironment<'_>,
    anchor: &ApiAnchor,
    relative: &Path,
) -> Result<(LspEvidence, Option<ApiLocationData>), ToolError> {
    let anchor = anchor.clone();
    let timeout = env.timeout;
    let root_path = env.root.path().to_owned();
    let position_out = std::sync::Arc::new(std::sync::Mutex::new(None));
    let position_slot = std::sync::Arc::clone(&position_out);
    let relative_owned = relative.to_path_buf();
    let evidence = with_rust_document(
        manager,
        env.root.path(),
        relative,
        move |client, uri, text| {
            Box::pin(async move {
                let (position, display_line) =
                    anchor_position(&anchor, &text).map_err(LspError::InvalidInput)?;
                if let Ok(mut slot) = position_slot.lock() {
                    *slot = Some(ApiLocationData {
                        file: relative_display(&root_path, &relative_owned),
                        line: display_line,
                    });
                }
                let position_value =
                    json!({"line": position.line, "character": position.character});
                let mut evidence = LspEvidence::default();
                match request_until(
                    client.as_ref(),
                    "textDocument/hover",
                    json!({"textDocument": {"uri": uri}, "position": position_value.clone()}),
                    timeout,
                    |value| !value.is_null(),
                )
                .await
                {
                    Ok(value) => {
                        evidence.hover = value.get("contents").and_then(hover_text);
                        evidence.responded |= evidence.hover.is_some();
                    }
                    Err(error) => evidence.failures.push(format!("hover: {error}")),
                }
                match request_until(
                    client.as_ref(),
                    "textDocument/definition",
                    json!({"textDocument": {"uri": uri}, "position": position_value.clone()}),
                    timeout,
                    |value| !is_empty_locations(value),
                )
                .await
                {
                    Ok(value) => {
                        evidence.definition =
                            parse_locations(&value, &root_path, 1).into_iter().next();
                        evidence.responded |= evidence.definition.is_some();
                    }
                    Err(error) => evidence.failures.push(format!("definition: {error}")),
                }
                match request_until(
                    client.as_ref(),
                    "textDocument/references",
                    json!({
                        "textDocument": {"uri": uri},
                        "position": position_value.clone(),
                        "context": {"includeDeclaration": true}
                    }),
                    timeout,
                    |value| value.is_array(),
                )
                .await
                {
                    Ok(value) => {
                        let locations = parse_locations(&value, &root_path, MAX_LOCATIONS);
                        evidence.responded |= !locations.is_empty();
                        evidence.references = locations;
                    }
                    Err(error) => evidence.failures.push(format!("references: {error}")),
                }
                match request_until(
                    client.as_ref(),
                    "textDocument/completion",
                    json!({"textDocument": {"uri": uri}, "position": position_value}),
                    timeout,
                    |value| completion_items(value).is_some(),
                )
                .await
                {
                    Ok(value) => {
                        if let Some(symbol) = anchor.symbol.as_deref() {
                            evidence.imports = extract_imports(&value, symbol, MAX_IMPORTS);
                            evidence.responded |= !evidence.imports.is_empty();
                        }
                    }
                    Err(error) => evidence.failures.push(format!("completion: {error}")),
                }
                Ok(evidence)
            })
        },
    )
    .await?;
    let position = position_out.lock().ok().and_then(|slot| slot.clone());
    Ok((evidence, position))
}

fn relative_display(root: &Path, relative: &Path) -> String {
    match resolve_asset_path(root, relative) {
        Some(resolved) => resolved.to_string_lossy().replace('\\', "/"),
        None => relative.to_string_lossy().replace('\\', "/"),
    }
}

fn is_empty_locations(value: &Value) -> bool {
    value.is_null() || value.as_array().is_some_and(Vec::is_empty)
}

fn parse_locations(raw: &Value, root: &Path, limit: usize) -> Vec<RawLocation> {
    let mut output = Vec::new();
    let items = raw
        .as_array()
        .map_or_else(|| vec![raw], |items| items.iter().collect());
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let uri = object
            .get("uri")
            .and_then(Value::as_str)
            .or_else(|| object.get("targetUri").and_then(Value::as_str));
        let Some(uri) = uri else {
            continue;
        };
        let line = object
            .get("range")
            .and_then(|range| range.get("start"))
            .and_then(|start| start.get("line"))
            .and_then(Value::as_u64)
            .or_else(|| {
                object
                    .get("targetSelectionRange")
                    .and_then(|range| range.get("start"))
                    .and_then(|start| start.get("line"))
                    .and_then(Value::as_u64)
            });
        let Some(line) = line else {
            continue;
        };
        let file = file_path_from_uri(uri).and_then(|path| resolve_asset_path(root, &path));
        output.push(RawLocation {
            file: file.map(|path| path.to_string_lossy().replace('\\', "/")),
            line: u32::try_from(line).unwrap_or(u32::MAX),
        });
        if output.len() >= limit {
            break;
        }
    }
    output
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

fn completion_items(value: &Value) -> Option<&Vec<Value>> {
    if let Some(items) = value.as_array() {
        return Some(items);
    }
    value.get("items").and_then(Value::as_array)
}

fn extract_imports(raw: &Value, symbol: &str, limit: usize) -> Vec<String> {
    let Some(items) = completion_items(raw) else {
        return Vec::new();
    };
    let mut imports = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items.iter().take(256) {
        let label = item.get("label").and_then(Value::as_str).unwrap_or("");
        if label.is_empty() || !label_matches(label, symbol) {
            continue;
        }
        if let Some(edits) = item.get("additionalTextEdits").and_then(Value::as_array) {
            for edit in edits {
                let Some(new_text) = edit.get("newText").and_then(Value::as_str) else {
                    continue;
                };
                for line in new_text.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("use ") && seen.insert(trimmed.to_owned()) {
                        imports.push(bounded_chars(trimmed, 300));
                    }
                }
            }
        }
        if let Some(description) = item
            .get("labelDetails")
            .and_then(|details| details.get("description"))
            .and_then(Value::as_str)
        {
            let entry = bounded_chars(&format!("{label} — {description}"), 300);
            if seen.insert(entry.clone()) {
                imports.push(entry);
            }
        } else if let Some(detail) = item.get("detail").and_then(Value::as_str)
            && !detail.trim().is_empty()
        {
            let entry = bounded_chars(&format!("{label}: {detail}"), 300);
            if seen.insert(entry.clone()) {
                imports.push(entry);
            }
        }
        if imports.len() >= limit {
            break;
        }
    }
    imports
}

fn label_matches(label: &str, symbol: &str) -> bool {
    label == symbol || label.starts_with(symbol) || label.contains(symbol)
}

fn extract_trait_requirements(
    hover: Option<&str>,
    definition_excerpt: Option<&str>,
    limit: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    for text in [hover, definition_excerpt].into_iter().flatten() {
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let relevant = trimmed.contains("where ")
                || trimmed.contains("where\n")
                || trimmed.starts_with("impl ")
                || trimmed.contains(": Iterator")
                || trimmed.contains(": Into")
                || trimmed.contains(": AsRef")
                || trimmed.contains(": Clone")
                || trimmed.contains("dyn ");
            if relevant {
                lines.push(bounded_chars(trimmed, 300));
            }
            if lines.len() >= limit {
                return lines;
            }
        }
    }
    lines
}

fn feature_data(env: &ApiEnvironment<'_>, relative: &Path, source: &str) -> ApiFeatureData {
    let source_gates = source_feature_gates(source, MAX_SOURCE_GATES);
    let Some(snapshot) = env.snapshot else {
        return ApiFeatureData {
            package: None,
            enabled: Vec::new(),
            source_gates,
        };
    };
    let absolute = env.root.path().join(relative);
    let node = owning_node(&snapshot.graph, &absolute);
    let (package, enabled) = match node {
        Some(node) => (
            Some(node.name.clone()),
            node.enabled_features
                .iter()
                .take(MAX_FEATURES)
                .cloned()
                .collect(),
        ),
        None => (None, Vec::new()),
    };
    ApiFeatureData {
        package,
        enabled,
        source_gates,
    }
}

fn owning_node<'a>(graph: &'a PackageGraph, absolute: &Path) -> Option<&'a PackageNode> {
    graph
        .nodes()
        .values()
        .filter(|node| absolute.starts_with(&node.root))
        .max_by_key(|node| node.root.as_os_str().len())
}

fn source_feature_gates(source: &str, limit: usize) -> Vec<String> {
    let mut gates = Vec::new();
    let mut seen = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let Some(found) = source[index..].find("feature") else {
            break;
        };
        let start = index + found;
        index = start + "feature".len();
        let rest = &source[start + "feature".len()..];
        let mut chars = rest.chars();
        let Some(mut next) = chars.next() else { break };
        let mut equals_seen = false;
        while next.is_whitespace() {
            let Some(following) = chars.next() else { break };
            next = following;
        }
        if next == '=' {
            equals_seen = true;
            let Some(following) = chars.next() else { break };
            next = following;
        }
        if !equals_seen {
            continue;
        }
        while next.is_whitespace() {
            let Some(following) = chars.next() else { break };
            next = following;
        }
        if next != '"' {
            continue;
        }
        let value: String = chars.by_ref().take_while(|ch| *ch != '"').collect();
        if !value.is_empty() && seen.insert(value.clone()) {
            gates.push(bounded_chars(&value, 128));
            if gates.len() >= limit {
                break;
            }
        }
    }
    gates
}

fn collect_examples(env: &ApiEnvironment<'_>, locations: &[RawLocation]) -> Vec<ApiExampleData> {
    let mut examples = Vec::new();
    for location in locations {
        if examples.len() >= MAX_LOCATIONS {
            break;
        }
        let Some(file) = location.file.as_deref() else {
            continue;
        };
        let Ok(source) = read_source(env.root, Path::new(file), location.line) else {
            continue;
        };
        examples.push(ApiExampleData {
            file: source.display.clone(),
            line: location.line,
            excerpt: source.excerpt,
        });
    }
    examples
}

struct SourceRead {
    display: String,
    content: String,
    excerpt: String,
}

fn read_source(root: &WorkspaceRoot, relative: &Path, line: u32) -> Result<SourceRead, String> {
    let bytes = root
        .read_file(relative, MAX_SOURCE_BYTES)
        .map_err(|error| bounded_root_error(&error))?;
    let content = String::from_utf8(bytes).map_err(|_| "source file is not UTF-8".to_owned())?;
    let display = relative.to_string_lossy().replace('\\', "/");
    let excerpt =
        excerpt_from_content(&content, line.saturating_sub(1) as usize, MAX_EXCERPT_CHARS);
    Ok(SourceRead {
        display,
        content,
        excerpt,
    })
}

fn bounded_root_error(error: &crate::workspace::RootError) -> String {
    use crate::workspace::RootError;
    match error {
        RootError::TooLarge { max_bytes, .. } => {
            format!("source file exceeds the {max_bytes}-byte probe cap")
        }
        RootError::NotRegularFile(_) => "source path is not a regular file".to_owned(),
        RootError::Symlink(_) => "source path is a symlink and was not followed".to_owned(),
        RootError::PathNotFound(_) => "source path does not exist".to_owned(),
        RootError::PathOutsideRoot(_) => {
            "source path is outside the authorized workspace".to_owned()
        }
        _ => "source file could not be read inside the authorized workspace".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

struct ProbeRefusal {
    status: &'static str,
    reason: String,
}

impl ProbeRefusal {
    fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            status: "UNSUPPORTED_CONTEXT",
            reason: reason.into(),
        }
    }

    fn invalid(reason: impl Into<String>) -> Self {
        Self {
            status: "INVALID",
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct DependencyInfo {
    name: String,
    version: String,
    features: BTreeSet<String>,
}

struct ProbePlan {
    anchor_relative: PathBuf,
    harness_relative: PathBuf,
    module_name: String,
    harness_content: String,
    anchor_patch_old: String,
    anchor_patch_new: String,
    patched_sha256: String,
    patched_bytes: u64,
    harness_sha256: String,
    harness_bytes: u64,
    toolchain: Option<String>,
    dependencies: BTreeMap<String, DependencyInfo>,
    own_features: BTreeSet<String>,
    original_hashes: Vec<ApiSourceHashData>,
}

async fn probe(request: ApiRequest, env: &ApiEnvironment<'_>) -> ApiData {
    let anchor = request.anchor.clone();
    let deadline = Duration::from_millis(env.limits.compile_timeout_ms.max(1_000));
    let work = probe_pipeline(&request, env);
    tokio::pin!(work);
    let outcome = tokio::select! {
        data = &mut work => data,
        () = tokio::time::sleep(deadline) => {
            env.cancellation.cancel();
            let mut data = tokio::time::timeout(Duration::from_secs(10), &mut work)
                .await
                .unwrap_or_else(|_| {
                    ApiData::failure(
                        ApiAction::Probe,
                        &anchor,
                        "TIMEOUT",
                        "the probe exceeded its compile budget and cleanup did not finish in time",
                    )
                });
            let terminal = matches!(
                data.status.as_str(),
                "COMPILES_IN_CONFIGURATION"
                    | "COMPILE_FAILED"
                    | "INCOMPLETE_IMPLEMENTATION"
                    | "UNSUPPORTED_CONTEXT"
            );
            if !terminal {
                data.status = "TIMEOUT".to_owned();
                data.reason = format!(
                    "the probe exceeded its {}-ms compile budget",
                    env.limits.compile_timeout_ms
                );
            }
            data
        }
    };
    outcome
}

async fn probe_pipeline(request: &ApiRequest, env: &ApiEnvironment<'_>) -> ApiData {
    let anchor = &request.anchor;
    let plan = match build_probe_plan(request, env) {
        Ok(plan) => plan,
        Err(refusal) => {
            return probe_failure(request, refusal.status, refusal.reason);
        }
    };
    let Some(change_service) = env.change else {
        return probe_failure(
            request,
            "UNSUPPORTED_CONTEXT",
            "the change engine is disabled (tools.change=false); probe cannot stage an isolated harness",
        );
    };
    if env.cancellation.is_cancelled() {
        return probe_failure(
            request,
            "CANCELLED",
            "the probe was cancelled before the candidate was captured",
        );
    }

    let input_hash = probe_input_hash(request, &plan);
    let mut notes = vec![
        "The probe stages a temporary module inside a server-owned change candidate and discards that change; the original workspace is never written.".to_owned(),
        "COMPILES_IN_CONFIGURATION means the snippet type-checked with the pinned lockfile, dependency graph, toolchain, and feature selection; it never proves runtime correctness.".to_owned(),
        "No lockfile, dependency, or feature was changed; any dependency or feature need is returned only as an advisory proposal.".to_owned(),
        "Snippet value is discarded unless expectedSignature is supplied, in which case the snippet is returned as that type so the compiler must unify it.".to_owned(),
        "changeId is only a binding label; the referenced change is neither read nor modified.".to_owned(),
    ];
    if let Some(error) = &env.snapshot_error {
        notes.push(format!("cargo metadata unavailable: {error}"));
    }

    let created = change_service
        .execute(
            probe_change_request(
                ChangeAction::Create,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                ValidationOptions::default(),
            ),
            env.root,
            env.cancellation.clone(),
            None,
        )
        .await;
    if created.status != "CREATED" {
        return probe_failure(
            request,
            "UNSUPPORTED_CONTEXT",
            format!(
                "the workspace could not be captured for an isolated probe: {}",
                bounded_chars(&created.data.reason, MAX_DIAGNOSTIC_CHARS)
            ),
        );
    }
    let change_id = created.data.change_id.clone().unwrap_or_default();
    let base_identity = created.data.base_identity.clone().unwrap_or_default();
    let harness = vec![ApiHarnessFileData {
        file: plan.harness_relative.to_string_lossy().replace('\\', "/"),
        kind: "created".to_owned(),
        sha256: plan.harness_sha256.clone(),
        bytes: plan.harness_bytes,
    }];
    let staged = change_service
        .execute(
            probe_change_request(
                ChangeAction::Stage,
                Some(change_id.clone()),
                Some(0),
                Some(base_identity.clone()),
                vec![PatchInput {
                    file: plan.anchor_relative.to_string_lossy().replace('\\', "/"),
                    old_string: plan.anchor_patch_old.clone(),
                    new_string: plan.anchor_patch_new.clone(),
                }],
                vec![NewFileInput {
                    file: plan.harness_relative.to_string_lossy().replace('\\', "/"),
                    content: plan.harness_content.clone(),
                }],
                ValidationOptions::default(),
            ),
            env.root,
            env.cancellation.clone(),
            None,
        )
        .await;
    if staged.status != "STAGED" {
        cleanup_probe(change_service, &change_id, env).await;
        return probe_failure(
            request,
            "UNSUPPORTED_CONTEXT",
            format!(
                "the harness could not be staged into the candidate: {}",
                bounded_chars(&staged.data.reason, MAX_DIAGNOSTIC_CHARS)
            ),
        );
    }
    let probe_revision = staged.data.revision;
    let validated = change_service
        .execute(
            probe_change_request(
                ChangeAction::Validate,
                Some(change_id.clone()),
                Some(probe_revision),
                Some(base_identity.clone()),
                Vec::new(),
                Vec::new(),
                request.configuration.gate_options(),
            ),
            env.root,
            env.cancellation.clone(),
            None,
        )
        .await;
    cleanup_probe(change_service, &change_id, env).await;

    let cancelled = env.cancellation.is_cancelled();
    let mut data = match probe_data(
        request,
        env,
        &plan,
        &input_hash,
        &change_id,
        &base_identity,
        probe_revision,
        &validated.data,
        harness,
        notes,
    ) {
        Ok(probe) => {
            let status = probe.result.clone();
            let mut data = ApiData::failure(
                ApiAction::Probe,
                anchor,
                &status,
                "candidate type-checking finished against the isolated candidate copy",
            );
            data.probe = Some(probe);
            data
        }
        Err(reason) => probe_failure(request, "UNSUPPORTED_CONTEXT", reason),
    };
    if cancelled {
        data.status = "CANCELLED".to_owned();
        data.reason = "the probe was cancelled before validation evidence could be read".to_owned();
    }
    data
}

fn probe_failure(request: &ApiRequest, status: &str, reason: impl Into<String>) -> ApiData {
    let mut data = ApiData::failure(ApiAction::Probe, &request.anchor, status, reason);
    data.change_id = request.change_id.clone();
    data
}

async fn cleanup_probe(service: &ChangeService, change_id: &str, env: &ApiEnvironment<'_>) {
    if change_id.is_empty() {
        return;
    }
    let outcome = service
        .execute(
            probe_change_request(
                ChangeAction::Discard,
                Some(change_id.to_owned()),
                None,
                None,
                Vec::new(),
                Vec::new(),
                ValidationOptions::default(),
            ),
            env.root,
            CancellationToken::new(),
            None,
        )
        .await;
    if outcome.status != "DISCARDED" {
        tracing::warn!(
            status = outcome.status,
            "ephemeral api probe change could not be discarded"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn probe_change_request(
    action: ChangeAction,
    change_id: Option<String>,
    expected_revision: Option<u64>,
    base_identity: Option<String>,
    patches: Vec<PatchInput>,
    new_files: Vec<NewFileInput>,
    options: ValidationOptions,
) -> ChangeRequest {
    ChangeRequest {
        action,
        change_id,
        expected_revision,
        base_identity,
        patches,
        new_files,
        target: GateTargetId::Check,
        options,
        detail: GateDetail::Compact,
        timings: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn probe_data(
    request: &ApiRequest,
    env: &ApiEnvironment<'_>,
    plan: &ProbePlan,
    input_hash: &str,
    change_id: &str,
    base_identity: &str,
    probe_revision: u64,
    validated: &ChangeData,
    mut harness: Vec<ApiHarnessFileData>,
    mut notes: Vec<String>,
) -> Result<ApiProbeData, String> {
    let evidence = validated
        .evidence
        .iter()
        .rev()
        .find(|row| row.revision == probe_revision)
        .cloned()
        .ok_or_else(|| {
            "the candidate validation produced no evidence row for the probe revision".to_owned()
        })?;
    harness.insert(
        0,
        ApiHarnessFileData {
            file: plan.anchor_relative.to_string_lossy().replace('\\', "/"),
            kind: "patched".to_owned(),
            sha256: plan.patched_sha256.clone(),
            bytes: plan.patched_bytes,
        },
    );
    let snippets = request
        .snippets
        .iter()
        .enumerate()
        .map(|(index, snippet)| ApiSnippetData {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            sha256: sha256_hex(snippet.as_bytes()),
            bytes: snippet.len() as u64,
            text: bounded_chars(snippet, MAX_SNIPPET_CHARS),
            diverging: diverging_constructs(snippet),
        })
        .collect::<Vec<_>>();
    let diagnostics = bounded_diagnostics(&evidence.diagnostics);
    let compile_status = evidence.status.clone();
    let compile_passed = matches!(compile_status.as_str(), "PASS" | "FULL_PASS" | "FAST_PASS");
    let diverging = snippets.iter().any(|snippet| !snippet.diverging.is_empty());
    let harness_name = harness_file_name(&plan.harness_relative);
    let anchor_name = plan.anchor_relative.to_string_lossy().replace('\\', "/");
    let snippet_attributed =
        diagnostics
            .iter()
            .any(|diagnostic| match diagnostic.file.as_deref() {
                Some(file) => {
                    let normalized = file.replace('\\', "/");
                    normalized.ends_with(&harness_name) || normalized.ends_with(&anchor_name)
                }
                None => true,
            });
    let proposals = if compile_passed {
        Vec::new()
    } else {
        build_proposals(&diagnostics, plan)
    };
    let (result, reason) = if compile_passed && !diverging {
        (
            "COMPILES_IN_CONFIGURATION",
            "The snippet type-checked in this configuration; runtime correctness is not claimed.",
        )
    } else if compile_passed {
        (
            "INCOMPLETE_IMPLEMENTATION",
            "Type-checking passed, but the snippet contains diverging constructs (for example todo! or unimplemented!); it is not a completed implementation.",
        )
    } else if !snippet_attributed {
        (
            "UNSUPPORTED_CONTEXT",
            "cargo check failed without a diagnostic attributed to the harness or anchor file; the failure cannot be attributed to the snippet.",
        )
    } else {
        (
            "COMPILE_FAILED",
            "The snippet did not type-check in this configuration.",
        )
    };
    notes.push(reason.to_owned());
    if !compile_passed && !snippet_attributed {
        notes.push(
            "The candidate workspace did not compile independently of the probe harness; no snippet verdict is reported."
                .to_owned(),
        );
    }
    let (original_source, original_unchanged) = verify_original_unchanged(env, plan);
    notes.push(format!(
        "Harness module {} and change {change_id} were discarded after revision {probe_revision}; binding label: {}.",
        plan.module_name,
        request.change_id.as_deref().unwrap_or("none")
    ));
    Ok(ApiProbeData {
        result: result.to_owned(),
        change_id: request.change_id.clone(),
        ephemeral: true,
        probe_revision: Some(probe_revision),
        base_identity: Some(base_identity.to_owned()),
        configuration: ApiProbeConfigurationData {
            features: request.configuration.features.clone(),
            all_features: request.configuration.all_features,
            no_default_features: request.configuration.no_default_features,
        },
        toolchain: plan.toolchain.clone(),
        expected_signature: request.expected_signature.clone(),
        assertion: if request.expected_signature.is_some() {
            "expected-type-return".to_owned()
        } else {
            "discard-value".to_owned()
        },
        input_hash: input_hash.to_owned(),
        harness,
        snippets,
        implementation_complete: compile_passed && !diverging,
        compile: ApiCompileData {
            target: "check".to_owned(),
            command: compile_command(request),
            status: compile_status,
            exit_code: evidence.exit_code,
            total_ms: evidence.total_ms,
        },
        diagnostics,
        proposals,
        original_source,
        original_unchanged,
        budget: ApiProbeBudgetData {
            max_snippets: env.limits.max_snippets,
            max_snippet_bytes: env.limits.max_snippet_bytes,
            compile_timeout_ms: env.limits.compile_timeout_ms,
        },
        notes,
    })
}

fn harness_file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

fn compile_command(request: &ApiRequest) -> String {
    let mut command = "cargo check --locked".to_owned();
    if request.configuration.all_features {
        command.push_str(" --all-features");
    }
    if request.configuration.no_default_features {
        command.push_str(" --no-default-features");
    }
    if !request.configuration.features.is_empty() {
        let mut features = request.configuration.features.clone();
        features.sort();
        features.dedup();
        command.push_str(" --features ");
        command.push_str(&features.join(","));
    }
    command
}

fn bounded_diagnostics(diagnostics: &[ChangeDiagnosticData]) -> Vec<ApiDiagnosticData> {
    diagnostics
        .iter()
        .take(MAX_DIAGNOSTICS)
        .map(|diagnostic| ApiDiagnosticData {
            code: diagnostic.code.clone(),
            level: diagnostic.level.clone(),
            file: diagnostic.file.clone(),
            line: diagnostic.line,
            message: bounded_chars(&diagnostic.message, MAX_DIAGNOSTIC_CHARS),
        })
        .collect()
}

fn probe_input_hash(request: &ApiRequest, plan: &ProbePlan) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-coder/api-probe/v1\0");
    hasher.update(request.anchor.path.as_bytes());
    hasher.update([0]);
    hasher.update(plan.harness_sha256.as_bytes());
    hasher.update([0]);
    hasher.update(plan.anchor_relative.to_string_lossy().as_bytes());
    hasher.update([0]);
    if let Some(expected) = &request.expected_signature {
        hasher.update(expected.as_bytes());
    }
    hasher.update([0]);
    if request.configuration.all_features {
        hasher.update(b"all-features\0");
    }
    if request.configuration.no_default_features {
        hasher.update(b"no-default-features\0");
    }
    for feature in &request.configuration.features {
        hasher.update(feature.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn build_probe_plan(
    request: &ApiRequest,
    env: &ApiEnvironment<'_>,
) -> Result<ProbePlan, ProbeRefusal> {
    let anchor = &request.anchor;
    if request.snippets.is_empty() {
        return Err(ProbeRefusal::invalid(
            "action=probe requires at least one snippet",
        ));
    }
    if request.snippets.len() > usize::try_from(env.limits.max_snippets).unwrap_or(usize::MAX) {
        return Err(ProbeRefusal::invalid(format!(
            "at most {} snippet(s) are accepted per probe",
            env.limits.max_snippets
        )));
    }
    let total_bytes: usize = request.snippets.iter().map(String::len).sum();
    if total_bytes > usize::try_from(env.limits.max_snippet_bytes).unwrap_or(usize::MAX) {
        return Err(ProbeRefusal::invalid(format!(
            "snippets exceed the {}-byte probe budget",
            env.limits.max_snippet_bytes
        )));
    }
    if let Some(expected) = request.expected_signature.as_deref()
        && (expected.trim().is_empty()
            || expected.chars().count() > MAX_EXPECTED_CHARS
            || expected.chars().any(char::is_control))
    {
        return Err(ProbeRefusal::invalid(
            "expectedSignature must be a bounded single-line Rust type",
        ));
    }
    let Some(snapshot) = env.snapshot else {
        return Err(ProbeRefusal::unsupported(
            "cargo metadata is unavailable; probe cannot establish the same dependency graph and feature selection",
        ));
    };
    let Some(relative) = resolve_anchor_path(env.root, &anchor.path) else {
        return Err(ProbeRefusal::invalid(
            "the anchor path is outside the authorized workspace or does not exist",
        ));
    };
    let source = match read_source(env.root, &relative, anchor.line.unwrap_or(1)) {
        Ok(source) => source,
        Err(reason) => return Err(ProbeRefusal::invalid(reason)),
    };
    if source.content.is_empty() {
        return Err(ProbeRefusal::unsupported(
            "the anchor file is empty and cannot host a probe module declaration",
        ));
    }
    // `--locked` is only honest when a pinned lockfile exists; the probe never
    // creates one.
    let lock = match env
        .root
        .read_file(Path::new("Cargo.lock"), MAX_SOURCE_BYTES)
    {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(ProbeRefusal::unsupported(
                "the authorized root has no readable Cargo.lock; probe compiles only against a pinned graph and never creates a lockfile",
            ));
        }
    };
    let manifest = match env
        .root
        .read_file(Path::new("Cargo.toml"), MAX_SOURCE_BYTES)
    {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(ProbeRefusal::unsupported(
                "the authorized root has no readable Cargo.toml",
            ));
        }
    };
    let absolute = env.root.path().join(&relative);
    let node = match owning_node(&snapshot.graph, &absolute) {
        Some(node) => node,
        None => {
            return Err(ProbeRefusal::unsupported(
                "the anchor is not inside a package known to cargo metadata",
            ));
        }
    };
    if !probe_package_selected(snapshot, node) {
        return Err(ProbeRefusal::unsupported(format!(
            "cargo check at the workspace root does not select package '{}' by default",
            node.name
        )));
    }
    let Some(target) = probe_target(env, snapshot, node, &absolute) else {
        return Err(ProbeRefusal::unsupported(
            "the anchor file is not a declared module of a checked library or binary target",
        ));
    };
    let module_name = probe_module_name(request, &relative, target.as_path());
    let harness_relative = relative
        .parent()
        .map(|parent| parent.join(format!("{module_name}.rs")))
        .unwrap_or_else(|| PathBuf::from(format!("{module_name}.rs")));
    let harness_content = harness_content(request);
    if harness_content.len() > MAX_HARNESS_BYTES {
        return Err(ProbeRefusal::invalid(
            "the generated harness exceeds the bounded harness size",
        ));
    }
    let appended = format!(
        "\n#[allow(dead_code, unused_imports, unused_variables)]\n#[path = \"{module_name}.rs\"]\nmod {module_name};\n"
    );
    let Some((anchor_patch_old, anchor_patch_new)) = append_unique(&source.content, &appended)
    else {
        return Err(ProbeRefusal::unsupported(
            "the anchor file could not be patched unambiguously",
        ));
    };
    if anchor_patch_new.len() > MAX_HARNESS_BYTES {
        return Err(ProbeRefusal::invalid(
            "the patched anchor file exceeds the bounded harness size",
        ));
    }
    let toolchain = toolchain_label(env.root);
    let dependencies = direct_dependencies(snapshot, node);
    let own_features = snapshot
        .metadata
        .packages
        .iter()
        .find(|package| package.id.repr == node.package_id)
        .map(|package| package.features.keys().cloned().collect())
        .unwrap_or_default();
    let original_hashes = vec![
        ApiSourceHashData {
            file: relative.to_string_lossy().replace('\\', "/"),
            sha256: sha256_hex(source.content.as_bytes()),
            bytes: source.content.len() as u64,
        },
        ApiSourceHashData {
            file: "Cargo.toml".to_owned(),
            sha256: sha256_hex(&manifest),
            bytes: manifest.len() as u64,
        },
        ApiSourceHashData {
            file: "Cargo.lock".to_owned(),
            sha256: sha256_hex(&lock),
            bytes: lock.len() as u64,
        },
    ];
    Ok(ProbePlan {
        anchor_relative: relative,
        harness_relative,
        module_name,
        harness_sha256: sha256_hex(harness_content.as_bytes()),
        harness_bytes: harness_content.len() as u64,
        patched_sha256: sha256_hex(anchor_patch_new.as_bytes()),
        patched_bytes: anchor_patch_new.len() as u64,
        harness_content,
        anchor_patch_old,
        anchor_patch_new,
        toolchain,
        dependencies,
        own_features,
        original_hashes,
    })
}

/// Direct dependencies of the anchor package with pinned versions and declared
/// feature names, used to ground advisory proposals in cargo metadata.
fn direct_dependencies(
    snapshot: &WorkspaceSnapshot,
    node: &PackageNode,
) -> BTreeMap<String, DependencyInfo> {
    let mut map = BTreeMap::new();
    for edge in snapshot.graph.outgoing(&node.package_id) {
        let Some(dep) = snapshot.graph.node(&edge.to_package_id) else {
            continue;
        };
        let features = snapshot
            .metadata
            .packages
            .iter()
            .find(|package| package.id.repr == dep.package_id)
            .map(|package| package.features.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let info = DependencyInfo {
            name: dep.name.clone(),
            version: dep.version.clone(),
            features,
        };
        map.insert(edge.dependency_name.clone(), info.clone());
        map.entry(dep.name.clone()).or_insert(info);
    }
    map
}

/// Probe module name: deterministic, valid identifier, bounded, and unique to
/// the probe input.
fn probe_module_name(request: &ApiRequest, anchor: &Path, target: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(anchor.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(target.to_string_lossy().as_bytes());
    hasher.update([0]);
    for snippet in &request.snippets {
        hasher.update(snippet.as_bytes());
        hasher.update([0]);
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("__agz_api_probe_{}", &digest[..12])
}

fn harness_content(request: &ApiRequest) -> String {
    let mut content = String::new();
    content.push_str(
        "// Generated by the agz-rust-coder api probe for compile-only evidence.\n\
         #![allow(dead_code, unused_imports, unused_variables, unused_mut, clippy::all)]\n",
    );
    for (index, snippet) in request.snippets.iter().enumerate() {
        let return_type = request
            .expected_signature
            .as_deref()
            .map_or_else(|| "()".to_owned(), str::to_owned);
        content.push_str(&format!(
            "pub fn __agz_api_probe_fn_{index}() -> {return_type} {{\n{snippet}\n}}\n"
        ));
    }
    content
}

/// The probe owns package selection: cargo check at the candidate root must
/// compile the anchor package without extra flags.
fn probe_package_selected(snapshot: &WorkspaceSnapshot, node: &PackageNode) -> bool {
    let root_package = snapshot
        .metadata
        .root_package()
        .map(|package| package.id.repr.clone());
    if let Some(root_id) = root_package {
        return root_id == node.package_id;
    }
    if snapshot.metadata.workspace_default_members.is_available() {
        return snapshot
            .metadata
            .workspace_default_members
            .contains(&cargo_metadata_id(&node.package_id));
    }
    snapshot
        .metadata
        .workspace_members
        .iter()
        .any(|member| member.repr == node.package_id)
}

fn cargo_metadata_id(repr: &str) -> cargo_metadata::PackageId {
    // `PackageId` parses the Cargo metadata `id` representation verbatim.
    cargo_metadata::PackageId {
        repr: repr.to_owned(),
    }
}

/// Find the checked library/binary target whose module tree contains the
/// anchor file. The anchor file must be reachable through textual `mod`
/// declarations so a pass cannot be vacuous.
fn probe_target(
    env: &ApiEnvironment<'_>,
    snapshot: &WorkspaceSnapshot,
    node: &PackageNode,
    anchor_absolute: &Path,
) -> Option<PathBuf> {
    let package = snapshot
        .metadata
        .packages
        .iter()
        .find(|package| package.id.repr == node.package_id)?;
    let mut targets = package
        .targets
        .iter()
        .filter(|target| {
            target.kind.iter().any(|kind| {
                matches!(
                    kind,
                    cargo_metadata::TargetKind::Lib
                        | cargo_metadata::TargetKind::RLib
                        | cargo_metadata::TargetKind::ProcMacro
                        | cargo_metadata::TargetKind::Bin
                )
            })
        })
        .filter(|target| {
            target
                .src_path
                .parent()
                .is_some_and(|parent| anchor_absolute.starts_with(parent.as_std_path()))
        })
        .collect::<Vec<_>>();
    targets.sort_by_key(|target| target.src_path.as_std_path().as_os_str().len());
    targets
        .into_iter()
        .find(|target| {
            anchor_absolute == target.src_path.as_std_path()
                || declared_module(env.root, target.src_path.as_std_path(), anchor_absolute)
        })
        .map(|target| target.src_path.as_std_path().to_owned())
}

/// Conservative textual module-tree check. Returns false on any ambiguity so
/// the probe refuses instead of reporting a vacuous pass.
fn declared_module(root: &WorkspaceRoot, target_src: &Path, anchor: &Path) -> bool {
    let mut current = anchor.to_path_buf();
    for _ in 0..MAX_MODULES {
        if current == target_src {
            return true;
        }
        let Some((name, candidates)) = declarer_candidates(target_src, &current) else {
            return false;
        };
        let mut matches = Vec::new();
        for candidate in candidates {
            let Ok(relative) = candidate.strip_prefix(root.path()) else {
                continue;
            };
            if let Ok(bytes) = root.read_file(relative, MAX_SOURCE_BYTES)
                && let Ok(content) = String::from_utf8(bytes)
                && declares_module(&content, &name)
            {
                matches.push(candidate);
            }
        }
        if matches.len() != 1 {
            return false;
        }
        current = matches.remove(0);
    }
    false
}

/// Candidate parent module files for one file plus the module name it should
/// declare.
fn declarer_candidates(target_src: &Path, file: &Path) -> Option<(String, Vec<PathBuf>)> {
    let file_name = file.file_name()?.to_string_lossy().to_string();
    let (directory, name) = if file_name == "mod.rs" {
        let module_dir = file.parent()?;
        let name = module_dir.file_name()?.to_string_lossy().to_string();
        (module_dir.parent()?.to_path_buf(), name)
    } else {
        let name = Path::new(&file_name)
            .file_stem()?
            .to_string_lossy()
            .to_string();
        (file.parent()?.to_path_buf(), name)
    };
    let mut candidates = Vec::new();
    if let Some(parent) = directory.parent()
        && let Some(directory_name) = directory.file_name()
    {
        candidates.push(parent.join(format!("{}.rs", directory_name.to_string_lossy())));
    }
    candidates.push(directory.join("mod.rs"));
    if target_src.parent() == Some(directory.as_path()) {
        candidates.insert(0, target_src.to_path_buf());
    }
    let candidates = candidates
        .into_iter()
        .filter(|candidate| candidate.exists() && candidate != file)
        .collect::<Vec<_>>();
    Some((name, candidates))
}

/// True when the cleaned source text declares `mod <name>;` without a
/// `#[path]` attribute that would redirect the module.
fn declares_module(content: &str, name: &str) -> bool {
    let cleaned = strip_comments_and_strings(content);
    let bytes = cleaned.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let Some(found) = cleaned[index..].find("mod") else {
            return false;
        };
        let start = index + found;
        index = start + 3;
        if start > 0 {
            let before = bytes[start - 1];
            if before.is_ascii_alphanumeric() || before == b'_' {
                continue;
            }
        }
        let after = bytes.get(index).copied().unwrap_or(b' ');
        if after.is_ascii_alphanumeric() || after == b'_' {
            continue;
        }
        let mut cursor = index;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let name_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if &cleaned[name_start..cursor] != name {
            continue;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b';') {
            continue;
        }
        let prefix_start = start.saturating_sub(256);
        if cleaned[prefix_start..start].contains("#[path") {
            return false;
        }
        return true;
    }
    false
}

/// Replace the longest unique suffix of `content` with that suffix plus
/// `appended`, so the patch anchor can never apply to more than one position.
fn append_unique(content: &str, appended: &str) -> Option<(String, String)> {
    let mut size = 64usize;
    loop {
        let start = content.len().saturating_sub(size);
        let start = floor_char_boundary(content, start);
        let suffix = &content[start..];
        let occurrences = if suffix.is_empty() {
            0
        } else {
            content.matches(suffix).count()
        };
        if start == 0 || occurrences == 1 {
            if suffix.is_empty() {
                return Some((String::new(), appended.to_owned()));
            }
            return Some((suffix.to_owned(), format!("{suffix}{appended}")));
        }
        if size >= content.len() {
            return None;
        }
        size = size.saturating_mul(2).min(content.len());
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn strip_comments_and_strings(content: &str) -> String {
    let chars = content.as_bytes();
    let mut output = String::with_capacity(content.len());
    let mut index = 0usize;
    while index < chars.len() {
        match chars[index] {
            b'/' if chars.get(index + 1) == Some(&b'/') => {
                while index < chars.len() && chars[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if chars.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < chars.len() && !(chars[index] == b'*' && chars[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(chars.len());
            }
            b'"' => {
                index += 1;
                while index < chars.len() && chars[index] != b'"' {
                    if chars[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
                index = (index + 1).min(chars.len());
            }
            byte => {
                output.push(byte as char);
                index += 1;
            }
        }
    }
    output
}

fn toolchain_label(root: &WorkspaceRoot) -> Option<String> {
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        if let Ok(bytes) = root.read_file(Path::new(name), 4_096)
            && let Ok(text) = String::from_utf8(bytes)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(bounded_chars(trimmed, 256));
            }
        }
    }
    None
}

/// Re-read the original anchor, manifest, and lockfile after the probe and
/// compare them with the hashes captured before the candidate was created.
/// The probe never writes the original workspace, so a mismatch is a hard
/// integrity signal rather than an expected outcome.
fn verify_original_unchanged(
    env: &ApiEnvironment<'_>,
    plan: &ProbePlan,
) -> (Vec<ApiSourceHashData>, bool) {
    let mut unchanged = true;
    let mut hashes = Vec::new();
    for expected in &plan.original_hashes {
        let current = env
            .root
            .read_file(Path::new(&expected.file), MAX_SOURCE_BYTES)
            .ok();
        let matches = current
            .as_deref()
            .is_some_and(|bytes| sha256_hex(bytes) == expected.sha256);
        if !matches {
            unchanged = false;
        }
        hashes.push(expected.clone());
        if hashes.len() >= MAX_SOURCE_HASHES {
            break;
        }
    }
    (hashes, unchanged)
}

// ---------------------------------------------------------------------------
// Divergence and proposals
// ---------------------------------------------------------------------------

fn diverging_constructs(snippet: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    for (needle, label) in [
        ("todo!", "todo!"),
        ("unimplemented!", "unimplemented!"),
        ("unreachable!", "unreachable!"),
        ("panic!", "panic!"),
        ("std::process::exit", "std::process::exit"),
        ("process::abort", "process::abort"),
    ] {
        if snippet.contains(needle) {
            found.insert(label.to_owned());
        }
    }
    if contains_word(snippet, "loop") {
        found.insert("loop".to_owned());
    }
    let tail = snippet
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty());
    if let Some(tail) = tail {
        let tail = tail.trim_end_matches(';').trim_start();
        if tail.starts_with("return") || tail.starts_with("break") || tail.starts_with("continue") {
            found.insert("tail divergence".to_owned());
        }
    }
    found.into_iter().collect()
}

fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(found) = text[index..].find(word) {
        let start = index + found;
        index = start + word.len();
        let before_ok =
            start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
        let after_ok =
            index >= bytes.len() || !bytes[index].is_ascii_alphanumeric() && bytes[index] != b'_';
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

#[derive(Debug, Default)]
struct UnresolvedReference {
    crate_name: Option<String>,
    item: Option<String>,
}

fn parse_unresolved(message: &str) -> Option<UnresolvedReference> {
    let mut reference = UnresolvedReference::default();
    if let Some(rest) = extract_after(message, "use of undeclared crate or module `") {
        reference.crate_name = Some(leading_backtick(rest));
        return Some(reference);
    }
    if let Some(rest) = extract_after(message, "unresolved import `") {
        let path = leading_backtick(rest);
        let parts = path.split("::").collect::<Vec<_>>();
        reference.crate_name = parts.first().map(|part| (*part).to_owned());
        if parts.len() > 1 {
            reference.item = parts.last().map(|part| (*part).to_owned());
        }
        return Some(reference);
    }
    if let Some(rest) = extract_after(message, "in crate `") {
        reference.crate_name = Some(leading_backtick(rest));
        reference.item = first_backtick(message);
        return Some(reference);
    }
    if let Some(rest) = extract_after(message, "can't find crate for `") {
        reference.crate_name = Some(leading_backtick(rest));
        return Some(reference);
    }
    if let Some(rest) = extract_after(message, "can't find crate `") {
        reference.crate_name = Some(leading_backtick(rest));
        return Some(reference);
    }
    None
}

fn extract_after<'a>(message: &'a str, marker: &str) -> Option<&'a str> {
    message
        .find(marker)
        .map(|index| &message[index + marker.len()..])
}

fn leading_backtick(text: &str) -> String {
    text.split('`').next().unwrap_or(text).trim().to_owned()
}

fn first_backtick(message: &str) -> Option<String> {
    let start = message.find('`')? + 1;
    let rest = &message[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_owned())
}

fn build_proposals(diagnostics: &[ApiDiagnosticData], plan: &ProbePlan) -> Vec<ApiProposalData> {
    let mut proposals = Vec::new();
    let mut seen = BTreeSet::new();
    for diagnostic in diagnostics {
        if proposals.len() >= MAX_PROPOSALS {
            break;
        }
        if let Some(name) = feature_not_found(&diagnostic.message)
            && seen.insert(format!("feature:{name}"))
        {
            proposals.push(ApiProposalData {
                kind: "feature".to_owned(),
                name,
                current_version: None,
                reason: "cargo rejected the requested feature selection; enable it explicitly after checking the manifest".to_owned(),
                requires_explicit_host_change: true,
            });
            continue;
        }
        let Some(reference) = parse_unresolved(&diagnostic.message) else {
            continue;
        };
        let Some(crate_name) = reference.crate_name.as_deref() else {
            continue;
        };
        if let Some(dependency) = plan.dependencies.get(crate_name) {
            if let Some(item) = reference.item.as_deref()
                && dependency.features.contains(item)
                && seen.insert(format!("feature:{}/{}", dependency.name, item))
            {
                proposals.push(ApiProposalData {
                    kind: "feature".to_owned(),
                    name: format!("{}/{}", dependency.name, item),
                    current_version: None,
                    reason: format!(
                        "the unresolved item '{item}' matches a declared feature of the pinned dependency '{}'; selecting that feature may expose it (no feature or manifest was changed by the probe)",
                        dependency.name
                    ),
                    requires_explicit_host_change: true,
                });
                continue;
            }
            if seen.insert(format!("dependency_version:{}", dependency.name)) {
                proposals.push(ApiProposalData {
                    kind: "dependency_version".to_owned(),
                    name: dependency.name.clone(),
                    current_version: Some(dependency.version.clone()),
                    reason: "the snippet references an API that does not resolve from the pinned dependency version; review the version before changing it (the probe did not touch the lockfile)"
                        .to_owned(),
                    requires_explicit_host_change: true,
                });
            }
            continue;
        }
        if let Some(item) = reference.item.as_deref()
            && plan.own_features.contains(item)
            && seen.insert(format!("feature:{item}"))
        {
            proposals.push(ApiProposalData {
                kind: "feature".to_owned(),
                name: item.to_owned(),
                current_version: None,
                reason:
                    "the unresolved item matches a declared feature of the anchor package; enabling it explicitly may expose the item"
                        .to_owned(),
                requires_explicit_host_change: true,
            });
            continue;
        }
        // An uppercase single identifier without a crate-shaped name is almost
        // always an unimported type rather than a missing dependency.
        let crate_shaped = crate_name.contains('_')
            || crate_name.contains('-')
            || diagnostic.message.contains("can't find crate")
            || diagnostic.message.contains("unresolved import");
        if !crate_shaped {
            continue;
        }
        if seen.insert(format!("dependency:{crate_name}")) {
            proposals.push(ApiProposalData {
                kind: "dependency".to_owned(),
                name: crate_name.to_owned(),
                current_version: None,
                reason: "the snippet uses a crate or module that is not in the pinned dependency graph; add it explicitly if intended (the probe did not change any manifest or lockfile)"
                    .to_owned(),
                requires_explicit_host_change: true,
            });
        }
    }
    proposals
}

fn feature_not_found(message: &str) -> Option<String> {
    let rest = extract_after(message, "does not have feature `")?;
    Some(leading_backtick(rest))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_suffix_patch_is_exact_and_reversible_safe() {
        let content = "pub fn a() {}\npub fn b() {}\n";
        let (old, new) = append_unique(content, "\nmod probe;\n").expect("patch");
        assert!(new.starts_with(&old));
        assert!(new.ends_with("mod probe;\n"));
        assert_eq!(content.matches(&old).count(), 1);
    }

    #[test]
    fn divergence_detection_is_conservative() {
        assert!(diverging_constructs("todo!()").contains(&"todo!".to_owned()));
        assert!(diverging_constructs("unimplemented!()").contains(&"unimplemented!".to_owned()));
        assert!(diverging_constructs("loop { }").contains(&"loop".to_owned()));
        assert!(
            diverging_constructs("let x = 1;\nreturn x;").contains(&"tail divergence".to_owned())
        );
        assert!(diverging_constructs("let x = 1;\nx + 1").is_empty());
    }

    #[test]
    fn module_declaration_scanner_ignores_comments_and_paths() {
        assert!(declares_module("mod probe;\n", "probe"));
        assert!(!declares_module("// mod probe;\nmod other;", "probe"));
        assert!(!declares_module(
            "#[path = \"elsewhere.rs\"]\nmod probe;\n",
            "probe"
        ));
    }

    #[test]
    fn unresolved_imports_map_to_bounded_proposals() {
        let message = "unresolved import `serde_json::Value`";
        let reference = parse_unresolved(message).expect("reference");
        assert_eq!(reference.crate_name.as_deref(), Some("serde_json"));
        assert_eq!(reference.item.as_deref(), Some("Value"));
    }
}
