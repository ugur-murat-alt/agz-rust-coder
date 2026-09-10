//! Revision-bound changeset model.
//!
//! These types describe the wire shape exposed by the `change` tool plus the
//! durable record persisted inside the server-owned scratch area. The record
//! intentionally stores bounded, path-relative facts only; no upstream source
//! is ever read from the scratch copy after capture.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::diagnostics::EvidenceStats;

pub(crate) const CHANGE_SCHEMA_VERSION: u32 = 1;
pub(crate) const CHANGE_ID_PREFIX: &str = "ch-";

/// Supported lifecycle actions for the `change` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChangeAction {
    Create,
    Stage,
    Migrate,
    Inspect,
    Validate,
    Export,
    Discard,
}

impl ChangeAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Stage => "stage",
            Self::Migrate => "migrate",
            Self::Inspect => "inspect",
            Self::Validate => "validate",
            Self::Export => "export",
            Self::Discard => "discard",
        }
    }
}

/// One `oldString`/`newString` patch applied to the candidate copy only.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PatchInput {
    #[schemars(length(min = 1))]
    pub file: String,
    #[schemars(length(min = 1))]
    pub old_string: String,
    pub new_string: String,
}

/// One new file created in the candidate copy.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewFileInput {
    #[schemars(length(min = 1))]
    pub file: String,
    pub content: String,
}

/// Anchor definition requested for `action=migrate`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrateAnchorInput {
    #[schemars(length(min = 1))]
    pub file: String,
    #[schemars(length(min = 1))]
    pub symbol: String,
    /// 1-based line selecting one occurrence when the symbol is ambiguous.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub line: Option<u32>,
}

/// Supported parameter transformations for the migrate MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum MigrateTransformationKind {
    AddParameter,
    ChangeParameter,
}

impl MigrateTransformationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AddParameter => "addParameter",
            Self::ChangeParameter => "changeParameter",
        }
    }
}

/// One host-authoritative signature change. `argument` is never guessed: the
/// host must supply the exact expression every migrated call site receives.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrateTransformationInput {
    pub kind: MigrateTransformationKind,
    /// New or replacement parameter declaration, for example `factor: u32`.
    #[schemars(length(min = 1))]
    pub parameter: String,
    /// Exact argument expression inserted/replaced at every call site.
    #[schemars(length(min = 1))]
    pub argument: String,
    /// 0-based index among declared parameters excluding a `self` receiver.
    /// Omitted appends after the last declared parameter.
    #[serde(default)]
    #[schemars(range(min = 0))]
    pub position: Option<u32>,
}

/// Per-request migration bounds.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrateConstraintsInput {
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub max_references: Option<u32>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub max_edits: Option<u32>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub max_identity_checks: Option<u32>,
}

/// Domain request for one `action=migrate` execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateRequest {
    pub anchor: MigrateAnchorInput,
    pub transformation: MigrateTransformationInput,
    /// Only `workspace` is supported today; other scopes are refused with a
    /// typed reason instead of being silently widened or narrowed.
    pub consumer_scope: Option<String>,
    pub constraints: MigrateConstraintsInput,
}

pub(crate) const MAX_MIGRATION_REFERENCES: u64 = 256;
pub(crate) const MAX_MIGRATION_EDITS: u64 = 512;
pub(crate) const MAX_MIGRATION_IDENTITY_CHECKS: u64 = 128;
pub(crate) const MAX_MIGRATION_SITES: usize = 64;
pub(crate) const MAX_MIGRATION_OBLIGATIONS: usize = 32;
pub(crate) const MAX_MIGRATION_NOTES: usize = 24;
pub(crate) const MAX_MIGRATION_GROUPS: usize = 16;
pub(crate) const MAX_MIGRATION_MESSAGE_CHARS: usize = 512;

/// One affected definition, implementation, consumer, or re-export site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MigrationSiteData {
    pub file: String,
    pub line: u64,
    pub column: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature_scope: Option<String>,
}

