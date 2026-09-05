use agz_rust_coder::diagnostics::CargoStream;
use proptest::prelude::*;
use serde_json::json;

fn diagnostic(level: &str, message: &str, package: &str, column: usize) -> String {
    json!({"reason":"compiler-message","package_id":package,"target":{"name":"lib","kind":["lib"]},
        "message":{"level":level,"message":message,"code":{"code":"E0308"},"children":[],
        "spans":[{"file_name":"src/lib.rs","line_start":1,"line_end":1,
        "column_start":column,"column_end":column+1,"is_primary":true}]}})
    .to_string()
        + "\n"
}

#[test]
fn errors_survive_warning_flood_without_unbounded_storage() {
    let mut stream = CargoStream::new(8192, 2, 8192);
    stream.push(diagnostic("warning", "one", "p", 1).as_bytes());
    stream.push(diagnostic("warning", "two", "p", 2).as_bytes());
    assert!(stream.push(diagnostic("error", "important", "p", 3).as_bytes()));
    for _ in 0..1000 {
        stream.push(b"uninteresting log line\n");
    }
    let (output, stats) = stream.finish();
    assert_eq!(output.diagnostics.len(), 2);
    assert!(output.diagnostics.iter().any(|d| d.message == "important"));
    assert_eq!(stats.omitted_records, 1);
    assert!(output.truncated);
}

#[test]
fn detects_corruption_and_recovers_at_next_line() {
    let mut stream = CargoStream::new(1024, 8, 8192);
    stream.push(&vec![b'x'; 2048]);
    stream.push(b"\n{bad\n{\"reason\":\"compiler-message\",\"message\":null}\n\xff\n");
    stream.push(diagnostic("error", "survives", "p", 1).as_bytes());
    stream.push(b"{\"reason\":\"build-finished\",\"success\":false}");
    let (output, stats) = stream.finish();
    assert_eq!(stats.oversized_lines, 1);
    assert_eq!(stats.malformed_lines, 3);
    assert_eq!(stats.build_success, Some(false));
    assert_eq!(output.diagnostics.len(), 1);
}

#[test]
fn deduplication_preserves_different_packages_and_columns() {
    let mut stream = CargoStream::default();
    for (p, col) in [("one", 1), ("one", 1), ("two", 1), ("one", 2)] {
        stream.push(diagnostic("error", "mismatch", p, col).as_bytes());
    }
    let (output, stats) = stream.finish();
    assert_eq!(stats.duplicates, 1);
    assert_eq!(output.diagnostics.len(), 3);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn arbitrary_utf8_is_independent_of_chunk_boundaries(message in ".{1,160}", chunk in 1usize..128) {
        let input=diagnostic("error", &message, "package", 2);
        let mut whole=CargoStream::default(); whole.push(input.as_bytes());
        let mut split=CargoStream::default();
        for bytes in input.as_bytes().chunks(chunk) { split.push(bytes); }
        let (a, sa)=whole.finish(); let (b,sb)=split.finish();
        prop_assert_eq!(a.diagnostics,b.diagnostics);
        prop_assert_eq!(sa,sb);
    }
}

#[test]
fn future_cargo_records_are_not_mislabeled_as_corruption() {
    let mut stream = CargoStream::default();
    stream.push(b"{\"reason\":\"future-cargo-event\",\"message\":42}\n");
    let (_, stats) = stream.finish();
    assert_eq!(stats.malformed_lines, 0);
}
