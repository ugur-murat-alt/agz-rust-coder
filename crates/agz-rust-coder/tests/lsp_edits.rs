use std::{collections::HashMap, fs, path::PathBuf, time::SystemTime};

use agz_rust_coder::{
    lsp::{Position, Range, path_to_file_uri},
    tools::{AdvisoryEdit, build_write_free_package, normalize_workspace_edit},
};
use serde_json::Value;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(source: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "agz-rust-coder-lsp-edits-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("src")).expect("create edit fixture");
        fs::write(root.join("src/lib.rs"), source).expect("write edit fixture");
        Self(fs::canonicalize(root).expect("canonical fixture"))
    }

    fn file(&self) -> PathBuf {
        self.0.join("src/lib.rs")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn range(start: u32, end: u32) -> Range {
    Range {
        start: Position {
            line: 0,
            character: start,
        },
        end: Position {
            line: 0,
            character: end,
        },
    }
}

#[test]
fn changes_and_document_changes_normalize_with_limits_and_resource_operations() {
    let root = TempRoot::new("fn greet() {}\n");
    let uri = path_to_file_uri(&root.file()).expect("file uri");
    let mut versions = HashMap::new();
    versions.insert(PathBuf::from("src/lib.rs"), 7);
    let raw = serde_json::json!({
        "changes": {
            uri.clone(): [
                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "newText": "hello"},
                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "newText": "hello"}
            ]
        },
        "documentChanges": [
            {"textDocument": {"uri": uri, "version": 7}, "edits": [
                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "newText": "welcome", "annotationId": "safe"}
            ]},
            {"kind": "create", "uri": "file:///outside.rs"},
            {"kind": "rename", "oldUri": "file:///a", "newUri": "file:///b"},
            {"kind": "delete", "uri": "file:///outside.rs"}
        ]
    });

    let normalized = normalize_workspace_edit(&root.0, &raw, 2, Some(&versions));
    assert_eq!(normalized.edits.len(), 2);
    assert!(normalized.omitted >= 1);
    assert!(
        normalized
            .unsupported_operations
            .iter()
            .any(|item| item == "create")
    );
    assert!(
        normalized
            .unsupported_operations
            .iter()
            .any(|item| item == "rename")
    );
    assert!(
        normalized
            .unsupported_operations
            .iter()
            .any(|item| item == "delete")
    );
    assert!(
        normalized
            .unsupported_operations
            .iter()
            .any(|item| item.contains("ambiguous"))
    );
}

#[test]
fn stale_version_and_workspace_escape_are_rejected() {
    let root = TempRoot::new("fn greet() {}\n");
    let uri = path_to_file_uri(&root.file()).expect("file uri");
    let raw = serde_json::json!({"documentChanges": [
        {"textDocument": {"uri": uri, "version": 2}, "edits": [
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "newText": "hello"}
        ]},
        {"textDocument": {"uri": "file:///outside.rs", "version": null}, "edits": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "x"}
        ]}
    ]});
    let versions = HashMap::from([(PathBuf::from("src/lib.rs"), 1)]);

    let normalized = normalize_workspace_edit(&root.0, &raw, 10, Some(&versions));
    assert!(normalized.edits.is_empty());
    assert!(
        normalized
            .unsupported_operations
            .iter()
            .any(|item| item.contains("versioned edits rejected"))
    );
    assert!(
        normalized
            .unsupported_operations
            .iter()
            .any(|item| item.contains("outside the workspace"))
    );
}

#[test]
fn utf16_edits_build_ordered_patches_without_writing_source() {
    let source = "fn greet() { let face = \"😀\"; greet(); }\n";
    let root = TempRoot::new(source);
    let before = fs::read(root.file()).expect("read source before");
    let edits = vec![
        AdvisoryEdit {
            file: PathBuf::from("src/lib.rs"),
            range: range(3, 8),
            new_text: "hello".to_owned(),
            version: Some(Some(1)),
            annotation_id: None,
        },
        AdvisoryEdit {
            file: PathBuf::from("src/lib.rs"),
            // The emoji occupies two UTF-16 code units before this occurrence.
            range: range(30, 35),
            new_text: "hello".to_owned(),
            version: Some(None),
            annotation_id: None,
        },
    ];
    let snapshots = HashMap::from([(PathBuf::from("src/lib.rs"), source.to_owned())]);

    let package = build_write_free_package(&root.0, &edits, Some(&snapshots));
    assert_eq!(package.patches.len(), 2, "{package:#?}");
    assert!(package.skipped.is_empty(), "{package:#?}");
    assert_eq!(fs::read(root.file()).expect("read source after"), before);
}