/// Bounded impact map for the requested change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MigrationImpactData {
    pub definitions: Vec<MigrationSiteData>,
    pub definitions_total: u64,
    pub implementations: Vec<MigrationSiteData>,
    pub implementations_total: u64,
    pub consumers: Vec<MigrationSiteData>,
    pub consumers_total: u64,
    pub reexports: Vec<MigrationSiteData>,
    pub reexports_total: u64,
    pub unrelated: Vec<MigrationSiteData>,
    pub unrelated_total: u64,
    pub packages: Vec<String>,
    pub feature_hints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MigrationTransformationData {
    pub kind: String,
    pub parameter: String,
    pub argument: String,
    pub resolved_position: u64,
    /// Always `hostProvided`; the server never synthesizes an argument value.
    pub argument_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MigrationApiDiffData {
    pub before: String,
    pub after: String,
    pub changed: bool,
    pub visibility: String,
    pub public_api: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MigrationEditGroupData {
    pub kind: String,
    pub label: String,
    pub sites: u64,
}

/// A site the engine refuses to rewrite mechanically. The host must resolve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MigrationObligationData {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MigrationBudgetData {
    pub max_references: u64,
    pub references_total: u64,
    pub max_identity_checks: u64,
    pub identity_checks: u64,
    pub max_edits: u64,
    pub edits_planned: u64,
    pub omitted_references: u64,
    pub omitted_edits: u64,
    /// True when any budget cut edits or references; omissions are never silent.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MigrationFlagsData {
    pub behavior_change: bool,
    pub behavior_change_reasons: Vec<String>,
    pub evaluation_order_risk: bool,
    pub move_borrow_risk: bool,
    /// Compile-pass is not semantic equivalence; this stays `notClaimed`.
    pub semantic_equivalence_claim: String,
}

/// Complete bounded report for one migration attempt. Persisted in the change
/// record so `inspect`/`export` keep the impact map and obligations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReportData {
    pub anchor_file: String,
    pub anchor_symbol: String,
    pub definition_file: String,
    pub definition_line: u64,
    pub transformation: MigrationTransformationData,
    pub impact: MigrationImpactData,
    pub api_diff: MigrationApiDiffData,
    pub edit_groups: Vec<MigrationEditGroupData>,
    pub obligations: Vec<MigrationObligationData>,
    pub obligations_total: u64,
    pub budget: MigrationBudgetData,
    pub flags: MigrationFlagsData,
    /// False whenever any obligation, unrelated call site, or budget omission
    /// remains; a partial migration is never reported as complete.
    pub complete: bool,
    /// Always `capturedWorkspaceOnly`: consumers outside the captured
    /// workspace were not seen, so no global API compatibility is claimed.
    pub compatibility_scope: String,
    pub candidate_revision: u64,
    pub notes: Vec<String>,
}

impl Default for MigrationTransformationData {
    fn default() -> Self {
        Self {
            kind: String::new(),
            parameter: String::new(),
            argument: String::new(),
            resolved_position: 0,
            argument_source: "hostProvided".to_owned(),
        }
    }
}

