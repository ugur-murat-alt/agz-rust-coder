//! Revision-bound changeset engine.
//!
//! `create` captures the complete authorized working tree (dirty tracked and
//! untracked files included) into server-owned scratch. `stage`, `validate`,
//! `inspect`, `export`, and `discard` only ever touch that scratch copy; the
//! original workspace is never written.

mod capture;
mod model;
mod patch;
mod runtime;
mod service;
mod store;

pub use model::{
    ChangeAction, ChangeCaptureData, ChangeData, ChangeDiagnosticData, ChangeEvidenceData,
    ChangeNewFileData, ChangeOutcome, ChangePatchData, ChangeRequest, ChangeSourceHashData,
    ChangeSuggestionPackageData, ChangeSuggestionPatchData, NewFileInput, PatchInput,
};
pub use runtime::{RuntimeSnapshotPair, SnapshotError};
pub use service::{CandidateTree, CandidateTreeSkip, ChangeService};
