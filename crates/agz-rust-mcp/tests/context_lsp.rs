//! End-to-end `context` coverage against the fake semantic Rust Analyzer
//! fixture: definition, implementation, consumer, and test classification,
//! hierarchy, source-hash staleness, and revision deltas.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

use agz_rust_mcp::{
    config::WorkspaceCode,
    context::{CapsuleItemKind, CapsuleStore, ContextAction, ContextAnchor, ContextData},
    lsp::{ManagerOptions, RustAnalyzerManager},
    tools::{ContextEnvironment, ContextRequest, execute_context, with_lsp_authority},
    workspace::{ClientRoots, RootGuard, WorkspaceRoot},
};

static SEMANTIC_BINARY: OnceLock<PathBuf> = OnceLock::new();

fn semantic_binary() -> &'static PathBuf {
    SEMANTIC_BINARY.get_or_init(|| compile_semantic_binary("semantic-ra-context-refs"))
}

fn compile_semantic_binary(name: &str) -> PathBuf {
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp/semantic_ra.rs");
    let output_dir = std::env::temp_dir().join(format!(
        "agz-rust-mcp-context-lsp-fixture-{}",
        std::process::id()
    ));
    fs::create_dir_all(&output_dir).expect("create fixture output directory");
    let output = output_dir.join(format!("{name}{}", env::consts::EXE_SUFFIX));
    let rustc = env::var_os("RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustc"));
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
        let path = std::env::temp_dir().join(format!(
            "agz-rust-mcp-context-lsp-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("src")).expect("create src dir");
        fs::create_dir_all(path.join("tests")).expect("create tests dir");
        fs::write(
            path.join("src/lib.rs"),
            "pub fn mock_fn() -> i32 {\n    42\n}\n",
        )
        .expect("write lib source");
        fs::write(
            path.join("src/app.rs"),
            "pub fn call_site() {\n    let _ = mock_fn();\n}\n",
        )
        .expect("write consumer source");
        fs::write(
            path.join("tests/widget.rs"),
            "#[test]\nfn widget_calls_mock_fn() {\n    let _ = mock_fn();\n}\n",
        )
        .expect("write test source");
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

fn workspace_root(path: &Path) -> WorkspaceRoot {
    let guard = RootGuard::new([path.to_owned()], std::iter::empty()).expect("root guard");
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("root snapshot");
    snapshot.select(None).expect("select workspace root")
}

fn manager() -> RustAnalyzerManager {
    RustAnalyzerManager::new_authorized(
        ManagerOptions::default()
            .with_binary(semantic_binary())
            .with_workspace_code(WorkspaceCode::Allow)
            .with_timeout(Duration::from_secs(2))
            .with_wait_timeout(Duration::from_secs(2))
            .with_shutdown_timeout(Duration::from_millis(200)),
    )
    .expect("create semantic manager")
}

fn symbol_anchor() -> ContextAnchor {
    ContextAnchor::Symbol {
        symbol: "mock_fn".to_owned(),
        file: Some("src/lib.rs".to_owned()),
        line: Some(1),
    }
}

fn request(action: ContextAction) -> ContextRequest {
    ContextRequest {
        action,
        anchors: vec![symbol_anchor()],
        purpose: Some("api change fixture".to_owned()),
        change_id: Some("change-1".to_owned()),
        byte_budget: Some(16_384),
        capsule_id: None,
        previous_capsule_id: None,
        item_ids: Vec::new(),
        cursor: None,
        page_size: None,
    }
}

/// Mirrors the handler: semantic locations may use the manager's descriptor
/// protocol alias, so the LSP authority context must scope the whole call.
async fn run_context(request: ContextRequest, env: ContextEnvironment<'_>) -> ContextData {
    let authority = env.root.requested_authority().clone();
    Box::pin(with_lsp_authority(authority, execute_context(request, env))).await
}

#[tokio::test]
async fn context_prepare_classifies_definition_impl_consumer_and_test_end_to_end() {
    let root = TestRoot::new("prepare");
    let manager = manager();
    let workspace = workspace_root(root.path());
    let store = CapsuleStore::new(8, Duration::from_secs(120));

    let data = Box::pin(run_context(
        request(ContextAction::Prepare),
        ContextEnvironment {
            manager: Some(&manager),
            root: &workspace,
            snapshot: None,
            snapshot_error: None,
            store: &store,
            timeout: Duration::from_secs(2),
            max_items: 64,
            tool_output_bytes: 49_152,
        },
    ))
    .await;

    assert_eq!(data.status, "OK");
    assert_eq!(
        data.identity
            .as_ref()
            .expect("identity")
            .analyzer
            .as_deref(),
        Some("rust-analyzer"),
        "successful analyzer evidence must be recorded"
    );
    assert!(data.identity.as_ref().expect("identity").workspace_only);

    let kinds = data.items.iter().map(|item| item.kind).collect::<Vec<_>>();
    for expected in [
        CapsuleItemKind::Definition,
        CapsuleItemKind::Implementation,
        CapsuleItemKind::Consumer,
        CapsuleItemKind::TestReference,
        CapsuleItemKind::Signature,
        CapsuleItemKind::CallHierarchy,
    ] {
        assert!(kinds.contains(&expected), "missing {expected:?}: {kinds:?}");
    }
    for item in &data.items {
        assert!(
            !item.reason.trim().is_empty(),
            "every item needs a selection reason"
        );
    }

    let consumer = data
        .items
        .iter()
        .find(|item| item.kind == CapsuleItemKind::Consumer)
        .expect("consumer item");
    assert_eq!(consumer.file.as_deref(), Some("src/app.rs"));
    assert!(consumer.reason.contains("outside the definition"));
    assert!(consumer.source_hash.is_some());

    let test = data
        .items
        .iter()
        .find(|item| item.kind == CapsuleItemKind::TestReference)
        .expect("test item");
    assert_eq!(test.file.as_deref(), Some("tests/widget.rs"));
    assert!(test.reason.contains("test"));
    assert!(test.source_hash.is_some());

    let hierarchy = data
        .items
        .iter()
        .find(|item| item.kind == CapsuleItemKind::CallHierarchy)
        .expect("hierarchy item");
    assert!(hierarchy.reason.contains("not a complete call graph"));
    assert!(
        hierarchy
            .excerpt
            .as_deref()
            .is_some_and(|text| text.contains("caller"))
    );

    assert!(data.notes.iter().any(|note| {
        note.contains("macro expansion") && note.contains("trait-method resolution")
    }));
    assert!(
        data.notes.iter().any(|note| {
            note.contains("Call hierarchy") && note.contains("complete call graph")
        })
    );
    assert!(
        data.notes
            .iter()
            .any(|note| note.contains("full test-impact") || note.contains("test-impact analysis"))
    );

    let capsule_id = data.capsule_id.clone().expect("capsule id");
    let consumer_id = consumer.id.clone();

    // Changed consumer source must surface as a stale expanded item...
    fs::write(
        root.path().join("src/app.rs"),
        "pub fn call_site() {\n    let _ = 7;\n}\n",
    )
    .expect("rewrite consumer source");
    let mut expand = request(ContextAction::Expand);
    expand.anchors = Vec::new();
    expand.item_ids = vec![consumer_id.clone(), "missing-item".to_owned()];
    expand.capsule_id = Some(capsule_id.clone());
    expand.byte_budget = Some(16_384);
    let expanded = Box::pin(run_context(
        expand,
        ContextEnvironment {
            manager: Some(&manager),
            root: &workspace,
            snapshot: None,
            snapshot_error: None,
            store: &store,
            timeout: Duration::from_secs(2),
            max_items: 64,
            tool_output_bytes: 49_152,
        },
    ))
    .await;
    assert_eq!(expanded.status, "OK");
    let returned = expanded
        .items
        .iter()
        .find(|item| item.id == consumer_id)
        .expect("filtered consumer item");
    assert!(
        returned.stale,
        "changed source must mark the item stale, not silently current"
    );
    assert!(
        expanded
            .omitted
            .iter()
            .any(|entry| entry.detail.contains("missing-item")),
        "unknown item ids must be visible omissions"
    );

    // ...and must appear as a changed delta entry versus the stored capsule.
    let mut delta = request(ContextAction::Delta);
    delta.previous_capsule_id = Some(capsule_id);
    let delta = Box::pin(run_context(
        delta,
        ContextEnvironment {
            manager: Some(&manager),
            root: &workspace,
            snapshot: None,
            snapshot_error: None,
            store: &store,
            timeout: Duration::from_secs(2),
            max_items: 64,
            tool_output_bytes: 49_152,
        },
    ))
    .await;
    assert_eq!(delta.status, "OK");
    let report = delta.delta.expect("delta report");
    assert!(
        report.changed.iter().any(|entry| entry.id == consumer_id),
        "changed source must appear in the delta"
    );
    assert!(delta.items.is_empty(), "delta returns only the report");
}
