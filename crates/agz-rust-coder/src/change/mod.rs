//! Revision-bound changeset engine.
//!
//! `create` captures the complete authorized working tree (dirty tracked and
//! untracked files included) into server-owned scratch. `stage`, `validate`,
//! `inspect`, `export`, and `discard` only ever touch that scratch copy; the
//! original workspace is never written.

mod capture;
mod model;
mod patch;
mod service;
mod store;

pub use model::{
    ChangeAction, ChangeCaptureData, ChangeData, ChangeEvidenceData, ChangeNewFileData,
    ChangeOutcome, ChangePatchData, ChangeRequest, ChangeSourceHashData, NewFileInput, PatchInput,
};
pub use service::ChangeService;
