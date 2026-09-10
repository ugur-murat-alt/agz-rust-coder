//! Bounded work executor model.
//!
//! These types describe the wire shape exposed by the `work` tool plus the
//! in-memory record kept by the server. A work item only orchestrates the
//! existing validated domain APIs (`change` create/stage/validate) with typed
//! templates, explicit budgets, and a bounded host handoff; it never executes a
//! shell command and never writes workspace source.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::change::{ChangeDiagnosticData, ChangeSuggestionPackageData, NewFileInput, PatchInput};
use crate::gate::GateTargetId;

pub(crate) const WORK_ID_PREFIX: &str = "wk-";

pub(crate) const MAX_SCOPE_PATHS: usize = 64;
pub(crate) const MAX_SCOPE_PATH_CHARS: usize = 512;
pub(crate) const MAX_CONTRACT_CHARS: usize = 4_096;
pub(crate) const MAX_STOP_CONDITION_CHARS: usize = 1_024;
pub(crate) const MAX_ACCEPTANCE_GATES: usize = 8;
pub(crate) const MAX_REQUIRED_TOOLS: usize = 8;
pub(crate) const MAX_LISTED_WORK_EVIDENCE: usize = 32;
pub(crate) const MAX_LISTED_HANDOFF_DIAGNOSTICS: usize = 24;
pub(crate) const MAX_UNRESOLVED_OBLIGATIONS: usize = 16;
pub(crate) const MAX_WORK_RECORDS: usize = 256;
pub(crate) const MAX_CANDIDATE_PATCHES: usize = 512;
pub(crate) const MAX_CANDIDATE_NEW_FILES: usize = 512;

/// Supported lifecycle actions for the `work` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkAction {
    Start,
    Resume,
    Inspect,
    Cancel,
}

impl WorkAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Resume => "resume",
            Self::Inspect => "inspect",
            Self::Cancel => "cancel",
        }
    }
}

/// Typed intent templates. A template is a bounded plan: explicit scope,
/// behavior contract, stop condition, acceptance gates, and change budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkTemplate {
    /// The host supplies the implementation candidate for the contract.
    ImplementWithContract,
    /// The host repairs the current candidate after a compile failure.
    RepairCompileFailure,
    /// The host refactors and the executor verifies the requested gates.
    RefactorAndVerify,
}

impl WorkTemplate {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ImplementWithContract => "implement_with_contract",
            Self::RepairCompileFailure => "repair_compile_failure",
            Self::RefactorAndVerify => "refactor_and_verify",
        }
    }
}

/// Typed acceptance gate. Only existing validated Cargo targets are allowed;
/// arbitrary shell commands cannot become plan nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkGate {
    Check,
    Clippy,
    Test,
    Doc,
    Fmt,
    All,
}

impl WorkGate {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Doc => "doc",
            Self::Fmt => "fmt",
            Self::All => "all",
        }
    }

    pub const fn target(self) -> GateTargetId {
        match self {
            Self::Check => GateTargetId::Check,
            Self::Clippy => GateTargetId::Clippy,
            Self::Test => GateTargetId::Test,
            Self::Doc => GateTargetId::Doc,
            Self::Fmt => GateTargetId::Fmt,
            Self::All => GateTargetId::All,
        }
    }
}

/// Tools a work item may declare as required. A disabled or offline-backed
/// required tool blocks the work instead of silently changing the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkTool {
    Change,
    Check,
    Docs,
    Lsp,
    Audit,
    CrateLookup,
}

impl WorkTool {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Change => "change",
            Self::Check => "check",
            Self::Docs => "docs",
            Self::Lsp => "lsp",
            Self::Audit => "audit",
            Self::CrateLookup => "crate_lookup",
        }
    }
}

/// Per-candidate change budget. Both fields are explicit; the executor refuses
/// a candidate that exceeds them before any candidate byte is staged.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkChangeBudget {
    #[schemars(range(min = 1, max = 512))]
    pub max_patches: u32,
    #[schemars(range(min = 0, max = 512))]
    pub max_new_files: u32,
}

/// Typed work intent assembled by the protocol boundary.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkIntent {
    pub template: WorkTemplate,
    /// Explicit target scope: candidate patches must stay inside these
    /// workspace-relative paths.
    #[schemars(length(min = 1, max = 64), inner(length(min = 1, max = 512)))]
    pub scope_paths: Vec<String>,
    /// Behavior contract the candidate must satisfy (bounded text, never a
    /// command).
    #[schemars(length(min = 1, max = 4096))]
    pub contract: String,
    /// Explicit stop condition for the bounded repair loop.
    #[schemars(length(min = 1, max = 1024))]
    pub stop_condition: String,
    #[schemars(length(min = 1, max = 8))]
    pub acceptance_gates: Vec<WorkGate>,
    pub change_budget: WorkChangeBudget,
}

