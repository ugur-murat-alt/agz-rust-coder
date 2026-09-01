use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_RENDER_BYTES: usize = 49_152;
pub const MAX_PATCH_CONTEXT_BYTES: usize = 8_000;
pub const MAX_SOURCE_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticLevel {
    Error,
    Warning,
}

impl DiagnosticLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

impl fmt::Display for DiagnosticLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionApplicability {
    #[serde(rename = "MachineApplicable")]
    MachineApplicable,
    #[serde(rename = "MaybeIncorrect")]
    MaybeIncorrect,
    #[serde(rename = "HasPlaceholders")]
    HasPlaceholders,
    #[serde(rename = "Unspecified")]
    Unspecified,
}

impl SuggestionApplicability {
    pub fn rank(self) -> u8 {
        match self {
            Self::MachineApplicable => 0,
            Self::MaybeIncorrect => 1,
            Self::HasPlaceholders => 2,
            Self::Unspecified => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MachineApplicable => "MachineApplicable",
            Self::MaybeIncorrect => "MaybeIncorrect",
            Self::HasPlaceholders => "HasPlaceholders",
            Self::Unspecified => "Unspecified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanText {
    pub text: String,
    pub highlight_start: usize,
    pub highlight_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticSpan {
    pub file: String,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    pub line_start: usize,
    pub line_end: usize,
    pub column_start: usize,
    pub column_end: usize,
    pub is_primary: bool,
    pub label: Option<String>,
    pub suggested_replacement: Option<String>,
    pub suggestion_applicability: Option<SuggestionApplicability>,
    pub expansion: Option<MacroExpansion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text: Vec<SpanText>,
    /// The compiler supplied every coordinate needed for a safe edit.
    #[serde(skip)]
    pub range_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacroExpansion {
    pub macro_decl_name: Option<String>,
    pub span: Box<DiagnosticSpan>,
    pub definition_span: Option<Box<DiagnosticSpan>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticChild {
    pub level: String,
    pub message: String,
    pub rendered: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<DiagnosticChild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompilerSuggestionEdit {
    pub file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub column_start: usize,
    pub column_end: usize,
    pub replacement: String,
    #[serde(skip)]
    pub byte_start: Option<u64>,
    #[serde(skip)]
    pub byte_end: Option<u64>,
    #[serde(skip)]
    pub range_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompilerSuggestion {
    pub message: String,
    pub applicability: SuggestionApplicability,
    pub edits: Vec<CompilerSuggestionEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: Option<String>,
    pub level: DiagnosticLevel,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub message: String,
    pub root_key: Option<String>,
    pub rendered: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<DiagnosticChild>,
    pub suggestions: Vec<CompilerSuggestion>,
    /// Cargo/rustc output is evidence supplied by a child process, never an instruction.
    pub untrusted_data: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticDetail {
    Compact,
    Standard,
    Full,
}

impl DiagnosticDetail {
    pub fn default_diagnostic_limit(self) -> usize {
        match self {
            Self::Compact => 5,
            Self::Standard => 12,
            Self::Full => 24,
        }
    }

    pub fn suggestion_limit(self) -> usize {
        match self {
            Self::Compact => 1,
            Self::Standard | Self::Full => 3,
        }
    }

    pub fn child_limit(self) -> usize {
        match self {
            Self::Compact => 0,
            Self::Standard | Self::Full => 3,
        }
    }

    pub fn rendered_limit(self) -> usize {
        match self {
            Self::Compact => 0,
            Self::Standard => 600,
            Self::Full => 1_500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub detail: DiagnosticDetail,
    pub max_diagnostics: usize,
    pub max_bytes: usize,
}

impl RenderOptions {
    pub fn for_detail(detail: DiagnosticDetail, max_bytes: usize) -> Self {
        Self {
            detail,
            max_diagnostics: detail.default_diagnostic_limit(),
            max_bytes,
        }
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self::for_detail(DiagnosticDetail::Compact, DEFAULT_RENDER_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedSuggestion {
    pub message: String,
    pub applicability: SuggestionApplicability,
    pub edit_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedDiagnostic {
    pub code: Option<String>,
    pub level: DiagnosticLevel,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub message: String,
    pub rendered: Option<String>,
    pub suggestions: Vec<RenderedSuggestion>,
    pub children: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredDiagnostics {
    pub errors: usize,
    pub warnings: usize,
    pub diagnostics: Vec<RenderedDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedDiagnostics {
    pub text: String,
    pub structured: StructuredDiagnostics,
    pub untrusted_data: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CargoBuildTelemetry {
    pub total_units: usize,
    pub fresh_units: usize,
    pub rebuilt_units: usize,
    pub build_scripts: usize,
    pub linked_units: usize,
}

impl CargoBuildTelemetry {
    pub fn is_empty(&self) -> bool {
        self.total_units == 0 && self.build_scripts == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CargoOutput {
    pub diagnostics: Vec<Diagnostic>,
    pub build: Option<CargoBuildTelemetry>,
    pub untrusted_data: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePosition {
    pub line: usize,
    pub character: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvisoryEdit {
    pub file: String,
    pub range: SourceRange,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteFreePatch {
    pub file: String,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedEdit {
    pub edit: AdvisoryEdit,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteFreePackage {
    pub patches: Vec<WriteFreePatch>,
    pub skipped: Vec<SkippedEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedEdit {
    pub group: usize,
    pub edit: AdvisoryEdit,
    pub file: PathBuf,
    pub source: String,
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}
