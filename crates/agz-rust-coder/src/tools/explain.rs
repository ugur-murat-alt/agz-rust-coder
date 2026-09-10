//! Bounded explanation of compiler diagnostics for macros, trait obligations,
//! and cfg-selected items.
//!
//! Compiler and Cargo output is authority. Rust Analyzer output is advisory and
//! is only requested through an already negotiated capability. When structured
//! provenance is missing, fragments are labelled `unknown`; locations are never
//! guessed from generated code.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    diagnostics::sanitize_text,
    gate::{DiagnosticChild, DiagnosticSpan, GateDiagnostic},
    lsp::{LspError, Position, RustAnalyzerManager},
};

use super::symbol::{
    ToolError, bounded_chars, find_symbol_column, request_until, with_rust_document,
};

pub const MAX_MACRO_DEPTH: usize = 8;
pub const MAX_DIAGNOSTICS: usize = 12;
pub const MAX_EXPANSION_BYTES: usize = 16_384;
pub const MAX_EXCERPT_BYTES: usize = 4_096;
pub const MAX_DETAIL_CHARS: usize = 2_000;
pub const MAX_CFG_DEPTH: usize = 16;
pub const MAX_CFG_BYTES: usize = 4_096;
pub const MAX_CFG_ATTRS: usize = 8;
pub const MAX_CFG_SCAN_LINES: usize = 64;
pub const MAX_ANALYZER_ITEMS: usize = 16;
pub const MAX_RELATED_SOURCE: usize = 5;

/// Where one explanation fragment came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ExplainProvenance {
    /// Direct rustc/Cargo output retained by this server.
    ObservedCompiler,
    /// Rust Analyzer output; never authoritative.
    AdvisoryAnalyzer,
    /// Selection or evaluation performed by this server over recorded evidence.
    Inferred,
    /// No verified source; nothing may be claimed.
    Unknown,
}

impl ExplainProvenance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ObservedCompiler => "observedCompiler",
            Self::AdvisoryAnalyzer => "advisoryAnalyzer",
            Self::Inferred => "inferred",
            Self::Unknown => "unknown",
        }
    }
}

/// One bounded explanation fragment with explicit provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExplainFragment {
    pub provenance: ExplainProvenance,
    pub kind: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ExplainSourceRef>,
    #[serde(default)]
    pub truncated: bool,
}

impl ExplainFragment {
    pub fn new(provenance: ExplainProvenance, kind: &str, detail: impl Into<String>) -> Self {
        let detail = sanitize_text(&detail.into());
        let truncated = detail.chars().count() > MAX_DETAIL_CHARS;
        Self {
            provenance,
            kind: kind.to_owned(),
            detail: bounded_chars(&detail, MAX_DETAIL_CHARS),
            source: None,
            truncated,
        }
    }

    pub fn with_source(mut self, source: ExplainSourceRef) -> Self {
        self.source = Some(source);
        self
    }

    pub fn mark_truncated(mut self, truncated: bool) -> Self {
        self.truncated = self.truncated || truncated;
        self
    }
}

/// A concrete source location. Only compiler- or Cargo-reported coordinates
/// (or coordinates this server read directly from the workspace file) are used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExplainSourceRef {
    pub path: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macro_name: Option<String>,
}

/// A visible compiler/analyzer disagreement. The compiler side is authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExplainConflict {
    pub topic: String,
    pub compiler: String,
    pub analyzer: String,
}

/// Fixed budgets applied to one explanation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExplainBounds {
    pub max_macro_depth: u64,
    pub max_diagnostics: u64,
    pub max_expansion_bytes: u64,
    pub max_excerpt_bytes: u64,
    pub diagnostics_shown: u64,
    pub macro_depth_reached: u64,
    pub expansion_bytes: u64,
    pub truncated: bool,
}

impl Default for ExplainBounds {
    fn default() -> Self {
        Self {
            max_macro_depth: MAX_MACRO_DEPTH as u64,
            max_diagnostics: MAX_DIAGNOSTICS as u64,
            max_expansion_bytes: MAX_EXPANSION_BYTES as u64,
            max_excerpt_bytes: MAX_EXCERPT_BYTES as u64,
            diagnostics_shown: 0,
            macro_depth_reached: 0,
            expansion_bytes: 0,
            truncated: false,
        }
    }
}

/// Result of a bounded compiler-diagnostic walk for one action.
#[derive(Debug, Default)]
pub struct CompilerView {
    pub fragments: Vec<ExplainFragment>,
    pub matched: usize,
    pub shown: usize,
    pub macro_depth: usize,
    pub truncated: bool,
}

/// Qualified file match: the diagnostic file must name the anchor, and vice
/// versa, across absolute and workspace-relative spellings.
pub fn file_matches(anchor: &str, file: &str) -> bool {
    let anchor = anchor.replace('\\', "/");
    let file = file.replace('\\', "/");
    if anchor.is_empty() || file.is_empty() {
        return false;
    }
    anchor == file || file.ends_with(&format!("/{anchor}")) || anchor.ends_with(&format!("/{file}"))
}

fn span_covers_line(span: &DiagnosticSpan, line: u32) -> bool {
    let line = u64::from(line);
    let start = span.line_start;
    let end = span.line_end.max(start);
    line >= start && line <= end
}

/// Selects the diagnostics that match the anchor, in compiler order.
pub fn select_diagnostics<'a>(
    diagnostics: &'a [GateDiagnostic],
    anchor_path: &str,
    anchor_line: Option<u32>,
    diagnostic_id: Option<&str>,
) -> Vec<&'a GateDiagnostic> {
    let mut matches = Vec::new();
    for diagnostic in diagnostics {
        if let Some(id) = diagnostic_id {
            let code_matches = diagnostic
                .code
                .as_deref()
                .is_some_and(|code| code.eq_ignore_ascii_case(id));
            if !code_matches {
                continue;
            }
        }
        let mut file_matched = false;
        let mut line_matched = anchor_line.is_none();
        for span in &diagnostic.spans {
            if !file_matches(anchor_path, &span.file) {
                continue;
            }
            file_matched = true;
            if let Some(line) = anchor_line
                && span_covers_line(span, line)
            {
                line_matched = true;
                break;
            }
        }
        if file_matched && line_matched {
            matches.push(diagnostic);
        }
    }
    matches
}

