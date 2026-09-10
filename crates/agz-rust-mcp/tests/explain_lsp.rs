//! Capability negotiation for `explain` macro expansion and advisory
//! obligations against the scripted Rust Analyzer fixture. A real-analyzer
//! check stays `#[ignore]`.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

use agz_rust_mcp::{
    config::WorkspaceCode,
    lsp::{ManagerOptions, RustAnalyzerManager},
    tools::{
        explain::{self, RaStatus},
        with_lsp_cancellation,
    },
};

static EXPAND_BINARY: OnceLock<PathBuf> = OnceLock::new();
static PLAIN_BINARY: OnceLock<PathBuf> = OnceLock::new();

fn compile_fixture(name: &str) -> PathBuf {
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp/semantic_ra.rs");
    let output_dir = fs::canonicalize(env::temp_dir())
        .expect("canonical temp directory")
        .join(format!(
            "agz-rust-mcp-explain-fixture-{}",
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

fn expand_binary() -> &'static Path {
    EXPAND_BINARY.get_or_init(|| compile_fixture("semantic-ra-explain-expand-macro"))
}

fn plain_binary() -> &'static Path {
    PLAIN_BINARY.get_or_init(|| compile_fixture("semantic-ra-explain-plain"))
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let path = fs::canonicalize(env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-mcp-explain-lsp-{label}-{}",
                std::process::id()
            ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("src")).expect("create workspace");
        fs::write(
            path.join("src/lib.rs"),
            "pub fn mock_fn() -> i32 {\n    42\n}\n",
        )
        .expect("write source");
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

fn manager(binary: &Path) -> RustAnalyzerManager {
    RustAnalyzerManager::new_authorized(
        ManagerOptions::default()
            .with_binary(binary)
            .with_workspace_code(WorkspaceCode::Allow)
            .with_timeout(Duration::from_secs(2))
            .with_wait_timeout(Duration::from_secs(2))
            .with_shutdown_timeout(Duration::from_millis(200)),
    )
    .expect("create explain manager")
}

#[tokio::test]
async fn negotiated_expand_macro_is_advisory_and_obligations_parse() {
    let root = TestRoot::new("negotiated");
    let manager = manager(expand_binary());

    let expansion = explain::expand_macro(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        Some("mock_fn".to_owned()),
        1,
        Duration::from_secs(2),
    )
    .await;
    match expansion {
        RaStatus::Available(expansion) => {
            assert_eq!(expansion.name.as_deref(), Some("mock_macro"));
            assert!(
                expansion.expansion.contains("mock_fn"),
                "{}",
                expansion.expansion
            );
            assert!(!expansion.truncated);
        }
        other => panic!("expected negotiated expansion, got {other:?}"),
    }

    let obligations = explain::failed_obligations(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        Some("mock_fn".to_owned()),
        1,
        Duration::from_secs(2),
    )
    .await;
    match obligations {
        RaStatus::Available(obligations) => {
            assert_eq!(obligations.items.len(), 1);
            assert!(obligations.items[0].contains("MockTrait"));
        }
        other => panic!("expected negotiated obligations, got {other:?}"),
    }

    // The compiler remains the authority; the conflict helper keeps the
    // compiler side visible when the advisory side disagrees.
    let conflicts = explain::obligation_conflicts(true, &Default::default(), None);
    assert_eq!(conflicts.len(), 1);

    assert_eq!(manager.close_all().await.remaining, 0);
}

#[tokio::test]
async fn unsupported_capability_returns_typed_status() {
    let root = TestRoot::new("unsupported");
    let manager = manager(plain_binary());

    let expansion = explain::expand_macro(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        Some("mock_fn".to_owned()),
        1,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        matches!(expansion, RaStatus::Unsupported(_)),
        "unexpected expansion status: {expansion:?}"
    );

    let obligations = explain::failed_obligations(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        Some("mock_fn".to_owned()),
        1,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        matches!(obligations, RaStatus::Unsupported(_)),
        "unexpected obligations status: {obligations:?}"
    );

    assert_eq!(manager.close_all().await.remaining, 0);
}

#[tokio::test]
async fn cancelled_capability_probe_does_not_start_the_analyzer() {
    let root = TestRoot::new("cancelled-probe");
    // A missing binary discriminates the cancellation-aware probe: an
    // uncancelled probe would try to start it and report a startup failure.
    let missing = root.path().join("missing-rust-analyzer");
    let manager = manager(&missing);
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();

    let expansion = with_lsp_cancellation(
        cancellation,
        explain::expand_macro(
            &manager,
            root.path(),
            Path::new("src/lib.rs"),
            Some("mock_fn".to_owned()),
            1,
            Duration::from_secs(2),
        ),
    )
    .await;
    match expansion {
        RaStatus::Unavailable(reason) => {
            assert!(reason.contains("cancel"), "{reason}");
        }
        other => panic!("expected a cancellation fallback, got {other:?}"),
    }

    assert_eq!(manager.close_all().await.remaining, 0);
}

#[tokio::test]
async fn workspace_code_deny_degrades_to_a_typed_fallback() {
    let root = TestRoot::new("deny-fallback");
    let manager = RustAnalyzerManager::new_authorized(
        ManagerOptions::default()
            .with_binary(expand_binary())
            .with_workspace_code(WorkspaceCode::Deny)
            .with_timeout(Duration::from_secs(2))
            .with_wait_timeout(Duration::from_secs(2))
            .with_shutdown_timeout(Duration::from_millis(200)),
    )
    .expect("create deny manager");

    // Deny gates analyzer startup behind the binary schema probe; when the
    // probe cannot verify the binary, advisory evidence degrades to a typed
    // fallback instead of being fabricated.
    let expansion = explain::expand_macro(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        Some("mock_fn".to_owned()),
        1,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        !matches!(expansion, RaStatus::Available(_)),
        "unexpected expansion status: {expansion:?}"
    );

    let obligations = explain::failed_obligations(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        Some("mock_fn".to_owned()),
        1,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        !matches!(obligations, RaStatus::Available(_)),
        "unexpected obligations status: {obligations:?}"
    );

    assert_eq!(manager.close_all().await.remaining, 0);
}

#[tokio::test]
#[ignore = "requires a real rust-analyzer binary on PATH"]
async fn real_rust_analyzer_returns_available_or_typed_fallback() {
    let root = TestRoot::new("real");
    let manager = RustAnalyzerManager::new_authorized(
        ManagerOptions::default()
            .with_workspace_code(WorkspaceCode::Allow)
            .with_timeout(Duration::from_secs(30))
            .with_wait_timeout(Duration::from_secs(30)),
    )
    .expect("create real analyzer manager");
    let status = explain::expand_macro(
        &manager,
        root.path(),
        Path::new("src/lib.rs"),
        Some("mock_fn".to_owned()),
        1,
        Duration::from_secs(30),
    )
    .await;
    assert!(!matches!(status, RaStatus::Unsupported(_)));
    let _ = manager.close_all().await;
}
