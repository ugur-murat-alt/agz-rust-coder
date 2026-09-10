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

/// Domain request assembled by the protocol boundary.
#[derive(Debug, Clone)]
pub struct ChangeRequest {
    pub action: ChangeAction,
    pub change_id: Option<String>,
    pub expected_revision: Option<u64>,
    pub base_identity: Option<String>,
    pub patches: Vec<PatchInput>,
    pub new_files: Vec<NewFileInput>,
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