/// Work policy constraints. Offline work cannot require network-backed tools.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkConstraints {
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    #[schemars(length(max = 8))]
    pub required_tools: Vec<WorkTool>,
}

/// Effective bounded budgets for one work item. Per-call values are clamped to
/// the server configuration before they reach the domain service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkBudget {
    pub max_compiles: u64,
    pub max_candidates: u64,
    pub max_handoffs: u64,
    pub wall_time_ms: u64,
}

/// Domain request assembled by the protocol boundary.
#[derive(Debug, Clone)]
pub struct WorkRequest {
    pub action: WorkAction,
    pub work_id: Option<String>,
    pub continuation_token: Option<String>,
    pub change_id: Option<String>,
    pub intent: Option<WorkIntent>,
    pub patches: Vec<PatchInput>,
    pub new_files: Vec<NewFileInput>,
    pub constraints: WorkConstraints,
    pub budget: WorkBudget,
}

/// Candidate patch input schema advertised in a [`WorkHandoffData`] package so
/// the host knows the exact typed shape a `resume` accepts.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkCandidateInput {
    #[serde(default)]
    pub patches: Vec<PatchInput>,
    #[serde(default)]
    pub new_files: Vec<NewFileInput>,
}

/// Effective budgets echoed in the wire result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkBudgetData {
    pub max_compiles: u64,
    pub max_candidates: u64,
    pub max_handoffs: u64,
    pub wall_time_ms: u64,
}

/// Budget consumption for the current record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkBudgetUsedData {
    pub candidates: u64,
    pub compiles: u64,
    pub handoffs: u64,
    pub elapsed_ms: u64,
}

/// One bounded gate evidence row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkEvidenceData {
    pub revision: u64,
    pub gate: String,
    pub status: String,
    pub fresh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub total_ms: u64,
    pub diagnostics_total: u64,
}

/// Bounded decision package for a single-use, revision-bound continuation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkHandoffData {
    /// Opaque single-use token. It is valid only for the recorded work,
    /// revision, and patch hash, and only until `expiresAtMs`.
    pub continuation_token: String,
    pub expires_at_ms: u64,
    /// Why the handoff was issued: `compile`, `test`, or `no_candidate`.
    pub failure_kind: String,
    pub diagnostics: Vec<ChangeDiagnosticData>,
    pub diagnostics_total: u64,
    pub diagnostics_omitted: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion_package: Option<ChangeSuggestionPackageData>,
    /// Unresolved obligations for the host candidate (contract, stop
    /// condition, failed gates, skipped suggestions). Bounded strings only.
    pub unresolved_obligations: Vec<String>,
    /// JSON schema for the typed `resume` candidate input.
    pub candidate_input_schema: Value,
}

/// Bounded payload returned by every `work` action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkData {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_hash: Option<String>,
    pub acceptance_gates: Vec<String>,
    pub scope_paths: Vec<String>,
    pub budget: WorkBudgetData,
    pub used: WorkBudgetUsedData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff: Option<WorkHandoffData>,
    pub evidence: Vec<WorkEvidenceData>,
    pub evidence_total: u64,
    pub reason: String,
    pub warnings: Vec<String>,
}

impl Default for WorkData {
    fn default() -> Self {
        Self {
            action: "inspect".to_owned(),
            work_id: None,
            state: "absent".to_owned(),
            template: None,
            change_id: None,
            revision: 0,
            patch_hash: None,
            acceptance_gates: Vec::new(),
            scope_paths: Vec::new(),
            budget: WorkBudgetData {
                max_compiles: 0,
                max_candidates: 0,
                max_handoffs: 0,
                wall_time_ms: 0,
            },
            used: WorkBudgetUsedData {
                candidates: 0,
                compiles: 0,
                handoffs: 0,
                elapsed_ms: 0,
            },
            handoff: None,
            evidence: Vec::new(),
            evidence_total: 0,
            reason: String::new(),
            warnings: Vec::new(),
        }
    }
}

/// Terminal result of one `work` action.
#[derive(Debug, Clone)]
pub struct WorkOutcome {
    pub status: &'static str,
    pub summary: String,
    pub is_error: bool,
    pub data: WorkData,
}
