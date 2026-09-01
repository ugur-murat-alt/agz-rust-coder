use std::collections::BTreeMap;

use serde::Deserialize;

use super::model::{
    CargoBuildTelemetry, CargoOutput, CompilerSuggestion, CompilerSuggestionEdit, Diagnostic,
    DiagnosticChild, DiagnosticLevel, DiagnosticSpan, MacroExpansion, SpanText,
    SuggestionApplicability,
};

#[derive(Debug, Deserialize)]
struct CargoMessage {
    reason: Option<String>,
    message: Option<RawDiagnostic>,
    fresh: Option<bool>,
    executable: Option<String>,
    target: Option<RawTarget>,
}

#[derive(Debug, Deserialize)]
struct RawTarget {
    kind: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawDiagnostic {
    code: Option<RawCode>,
    level: Option<String>,
    message: Option<String>,
    #[serde(default)]
    spans: Vec<RawSpan>,
    #[serde(default)]
    children: Vec<RawDiagnostic>,
    rendered: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawCode {
    Object { code: Option<String> },
    String(String),
}

#[derive(Debug, Deserialize)]
struct RawSpan {
    file_name: Option<String>,
    byte_start: Option<u64>,
    byte_end: Option<u64>,
    line_start: Option<u64>,
    line_end: Option<u64>,
    column_start: Option<u64>,
    column_end: Option<u64>,
    is_primary: Option<bool>,
    label: Option<String>,
    suggested_replacement: Option<String>,
    suggestion_applicability: Option<String>,
    expansion: Option<RawExpansion>,
    #[serde(default)]
    text: Vec<RawSpanText>,
}

#[derive(Debug, Deserialize)]
struct RawSpanText {
    text: Option<String>,
    highlight_start: Option<u64>,
    highlight_end: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawExpansion {
    span: Option<Box<RawSpan>>,
    macro_decl_name: Option<String>,
    def_site_span: Option<Box<RawSpan>>,
}

/// Parse Cargo's line-delimited JSON and its short human-readable fallback.
pub fn parse_cargo_output(output: &str) -> CargoOutput {
    let clean_output = sanitize_text(output);
    let mut diagnostics = Vec::new();
    let mut indexes = BTreeMap::new();
    let mut build = CargoBuildTelemetry {
        total_units: 0,
        fresh_units: 0,
        rebuilt_units: 0,
        build_scripts: 0,
        linked_units: 0,
    };

    for line in clean_output.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();
        let mut handled_json = false;
        if trimmed.starts_with('{') {
            if let Ok(message) = serde_json::from_str::<CargoMessage>(trimmed) {
                handled_json = true;
                match message.reason.as_deref() {
                    Some("compiler-artifact") => {
                        build.total_units = build.total_units.saturating_add(1);
                        if message.fresh.unwrap_or(false) {
                            build.fresh_units = build.fresh_units.saturating_add(1);
                        } else {
                            build.rebuilt_units = build.rebuilt_units.saturating_add(1);
                        }
                        let executable = message
                            .executable
                            .as_deref()
                            .is_some_and(|path| !path.is_empty());
                        let link_kind = message
                            .target
                            .as_ref()
                            .and_then(|target| target.kind.as_ref())
                            .is_some_and(|kinds| {
                                kinds.iter().any(|kind| {
                                    matches!(
                                        kind.as_str(),
                                        "bin" | "cdylib" | "dylib" | "proc-macro"
                                    )
                                })
                            });
                        if executable || link_kind {
                            build.linked_units = build.linked_units.saturating_add(1);
                        }
                    }
                    Some("build-script-executed") => {
                        build.build_scripts = build.build_scripts.saturating_add(1);
                    }
                    Some("compiler-message") => {
                        if let Some(raw) = message.message {
                            if let Some(diagnostic) = normalize_diagnostic(raw) {
                                insert_diagnostic(&mut diagnostics, &mut indexes, diagnostic);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !handled_json {
            if let Some(diagnostic) = parse_short_diagnostic_line(line) {
                insert_diagnostic(&mut diagnostics, &mut indexes, diagnostic);
            }
        }
    }

    CargoOutput {
        untrusted_data: !clean_output.trim().is_empty(),
        truncated: false,
        diagnostics,
        build: (!build.is_empty()).then_some(build),
    }
}

pub fn parse_compiler_diagnostics(output: &str) -> Vec<Diagnostic> {
    parse_cargo_output(output).diagnostics
}

pub fn parse_cargo_build_telemetry(output: &str) -> Option<CargoBuildTelemetry> {
    parse_cargo_output(output).build
}

pub fn parse_short_diagnostic_line(line: &str) -> Option<Diagnostic> {
    let line = sanitize_text(line);
    let (severity_start, marker) = [": error", ": warning"]
        .iter()
        .filter_map(|marker| line.find(marker).map(|index| (index, *marker)))
        .min_by_key(|(index, _)| *index)?;
    let level = if marker == ": error" {
        DiagnosticLevel::Error
    } else {
        DiagnosticLevel::Warning
    };
    let prefix = &line[..severity_start];
    let mut parts = prefix.rsplitn(3, ':');
    let column = parts.next()?.parse::<usize>().ok()?;
    let line_number = parts.next()?.parse::<usize>().ok()?;
    let file = parts.next()?.trim();
    if file.is_empty() || line_number == 0 || column == 0 {
        return None;
    }

    let rest = &line[severity_start + marker.len()..];
    let (code, message) = if let Some(rest) = rest.strip_prefix('[') {
        let close = rest.find(']')?;
        let candidate = &rest[..close];
        if !is_e_code(candidate) {
            return None;
        }
        let code = Some(candidate.to_owned());
        let message = rest[close + 1..]
            .strip_prefix(':')
            .unwrap_or(&rest[close + 1..]);
        (code, message.trim())
    } else {
        (None, rest.strip_prefix(':').unwrap_or(rest).trim())
    };

    Some(Diagnostic {
        code,
        level,
        file: Some(file.to_owned()),
        line: Some(line_number),
        message: sanitize_text(message),
        root_key: None,
        rendered: None,
        spans: Vec::new(),
        children: Vec::new(),
        suggestions: Vec::new(),
        untrusted_data: true,
    })
}

pub fn sanitize_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut state = EscapeState::Normal;
    for character in input.chars() {
        match state {
            EscapeState::Normal => match character {
                '\u{1b}' => state = EscapeState::Escape,
                '\u{9b}' => state = EscapeState::Csi,
                '\u{9d}' => state = EscapeState::Osc,
                '\n' | '\r' | '\t' => output.push(character),
                character if character.is_control() => {}
                character => output.push(character),
            },
            EscapeState::Escape => match character {
                '[' => state = EscapeState::Csi,
                ']' => state = EscapeState::Osc,
                _ => state = EscapeState::Normal,
            },
            EscapeState::Csi => {
                if ('@'..='~').contains(&character) {
                    state = EscapeState::Normal;
                }
            }
            EscapeState::Osc => match character {
                '\u{7}' | '\u{9c}' => state = EscapeState::Normal,
                '\u{1b}' => state = EscapeState::OscEscape,
                _ => {}
            },
            EscapeState::OscEscape => {
                state = if character == '\\' || character == '\u{9c}' {
                    EscapeState::Normal
                } else if character == '\u{1b}' {
                    EscapeState::OscEscape
                } else {
                    EscapeState::Osc
                };
            }
        }
    }
    output
}

fn normalize_diagnostic(raw: RawDiagnostic) -> Option<Diagnostic> {
    let level = match raw.level.as_deref() {
        Some("error") => DiagnosticLevel::Error,
        Some("warning") => DiagnosticLevel::Warning,
        _ => return None,
    };
    let spans = raw
        .spans
        .into_iter()
        .filter_map(normalize_span)
        .collect::<Vec<_>>();
    let primary = spans
        .iter()
        .find(|span| span.is_primary)
        .or_else(|| spans.first());
    let children = raw
        .children
        .into_iter()
        .map(normalize_child)
        .collect::<Vec<_>>();
    let suggestions = collect_suggestions(&raw.message, &spans, &children);
    let mut diagnostic = Diagnostic {
        code: raw.code.and_then(normalize_code),
        level,
        file: primary.map(|span| span.file.clone()),
        line: primary.map(|span| span.line_start),
        message: sanitize_text(raw.message.as_deref().unwrap_or_default()),
        root_key: None,
        rendered: raw.rendered.map(|rendered| sanitize_text(&rendered)),
        spans,
        children,
        suggestions,
        untrusted_data: true,
    };
    diagnostic.root_key = Some(root_key(&diagnostic));
    Some(diagnostic)
}

fn normalize_code(raw: RawCode) -> Option<String> {
    let code = match raw {
        RawCode::Object { code } => code,
        RawCode::String(code) => Some(code),
    }?;
    let code = sanitize_text(&code);
    (!code.is_empty()).then_some(code)
}

fn normalize_child(raw: RawDiagnostic) -> DiagnosticChild {
    DiagnosticChild {
        level: sanitize_text(raw.level.as_deref().unwrap_or("help")),
        message: sanitize_text(raw.message.as_deref().unwrap_or_default()),
        rendered: raw.rendered.map(|rendered| sanitize_text(&rendered)),
        spans: raw.spans.into_iter().filter_map(normalize_span).collect(),
        children: raw.children.into_iter().map(normalize_child).collect(),
    }
}

fn normalize_span(raw: RawSpan) -> Option<DiagnosticSpan> {
    let file = raw
        .file_name
        .as_deref()
        .map(sanitize_text)
        .unwrap_or_default();
    if file.is_empty() && raw.suggested_replacement.is_none() {
        return None;
    }
    let range_complete = raw.line_start.is_some()
        && raw.line_end.is_some()
        && raw.column_start.is_some()
        && raw.column_end.is_some();
    let span = DiagnosticSpan {
        file,
        byte_start: raw.byte_start,
        byte_end: raw.byte_end,
        line_start: raw.line_start.and_then(as_usize).unwrap_or(0),
        line_end: raw
            .line_end
            .or(raw.line_start)
            .and_then(as_usize)
            .unwrap_or(0),
        column_start: raw.column_start.and_then(as_usize).unwrap_or(1),
        column_end: raw
            .column_end
            .or(raw.column_start)
            .and_then(as_usize)
            .unwrap_or(1),
        is_primary: raw.is_primary.unwrap_or(false),
        label: raw.label.map(|label| sanitize_text(&label)),
        suggested_replacement: raw
            .suggested_replacement
            .map(|replacement| sanitize_text(&replacement)),
        suggestion_applicability: raw
            .suggestion_applicability
            .as_deref()
            .and_then(parse_applicability),
        expansion: raw.expansion.and_then(normalize_expansion),
        text: raw
            .text
            .into_iter()
            .filter_map(|text| {
                Some(SpanText {
                    text: sanitize_text(text.text.as_deref()?),
                    highlight_start: text.highlight_start.and_then(as_usize).unwrap_or(0),
                    highlight_end: text.highlight_end.and_then(as_usize).unwrap_or(0),
                })
            })
            .collect(),
        range_complete,
    };
    Some(span)
}

fn normalize_expansion(raw: RawExpansion) -> Option<MacroExpansion> {
    Some(MacroExpansion {
        macro_decl_name: raw
            .macro_decl_name
            .map(|name| sanitize_text(&name))
            .filter(|name| !name.is_empty()),
        span: Box::new(normalize_span(*raw.span?)?),
        definition_span: raw
            .def_site_span
            .and_then(|span| normalize_span(*span).map(Box::new)),
    })
}

fn collect_suggestions(
    root_message: &Option<String>,
    root_spans: &[DiagnosticSpan],
    children: &[DiagnosticChild],
) -> Vec<CompilerSuggestion> {
    let mut suggestions = Vec::new();
    add_suggestion(
        &mut suggestions,
        root_message
            .as_deref()
            .map(sanitize_text)
            .unwrap_or_else(|| "compiler suggestion".to_owned()),
        root_spans,
    );
    for child in children {
        collect_child_suggestions(&mut suggestions, child);
    }
    suggestions
}

fn collect_child_suggestions(suggestions: &mut Vec<CompilerSuggestion>, child: &DiagnosticChild) {
    add_suggestion(suggestions, child.message.clone(), &child.spans);
    for nested in &child.children {
        collect_child_suggestions(suggestions, nested);
    }
}

fn add_suggestion(
    suggestions: &mut Vec<CompilerSuggestion>,
    message: String,
    spans: &[DiagnosticSpan],
) {
    let spans = spans
        .iter()
        .filter(|span| span.suggested_replacement.is_some())
        .collect::<Vec<_>>();
    if spans.is_empty() {
        return;
    }
    let applicability = spans
        .iter()
        .filter_map(|span| span.suggestion_applicability)
        .max_by_key(|applicability| applicability.rank())
        .unwrap_or(SuggestionApplicability::Unspecified);
    suggestions.push(CompilerSuggestion {
        message,
        applicability,
        edits: spans
            .into_iter()
            .map(|span| CompilerSuggestionEdit {
                file: span.file.clone(),
                line_start: span.line_start,
                line_end: span.line_end,
                column_start: span.column_start,
                column_end: span.column_end,
                replacement: span.suggested_replacement.clone().unwrap_or_default(),
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                range_complete: span.range_complete,
            })
            .collect(),
    });
}

fn parse_applicability(value: &str) -> Option<SuggestionApplicability> {
    match value {
        "MachineApplicable" => Some(SuggestionApplicability::MachineApplicable),
        "MaybeIncorrect" => Some(SuggestionApplicability::MaybeIncorrect),
        "HasPlaceholders" => Some(SuggestionApplicability::HasPlaceholders),
        "Unspecified" => Some(SuggestionApplicability::Unspecified),
        _ => None,
    }
}

fn insert_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    indexes: &mut BTreeMap<String, usize>,
    mut diagnostic: Diagnostic,
) {
    let key = diagnostic
        .root_key
        .clone()
        .unwrap_or_else(|| root_key(&diagnostic));
    diagnostic.root_key = Some(key.clone());
    if let Some(index) = indexes.get(&key).copied() {
        if detail_score(&diagnostic) > detail_score(&diagnostics[index]) {
            diagnostics[index] = diagnostic;
        }
    } else {
        indexes.insert(key, diagnostics.len());
        diagnostics.push(diagnostic);
    }
}

pub fn root_key(diagnostic: &Diagnostic) -> String {
    let primary = diagnostic
        .spans
        .iter()
        .find(|span| span.is_primary)
        .or_else(|| diagnostic.spans.first());
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        diagnostic.code.as_deref().unwrap_or_default(),
        diagnostic.level,
        diagnostic.file.as_deref().unwrap_or_default(),
        diagnostic.line.unwrap_or_default(),
        primary.map_or(0, |span| span.column_start),
        primary.map_or(0, |span| span.line_end),
        primary.map_or(0, |span| span.column_end),
        collapse_whitespace(&diagnostic.message),
    )
}

fn detail_score(diagnostic: &Diagnostic) -> usize {
    diagnostic.spans.len()
        + diagnostic.children.len() * 2
        + diagnostic.suggestions.len() * 4
        + diagnostic.rendered.as_ref().map_or(0, String::len) / 1_000
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn as_usize(value: u64) -> Option<usize> {
    usize::try_from(value).ok()
}

fn is_e_code(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next() == Some('E')
        && chars.next().is_some_and(|first| first.is_ascii_digit())
        && chars.all(|character| character.is_ascii_digit())
}

#[derive(Debug, Clone, Copy)]
enum EscapeState {
    Normal,
    Escape,
    Csi,
    Osc,
    OscEscape,
}
