//! Wire and domain types for the compiler-driven `repair` tool.
//!
//! Every repair result is bounded, revision-bound evidence. Compiler text,
//! candidate patches, and source excerpts are data, never instructions, and
//! the tool never writes the original workspace.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::change::{ChangeData, ChangeDiagnosticData, ChangeEvidenceData, PatchInput};
use crate::gate::{GateTargetId, ValidationOptions};

/// Supported lifecycle actions for the `repair` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RepairAction {
    Analyze,
    Try,
    Compare,
}

impl RepairAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Analyze => "analyze",
            Self::Try => "try",
            Self::Compare => "compare",
        }
    }
}

/// Cargo gate requested by `constraints.testTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RepairTarget {
    Check,
    Clippy,
    Test,
    Doc,
    Fmt,
    All,
}

impl RepairTarget {
    pub const fn as_gate_target(self) -> GateTargetId {
        match self {
            Self::Check => GateTargetId::Check,
            Self::Clippy => GateTargetId::Clippy,
            Self::Test => GateTargetId::Test,
            Self::Doc => GateTargetId::Doc,
            Self::Fmt => GateTargetId::Fmt,
            Self::All => GateTargetId::All,
        }
    }

    pub const fn as_str(self) -> &'static str {
        self.as_gate_target().as_str()
    }
}

/// Optional narrowing budget from the request. Omitted values inherit the
/// configured `[repair]` limits; supplied values may only narrow, never widen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairBudgetInput {
    #[serde(default)]
    #[schemars(range(min = 1, max = 32))]
    pub max_candidates: Option<u32>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 64))]
    pub max_compiles: Option<u32>,
    #[serde(default)]
    #[schemars(range(min = 1_000, max = 3_600_000))]
    pub wall_time_ms: Option<u64>,
}

/// Explicit constraints for candidate generation and comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairConstraintsInput {
    /// Optional Cargo gate run for every tried candidate. Without it a
    /// comparison is `compileVerified` only.
    #[serde(default)]
    pub test_target: Option<RepairTarget>,
}

/// One host-supplied or generated candidate. A candidate's patches are always
/// applied as one atomic unit; a stale or overlapping part rejects the whole
/// candidate.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairCandidateInput {
    #[serde(default)]
    #[schemars(length(min = 1, max = 64))]
    pub id: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 64))]
    pub source: Option<String>,
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub patches: Vec<PatchInput>,
}

/// Effective, config-capped budget for one repair action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairBudget {
    pub max_candidates: u32,
    pub max_compiles: u32,
    pub wall_time_ms: u64,
}

/// Domain request assembled by the protocol boundary.
#[derive(Debug, Clone)]
pub struct RepairRequest {
    pub action: RepairAction,
    pub change_id: String,
    pub diagnostic_ids: Vec<String>,
    pub candidates: Vec<RepairCandidateInput>,
    pub test_target: Option<GateTargetId>,
    pub budget: RepairBudget,
}

