use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
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

static SEMANTIC_BINARY: OnceLock<PathBuf> = OnceLock::new();

fn semantic_binary() -> &'static PathBuf {
    SEMANTIC_BINARY.get_or_init(|| compile_semantic_binary("semantic-ra"))
}

fn compile_semantic_binary(name: &str) -> PathBuf {
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp/semantic_ra.rs");
    let output_dir = env::temp_dir().join(format!(
        "agz-rust-coder-semantic-fixture-{}",
        std::process::id()
    ));
    fs::create_dir_all(&output_dir).expect("create fixture output directory");
    let output = output_dir.join(name);
    let rustc = env::var_os("RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/ugur/.cargo/bin/rustc"));
    let status = Command::new(rustc)
        .args(["--edition=2024"])
        .arg(source)
        .arg("-o")
        .arg(&output)
        .status()
        .expect("compile semantic fixture");
    assert!(status.success(), "semantic fixture compilation failed");
    output
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "agz-rust-coder-navigation-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("src")).expect("create navigation workspace");
        fs::write(
            path.join("src/lib.rs"),
            "pub fn mock_fn() -> i32 {\n    42\n}\n\nmod inner {\n    pub fn mock_fn() {}\n}\n",
        )
        .expect("write navigation source");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn manager_with_binary(binary: &Path) -> RustAnalyzerManager {
    RustAnalyzerManager::new(
        ManagerOptions::default()
            .with_binary(binary)
            .with_workspace_code(WorkspaceCode::Allow)
            .with_timeout(Duration::from_secs(2))
            .with_wait_timeout(Duration::from_secs(2))
            .with_shutdown_timeout(Duration::from_millis(200)),
    )
    .expect("create semantic manager")
}

fn manager() -> RustAnalyzerManager {
    manager_with_binary(semantic_binary())
}

#[tokio::test]
async fn semantic_navigation_preserves_shapes_and_stays_bounded() {
    let root = TestRoot::new("navigation");
    let manager = manager();
    let path = Path::new("src/lib.rs");

    let symbols = document_symbols(&manager, root.path(), path, Duration::from_secs(2))
        .await
        .expect("document symbols");
    assert!(symbols.contains("function  mock_fn in mock_mod"));
    assert!(symbols.contains("(1 total, showing 1)"));

    let hover = symbol_hover(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        Duration::from_secs(2),
    )
    .await
    .expect("hover");
    assert!(hover.contains("mock_fn: fn() -> i32"));
    assert!(hover.chars().count() <= 2_000);

    let references = symbol_references(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        Duration::from_secs(2),
    )
    .await
    .expect("references");
    assert!(references.contains("src/lib.rs:1"));
    assert!(references.contains("content omitted: location is outside the workspace"));

    let definition = symbol_definition(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        Duration::from_secs(2),
    )
    .await
    .expect("definition");
    assert!(definition.contains("src/lib.rs:1"));
    assert!(definition.contains("pub fn mock_fn"));

    let implementations = symbol_implementations(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        true,
        Duration::from_secs(2),
    )
    .await
    .expect("implementations");
    assert!(
        implementations.contains("src/lib.rs:6"),
        "{implementations}"
    );
    assert!(implementations.contains("full contents for 1 files"));

    let hierarchy = symbol_hierarchy(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        "both",
        2,
        Duration::from_secs(2),
    )
    .await
    .expect("hierarchy");
    assert!(hierarchy.contains("<- caller"));
    assert!(hierarchy.contains("-> callee"));
    assert!(hierarchy.contains("depth 2"));
    assert!(hierarchy.chars().count() <= 24_000);

    let report = manager.close_all().await;
    assert_eq!(report.remaining, 0);
}

#[tokio::test]
async fn semantic_edits_are_write_free_and_return_commands_as_text() {
    let root = TestRoot::new("edits");
    let path = root.path().join("src/lib.rs");
    let original = fs::read(&path).expect("read original source");
    let manager = manager();

    let rename = semantic_rename(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        "mock_fn",
        Some(1),
        "renamed",
        true,
        20,
        Duration::from_secs(2),
    )
    .await
    .expect("rename");
    assert!(rename.patches.iter().any(|patch| {
        patch.old_string.contains("mock_fn") && patch.new_string.contains("renamed")
    }));
    assert!(rename.reason.contains("context src/lib.rs"));
    assert_eq!(fs::read(&path).expect("read unchanged source"), original);

    let refactor = semantic_refactor(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        "mock_fn",
        Some(1),
        None,
        true,
        20,
        Duration::from_secs(2),
    )
    .await
    .expect("refactor");
    assert!(refactor.reason.contains("Replace literal"));
    assert!(
        refactor
            .reason
            .contains("follow-up command not executed: mock.format")
    );
    assert!(
        refactor
            .reason
            .contains("command suggestion only (not executed): mock.extract")
    );
    assert_eq!(fs::read(&path).expect("read unchanged source"), original);

    let report = manager.close_all().await;
    assert_eq!(report.remaining, 0);
}

#[tokio::test]
async fn semantic_variants_retry_and_stay_bounded() {
    let root = TestRoot::new("variants");
    let path = Path::new("src/lib.rs");

    let hierarchical = compile_semantic_binary("semantic-ra-hierarchical-reciprocal");
    let manager = manager_with_binary(&hierarchical);
    let symbols = document_symbols(&manager, root.path(), path, Duration::from_secs(2))
        .await
        .expect("hierarchical symbols");
    assert!(symbols.contains("mock_mod"));
    assert!(symbols.contains("mock_fn"));
    let hierarchy = symbol_hierarchy(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        "both",
        99,
        Duration::from_secs(2),
    )
    .await
    .expect("bounded reciprocal hierarchy");
    assert!(hierarchy.contains("peer"));
    assert!(hierarchy.contains("depth 2"));
    assert!(!hierarchy.contains("-> mock_fn"));
    assert!(hierarchy.chars().count() <= 24_000);
    assert_eq!(manager.close_all().await.remaining, 0);

    let retry = compile_semantic_binary("semantic-ra-retry-hover");
    let manager = manager_with_binary(&retry);
    let hover = symbol_hover(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        Duration::from_secs(2),
    )
    .await
    .expect("retried hover");
    assert!(hover.contains("mock_fn: fn() -> i32"));
    assert_eq!(manager.close_all().await.remaining, 0);

    let message_error = compile_semantic_binary("semantic-ra-message-retry");
    let manager = manager_with_binary(&message_error);
    let error = symbol_hover(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        Duration::from_secs(2),
    )
    .await
    .expect_err("non-content-modified response error");
    assert!(
        error
            .to_string()
            .contains("No references found at position")
    );
    assert_eq!(manager.close_all().await.remaining, 0);

    let rejected = compile_semantic_binary("semantic-ra-reject");
    let manager = manager_with_binary(&rejected);
    let result = semantic_rename(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        "renamed",
        false,
        20,
        Duration::from_secs(2),
    )
    .await
    .expect("rejected rename result");
    assert!(result.patches.is_empty());
    assert!(result.reason.contains("not valid"));
    assert_eq!(manager.close_all().await.remaining, 0);

    let default_rename = compile_semantic_binary("semantic-ra-default");
    let manager = manager_with_binary(&default_rename);
    let result = semantic_rename(
        &manager,
        root.path(),
        path,
        "mock_fn",
        Some(1),
        "renamed",
        false,
        20,
        Duration::from_secs(2),
    )
    .await
    .expect("default rename result");
    assert!(!result.patches.is_empty());
    assert_eq!(manager.close_all().await.remaining, 0);
}
