//! Static advisory knowledge used by Phase 4 tools and resources.

pub mod borrow_errors;
pub mod iced;
pub mod pitfalls;
pub mod std_modules;
pub mod workflow;

pub use borrow_errors::{BORROW_HINTS, BorrowHint, EXPLAIN_ADVICE, hint_for};
pub use iced::{IcedProfile, build_iced_block, iced_profile};
pub use pitfalls::{
    AUDIT_PATTERNS, AuditPattern, AuditPatternId, AuditSeverity, PatternId, PitfallDefinition,
    Severity, pattern_by_id, pattern_by_name,
};
pub use std_modules::{STD_MODULES, STD_POLICY, StdModuleEntry, std_module_lookup};
pub use workflow::{
    WORKFLOW_FOOTER, WORKFLOW_HEADER, WORKFLOW_SECTIONS, build_workflow_block, estimate_tokens,
};