/// Domain request assembled by the protocol boundary.
#[derive(Debug, Clone)]
pub struct ChangeRequest {
    pub action: ChangeAction,
    pub change_id: Option<String>,
    pub expected_revision: Option<u64>,
    pub base_identity: Option<String>,
    pub patches: Vec<PatchInput>,
    pub new_files: Vec<NewFileInput>,
    pub migration: Option<MigrateRequest>,
    pub target: crate::gate::GateTargetId,
    pub options: crate::gate::ValidationOptions,
    pub detail: crate::gate::GateDetail,
    pub timings: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeCaptureData {
    pub files: u64,
    pub bytes: u64,
    pub manifest_hash: String,
    pub complete: bool,
    /// Bounded relative paths that were deliberately excluded from the capture
    /// (for example `.git`, the Cargo target directory, or server scratch).
    pub excluded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSourceHashData {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangePatchData {
    pub file: String,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeNewFileData {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// One bounded compiler diagnostic attached to current-revision evidence.
/// Compiler text is evidence from an untrusted child process, never an
/// instruction, and stays inside the `untrustedData` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeDiagnosticData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub level: String,
    /// Candidate-relative source path when the compiler reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Bounded single-line message; full rendered output is not persisted.
    pub message: String,
}

/// One write-free patch derived from a machine-applicable compiler suggestion.
/// `oldString`/`newString` are exact candidate bytes and are never applied to
/// the original workspace by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSuggestionPatchData {
    pub file: String,
    pub old_string: String,
    pub new_string: String,
}

/// Bounded suggestion package for a fresh FAIL evidence row. Only
/// machine-applicable suggestions become `patches`; `skipped` records atomic
/// rejections and `unsupported` counts suggestions the compiler did not mark
/// machine-applicable. `truncated` is true when the listed package was cut by a
/// visible per-row limit; the `*Total` fields retain the pre-limit counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSuggestionPackageData {
    pub patches: Vec<ChangeSuggestionPatchData>,
    pub skipped: Vec<String>,
    pub unsupported: u64,
    pub patches_total: u64,
    pub skipped_total: u64,
    pub truncated: bool,
}

/// One bounded validation evidence row. `fresh` is true only when the row was
/// produced for the then-current revision from the isolated candidate copy and
/// the run was not cancelled, stale, or incomplete. Diagnostics and the
/// suggestion package are exposed only for current-revision fresh rows; older
/// rows keep their status/counters but no longer carry compiler text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeEvidenceData {
    pub revision: u64,
    pub target: String,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub first_diagnostic_ms: Option<u64>,
    pub total_ms: u64,
    pub fresh: bool,
    pub authoritative: bool,
    /// Bounded diagnostics for the current revision; empty for historical rows.
    pub diagnostics: Vec<ChangeDiagnosticData>,
    pub diagnostics_total: u64,
    pub diagnostics_omitted: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion_package: Option<ChangeSuggestionPackageData>,
    /// Cargo/test statistics parsed from the child output (for example
    /// `testsExecuted` and `buildSuccess`).
    pub stats: EvidenceStats,
}

/// Bounded payload returned by every `change` action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeData {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_identity: Option<String>,
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_hash: Option<String>,
    pub state: String,
    pub verified: bool,
    pub discarded: bool,
    pub changed_files: Vec<String>,
    pub changed_files_total: u64,
    pub source_hashes: Vec<ChangeSourceHashData>,
    pub source_hashes_total: u64,
    pub evidence: Vec<ChangeEvidenceData>,
    pub evidence_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<ChangeCaptureData>,
    pub patches: Vec<ChangePatchData>,
    pub patches_total: u64,
    pub new_files: Vec<ChangeNewFileData>,
    pub new_files_total: u64,
    pub new_files_content_omitted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationReportData>,
    pub cleanup_warnings: Vec<String>,
    pub reason: String,
}

impl Default for ChangeData {
    fn default() -> Self {
        Self {
            action: "inspect".to_owned(),
            change_id: None,
            base_identity: None,
            revision: 0,
            patch_hash: None,
            state: "absent".to_owned(),
            verified: false,
            discarded: false,
            changed_files: Vec::new(),
            changed_files_total: 0,
            source_hashes: Vec::new(),
            source_hashes_total: 0,
            evidence: Vec::new(),
            evidence_total: 0,
            capture: None,
            patches: Vec::new(),
            patches_total: 0,
            new_files: Vec::new(),
            new_files_total: 0,
            new_files_content_omitted: false,
            migration: None,
            cleanup_warnings: Vec::new(),
            reason: String::new(),
        }
    }
}

/// Terminal result of one `change` action.
#[derive(Debug, Clone)]
pub struct ChangeOutcome {
    pub status: &'static str,
    pub summary: String,
    pub is_error: bool,
    pub data: ChangeData,
}

