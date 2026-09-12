//! Authoritative Cargo validation, scheduling, and evidence.

pub(crate) mod acceleration;
pub mod cache;
pub mod lease;
pub mod profile;
pub mod scheduler;
pub(crate) mod selection;
pub mod targets;
pub mod types;
pub use profile::{TestRunner, ValidationOptions};
pub use selection::CargoTargetSelection;

pub use cache::{CacheMode, CacheSelection, select_gate_cache};
pub use scheduler::{
    GateScheduler, JobSubscription, ProgressHub, ProgressRegistration, ResourceSnapshot,
    ScheduledJob, ScheduledJobContext, SchedulerError, SchedulerOptions,
};
pub use targets::{target_for, targets_for};
pub use types::{
    CompilerSuggestion, DiagnosticChild, DiagnosticSpan, GateAuthority, GateBuildInfo, GateDetail,
    GateDiagnostic, GateEvidence, GateMode, GateRequest, GateScope, GateScopeStrategy, GateSource,
    GateStatus, GateStepResult, GateTarget, GateTargetId, MacroExpansion, ProgressCallback,
    ProgressEvent, ProgressStage, SuggestionApplicability, SuggestionEdit, SuggestionPackage,
    SuggestionPatch, ValidationProfile, validate_toolchain_name,
};

pub use crate::tools::CheckService;
