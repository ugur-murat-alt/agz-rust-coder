//! Revision-bound changeset engine.
//!
//! `create` captures the complete authorized working tree (dirty tracked and
//! untracked files included) into server-owned scratch. `stage`, `validate`,
//! `inspect`, `export`, and `discard` only ever touch that scratch copy; the
//! original workspace is never written.

mod capture;
mod migrate;
pub(crate) mod model;
mod patch;
mod runtime;
mod service;
mod store;

pub use migrate::{
    AnalyzeRequest, AnalyzedSite, AnalyzerError, AnalyzerFuture, AnchorAnalysis, MigrationAnalyzer,
    RustAnalyzerMigrationAnalyzer,
};
pub use model::{
    ChangeAction, ChangeCaptureData, ChangeData, ChangeDiagnosticData, ChangeEvidenceData,
    ChangeNewFileData, ChangeOutcome, ChangePatchData, ChangeRequest, ChangeSourceHashData,
    ChangeSuggestionPackageData, ChangeSuggestionPatchData, MigrateAnchorInput,
    MigrateConstraintsInput, MigrateRequest, MigrateTransformationInput, MigrateTransformationKind,
    MigrationApiDiffData, MigrationBudgetData, MigrationEditGroupData, MigrationFlagsData,
    MigrationImpactData, MigrationObligationData, MigrationReportData, MigrationSiteData,
    MigrationTransformationData, NewFileInput, PatchInput,
};
pub use runtime::{RuntimeSnapshotPair, SnapshotError};
pub use service::{CandidateTree, CandidateTreeSkip, ChangeService};
