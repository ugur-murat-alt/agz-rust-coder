//! Rust Analyzer transport and workspace-scoped process lifecycle.

pub mod client;
pub mod documents;
pub mod manager;
pub mod normalize;

pub use crate::config::WorkspaceCode;

pub use client::{
    CONTENT_MODIFIED_CODE, DocumentSnapshot, DocumentSyncGuard, DocumentSyncOptions, FrameDecoder,
    FrameError, LspClient, LspError, Position, Range, TextDocumentChange, incremental_change,
    parse_frames, position_at_byte, value_position, value_range,
};
pub use manager::{
    BinarySchemaProbe, ClientCallbacks, ClientFuture, ClientLease, ClientRef, CloseReport,
    ConcreteBinarySchemaProbe, ConcreteClientAdapter, ConcreteClientFactory, LspClientFactory,
    LspClientLike, ManagerError, ManagerOptions, ProbeError, ProbeFuture, RustAnalyzerCapabilities,
    RustAnalyzerManager,
};
pub use normalize::{
    BinaryConfigSchema, NormalizeError, SchemaError, document_sync_options, path_to_file_uri,
    resolve_binary_path,
};
