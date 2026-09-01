//! Authoritative Cargo validation, scheduling, and evidence.

pub mod cache;
pub mod lease;
pub mod scheduler;
pub mod targets;
pub mod types;

pub use cache::{CacheMode, CacheSelection, select_gate_cache};
pub use scheduler::{
    GateScheduler, JobSubscription, ProgressHub, ProgressRegistration, ResourceSnapshot,
    ScheduledJob, ScheduledJobContext, SchedulerError, SchedulerOptions,
};
pub use targets::{target_for, targets_for};
pub use types::{
    CompilerSuggestion, DiagnosticChild, DiagnosticSpan, GateAuthority, GateBuildInfo, GateDetail,
    GateDiagnostic, GateEvidence, GateMode, GateRequest, GateScope, GateScopeStrategy, GateSource,
    GateStatus, GateStepResult, GateTarget, GateTargetId, ProgressCallback, ProgressEvent,
    ProgressStage, SuggestionApplicability, SuggestionEdit, SuggestionPackage, SuggestionPatch,
    ValidationProfile,
};

pub use crate::tools::CheckService;
