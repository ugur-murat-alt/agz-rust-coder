//! Bounded work executor.
//!
//! A work item turns a typed intent into the existing validated domain calls:
//! it creates/adopts a revision-bound `change`, stages host candidate patches,
//! runs the requested acceptance gates, and returns either honest
//! requested-gate `READY` evidence or a bounded `NEEDS_MODEL` handoff with a
//! single-use, revision-bound continuation token. It never runs a shell command
//! and never writes workspace source.

mod model;
mod service;

pub use model::{
    WorkAction, WorkBudget, WorkBudgetData, WorkBudgetUsedData, WorkCandidateInput,
    WorkChangeBudget, WorkConstraints, WorkData, WorkEvidenceData, WorkGate, WorkHandoffData,
    WorkIntent, WorkOutcome, WorkRequest, WorkTemplate, WorkTool,
};
pub use service::WorkService;
