use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use agz_rust_coder::{
    config::WorkspaceCode,
    lsp::{ManagerOptions, RustAnalyzerManager},
    tools::{
        document_symbols, semantic_refactor, semantic_rename, symbol_definition, symbol_hierarchy,
        symbol_hover, symbol_implementations, symbol_references,
    },
};

fn rust_sources(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut pending = vec![root.to_owned()];
    let mut sources = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).expect("read smoke fixture") {
            let entry = entry.expect("fixture entry");
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("fixture metadata");
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                sources.insert(
                    path.strip_prefix(root).expect("relative source").to_owned(),
                    fs::read(path).expect("read source"),
                );
            }
        }
    }
    sources
}

#[tokio::test]
#[ignore = "requires the explicitly selected real rust-analyzer binary"]
async fn real_rust_analyzer_semantics_are_source_write_free() {
    let binary = std::env::var_os("AGZ_RUST_CODER_RUST_ANALYZER__PATH")
        .map(PathBuf::from)
        .expect("set AGZ_RUST_CODER_RUST_ANALYZER__PATH");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stage7/clean");
    let root = fs::canonicalize(root).expect("canonical smoke fixture");
    let before = rust_sources(&root);
    let mut options = ManagerOptions::default()
        .with_binary(binary)
        .with_workspace_code(WorkspaceCode::Deny)
        .with_timeout(Duration::from_secs(60))
        .with_wait_timeout(Duration::from_secs(60))
        .with_shutdown_timeout(Duration::from_secs(5));
    options.probe_timeout = Duration::from_secs(20);
    let manager = RustAnalyzerManager::new(options).expect("real RA manager");

    let hover = symbol_hover(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        "answer",
        Some(1),
        Duration::from_secs(60),
    )
    .await
    .expect("real hover");
    assert!(hover.contains("answer"), "{hover}");

    let symbols = document_symbols(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        Duration::from_secs(60),
    )
    .await
    .expect("real document symbols");
    assert!(symbols.contains("answer"), "{symbols}");

    let references = symbol_references(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        "answer",
        Some(1),
        Duration::from_secs(60),
    )
    .await
    .expect("real references");
    assert!(references.contains("rust-analyzer output"), "{references}");

    let definition = symbol_definition(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        "answer",
        Some(11),
        Duration::from_secs(60),
    )
    .await
    .expect("real definition");
    assert!(definition.contains("rust-analyzer output"), "{definition}");

    let implementations = symbol_implementations(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        "answer",
        Some(1),
        false,
        Duration::from_secs(60),
    )
    .await
    .expect("real implementations");
    assert!(
        implementations.contains("rust-analyzer output"),
        "{implementations}"
    );

    let hierarchy = symbol_hierarchy(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        "answer",
        Some(1),
        "both",
        2,
        Duration::from_secs(60),
    )
    .await
    .expect("real hierarchy");
    assert!(hierarchy.contains("rust-analyzer output"), "{hierarchy}");

    let rename = semantic_rename(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        "answer",
        Some(1),
        "renamed_answer",
        false,
        50,
        Duration::from_secs(60),
    )
    .await
    .expect("real write-free rename");
    assert!(!rename.patches.is_empty(), "{rename:#?}");

    let refactor = semantic_refactor(
        &manager,
        &root,
        Path::new("src/lib.rs"),
        "answer",
        Some(1),
        Some(&[]),
        false,
        50,
        Duration::from_secs(60),
    )
    .await
    .expect("real write-free refactor");
    assert!(refactor.reason.contains("rust-analyzer"), "{refactor:#?}");

    let report = manager.close_all().await;
    assert_eq!(report.remaining, 0, "{report:#?}");
    assert_eq!(rust_sources(&root), before);
}
