//! Task-focused semantic context capsules with revision-aware delta access.
//!
//! The domain is deliberately small: typed anchors in, a bounded capsule with
//! per-item selection reasons out, and a bounded in-memory store for
//! revision-bound `expand`/`delta` reads. No new semantic engine, embedding
//! index, or graph database is introduced; capsules are assembled from the
//! existing Rust Analyzer tools, authorized workspace reads, and cargo
//! metadata.

pub mod capsule;
pub mod planner;
pub mod store;

pub use capsule::{
    AnchorRange, CONTEXT_SCHEMA_VERSION, Capsule, CapsuleIdentity, CapsuleIdentityInput,
    CapsuleItemKind, ContextAction, ContextAnchor, ContextData, ContextItem, DeltaEntry,
    DeltaReport, ItemProvenance, ItemResolution, MAX_ANCHORS, MAX_CHANGE_ID_CHARS,
    MAX_PURPOSE_CHARS, MIN_BYTE_BUDGET, OmissionReason, OmittedItem, PackageIdentity, PageInfo,
    SizeReport, anchors_hash, compute_capsule_id, item_id, purpose_hash, sha256_hex,
};
pub use planner::{
    MAX_OMITTED_ENTRIES, PlanLimits, PlanOutcome, apply_budget_and_sizes, cap_items,
    enforce_data_budget, wire_bytes,
};
pub use store::{CapsuleStore, StoreLookup, StoredCapsule};