fn span_ref(span: &DiagnosticSpan) -> ExplainSourceRef {
    ExplainSourceRef {
        path: span.file.clone(),
        line: (span.line_start > 0).then_some(span.line_start),
        column: (span.column_start > 0).then_some(span.column_start),
        macro_name: None,
    }
}

fn diagnostic_summary(diagnostic: &GateDiagnostic) -> String {
    let code = diagnostic
        .code
        .as_deref()
        .map_or_else(String::new, |code| format!("[{code}] "));
    format!("{code}{}: {}", diagnostic.level, diagnostic.message)
}

fn primary_span(diagnostic: &GateDiagnostic) -> Option<&DiagnosticSpan> {
    diagnostic
        .spans
        .iter()
        .find(|span| span.is_primary)
        .or_else(|| diagnostic.spans.first())
}

fn expansion_fragments(
    span: &DiagnosticSpan,
    fragments: &mut Vec<ExplainFragment>,
    depth: &mut usize,
) {
    let Some(mut expansion) = span.expansion.as_ref() else {
        return;
    };
    let mut chain = 0usize;
    while chain < MAX_MACRO_DEPTH {
        let name = expansion
            .macro_decl_name
            .clone()
            .unwrap_or_else(|| "unknown macro".to_owned());
        let invocation = span_ref(&expansion.span);
        let definition = expansion
            .definition_span
            .as_deref()
            .map(span_ref)
            .map_or_else(String::new, |definition| {
                format!(
                    "; definition at {}:{}",
                    definition.path,
                    definition.line.unwrap_or_default()
                )
            });
        let mut fragment = ExplainFragment::new(
            ExplainProvenance::ObservedCompiler,
            "macroExpansion",
            format!(
                "rustc attributes this span to macro `{name}`; invocation site {}:{}{definition}",
                invocation.path,
                invocation.line.unwrap_or_default(),
            ),
        );
        fragment = fragment.with_source(invocation);
        fragments.push(fragment);
        *depth = (*depth).max(chain + 1);
        chain += 1;
        let Some(next) = expansion.span.expansion.as_ref() else {
            break;
        };
        expansion = next;
    }
    if chain == MAX_MACRO_DEPTH {
        fragments.push(ExplainFragment::new(
            ExplainProvenance::Unknown,
            "macroExpansion",
            format!("macro expansion chain was truncated at depth {MAX_MACRO_DEPTH}; nested provenance is not claimed"),
        ));
    }
}

fn walk_child_spans(
    child: &DiagnosticChild,
    fragments: &mut Vec<ExplainFragment>,
    depth: &mut usize,
    budget: &mut usize,
    output_bytes: &mut usize,
) {
    for span in &child.spans {
        let _ = output_bytes;
        expansion_fragments(span, fragments, depth);
    }
    if !child.message.trim().is_empty() && *budget < MAX_DIAGNOSTICS {
        fragments.push(ExplainFragment::new(
            ExplainProvenance::ObservedCompiler,
            "diagnosticChild",
            format!("{}: {}", child.level, child.message),
        ));
    }
    *budget += 1;
    for nested in &child.children {
        walk_child_spans(nested, fragments, depth, budget, output_bytes);
    }
}

/// Builds macro explanations from retained rustc expansion provenance.
///
/// rustc can omit `expansion` on a span; in that case this function records an
/// explicit `unknown` fragment and does not derive a line or range.
pub fn macro_compiler_view(diagnostics: &[&GateDiagnostic]) -> CompilerView {
    let mut view = CompilerView {
        matched: diagnostics.len(),
        ..CompilerView::default()
    };
    for diagnostic in diagnostics.iter().take(MAX_DIAGNOSTICS) {
        view.shown += 1;
        let mut fragment = ExplainFragment::new(
            ExplainProvenance::ObservedCompiler,
            "diagnostic",
            diagnostic_summary(diagnostic),
        );
        if let Some(span) = primary_span(diagnostic) {
            fragment = fragment.with_source(span_ref(span));
        }
        view.fragments.push(fragment);

        let mut expansions = 0usize;
        for span in &diagnostic.spans {
            let before = view.fragments.len();
            expansion_fragments(span, &mut view.fragments, &mut view.macro_depth);
            expansions += view.fragments.len().saturating_sub(before);
        }
        let mut child_budget = 0usize;
        let mut byte_budget = 0usize;
        for child in &diagnostic.children {
            walk_child_spans(
                child,
                &mut view.fragments,
                &mut view.macro_depth,
                &mut child_budget,
                &mut byte_budget,
            );
        }
        if expansions == 0 {
            view.fragments.push(ExplainFragment::new(
                ExplainProvenance::Unknown,
                "macroExpansion",
                "rustc retained no expansion provenance for this diagnostic; no expansion line or range is claimed",
            ));
        }
    }
    view.truncated = diagnostics.len() > MAX_DIAGNOSTICS;
    view
}

