//! Domain-level semantic tools. The MCP server adapter remains outside this
//! module; these functions return bounded, source-write-free results.

pub mod api;
pub mod audit;
pub mod check;
pub mod context;
pub mod crate_lookup;
pub mod edits;
pub mod explain;
pub mod navigation;
pub mod profile;
pub mod runtime;
pub mod symbol;
pub mod verify;

pub use api::{
    ApiAction, ApiAnchor, ApiAnchorData, ApiConfiguration, ApiData, ApiEnvironment, ApiProbeData,
    ApiRequest, ApiResolveData, execute_api,
};
pub use audit::{
    AuditCancellation, AuditCancellationReason, AuditError, AuditFinding, AuditLimits,
    AuditRequest, AuditService, AuditSkip, AuditSkipReason, AuditSummary,
};
pub use check::CheckService;
pub use context::{ContextEnvironment, ContextRequest, execute_context};
pub use crate_lookup::{
    CrateLookupInput, CrateLookupResult, CrateLookupStatus, CratesIoClient, CratesIoError,
    CratesIoRequest, CratesIoResponse, OfflineCratesIoClient, ReqwestCratesIoClient,
    execute_crate_lookup, format_lookup_result, lookup_crate, lookup_crate_with_client,
    validate_crate_lookup_input,
};
pub use edits::{
    AdvisoryEdit, NormalizedWorkspaceEdit, SemanticEditResult, SkippedEdit, WriteFreePackage,
    WriteFreePatch, build_write_free_package, normalize_workspace_edit, semantic_refactor,
    semantic_rename,
};
pub use explain::{
    AnchorFeatures, CfgVerdict, CompilerView, ExplainConflict, ExplainFragment, ExplainProvenance,
    ExplainSourceRef, FeatureSelection, RaObligations, RaStatus, anchor_feature_maps, cfg_view,
    evaluate_cfg, expand_macro, extract_expected_found, failed_obligations, feature_selection,
    find_cfg_attributes, macro_compiler_view, obligation_conflicts, parse_cfg, parse_expand_macro,
    parse_failed_obligations, related_source_fragments, resolve_anchor_line, select_diagnostics,
    source_sha256, trait_compiler_view, trait_hint,
};
pub use navigation::{
    DocumentSymbolEntry, NavigationLocation, document_symbols, symbol_hierarchy,
    symbol_implementations,
};
pub use profile::{
    BudgetSnapshot, ChangeBinding, CompareRequest, ComparisonSide, ConditionSnapshot,
    ConditionValue, ConfigurationSnapshot, CriticalPath, CriticalPathUnit, PhaseDelta,
    ProfileBudget, ProfileComparison, ProfileExplanation, ProfilePhase, ProfileRebuildReport,
    ProfileRecord, ProfileRequest, ProfileService, TimingsReport, build_critical_path,
    extract_timing_units,
};
pub use runtime::{
    RuntimeBinding, RuntimeCompareRequest, RuntimeCompareService, RuntimeComparison,
    RuntimeConditions, RuntimeExperiment, RuntimeGateResult, RuntimeGates, RuntimeInterpretation,
    RuntimeMeasurements, RuntimeSample, RuntimeSeries, UnavailableMetric,
};
pub use symbol::{
    DefinitionLocation, LspPosition, LspRange, SymbolEntry, ToolError, display_path,
    file_path_from_uri, find_symbol_column, flatten_symbols, match_symbol, match_symbol_candidates,
    read_workspace_file, read_workspace_file_with_hook, resolve_asset_path, snapshot_rust_files,
    symbol_definition, symbol_hover, symbol_hover_at_position, symbol_references,
    with_lsp_authority, with_lsp_cancellation, with_rust_document, with_symbol_position,
};
pub use verify::{
    BudgetOutcome, CellOutcome, RequiredConfigurations, SkippedCellData, VerifyAction,
    VerifyBudget, VerifyOutcome, VerifyRequest, VerifyRunner, VerifyService, VerifyStage,
};