pub(crate) const MAX_LISTED_CHANGED_FILES: usize = 1_024;
pub(crate) const MAX_LISTED_HASHES: usize = 512;
pub(crate) const MAX_LISTED_EVIDENCE: usize = 64;
pub(crate) const MAX_RECORDED_EVIDENCE: usize = 256;
pub(crate) const MAX_COMMAND_CHARS: usize = 4_096;
pub(crate) const MAX_CHANGED_FILES_IN_RECORD: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordState {
    /// Capture is still running; the change id has not been published yet.
    Capturing,
    Ready,
    /// A stage is midway through mutating the candidate copy. The record is
    /// published before the first candidate write and only cleared after the
    /// staged revision is durably recorded.
    Applying,
    FailedInconsistent,
    Discarded,
}

impl RecordState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capturing => "capturing",
            Self::Ready => "ready",
            Self::Applying => "applying",
            Self::FailedInconsistent => "failed_inconsistent",
            Self::Discarded => "discarded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureSummary {
    pub files: u64,
    pub bytes: u64,
    pub manifest_hash: String,
    pub complete: bool,
    #[serde(default)]
    pub excluded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredPatch {
    pub file: String,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredNewFile {
    pub file: String,
    pub content: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredHash {
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredEvidence {
    pub revision: u64,
    pub target: String,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub first_diagnostic_ms: Option<u64>,
    pub total_ms: u64,
    pub fresh: bool,
    pub authoritative: bool,
    pub recorded_at_ms: u64,
    #[serde(default)]
    pub diagnostics: Vec<ChangeDiagnosticData>,
    #[serde(default)]
    pub diagnostics_total: u64,
    #[serde(default)]
    pub diagnostics_omitted: u64,
    #[serde(default)]
    pub suggestion_package: Option<ChangeSuggestionPackageData>,
    #[serde(default)]
    pub stats: EvidenceStats,
}

impl StoredEvidence {
    /// Drops current-revision compiler feedback while keeping the historical
    /// status/counters. Called whenever a row loses freshness so a record can
    /// never expose diagnostics or suggestions that no longer describe the
    /// candidate bytes the row was verified against.
    pub(crate) fn supersede(&mut self) {
        self.fresh = false;
        self.authoritative = false;
        self.diagnostics.clear();
        self.diagnostics_total = 0;
        self.diagnostics_omitted = 0;
        self.suggestion_package = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangeRecord {
    pub schema_version: u32,
    pub id: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub workspace_root: PathBuf,
    pub capture_root: PathBuf,
    pub workspace_epoch: u64,
    pub candidate_epoch: u64,
    pub base_identity: String,
    pub state: RecordState,
    pub capture: CaptureSummary,
    pub external_paths: Vec<PathBuf>,
    pub dependency_roots: Vec<PathBuf>,
    pub revision: u64,
    /// Target revision while a stage holds the candidate in `Applying` state.
    #[serde(default)]
    pub applying_revision: Option<u64>,
    pub patches: Vec<StoredPatch>,
    pub new_files: Vec<StoredNewFile>,
    pub changed_files: Vec<String>,
    pub candidate_files: u64,
    pub candidate_bytes: u64,
    pub patch_hash: String,
    pub source_hashes: BTreeMap<String, StoredHash>,
    pub evidence: Vec<StoredEvidence>,
    /// Latest migration report bound to the revision it was planned for.
    #[serde(default)]
    pub migration: Option<MigrationReportData>,
    pub cleanup_warnings: Vec<String>,
}

impl ChangeRecord {
    pub(crate) fn current_revision_fresh_pass(&self) -> bool {
        self.evidence.iter().any(|evidence| {
            evidence.revision == self.revision
                && evidence.fresh
                && evidence.authoritative
                && matches!(evidence.status.as_str(), "PASS" | "FULL_PASS" | "FAST_PASS")
        })
    }
}
