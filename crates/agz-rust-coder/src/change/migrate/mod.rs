//! Migration analysis, impact mapping, and structural edit planning.

pub(crate) mod analyzer;
pub(crate) mod plan;
pub(crate) mod rewrite;

pub use analyzer::{
    AnalyzeRequest, AnalyzedSite, AnalyzerError, AnalyzerFuture, AnchorAnalysis, MigrationAnalyzer,
    RustAnalyzerMigrationAnalyzer,
};
pub(crate) use plan::{PlanBudgets, compose_patches, plan_migration};
