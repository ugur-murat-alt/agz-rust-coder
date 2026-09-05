use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use agz_rust_coder::{
    diagnostics::{
        DiagnosticDetail, RenderOptions, SuggestionApplicability, bounded_text, format_diagnostics,
        machine_applicable_package, machine_applicable_package_with_snapshots,
        parse_cargo_build_telemetry, parse_cargo_output, parse_compiler_diagnostics,
        render_diagnostics,
    },
    knowledge::borrow_errors,
};

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/gate/diagnostics")
        .join(path)
}

struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-coder-diagnostics-{label}-{}-{nonce}",
                std::process::id()
            ));
        fs::create_dir_all(path.join("src")).expect("create source directory");
        Self { path }
    }

    fn source(&self, source: &str) {
        fs::write(self.path.join("src/lib.rs"), source).expect("write source");
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn parses_cargo_events_and_preserves_structured_compiler_context() {
    let output = fs::read_to_string(fixture("cargo-output.jsonl")).expect("read fixture");
    let parsed = parse_cargo_output(&output);

    assert!(parsed.untrusted_data);
    let build = parsed.build.expect("build telemetry");
    assert_eq!(build.total_units, 2);
    assert_eq!(build.fresh_units, 1);
    assert_eq!(build.rebuilt_units, 1);
    assert_eq!(build.build_scripts, 1);
    assert_eq!(build.linked_units, 1);

    let diagnostic = &parsed.diagnostics[0];
    assert_eq!(diagnostic.code.as_deref(), Some("E0308"));
    assert_eq!(diagnostic.file.as_deref(), Some("src/lib.rs"));
    assert_eq!(diagnostic.line, Some(1));
    assert_eq!(
        diagnostic.spans[0].label.as_deref(),
        Some("expected a name")
    );
    assert_eq!(
        diagnostic.spans[0]
            .expansion
            .as_ref()
            .and_then(|expansion| expansion.macro_decl_name.as_deref()),
        Some("demo!")
    );
    assert_eq!(
        diagnostic.spans[0]
            .expansion
            .as_ref()
            .and_then(|expansion| expansion.definition_span.as_ref())
            .map(|span| span.file.as_str()),
        Some("src/macros.rs")
    );
    assert_eq!(
        diagnostic.children[0].children[0].message,
        "nested explanation"
    );
    assert_eq!(diagnostic.suggestions[0].edits.len(), 2);
    assert!(
        diagnostic
            .rendered
            .as_deref()
            .is_some_and(|rendered| rendered.contains("error[E0308]"))
    );
    assert!(
        !diagnostic
            .rendered
            .as_deref()
            .unwrap_or_default()
            .contains('\u{1b}')
    );
}

#[test]
fn parses_short_e_codes_deduplicates_json_and_ignores_other_lines() {
    let short = fs::read_to_string(fixture("short-output.txt")).expect("read short fixture");
    let diagnostics = parse_compiler_diagnostics(&short);
    assert_eq!(diagnostics.len(), 3);
    assert_eq!(diagnostics[0].code.as_deref(), Some("E0382"));
    assert_eq!(diagnostics[1].code, None);
    assert_eq!(diagnostics[2].code.as_deref(), Some("E0502"));
    assert!(parse_compiler_diagnostics("Compiling demo\nFinished dev\n").is_empty());

    let json = fs::read_to_string(fixture("cargo-output.jsonl")).expect("read fixture");
    let duplicate = format!(
        "{}\n{}",
        json.lines().last().unwrap_or_default(),
        json.lines().last().unwrap_or_default()
    );
    assert_eq!(parse_compiler_diagnostics(&duplicate).len(), 1);
    assert_eq!(parse_cargo_build_telemetry(&short), None);
}

#[test]
fn strips_ansi_osc_and_control_characters_from_all_text_fields() {
    let output =
        "\u{1b}[31msrc/lib.rs:1:1: error[E0001]: red\u{1b}[0m\u{7}\n\u{1b}]0;title\u{7}\u{1f}done";
    let parsed = parse_compiler_diagnostics(output);
    assert_eq!(parsed[0].message, "red");
    assert!(!format_diagnostics(&parsed, 5, DiagnosticDetail::Full).contains('\u{1b}'));
    assert!(!format_diagnostics(&parsed, 5, DiagnosticDetail::Full).contains('\u{1f}'));
}

#[test]
fn packages_complete_machine_suggestions_without_writing_source() {
    let root = TestRoot::new("complete");
    let original = fs::read_to_string(fixture("source/lib.rs")).expect("read source fixture");
    root.source(&original);
    let output = fs::read_to_string(fixture("cargo-output.jsonl")).expect("read fixture");
    let diagnostics = parse_compiler_diagnostics(&output);
    let package = machine_applicable_package(&root.path, &diagnostics);

    assert_eq!(package.patches.len(), 2);
    assert!(package.skipped.is_empty());
    assert_eq!(
        fs::read_to_string(root.path.join("src/lib.rs")).expect("read source"),
        original
    );
    let mut applied = original;
    for patch in package.patches {
        assert!(applied.contains(&patch.old_string));
        applied = applied.replacen(&patch.old_string, &patch.new_string, 1);
    }
    assert_eq!(applied, "let y = 2;\n");
}

#[test]
fn rejects_non_machine_and_invalid_multi_edit_suggestions_atomically() {
    let root = TestRoot::new("atomic");
    root.source("let x = 1;\n");
    let output = fs::read_to_string(fixture("cargo-output.jsonl")).expect("read fixture");
    let mut diagnostics = parse_compiler_diagnostics(&output);
    diagnostics[0].suggestions[0].edits[1].file = "../outside.rs".to_owned();
    let package = machine_applicable_package(&root.path, &diagnostics);
    assert!(package.patches.is_empty());
    assert_eq!(package.skipped.len(), 2);
    assert!(
        package
            .skipped
            .iter()
            .all(|item| item.reason.contains("atomically"))
    );

    diagnostics[0].suggestions[0].applicability = SuggestionApplicability::MaybeIncorrect;
    let package = machine_applicable_package(&root.path, &diagnostics);
    assert!(package.patches.is_empty());
    assert!(package.skipped.is_empty());
}

#[test]
fn requires_regular_rust_sources_and_matching_pre_request_snapshots() {
    let root = TestRoot::new("snapshot");
    root.source("let x = 1;\n");
    let output = fs::read_to_string(fixture("cargo-output.jsonl")).expect("read fixture");
    let diagnostics = parse_compiler_diagnostics(&output);
    let mut snapshots = BTreeMap::new();
    snapshots.insert("src/lib.rs".to_owned(), "let changed = 1;\n".to_owned());
    let package = machine_applicable_package_with_snapshots(&root.path, &diagnostics, &snapshots);
    assert!(package.patches.is_empty());
    assert_eq!(package.skipped.len(), 2);
    assert!(package.skipped[0].reason.contains("snapshot"));

    let mut incomplete = diagnostics.clone();
    incomplete[0].suggestions[0].edits[0].range_complete = false;
    let package = machine_applicable_package(&root.path, &incomplete);
    assert!(package.patches.is_empty());
    assert_eq!(package.skipped.len(), 2);

    let mut non_source = diagnostics;
    non_source[0].suggestions[0].edits[0].file = "src".to_owned();
    let package = machine_applicable_package(&root.path, &non_source);
    assert!(package.patches.is_empty());
    assert_eq!(package.skipped.len(), 2);
}

#[test]
fn rejects_overlapping_suggestions_and_accepts_utf8_byte_ranges() {
    let root = TestRoot::new("ranges");
    root.source("let café = 1;\n");
    let mut diagnostics = parse_compiler_diagnostics(
        r#"{"reason":"compiler-message","message":{"level":"error","message":"unicode","spans":[{"file_name":"src/lib.rs","line_start":1,"line_end":1,"column_start":8,"column_end":9,"byte_start":7,"byte_end":9,"is_primary":true,"suggested_replacement":"e","suggestion_applicability":"MachineApplicable"}]}}"#,
    );
    let package = machine_applicable_package(&root.path, &diagnostics);
    assert_eq!(package.patches.len(), 1);
    assert_eq!(package.patches[0].new_string, "let cafe = 1;\n");

    let mut overlapping = diagnostics.pop().expect("unicode diagnostic");
    let duplicate_edit = overlapping.suggestions[0].edits[0].clone();
    overlapping.suggestions[0].edits.push(duplicate_edit);
    let package = machine_applicable_package(&root.path, &[overlapping]);
    assert!(package.patches.is_empty());
    assert_eq!(package.skipped.len(), 2);
}

#[test]
fn renders_bounded_utf8_safe_detail_levels_and_typed_json() {
    let output = fs::read_to_string(fixture("cargo-output.jsonl")).expect("read fixture");
    let diagnostics = parse_compiler_diagnostics(&output);
    let compact = render_diagnostics(
        &diagnostics,
        RenderOptions::for_detail(DiagnosticDetail::Compact, 512),
    );
    let standard = render_diagnostics(
        &diagnostics,
        RenderOptions::for_detail(DiagnosticDetail::Standard, 4_096),
    );
    let full = render_diagnostics(
        &diagnostics,
        RenderOptions::for_detail(DiagnosticDetail::Full, 64),
    );
    assert!(!compact.text.contains("nested explanation"));
    assert!(standard.text.contains("nested explanation"));
    assert!(full.text.len() <= 64);
    assert!(full.truncated);
    assert!(full.text.is_char_boundary(full.text.len()));
    assert!(serde_json::to_string(&full).is_ok());

    let text = bounded_text(&"é漢字".repeat(40), 17);
    assert!(text.len() <= 17);
    assert!(text.is_char_boundary(text.len()));
}

#[test]
fn exposes_borrow_hints_for_compiler_codes() {
    assert!(borrow_errors::hint_for("E0382").is_some());
    assert!(borrow_errors::hint_for("E0596").is_some());
    assert!(borrow_errors::hint_for("E9999").is_none());
    assert!(borrow_errors::EXPLAIN_ADVICE.contains("rustc --explain"));
}

#[test]
fn suggestion_sources_larger_than_the_snapshot_budget_are_rejected() {
    let root = TestRoot::new("bounded");
    let file = fs::File::create(root.path.join("src/lib.rs")).unwrap();
    file.set_len(agz_rust_coder::diagnostics::MAX_SOURCE_SNAPSHOT_BYTES + 1)
        .unwrap();
    let diagnostics = parse_compiler_diagnostics(
        r#"{"reason":"compiler-message","message":{"level":"error","message":"bounded","spans":[{"file_name":"src/lib.rs","line_start":1,"line_end":1,"column_start":1,"column_end":2,"byte_start":0,"byte_end":1,"is_primary":true,"suggested_replacement":"x","suggestion_applicability":"MachineApplicable"}]}}"#,
    );
    let package = machine_applicable_package(&root.path, &diagnostics);
    assert!(package.patches.is_empty());
    assert!(!package.skipped.is_empty());
    assert_eq!(
        file.metadata().unwrap().len(),
        agz_rust_coder::diagnostics::MAX_SOURCE_SNAPSHOT_BYTES + 1
    );
}
