use std::fmt::Write;

use super::{
    Diagnostic, DiagnosticChild, DiagnosticDetail, RenderOptions, RenderedDiagnostic,
    RenderedDiagnostics, RenderedSuggestion, StructuredDiagnostics, sanitize_text,
};

pub fn render_diagnostics(
    diagnostics: &[Diagnostic],
    options: RenderOptions,
) -> RenderedDiagnostics {
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic.level, super::DiagnosticLevel::Error))
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic.level, super::DiagnosticLevel::Warning))
        .count();
    let mut ordered = diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic.level, super::DiagnosticLevel::Error))
        .chain(
            diagnostics
                .iter()
                .filter(|diagnostic| matches!(diagnostic.level, super::DiagnosticLevel::Warning)),
        );
    let selected = ordered
        .by_ref()
        .take(options.max_diagnostics)
        .collect::<Vec<_>>();
    let truncated_by_count = diagnostics.len() > selected.len();
    let mut text = String::new();
    let _ = writeln!(text, "Errors: {errors}, warnings: {warnings}");
    let mut structured = Vec::with_capacity(selected.len());
    for diagnostic in selected {
        let message = bounded_text(
            &sanitize_text(&diagnostic.message),
            options.max_bytes.min(4_000),
        );
        let location = bounded_text(
            &sanitize_text(diagnostic.file.as_deref().unwrap_or("-")),
            options.max_bytes.min(2_000),
        );
        let code = diagnostic
            .code
            .as_deref()
            .map(|code| bounded_text(&sanitize_text(code), options.max_bytes.min(256)));
        let _ = write!(
            text,
            "- {}{} {}: {}",
            location,
            diagnostic
                .line
                .map_or_else(String::new, |line| format!(":{line}")),
            code.as_deref()
                .map_or(String::new(), |code| format!("[{code}]")),
            message
        );
        text.push('\n');

        let mut rendered_suggestions = Vec::new();
        for suggestion in diagnostic
            .suggestions
            .iter()
            .take(options.detail.suggestion_limit())
        {
            let message = bounded_text(
                &sanitize_text(&suggestion.message),
                options.max_bytes.min(2_000),
            );
            let _ = writeln!(
                text,
                "    suggestion({}): {message}",
                suggestion.applicability.as_str()
            );
            rendered_suggestions.push(RenderedSuggestion {
                message,
                applicability: suggestion.applicability,
                edit_count: suggestion.edits.len(),
            });
        }

        let mut rendered_children = Vec::new();
        if options.detail.child_limit() > 0 {
            for child in diagnostic
                .children
                .iter()
                .take(options.detail.child_limit())
            {
                render_child(&mut text, &mut rendered_children, child, options.detail, 4);
            }
        }
        let rendered = if options.detail.rendered_limit() > 0 {
            diagnostic.rendered.as_deref().map(|rendered| {
                bounded_text(&sanitize_text(rendered), options.detail.rendered_limit())
            })
        } else {
            None
        };
        if let Some(rendered_text) = &rendered {
            let _ = writeln!(text, "    rendered: {rendered_text}");
        }
        structured.push(RenderedDiagnostic {
            code,
            level: diagnostic.level,
            file: diagnostic
                .file
                .as_deref()
                .map(|file| bounded_text(&sanitize_text(file), options.max_bytes.min(2_000))),
            line: diagnostic.line,
            message,
            rendered,
            suggestions: rendered_suggestions,
            children: rendered_children,
        });
    }

    let (text, truncated_by_bytes) = bound_text(text, options.max_bytes);
    RenderedDiagnostics {
        text,
        structured: StructuredDiagnostics {
            errors,
            warnings,
            diagnostics: structured,
        },
        untrusted_data: !diagnostics.is_empty(),
        truncated: truncated_by_count || truncated_by_bytes,
    }
}

pub fn format_diagnostics(
    diagnostics: &[Diagnostic],
    max_diagnostics: usize,
    detail: DiagnosticDetail,
) -> String {
    let mut options = RenderOptions::for_detail(detail, super::DEFAULT_RENDER_BYTES);
    options.max_diagnostics = max_diagnostics.min(detail.default_diagnostic_limit());
    render_diagnostics(diagnostics, options).text
}

pub fn render_diagnostic(diagnostic: &Diagnostic, detail: DiagnosticDetail) -> String {
    render_diagnostics(
        std::slice::from_ref(diagnostic),
        RenderOptions::for_detail(detail, super::DEFAULT_RENDER_BYTES),
    )
    .text
}

pub fn bounded_text(input: &str, max_bytes: usize) -> String {
    bound_text(input.to_owned(), max_bytes).0
}

pub fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    bounded_text(input, max_bytes)
}

fn render_child(
    text: &mut String,
    structured: &mut Vec<String>,
    child: &DiagnosticChild,
    detail: DiagnosticDetail,
    indent: usize,
) {
    let level = bounded_text(
        &sanitize_text(&child.level),
        super::DEFAULT_RENDER_BYTES.min(256),
    );
    let message = bounded_text(
        &sanitize_text(&child.message),
        super::DEFAULT_RENDER_BYTES.min(2_000),
    );
    let _ = writeln!(text, "{:indent$}{level}: {message}", "");
    structured.push(format!("{level}: {message}"));
    if detail != DiagnosticDetail::Compact {
        for nested in child.children.iter().take(3) {
            render_child(text, structured, nested, detail, indent + 2);
        }
    }
}

fn bound_text(input: String, max_bytes: usize) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input, false);
    }
    let suffix = "... (truncated)";
    if max_bytes <= suffix.len() {
        let end = floor_char_boundary(&input, max_bytes);
        return (input[..end].to_owned(), true);
    }
    let end = floor_char_boundary(&input, max_bytes - suffix.len());
    (format!("{}{}", &input[..end], suffix), true)
}

fn floor_char_boundary(input: &str, at: usize) -> usize {
    let mut end = at.min(input.len());
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_text_never_splits_utf8() {
        let text = "é漢字".repeat(20);
        let bounded = bounded_text(&text, 7);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.len() <= 7 + "... (truncated)".len());
    }
}