/// Terminal result of one `repair` action.
#[derive(Debug, Clone)]
pub struct RepairOutcome {
    pub status: &'static str,
    pub summary: String,
    pub is_error: bool,
    pub data: RepairData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairSpanData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairDiagnosticGroupData {
    pub group_id: String,
    pub codes: Vec<String>,
    pub level: String,
    pub primary: RepairSpanData,
    pub secondary: Vec<RepairSpanData>,
    pub count: u64,
    pub messages: Vec<String>,
    pub diagnostic_ids: Vec<String>,
}

/// A root-cause relation is always a reasoned hypothesis, never a proven fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairRelationData {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub hypothesis: bool,
    pub reasoning: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairExcerptData {
    pub file: String,
    pub line_start: u64,
    pub line_end: u64,
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairOwnershipData {
    pub diagnostic_id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trait_name: Option<String>,
    pub explanation: String,
    pub source_evidence: Vec<RepairExcerptData>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairCandidateSourceData {
    pub kind: String,
    pub status: String,
    pub count: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairAnalysisData {
    pub diagnostics: Vec<RepairDiagnosticGroupData>,
    pub relations: Vec<RepairRelationData>,
    pub ownership: Vec<RepairOwnershipData>,
    pub sources: Vec<RepairCandidateSourceData>,
    pub unmatched_diagnostic_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairPatchData {
    pub file: String,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairGateData {
    pub target: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests_executed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_success: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairDeltaData {
    pub before_errors: u64,
    pub after_errors: u64,
    pub fixed_codes: Vec<String>,
    pub remaining_codes: Vec<String>,
    pub new_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairChangedData {
    pub files: Vec<String>,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub public_api_added: Vec<String>,
    pub public_api_removed: Vec<String>,
}

/// One behavior or performance impact surfaced by the guard scanner. A
/// candidate-added impact is never auto-accepted by `compare`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairImpactData {
    pub kind: String,
    pub category: String,
    pub detail: String,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairCandidateData {
    pub id: String,
    pub source: String,
    pub hash: String,
    pub status: String,
    pub patches: Vec<RepairPatchData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<RepairGateData>,
    pub diagnostics: Vec<ChangeDiagnosticData>,
    pub diagnostics_total: u64,
    pub diagnostics_omitted: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<RepairDeltaData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<RepairChangedData>,
    pub impacts: Vec<RepairImpactData>,
    pub inherited_impacts: Vec<RepairImpactData>,
    pub reason: String,
}

impl RepairCandidateData {
    pub(crate) fn new(
        id: String,
        source: &str,
        hash: String,
        patches: Vec<RepairPatchData>,
    ) -> Self {
        Self {
            id,
            source: source.to_owned(),
            hash,
            status: "candidate".to_owned(),
            patches,
            change_id: None,
            revision: None,
            compile: None,
            gate: None,
            diagnostics: Vec::new(),
            diagnostics_total: 0,
            diagnostics_omitted: 0,
            delta: None,
            changed: None,
            impacts: Vec::new(),
            inherited_impacts: Vec::new(),
            reason: String::new(),
        }
    }

    /// True when the candidate adds no behavior or performance impact of its
    /// own over the base change.
    pub(crate) fn behavior_preserving(&self) -> bool {
        self.impacts.is_empty()
    }

    pub(crate) fn compiled(&self) -> bool {
        self.compile.as_deref() == Some("compiled")
    }

    pub(crate) fn test_passed(&self) -> bool {
        self.gate.as_ref().is_some_and(|gate| {
            gate.build_success == Some(true) && !matches!(gate.status.as_str(), "FAIL")
        })
    }

    pub(crate) fn error_count(&self) -> u64 {
        u64::try_from(
            self.diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.level == "error")
                .count(),
        )
        .unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairSelectionData {
    pub id: String,
    pub reason: String,
    pub verification: String,
    pub patches: Vec<RepairPatchData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairEliminationData {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairConfigurationData {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_target: Option<String>,
    pub detail: String,
    pub options: ValidationOptions,
    pub base_identity: String,
    pub revision: u64,
    pub evidence_target: String,
}

impl RepairConfigurationData {
    pub(crate) fn from_base(
        target: GateTargetId,
        test_target: Option<GateTargetId>,
        data: &ChangeData,
        evidence: &ChangeEvidenceData,
    ) -> Self {
        Self {
            target: target.as_str().to_owned(),
            test_target: test_target.map(|target| target.as_str().to_owned()),
            detail: "compact".to_owned(),
            options: ValidationOptions::default(),
            base_identity: data.base_identity.clone().unwrap_or_default(),
            revision: data.revision,
            evidence_target: evidence.target.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepairBudgetData {
    pub max_candidates: u32,
    pub max_compiles: u32,
    pub wall_time_ms: u64,
    pub candidates_used: u32,
    pub compiles_used: u32,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct RepairData {
    pub action: String,
    pub usable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis: Option<RepairAnalysisData>,
    pub candidates: Vec<RepairCandidateData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<RepairSelectionData>,
    pub eliminated: Vec<RepairEliminationData>,
    pub verification: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<RepairConfigurationData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<RepairBudgetData>,
    pub stop_reason: String,
    pub remaining_risks: Vec<String>,
    pub reason: String,
}

pub(crate) const MAX_ANALYZED_DIAGNOSTICS: usize = 128;
pub(crate) const MAX_GROUPS: usize = 32;
pub(crate) const MAX_RELATIONS: usize = 48;
pub(crate) const MAX_OWNERSHIP_EXPLANATIONS: usize = 16;
pub(crate) const MAX_SOURCE_EXCERPTS: usize = 6;
pub(crate) const MAX_LISTED_CANDIDATES: usize = 16;
pub(crate) const MAX_CANDIDATE_DIAGNOSTICS: usize = 32;
pub(crate) const MAX_MESSAGES_PER_GROUP: usize = 4;
pub(crate) const MAX_IMPACTS: usize = 16;
pub(crate) const MAX_REMAINING_RISKS: usize = 16;
