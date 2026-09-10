//! Compiler-driven repair of a failing change revision.
//!
//! `repair` consumes the bounded diagnostics and machine-applicable suggestion
//! package of an existing `change` failure, adds source-backed ownership
//! explanations and a few explicit mechanical transforms, tries each candidate
//! on its own change through [`ChangeService`], and compares measured results.
//! `action=minimize` reduces the same revision-bound candidate to a portable,
//! export-verified reproducer for the exact failure predicate. It never writes
//! the original workspace and never writes source files.

mod analysis;
mod guards;
mod minimize;
mod model;
mod service;

pub use model::{
    RepairAction, RepairAnalysisData, RepairBudget, RepairBudgetInput, RepairCandidateData,
    RepairCandidateInput, RepairCandidateSourceData, RepairChangedData, RepairConfigurationData,
    RepairConstraintsInput, RepairData, RepairDeltaData, RepairDiagnosticGroupData,
    RepairEliminationData, RepairExcerptData, RepairFailurePredicateInput, RepairGateData,
    RepairImpactData, RepairOutcome, RepairOwnershipData, RepairPatchData, RepairPredicateData,
    RepairProofData, RepairProofFileData, RepairReductionData, RepairReductionScope,
    RepairRelationData, RepairRequest, RepairSelectionData, RepairSpanData, RepairTarget,
};
pub use service::RepairService;
