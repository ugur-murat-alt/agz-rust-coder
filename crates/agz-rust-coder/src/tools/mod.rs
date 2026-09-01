//! Domain-level semantic tools. The MCP server adapter remains outside this
//! module; these functions return bounded, source-write-free results.

pub mod audit;
pub mod check;
pub mod crate_lookup;
pub mod edits;
pub mod navigation;
pub mod symbol;

pub use audit::{
    AuditCancellation, AuditCancellationReason, AuditError, AuditFinding, AuditLimits,
    AuditRequest, AuditService, AuditSkip, AuditSkipReason, AuditSummary,
};
pub use check::CheckService;
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
pub use navigation::{
    DocumentSymbolEntry, NavigationLocation, document_symbols, symbol_hierarchy,
    symbol_implementations,
};
pub use symbol::{
    DefinitionLocation, LspPosition, LspRange, SymbolEntry, ToolError, display_path,
    file_path_from_uri, find_symbol_column, flatten_symbols, match_symbol, match_symbol_candidates,
    read_workspace_file, read_workspace_file_with_hook, resolve_asset_path, snapshot_rust_files,
    symbol_definition, symbol_hover, symbol_hover_at_position, symbol_references,
    with_lsp_cancellation, with_rust_document, with_symbol_position,
};
