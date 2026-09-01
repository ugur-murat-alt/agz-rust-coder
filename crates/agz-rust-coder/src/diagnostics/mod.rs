#![allow(dead_code, unused_imports)]

mod model;
mod parser;
mod render;
mod suggestions;

pub use model::{
    AdvisoryEdit, CargoBuildTelemetry, CargoOutput, CompilerSuggestion, CompilerSuggestionEdit,
    DEFAULT_RENDER_BYTES, Diagnostic, DiagnosticChild, DiagnosticDetail, DiagnosticLevel,
    DiagnosticSpan, MAX_SOURCE_SNAPSHOT_BYTES, MacroExpansion, RenderOptions, RenderedDiagnostic,
    RenderedDiagnostics, RenderedSuggestion, SkippedEdit, SourcePosition, SourceRange, SpanText,
    StructuredDiagnostics, SuggestionApplicability, WriteFreePackage, WriteFreePatch,
};
pub use parser::{
    parse_cargo_build_telemetry, parse_cargo_output, parse_compiler_diagnostics,
    parse_short_diagnostic_line, root_key, sanitize_text,
};
pub use render::{
    bounded_text, format_diagnostics, render_diagnostic, render_diagnostics, truncate_utf8,
};
pub use suggestions::{
    SnapshotLookup, advisory_edit, machine_applicable_package,
    machine_applicable_package_with_snapshots,
};