/// Extracts a compiler-reported `expected ... found ...` pair from raw rustc
/// text. Returns the compiler's own words; never a synthesized type.
pub fn extract_expected_found(text: &str) -> Option<(String, String)> {
    for line in text.lines() {
        let Some(at) = line.find("expected ") else {
            continue;
        };
        let rest = &line[at + "expected ".len()..];
        let Some((expected, after)) = split_expected(rest) else {
            continue;
        };
        let found_at = after.find("found ")?;
        let found = after[found_at + "found ".len()..]
            .split(['\n', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches('.')
            .to_owned();
        if expected.is_empty() || found.is_empty() {
            continue;
        }
        return Some((sanitize_text(&expected), sanitize_text(&found)));
    }
    None
}

fn split_expected(rest: &str) -> Option<(String, &str)> {
    let mark = [", found ", "; found ", ", but found "]
        .iter()
        .filter_map(|mark| rest.find(mark).map(|index| (index, *mark)))
        .min_by_key(|(index, _)| *index)?;
    let expected = rest[..mark.0].trim().to_owned();
    if expected.is_empty() || expected.starts_with("one of") {
        return None;
    }
    Some((expected, &rest[mark.0..]))
}

fn diagnostic_text(diagnostic: &GateDiagnostic) -> String {
    let mut text = String::new();
    text.push_str(&diagnostic.message);
    text.push('\n');
    for child in &diagnostic.children {
        text.push_str(&child.message);
        text.push('\n');
        collect_child_text(child, &mut text);
    }
    if let Some(rendered) = &diagnostic.rendered {
        text.push_str(rendered);
    }
    text
}

fn collect_child_text(child: &DiagnosticChild, text: &mut String) {
    for nested in &child.children {
        text.push_str(&nested.message);
        text.push('\n');
        collect_child_text(nested, text);
    }
}

pub fn trait_hint(diagnostics: &[&GateDiagnostic]) -> Option<String> {
    for diagnostic in diagnostics {
        let text = diagnostic_text(diagnostic);
        if let Some(at) = text.find("trait bound `") {
            let rest = &text[at + "trait bound `".len()..];
            if let Some(bound) = rest.split('`').next() {
                let name = bound.rsplit([':', ' ']).next().unwrap_or(bound).trim();
                if !name.is_empty() {
                    return Some(name.to_owned());
                }
            }
        }
        if let Some(at) = text.find("trait `") {
            let rest = &text[at + "trait `".len()..];
            let name = rest.split('`').next().unwrap_or("").trim();
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

fn is_failed_bound(diagnostic: &GateDiagnostic) -> bool {
    if diagnostic.code.as_deref() == Some("E0277") {
        return true;
    }
    let message = &diagnostic.message;
    message.contains("trait bound")
        || message.contains("doesn't implement")
        || message.contains("is not implemented")
        || message.contains("the trait `")
}

/// Builds trait explanations: compiler-reported diagnostics, expected/found
/// text, and a bounded source selection of related `impl`/`where` clauses.
pub fn trait_compiler_view(
    diagnostics: &[&GateDiagnostic],
    anchor_path: &str,
    anchor_line: u32,
    source: Option<&str>,
    trait_name: Option<&str>,
) -> CompilerView {
    let mut view = CompilerView {
        matched: diagnostics.len(),
        ..CompilerView::default()
    };
    for diagnostic in diagnostics.iter().take(MAX_DIAGNOSTICS) {
        view.shown += 1;
        let mut fragment = ExplainFragment::new(
            ExplainProvenance::ObservedCompiler,
            "diagnostic",
            diagnostic_summary(diagnostic),
        );
        if let Some(span) = primary_span(diagnostic) {
            fragment = fragment.with_source(span_ref(span));
        }
        view.fragments.push(fragment);

        let text = diagnostic_text(diagnostic);
        if let Some((expected, found)) = extract_expected_found(&text) {
            view.fragments.push(ExplainFragment::new(
                ExplainProvenance::ObservedCompiler,
                "expectedFound",
                format!("compiler reported expected {expected}, found {found}"),
            ));
        } else {
            view.fragments.push(ExplainFragment::new(
                ExplainProvenance::Unknown,
                "expectedFound",
                "compiler message did not contain a parseable `expected ... found ...` pair; no types are attributed",
            ));
        }
        if is_failed_bound(diagnostic) {
            view.fragments.push(ExplainFragment::new(
                ExplainProvenance::ObservedCompiler,
                "failedBound",
                diagnostic.message.clone(),
            ));
        }
        for child in &diagnostic.children {
            let mut child_budget = 0usize;
            let mut byte_budget = 0usize;
            walk_child_spans(
                child,
                &mut view.fragments,
                &mut view.macro_depth,
                &mut child_budget,
                &mut byte_budget,
            );
        }
    }
    if let Some(source) = source {
        view.fragments.extend(related_source_fragments(
            anchor_path,
            source,
            anchor_line,
            trait_name,
        ));
    }
    view.truncated = diagnostics.len() > MAX_DIAGNOSTICS;
    view
}

/// Selects `impl` and `where` lines near the anchor as bounded, labelled
/// source evidence. The selection is a heuristic and is marked `inferred`.
pub fn related_source_fragments(
    path: &str,
    source: &str,
    anchor_line: u32,
    trait_name: Option<&str>,
) -> Vec<ExplainFragment> {
    let lines = source.lines().collect::<Vec<_>>();
    let anchor = (anchor_line as usize).saturating_sub(1).min(lines.len());
    let from = anchor.saturating_sub(200);
    let to = lines.len().min(anchor.saturating_add(200));
    let mut candidates = Vec::new();
    for (index, line) in lines.iter().enumerate().take(to).skip(from) {
        let trimmed = line.trim_start();
        let kind = if trimmed.starts_with("impl ")
            || trimmed.starts_with("impl<")
            || trimmed.starts_with("unsafe impl")
        {
            Some("candidateImpl")
        } else if trimmed.starts_with("where ") || trimmed.contains(" where ") {
            Some("whereClause")
        } else {
            None
        };
        if let Some(kind) = kind {
            candidates.push((index + 1, kind, *line));
        }
    }
    if let Some(name) = trait_name {
        candidates.sort_by_key(|(_, _, line)| !line.contains(name));
    }
    candidates.truncate(MAX_RELATED_SOURCE);
    candidates
        .into_iter()
        .map(|(line, kind, text)| {
            ExplainFragment::new(
                ExplainProvenance::Inferred,
                kind,
                format!(
                    "source selection heuristic (not a compiler obligation): {}",
                    text.trim()
                ),
            )
            .with_source(ExplainSourceRef {
                path: path.to_owned(),
                line: Some(line as u64),
                column: None,
                macro_name: None,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// cfg
// ---------------------------------------------------------------------------

/// A parsed `cfg(...)` predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgExpr {
    All(Vec<CfgExpr>),
    Any(Vec<CfgExpr>),
    Not(Box<CfgExpr>),
    /// `key` or `key = "value"`.
    Flag(String, Option<String>),
}

/// Three-valued evaluation of one cfg predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfgVerdict {
    Enabled,
    Disabled,
    Unknown,
}

impl CfgVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Unknown => "unknown",
        }
    }
}

impl std::ops::Not for CfgVerdict {
    type Output = Self;

    fn not(self) -> Self {
        match self {
            Self::Enabled => Self::Disabled,
            Self::Disabled => Self::Enabled,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// Feature state for the recorded selection.
#[derive(Debug, Clone, Default)]
pub struct FeatureSelection {
    /// Features resolved by Cargo metadata for the recorded selection.
    pub recorded: BTreeSet<String>,
    /// Features this server can attribute to the requested configuration.
    pub effective: Option<BTreeSet<String>>,
    /// Requested feature names that no workspace package declares.
    pub unknown: BTreeSet<String>,
    pub note: String,
}

impl FeatureSelection {
    pub fn is_enabled(&self, feature: &str) -> CfgVerdict {
        if self.unknown.contains(feature) {
            return CfgVerdict::Unknown;
        }
        match &self.effective {
            Some(effective) if effective.contains(feature) => CfgVerdict::Enabled,
            Some(_) => CfgVerdict::Disabled,
            None if self.recorded.contains(feature) => CfgVerdict::Enabled,
            None => CfgVerdict::Unknown,
        }
    }
}

/// Builds the effective feature set for the recorded selection plus the
/// requested configuration. Enablement propagation is a bounded closure over
/// Cargo's declared feature map, so it is an inference and never compiler
/// verification.
pub fn feature_selection(
    recorded: BTreeSet<String>,
    declared: &BTreeMap<String, Vec<String>>,
    requested: &[String],
    all_features: bool,
    no_default_features: bool,
) -> FeatureSelection {
    let mut selection = FeatureSelection {
        recorded,
        effective: None,
        unknown: BTreeSet::new(),
        note: String::new(),
    };
    let mut declared_names = BTreeSet::new();
    for (name, enables) in declared {
        declared_names.insert(name.clone());
        for enable in enables {
            declared_names.insert(enable.clone());
        }
    }
    for feature in requested {
        if !declared_names.contains(feature) {
            selection.unknown.insert(feature.clone());
        }
    }
    if selection.unknown.is_empty() {
        for (name, enables) in declared {
            if enables
                .iter()
                .any(|enable| selection.unknown.contains(enable))
            {
                selection.unknown.insert(name.clone());
            }
        }
    }

    let mut effective = BTreeSet::new();
    if all_features {
        effective.extend(declared_names.iter().cloned());
        selection.note =
            "allFeatures requested: every declared feature of the workspace packages is treated as enabled"
                .to_owned();
    } else if requested.is_empty() && !no_default_features {
        effective.extend(selection.recorded.iter().cloned());
        selection.note =
            "feature state comes from Cargo metadata resolve for the recorded default selection"
                .to_owned();
    } else {
        let mut queue = requested.to_vec();
        while let Some(feature) = queue.pop() {
            if !effective.insert(feature.clone()) {
                continue;
            }
            if let Some(enables) = declared.get(&feature) {
                for enable in enables {
                    if !effective.contains(enable) {
                        queue.push(enable.clone());
                    }
                }
            }
        }
        if !no_default_features {
            effective.extend(selection.recorded.iter().cloned());
        }
        selection.note = format!(
            "feature state is inferred from the requested configuration (features: {}, allFeatures: {all_features}, noDefaultFeatures: {no_default_features}); it is not compiler-verified and dependency-activated features are not modelled",
            if requested.is_empty() {
                "none".to_owned()
            } else {
                requested.join(", ")
            }
        );
    }
    selection.effective = Some(effective);
    selection.note.push_str(
        "; target/compiler predicates stay unknown because no compiler cfg probe was executed",
    );
    selection
}

fn eval_flag(
    key: &str,
    value: Option<&str>,
    features: &FeatureSelection,
    target_cfgs: Option<&BTreeSet<String>>,
) -> CfgVerdict {
    match (key, value) {
        ("feature", Some(feature)) => features.is_enabled(feature),
        (
            "target_os"
            | "target_arch"
            | "target_family"
            | "target_env"
            | "target_vendor"
            | "target_pointer_width"
            | "target_endian"
            | "target_abi"
            | "target_feature",
            Some(value),
        ) => match target_cfgs {
            Some(target_cfgs) => {
                if target_cfgs.contains(&format!("{key}=\"{value}\"")) {
                    CfgVerdict::Enabled
                } else {
                    CfgVerdict::Disabled
                }
            }
            None => CfgVerdict::Unknown,
        },
        ("unix" | "windows", None) => match target_cfgs {
            Some(target_cfgs) => {
                if target_cfgs.contains(key) {
                    CfgVerdict::Enabled
                } else {
                    CfgVerdict::Disabled
                }
            }
            None => CfgVerdict::Unknown,
        },
        ("test" | "debug_assertions" | "proc_macro" | "doctest", None) => CfgVerdict::Unknown,
        _ => CfgVerdict::Unknown,
    }
}

/// Evaluates a parsed cfg predicate against the recorded feature selection.
/// Target predicates remain `unknown` unless a compiler cfg probe is supplied.
pub fn evaluate_cfg(
    expr: &CfgExpr,
    features: &FeatureSelection,
    target_cfgs: Option<&BTreeSet<String>>,
) -> CfgVerdict {
    match expr {
        CfgExpr::All(items) => {
            let mut verdict = CfgVerdict::Enabled;
            for item in items {
                match evaluate_cfg(item, features, target_cfgs) {
                    CfgVerdict::Disabled => return CfgVerdict::Disabled,
                    CfgVerdict::Unknown => verdict = CfgVerdict::Unknown,
                    CfgVerdict::Enabled => {}
                }
            }
            verdict
        }
        CfgExpr::Any(items) => {
            let mut verdict = CfgVerdict::Disabled;
            for item in items {
                match evaluate_cfg(item, features, target_cfgs) {
                    CfgVerdict::Enabled => return CfgVerdict::Enabled,
                    CfgVerdict::Unknown => verdict = CfgVerdict::Unknown,
                    CfgVerdict::Disabled => {}
                }
            }
            verdict
        }
        CfgExpr::Not(inner) => !evaluate_cfg(inner, features, target_cfgs),
        CfgExpr::Flag(key, value) => eval_flag(key, value.as_deref(), features, target_cfgs),
    }
}

/// Parses one cfg predicate with bounded depth and bytes.
pub fn parse_cfg(input: &str) -> Result<CfgExpr, String> {
    if input.len() > MAX_CFG_BYTES {
        return Err(format!("cfg expression exceeds {MAX_CFG_BYTES} bytes"));
    }
    let mut parser = CfgParser {
        input: input.as_bytes(),
        position: 0,
        depth: 0,
    };
    let expr = parser.parse_expr()?;
    parser.skip_whitespace();
    if parser.position != parser.input.len() {
        return Err("unexpected trailing input in cfg expression".to_owned());
    }
    Ok(expr)
}

struct CfgParser<'a> {
    input: &'a [u8],
    position: usize,
    depth: usize,
}

impl CfgParser<'_> {
    fn skip_whitespace(&mut self) {
        while self
            .input
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }

    fn parse_expr(&mut self) -> Result<CfgExpr, String> {
        if self.depth >= MAX_CFG_DEPTH {
            return Err(format!("cfg expression exceeds depth {MAX_CFG_DEPTH}"));
        }
        self.skip_whitespace();
        let name = self.parse_ident()?;
        self.skip_whitespace();
        if self.input.get(self.position) == Some(&b'(') {
            self.position += 1;
            self.depth += 1;
            let items = match name.as_str() {
                "all" | "any" => {
                    let mut items = Vec::new();
                    loop {
                        self.skip_whitespace();
                        if self.input.get(self.position) == Some(&b')') {
                            self.position += 1;
                            break;
                        }
                        items.push(self.parse_expr()?);
                        self.skip_whitespace();
                        match self.input.get(self.position) {
                            Some(b',') => self.position += 1,
                            Some(b')') => {
                                self.position += 1;
                                break;
                            }
                            _ => return Err("expected `,` or `)` in cfg list".to_owned()),
                        }
                    }
                    items
                }
                "not" => {
                    let item = self.parse_expr()?;
                    self.skip_whitespace();
                    if self.input.get(self.position) != Some(&b')') {
                        return Err("expected `)` after not(...)".to_owned());
                    }
                    self.position += 1;
                    self.depth -= 1;
                    return Ok(CfgExpr::Not(Box::new(item)));
                }
                other => return Err(format!("unsupported cfg function `{other}`")),
            };
            self.depth -= 1;
            return Ok(if name == "all" {
                CfgExpr::All(items)
            } else {
                CfgExpr::Any(items)
            });
        }
        if self.input.get(self.position) != Some(&b'=') {
            return Ok(CfgExpr::Flag(name, None));
        }
        self.position += 1;
        self.skip_whitespace();
        let value = self.parse_string()?;
        Ok(CfgExpr::Flag(name, Some(value)))
    }

    fn parse_ident(&mut self) -> Result<String, String> {
        let start = self.position;
        while self
            .input
            .get(self.position)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
        {
            self.position += 1;
        }
        if self.position == start {
            return Err("expected cfg identifier".to_owned());
        }
        String::from_utf8(self.input[start..self.position].to_vec())
            .map_err(|_| "cfg identifier is not UTF-8".to_owned())
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.input.get(self.position) != Some(&b'"') {
            return Err("expected quoted cfg value".to_owned());
        }
        self.position += 1;
        let mut value = String::new();
        loop {
            let Some(byte) = self.input.get(self.position).copied() else {
                return Err("unterminated cfg string".to_owned());
            };
            self.position += 1;
            match byte {
                b'"' => return Ok(value),
                b'\\' => {
                    let Some(escaped) = self.input.get(self.position).copied() else {
                        return Err("unterminated cfg escape".to_owned());
                    };
                    self.position += 1;
                    value.push(escaped as char);
                }
                byte => value.push(byte as char),
            }
            if value.len() > MAX_CFG_BYTES {
                return Err("cfg value is too long".to_owned());
            }
        }
    }
}

/// One `#[cfg(...)]` attribute found at or immediately above the anchor.
#[derive(Debug, Clone)]
pub struct CfgAttribute {
    pub line: u64,
    pub condition: String,
    pub parsed: Result<CfgExpr, String>,
    pub cfg_attr: bool,
}

/// Scans backwards from the anchor over attribute lines and extracts the
/// bounded `#[cfg(...)]` conditions that apply to the item.
pub fn find_cfg_attributes(source: &str, anchor_line: u32) -> Vec<CfgAttribute> {
    let lines = source.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return Vec::new();
    }
    let anchor = (anchor_line as usize)
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1));
    let mut attributes = Vec::new();
    let mut index = anchor;
    let mut skipped_item = false;
    let mut scanned = 0usize;
    loop {
        scanned += 1;
        if scanned > MAX_CFG_SCAN_LINES || attributes.len() >= MAX_CFG_ATTRS {
            break;
        }
        let trimmed = lines[index].trim_start();
        let is_attribute = trimmed.starts_with("#[") || trimmed.starts_with("#![");
        let cfg_attr = trimmed.contains("#[cfg_attr") || trimmed.contains("#![cfg_attr");
        if is_attribute {
            if let Some(condition) = extract_cfg_condition(trimmed) {
                attributes.push(CfgAttribute {
                    line: index as u64 + 1,
                    parsed: parse_cfg(&condition),
                    condition,
                    cfg_attr,
                });
            } else if cfg_attr {
                attributes.push(CfgAttribute {
                    line: index as u64 + 1,
                    parsed: Err("cfg_attr could not be evaluated".to_owned()),
                    condition: "#[cfg_attr(...)]".to_owned(),
                    cfg_attr: true,
                });
            } else if trimmed.contains("cfg(") {
                attributes.push(CfgAttribute {
                    line: index as u64 + 1,
                    parsed: Err("multi-line cfg attribute is not parsed".to_owned()),
                    condition: bounded_chars(trimmed, MAX_CFG_BYTES / 2),
                    cfg_attr,
                });
            }
        } else if trimmed.is_empty() || trimmed.starts_with("//") {
            if !attributes.is_empty() {
                break;
            }
        } else if attributes.is_empty() && !skipped_item {
            // The anchor usually names the item itself; its attributes are the
            // lines immediately above, so skip this one code line once.
            skipped_item = true;
        } else {
            break;
        }
        if index == 0 {
            break;
        }
        index -= 1;
    }
    attributes
}

fn extract_cfg_condition(line: &str) -> Option<String> {
    let marker = line.find("#[cfg(").map(|at| (at, "#[cfg("))?;
    let after = &line[marker.0 + marker.1.len()..];
    let end = balanced_end(after)?;
    Some(after[..end].trim().to_owned())
}

fn balanced_end(input: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn source_ref_for_line(path: &str, line: u64) -> ExplainSourceRef {
    ExplainSourceRef {
        path: path.to_owned(),
        line: Some(line),
        column: None,
        macro_name: None,
    }
}

/// Builds cfg explanation fragments for the recorded feature selection.
pub fn cfg_view(
    path: &str,
    anchor_line: u32,
    source: &str,
    features: &FeatureSelection,
    target_cfgs: Option<&BTreeSet<String>>,
) -> CompilerView {
    let mut view = CompilerView::default();
    let attributes = find_cfg_attributes(source, anchor_line);
    if attributes.is_empty() {
        view.fragments.push(ExplainFragment::new(
            ExplainProvenance::Unknown,
            "cfgCondition",
            "no `#[cfg(...)]` attribute was found at or immediately above the anchor; enablement is unknown",
        ));
    }
    for attribute in &attributes {
        let (verdict, detail) = match &attribute.parsed {
            Ok(expr) => {
                let verdict = evaluate_cfg(expr, features, target_cfgs);
                let detail = match verdict {
                    CfgVerdict::Enabled => format!(
                        "recorded selection enables `#[cfg({})]`",
                        attribute.condition
                    ),
                    CfgVerdict::Disabled => format!(
                        "recorded selection disables `#[cfg({})]`",
                        attribute.condition
                    ),
                    CfgVerdict::Unknown => format!(
                        "`#[cfg({})]` could not be decided from the recorded feature/target selection",
                        attribute.condition
                    ),
                };
                (verdict, detail)
            }
            Err(error) => (
                CfgVerdict::Unknown,
                format!(
                    "`#[cfg({})]` was not parsed ({error}); enablement is unknown",
                    attribute.condition
                ),
            ),
        };
        view.fragments.push(
            ExplainFragment::new(
                if verdict == CfgVerdict::Unknown {
                    ExplainProvenance::Unknown
                } else {
                    ExplainProvenance::Inferred
                },
                "cfgCondition",
                detail,
            )
            .with_source(source_ref_for_line(path, attribute.line)),
        );
        if attribute.cfg_attr {
            view.fragments.push(ExplainFragment::new(
                ExplainProvenance::Unknown,
                "cfgCondition",
                "a `cfg_attr` attribute is present; its conditional attributes are not evaluated",
            ));
        }
    }
    let mut feature_detail = format!(
        "recorded features: {}",
        if features.recorded.is_empty() {
            "none".to_owned()
        } else {
            features
                .recorded
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    if !features.unknown.is_empty() {
        feature_detail.push_str(&format!(
            "; requested features not declared by any workspace package: {}",
            features
                .unknown
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    feature_detail.push('\n');
    feature_detail.push_str(&features.note);
    view.fragments.push(ExplainFragment::new(
        ExplainProvenance::ObservedCompiler,
        "featureState",
        feature_detail,
    ));
    view.matched = attributes.len();
    view.shown = attributes.len();
    view.truncated = attributes.len() >= MAX_CFG_ATTRS;
    view
}

// ---------------------------------------------------------------------------
// Rust Analyzer advisory evidence
// ---------------------------------------------------------------------------

/// Availability of one Rust Analyzer extension.
#[derive(Debug)]
pub enum RaStatus<T> {
    Available(T),
    Unsupported(String),
    Unavailable(String),
}

impl<T> RaStatus<T> {
    pub fn unsupported_reason(&self) -> Option<&str> {
        match self {
            Self::Unsupported(reason) | Self::Unavailable(reason) => Some(reason),
            Self::Available(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaMacroExpansion {
    pub name: Option<String>,
    pub expansion: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RaObligations {
    pub items: Vec<String>,
    pub truncated: bool,
}

/// Parses `rust-analyzer/expandMacro`. Unknown shapes are rejected so no
/// structured payload is fabricated from debug text.
pub fn parse_expand_macro(value: &Value) -> Result<RaMacroExpansion, String> {
    if let Some(expansion) = value.as_str() {
        return Ok(RaMacroExpansion {
            name: None,
            expansion: bounded_chars(&sanitize_text(expansion), MAX_EXPANSION_BYTES),
            truncated: expansion.len() > MAX_EXPANSION_BYTES,
        });
    }
    let object = value
        .as_object()
        .ok_or_else(|| "rust-analyzer expandMacro returned an unrecognized schema".to_owned())?;
    let expansion = object
        .get("expansion")
        .and_then(Value::as_str)
        .ok_or_else(|| "rust-analyzer expandMacro response has no expansion text".to_owned())?;
    Ok(RaMacroExpansion {
        name: object
            .get("name")
            .and_then(Value::as_str)
            .map(sanitize_text),
        expansion: bounded_chars(&sanitize_text(expansion), MAX_EXPANSION_BYTES),
        truncated: expansion.len() > MAX_EXPANSION_BYTES,
    })
}

/// Parses `rust-analyzer/getFailedObligations` tolerantly. An unrecognized
/// shape is an error; no obligations are invented from arbitrary text.
pub fn parse_failed_obligations(value: &Value) -> Result<RaObligations, String> {
    let array = value
        .as_array()
        .or_else(|| value.get("failedObligations").and_then(Value::as_array))
        .or_else(|| value.get("obligations").and_then(Value::as_array))
        .ok_or_else(|| {
            "rust-analyzer failed-obligations response has an unrecognized schema".to_owned()
        })?;
    let mut obligations = RaObligations {
        items: Vec::new(),
        truncated: array.len() > MAX_ANALYZER_ITEMS,
    };
    for item in array.iter().take(MAX_ANALYZER_ITEMS) {
        let text = item.as_str().map(str::to_owned).or_else(|| {
            item.as_object().and_then(|object| {
                ["obligation", "message", "label", "reason"]
                    .iter()
                    .find_map(|key| object.get(*key).and_then(Value::as_str).map(str::to_owned))
                    .or_else(|| serde_json::to_string(item).ok())
            })
        });
        if let Some(text) = text {
            obligations.items.push(bounded_chars(
                &sanitize_text(&text),
                MAX_EXPANSION_BYTES / 4,
            ));
        }
    }
    Ok(obligations)
}

fn request_position(text: &str, symbol: Option<&str>, line: u32) -> Position {
    let line = line.saturating_sub(1);
    let line_text = text.lines().nth(line as usize).unwrap_or("");
    let character = symbol
        .and_then(|symbol| find_symbol_column(line_text, symbol))
        .unwrap_or(0);
    Position { line, character }
}

/// Requests one bounded Rust Analyzer macro expansion at the anchor.
pub async fn expand_macro(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: Option<String>,
    line: u32,
    timeout: Duration,
) -> RaStatus<RaMacroExpansion> {
    match manager
        .supports_internal(root, "rust-analyzer/expandMacro")
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return RaStatus::Unsupported(
                "rust-analyzer/expandMacro is not negotiated by the running rust-analyzer instance"
                    .to_owned(),
            );
        }
        Err(error) => {
            return RaStatus::Unavailable(format!(
                "rust-analyzer/expandMacro capability check failed: {error}"
            ));
        }
    }
    let result = with_rust_document(manager, root, relative_path, move |client, uri, text| {
        Box::pin(async move {
            let position = request_position(&text, symbol.as_deref(), line);
            let value = request_until(
                client.as_ref(),
                "rust-analyzer/expandMacro",
                serde_json::json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": position.line, "character": position.character}
                }),
                timeout,
                |value| !value.is_null(),
            )
            .await?;
            Ok(parse_expand_macro(&value))
        })
    })
    .await;
    match result {
        Ok(Ok(expansion)) => RaStatus::Available(expansion),
        Ok(Err(reason)) => RaStatus::Unavailable(reason),
        Err(ToolError::Lsp(LspError::NotFound(reason))) => RaStatus::Unavailable(reason),
        Err(error) => RaStatus::Unavailable(error.to_string()),
    }
}

/// Requests bounded advisory failed obligations at the anchor.
pub async fn failed_obligations(
    manager: &RustAnalyzerManager,
    root: &Path,
    relative_path: &Path,
    symbol: Option<String>,
    line: u32,
    timeout: Duration,
) -> RaStatus<RaObligations> {
    match manager
        .supports_internal(root, "rust-analyzer/getFailedObligations")
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return RaStatus::Unsupported(
                "rust-analyzer/getFailedObligations is not negotiated by the running rust-analyzer instance"
                    .to_owned(),
            );
        }
        Err(error) => {
            return RaStatus::Unavailable(format!(
                "rust-analyzer/getFailedObligations capability check failed: {error}"
            ));
        }
    }
    let result = with_rust_document(manager, root, relative_path, move |client, uri, text| {
        Box::pin(async move {
            let position = request_position(&text, symbol.as_deref(), line);
            let value = request_until(
                client.as_ref(),
                "rust-analyzer/getFailedObligations",
                serde_json::json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": position.line, "character": position.character}
                }),
                timeout,
                |value| !value.is_null(),
            )
            .await?;
            Ok(parse_failed_obligations(&value))
        })
    })
    .await;
    match result {
        Ok(Ok(obligations)) => RaStatus::Available(obligations),
        Ok(Err(reason)) => RaStatus::Unavailable(reason),
        Err(error) => RaStatus::Unavailable(error.to_string()),
    }
}

/// Makes a compiler/analyzer disagreement visible. The compiler side remains
/// authoritative and is never replaced by the advisory result.
pub fn obligation_conflicts(
    compiler_failed: bool,
    analyzer: &RaObligations,
    analyzer_reason: Option<&str>,
) -> Vec<ExplainConflict> {
    let analyzer_empty = analyzer.items.is_empty();
    let mut conflicts = Vec::new();
    if compiler_failed && analyzer_empty {
        conflicts.push(ExplainConflict {
            topic: "failed trait obligations".to_owned(),
            compiler:
                "rustc reported a failed trait obligation for the anchor; compiler output remains authoritative"
                    .to_owned(),
            analyzer: analyzer_reason.map_or_else(
                || "rust-analyzer reported no failed obligations at the anchor".to_owned(),
                |reason| format!("rust-analyzer advisory result unavailable ({reason})"),
            ),
        });
    } else if !compiler_failed && !analyzer_empty {
        conflicts.push(ExplainConflict {
            topic: "failed trait obligations".to_owned(),
            compiler: "rustc reported no failed trait obligation at the anchor".to_owned(),
            analyzer: format!(
                "rust-analyzer reported {} failed obligation(s) (advisory): {}",
                analyzer.items.len(),
                analyzer.items.join("; ")
            ),
        });
    }
    conflicts
}

/// SHA-256 binding for exact source bytes used in an explanation.
pub fn source_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Resolves an anchor symbol to a 1-based line using the exact workspace file.
pub fn resolve_anchor_line(source: &str, symbol: Option<&str>, line: Option<u32>) -> Option<u32> {
    if let Some(line) = line {
        return Some(line);
    }
    let symbol = symbol?;
    for (index, text) in source.lines().enumerate() {
        if find_symbol_column(text, symbol).is_some() {
            return u32::try_from(index + 1).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::MacroExpansion;

    fn span(file: &str, line: u64, expansion: Option<MacroExpansion>) -> DiagnosticSpan {
        DiagnosticSpan {
            file: file.to_owned(),
            byte_start: None,
            byte_end: None,
            line_start: line,
            line_end: line,
            column_start: 1,
            column_end: 5,
            is_primary: true,
            label: Some("label".to_owned()),
            suggested_replacement: None,
            suggestion_applicability: None,
            expansion,
        }
    }

    fn diagnostic(code: &str, message: &str, spans: Vec<DiagnosticSpan>) -> GateDiagnostic {
        GateDiagnostic {
            code: Some(code.to_owned()),
            level: "error".to_owned(),
            file: spans.first().map(|span| span.file.clone()),
            line: spans.first().map(|span| span.line_start),
            message: message.to_owned(),
            rendered: None,
            spans,
            children: Vec::new(),
            suggestions: Vec::new(),
        }
    }

    #[test]
    fn cfg_parser_evaluates_three_valued_logic() {
        let mut declared = BTreeMap::new();
        declared.insert("default".to_owned(), vec!["std".to_owned()]);
        declared.insert("std".to_owned(), Vec::new());
        declared.insert("extra".to_owned(), Vec::new());
        let recorded = BTreeSet::from(["default".to_owned(), "std".to_owned()]);
        let features = feature_selection(recorded, &declared, &[], false, false);

        let all = parse_cfg(r#"all(feature = "std", not(feature = "extra"))"#).expect("parse all");
        assert_eq!(evaluate_cfg(&all, &features, None), CfgVerdict::Enabled);
        let any = parse_cfg(r#"any(feature = "extra", unix)"#).expect("parse any");
        assert_eq!(evaluate_cfg(&any, &features, None), CfgVerdict::Unknown);
        let target = parse_cfg(r#"target_os = "linux""#).expect("parse target");
        assert_eq!(evaluate_cfg(&target, &features, None), CfgVerdict::Unknown);
    }

    #[test]
    fn cfg_view_reports_feature_gate_for_recorded_selection() {
        let mut declared = BTreeMap::new();
        declared.insert("default".to_owned(), Vec::new());
        declared.insert("extra".to_owned(), Vec::new());
        let features = feature_selection(
            BTreeSet::from(["default".to_owned()]),
            &declared,
            &[],
            false,
            false,
        );
        let source = "#[cfg(feature = \"extra\")]\npub fn gated() {}\n";
        let view = cfg_view("src/lib.rs", 2, source, &features, None);
        assert!(view.fragments.iter().any(|fragment| {
            fragment.kind == "cfgCondition"
                && fragment.provenance == ExplainProvenance::Inferred
                && fragment.detail.contains("disables")
        }));
    }

    #[test]
    fn cfg_view_refuses_to_decide_unexecuted_target_configuration() {
        let features = feature_selection(BTreeSet::new(), &BTreeMap::new(), &[], false, false);
        let source = "#[cfg(target_os = \"windows\")]\npub fn gated() {}\n";
        let view = cfg_view("src/lib.rs", 2, source, &features, None);
        assert!(view.fragments.iter().any(|fragment| {
            fragment.kind == "cfgCondition" && fragment.provenance == ExplainProvenance::Unknown
        }));
    }

    #[test]
    fn macro_view_keeps_compiler_expansion_provenance() {
        let expansion = MacroExpansion {
            macro_decl_name: Some("inner".to_owned()),
            span: Box::new(span("src/lib.rs", 1, None)),
            definition_span: Some(Box::new(span("src/lib.rs", 3, None))),
        };
        let diagnostic = diagnostic(
            "E0308",
            "mismatched types",
            vec![span("src/lib.rs", 2, Some(expansion))],
        );
        let view = macro_compiler_view(&[&diagnostic]);
        assert!(view.fragments.iter().any(|fragment| {
            fragment.kind == "macroExpansion"
                && fragment.provenance == ExplainProvenance::ObservedCompiler
                && fragment.detail.contains("`inner`")
        }));
        assert!(!view.fragments.iter().any(|fragment| {
            fragment.kind == "macroExpansion" && fragment.provenance == ExplainProvenance::Unknown
        }));
    }

    #[test]
    fn macro_view_never_invents_missing_expansion_locations() {
        let diagnostic = diagnostic(
            "E0308",
            "mismatched types",
            vec![span("src/lib.rs", 2, None)],
        );
        let view = macro_compiler_view(&[&diagnostic]);
        let unknown = view
            .fragments
            .iter()
            .find(|fragment| {
                fragment.kind == "macroExpansion"
                    && fragment.provenance == ExplainProvenance::Unknown
            })
            .expect("missing expansion must be unknown");
        assert!(unknown.source.is_none());
    }

    #[test]
    fn extract_expected_found_reads_compiler_text() {
        let text = "mismatched types\nexpected `i32`, found `&str`\nnote: ...";
        let (expected, found) = extract_expected_found(text).expect("expected/found");
        assert_eq!(expected, "`i32`");
        assert_eq!(found, "`&str`");
        assert!(extract_expected_found("expected one of: foo, bar").is_none());
    }

    #[test]
    fn obligation_conflict_is_visible_when_analyzer_disagrees() {
        let conflicts = obligation_conflicts(true, &RaObligations::default(), None);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].compiler.contains("authoritative"));
        assert!(conflicts[0].analyzer.contains("no failed obligations"));

        let conflicts = obligation_conflicts(
            true,
            &RaObligations {
                items: vec!["Foo: Bar".to_owned()],
                truncated: false,
            },
            None,
        );
        assert!(conflicts.is_empty());
    }

    #[test]
    fn analyzer_parsers_reject_unknown_shapes() {
        assert!(parse_expand_macro(&serde_json::json!({"unexpected": true})).is_err());
        assert!(parse_failed_obligations(&serde_json::json!({"unexpected": true})).is_err());
        let expansion = parse_expand_macro(&serde_json::json!({
            "name": "mock",
            "expansion": "pub fn f() {}"
        }))
        .expect("recognized expansion");
        assert_eq!(expansion.name.as_deref(), Some("mock"));
        let obligations = parse_failed_obligations(&serde_json::json!(["Foo: Bar"]))
            .expect("recognized obligations");
        assert_eq!(obligations.items, vec!["Foo: Bar".to_owned()]);
    }

    #[test]
    fn diagnostics_are_selected_near_the_anchor() {
        let matching = diagnostic(
            "E0308",
            "mismatched types",
            vec![span("src/lib.rs", 4, None)],
        );
        let other = diagnostic("E0277", "trait bound", vec![span("src/other.rs", 9, None)]);
        let diagnostics = [matching, other];
        let selected = select_diagnostics(&diagnostics, "lib.rs", Some(4), None);
        assert_eq!(selected.len(), 1);
        let selected = select_diagnostics(&[], "lib.rs", Some(4), None);
        assert!(selected.is_empty());
    }

    #[test]
    fn cfg_attribute_scanner_is_bounded_and_reads_the_condition() {
        let source = "#[cfg(all(feature = \"a\", not(feature = \"b\")))]\npub fn gated() {}\n";
        let attributes = find_cfg_attributes(source, 2);
        assert_eq!(attributes.len(), 1);
        assert_eq!(
            attributes[0].condition,
            "all(feature = \"a\", not(feature = \"b\"))"
        );
    }

    #[test]
    fn resolve_anchor_line_finds_a_symbol_occurrence() {
        let source = "pub fn first() {}\npub fn second() {}\n";
        assert_eq!(resolve_anchor_line(source, Some("second"), None), Some(2));
        assert_eq!(resolve_anchor_line(source, Some("missing"), None), None);
        assert_eq!(resolve_anchor_line(source, None, Some(9)), Some(9));
    }
}