#[test]
fn crlf_edit_positions_exclude_the_line_terminator() {
    let source = "fn greet() {}\r\n";
    let root = TempRoot::new(source);
    let edits = vec![AdvisoryEdit {
        file: PathBuf::from("src/lib.rs"),
        range: range(3, 8),
        new_text: "hello".to_owned(),
        version: None,
        annotation_id: None,
    }];
    let snapshots = HashMap::from([(PathBuf::from("src/lib.rs"), source.to_owned())]);

    let package = build_write_free_package(&root.0, &edits, Some(&snapshots));
    assert_eq!(package.patches.len(), 1, "{package:#?}");
    assert!(package.skipped.is_empty(), "{package:#?}");
}

#[test]
fn overlaps_and_changed_snapshots_are_skipped_without_writes() {
    let source = "fn greet() {}\n";
    let root = TempRoot::new(source);
    let before = fs::read(root.file()).expect("read source before");
    let overlapping = vec![
        AdvisoryEdit {
            file: PathBuf::from("src/lib.rs"),
            range: range(3, 8),
            new_text: "hello".to_owned(),
            version: None,
            annotation_id: None,
        },
        AdvisoryEdit {
            file: PathBuf::from("src/lib.rs"),
            range: range(4, 7),
            new_text: "x".to_owned(),
            version: None,
            annotation_id: None,
        },
    ];
    let snapshots = HashMap::from([(PathBuf::from("src/lib.rs"), source.to_owned())]);
    let overlap = build_write_free_package(&root.0, &overlapping, Some(&snapshots));
    assert_eq!(overlap.patches.len(), 1);
    assert!(
        overlap
            .skipped
            .iter()
            .any(|item| item.reason.contains("overlapping"))
    );

    let stale = HashMap::from([(PathBuf::from("src/lib.rs"), "fn stale() {}\n".to_owned())]);
    let stale_package = build_write_free_package(&root.0, &overlapping[..1], Some(&stale));
    assert!(stale_package.patches.is_empty());
    assert!(
        stale_package
            .skipped
            .iter()
            .any(|item| item.reason.contains("changed"))
    );
    assert_eq!(fs::read(root.file()).expect("read source after"), before);
}

#[test]
fn workspace_edit_input_and_context_scans_remain_bounded() {
    let root = TempRoot::new("fn greet() {}\n");
    let uri = path_to_file_uri(&root.file()).expect("file uri");
    let edits = (0..300)
        .map(|character| {
            serde_json::json!({
                "range": {
                    "start": {"line": 0, "character": character},
                    "end": {"line": 0, "character": character}
                },
                "newText": "x"
            })
        })
        .collect::<Vec<_>>();
    let mut changes = serde_json::Map::new();
    changes.insert(uri.clone(), Value::Array(edits));
    let normalized = normalize_workspace_edit(
        &root.0,
        &serde_json::json!({"changes": changes}),
        usize::MAX,
        None,
    );
    assert_eq!(normalized.edits.len(), 200);
    assert!(normalized.omitted >= 100);
    assert!(normalized.unsupported_operations.len() <= 128);

    let mut mixed_changes = serde_json::Map::new();
    for index in 0..256 {
        let file = root.0.join("src").join(format!("document-{index}.rs"));
        fs::write(&file, "fn document() {}\n").expect("write bounded document");
        let file_uri = path_to_file_uri(&file).expect("bounded document uri");
        mixed_changes.insert(
            file_uri,
            Value::Array(vec![serde_json::json!({
                "range": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 11}
                },
                "newText": "bounded"
            })]),
        );
    }
    let mixed = normalize_workspace_edit(
        &root.0,
        &serde_json::json!({
            "changes": mixed_changes,
            "documentChanges": [{
                "textDocument": {"uri": uri, "version": null},
                "edits": [{
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 8}
                    },
                    "newText": "hello"
                }]
            }]
        }),
        usize::MAX,
        None,
    );
    assert_eq!(mixed.edits.len(), 200);
    assert!(
        mixed
            .unsupported_operations
            .iter()
            .any(|item| item.contains("additional documentChanges entries omitted"))
    );

    let repeated = "fn main() {}\n".repeat(70);
    let repeated_root = TempRoot::new(&repeated);
    let package = build_write_free_package(
        &repeated_root.0,
        &[AdvisoryEdit {
            file: PathBuf::from("src/lib.rs"),
            range: range(0, 2),
            new_text: "pub".to_owned(),
            version: None,
            annotation_id: None,
        }],
        Some(&HashMap::from([(PathBuf::from("src/lib.rs"), repeated)])),
    );
    assert!(package.patches.is_empty(), "{package:#?}");
    assert!(
        package
            .skipped
            .iter()
            .any(|item| item.reason.contains("unique old_string"))
    );
}
