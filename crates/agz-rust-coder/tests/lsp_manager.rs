pub mod config {
    pub use agz_rust_coder::config::*;
}

pub mod process {
    pub use agz_rust_coder::process::*;
}

pub mod workspace {
    pub use agz_rust_coder::workspace::*;
}

use agz_rust_coder::lsp;
#[cfg(unix)]
use agz_rust_coder::tools::{read_workspace_file, read_workspace_file_with_hook};

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(unix)]
use lsp::manager::{ConcreteBinarySchemaProbe, ConcreteClientFactory};
use lsp::{
    client::{CloseHandler, DocumentSyncOptions, ServerRequestHandler},
    manager::{
        BinarySchemaProbe, ClientCallbacks, ClientFuture, ClientRef, LspClientFactory,
        LspClientLike, ManagerOptions, ProbeError, ProbeFuture, RustAnalyzerManager,
    },
    normalize::{
        self, BUILD_SCRIPTS_ENABLE_KEY, BinaryConfigSchema, CHECK_ON_SAVE_KEY,
        PROC_MACRO_ENABLE_KEY,
    },
};
use serde_json::{Value, json};
use tokio::{sync::Notify, time::sleep};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-coder-lsp-{label}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&path).expect("create test root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

fn binary(root: &Path) -> PathBuf {
    let path = root.join("rust-analyzer-fixture");
    fs::write(&path, b"not executed by mock factory").expect("write binary fixture");
    path
}

fn valid_schema() -> BinaryConfigSchema {
    BinaryConfigSchema::new([
        BUILD_SCRIPTS_ENABLE_KEY,
        PROC_MACRO_ENABLE_KEY,
        CHECK_ON_SAVE_KEY,
    ])
}

#[derive(Debug)]
struct MockProbe {
    schema: Arc<Mutex<Result<BinaryConfigSchema, ProbeError>>>,
    calls: Arc<AtomicUsize>,
}

impl Clone for MockProbe {
    fn clone(&self) -> Self {
        Self {
            schema: self.schema.clone(),
            calls: self.calls.clone(),
        }
    }
}

impl MockProbe {
    fn valid() -> Self {
        Self {
            schema: Arc::new(Mutex::new(Ok(valid_schema()))),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn unavailable() -> Self {
        Self {
            schema: Arc::new(Mutex::new(Ok(BinaryConfigSchema::new([CHECK_ON_SAVE_KEY])))),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl BinarySchemaProbe for MockProbe {
    fn probe<'a>(&'a self, _binary: &'a Path, _timeout: Duration) -> ProbeFuture<'a> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let result = self.schema.lock().expect("probe mutex").clone();
        Box::pin(async move { result })
    }
}

#[derive(Debug, Clone)]
struct BlockingProbe {
    started: Arc<Notify>,
}

impl BlockingProbe {
    fn new() -> Self {
        Self {
            started: Arc::new(Notify::new()),
        }
    }
}

impl BinarySchemaProbe for BlockingProbe {
    fn probe<'a>(&'a self, _binary: &'a Path, _timeout: Duration) -> ProbeFuture<'a> {
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_one();
            std::future::pending::<Result<BinaryConfigSchema, ProbeError>>().await
        })
    }
}

struct MockClient {
    id: usize,
    closed: AtomicBool,
    shutdowns: AtomicUsize,
    initialize_delay: Duration,
    request_delay: Duration,
    close_handler: Mutex<Option<CloseHandler>>,
    server_request_handler: Mutex<Option<ServerRequestHandler>>,
    requests: Mutex<Vec<(String, Value)>>,
}

impl std::fmt::Debug for MockClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MockClient")
            .field("id", &self.id)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .field("shutdowns", &self.shutdowns.load(Ordering::Acquire))
            .finish()
    }
}

impl MockClient {
    fn new(id: usize, initialize_delay: Duration, request_delay: Duration) -> Self {
        Self {
            id,
            closed: AtomicBool::new(false),
            shutdowns: AtomicUsize::new(0),
            initialize_delay,
            request_delay,
            close_handler: Mutex::new(None),
            server_request_handler: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn crash(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            if let Some(handler) = self
                .close_handler
                .lock()
                .expect("close handler mutex")
                .clone()
            {
                handler();
            }
        }
    }

    fn initialize_params(&self) -> Vec<Value> {
        self.requests
            .lock()
            .expect("request mutex")
            .iter()
            .filter(|(method, _)| method == "initialize")
            .map(|(_, params)| params.clone())
            .collect()
    }

    async fn workspace_folders(&self) -> Value {
        let handler = self
            .server_request_handler
            .lock()
            .expect("server request handler mutex")
            .clone()
            .expect("workspace callback is installed");
        handler(
            "workspace/workspaceFolders".to_owned(),
            Value::Null,
            CancellationToken::new(),
        )
        .await
        .expect("workspace folders callback")
    }
}

impl LspClientLike for MockClient {
    fn set_callbacks(&self, callbacks: ClientCallbacks) -> ClientFuture<'_, ()> {
        if let Ok(mut handler) = self.close_handler.lock() {
            *handler = callbacks.close_handler;
        }
        if let Ok(mut handler) = self.server_request_handler.lock() {
            *handler = callbacks.server_request_handler;
        }
        Box::pin(async { Ok(()) })
    }

    fn set_document_sync(&self, _options: DocumentSyncOptions) -> ClientFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn request(&self, method: &str, params: Value, _timeout: Duration) -> ClientFuture<'_, Value> {
        let method = method.to_owned();
        self.requests
            .lock()
            .expect("request mutex")
            .push((method.clone(), params));
        let initialize_delay = self.initialize_delay;
        let request_delay = self.request_delay;
        let id = self.id;
        Box::pin(async move {
            if method == "initialize" {
                sleep(initialize_delay).await;
                return Ok(json!({"capabilities": {"textDocumentSync": 2}}));
            }
            if method == "delayed" {
                sleep(request_delay).await;
                return Ok(Value::String("delayed".to_owned()));
            }
            if method == "mock/state" {
                return Ok(json!({"id": id}));
            }
            Ok(Value::Null)
        })
    }

    fn notify(&self, _method: &str, _params: Value) -> ClientFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn shutdown(&self, _timeout: Duration) -> ClientFuture<'_, ()> {
        self.shutdowns.fetch_add(1, Ordering::AcqRel);
        let close_handler = self
            .close_handler
            .lock()
            .expect("close handler mutex")
            .clone();
        let was_closed = self.closed.swap(true, Ordering::AcqRel);
        Box::pin(async move {
            if !was_closed {
                if let Some(handler) = close_handler {
                    handler();
                }
            }
            Ok(())
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct MockFactory {
    initialize_delay: Duration,
    request_delay: Duration,
    next_id: Arc<AtomicUsize>,
    clients: Arc<Mutex<Vec<Arc<MockClient>>>>,
    specs: Arc<Mutex<Vec<crate::process::CommandSpec>>>,
    protocol_root: Option<PathBuf>,
}

impl MockFactory {
    fn new(initialize_delay: Duration, request_delay: Duration) -> Self {
        Self {
            initialize_delay,
            request_delay,
            next_id: Arc::new(AtomicUsize::new(1)),
            clients: Arc::new(Mutex::new(Vec::new())),
            specs: Arc::new(Mutex::new(Vec::new())),
            protocol_root: None,
        }
    }

    fn clients(&self) -> Vec<Arc<MockClient>> {
        self.clients.lock().expect("clients mutex").clone()
    }

    fn spawn_count(&self) -> usize {
        self.clients.lock().expect("clients mutex").len()
    }

    fn specs(&self) -> Vec<crate::process::CommandSpec> {
        self.specs.lock().expect("spec mutex").clone()
    }

    fn with_protocol_root(mut self, root: PathBuf) -> Self {
        self.protocol_root = Some(root);
        self
    }
}

impl LspClientFactory for MockFactory {
    fn spawn<'a>(
        &'a self,
        spec: crate::process::CommandSpec,
        _default_timeout: Duration,
        _max_frame_bytes: usize,
    ) -> ClientFuture<'a, ClientRef> {
        self.specs.lock().expect("spec mutex").push(spec);
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        let client = Arc::new(MockClient::new(
            id,
            self.initialize_delay,
            self.request_delay,
        ));
        self.clients
            .lock()
            .expect("clients mutex")
            .push(client.clone());
        Box::pin(async move { Ok(client as ClientRef) })
    }

    fn protocol_root(&self, lexical: &Path) -> PathBuf {
        self.protocol_root
            .clone()
            .unwrap_or_else(|| lexical.to_owned())
    }
}

fn options(binary: &Path) -> ManagerOptions {
    ManagerOptions::default()
        .with_binary(binary)
        .with_timeout(Duration::from_secs(2))
        .with_wait_timeout(Duration::from_secs(2))
        .with_shutdown_timeout(Duration::from_millis(100))
}

fn manager(
    binary: &Path,
    probe: MockProbe,
    factory: MockFactory,
    max_instances: usize,
) -> RustAnalyzerManager<MockProbe, MockFactory> {
    RustAnalyzerManager::with_adapters(
        options(binary).with_max_instances(max_instances),
        probe,
        factory,
    )
    .expect("create manager")
}

#[tokio::test]
async fn same_workspace_start_is_singleflight_and_initializes_once() {
    let root = TestRoot::new("singleflight");
    let binary = binary(root.path());
    let factory = MockFactory::new(Duration::from_millis(60), Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::valid(), factory, 2);
    let left = manager.acquire(root.path());
    let right = manager.acquire(root.path());
    let (left, right) = tokio::join!(left, right);
    assert!(left.is_ok());
    assert!(right.is_ok());
    assert_eq!(manager.instance_count(), 1);
    assert_eq!(factory_observer.spawn_count(), 1);
    let _ = manager.close_all().await;
}

#[tokio::test]
async fn close_all_waits_for_initialize_and_does_not_leak_the_instance() {
    let root = TestRoot::new("initialize-cleanup");
    let binary = binary(root.path());
    let factory = MockFactory::new(Duration::from_millis(80), Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::valid(), factory, 1);
    let acquire_manager = manager.clone();
    let root_path = root.path().to_owned();
    let acquire = tokio::spawn(async move { acquire_manager.acquire(root_path).await });

    sleep(Duration::from_millis(10)).await;
    let report = manager.close_all().await;
    let _ = acquire.await.expect("initialize task");

    assert_eq!(report.requested, 1);
    assert_eq!(report.remaining, 0);
    assert_eq!(
        factory_observer.clients()[0]
            .shutdowns
            .load(Ordering::Acquire),
        1
    );
}

#[tokio::test]
async fn cancellation_during_initialize_shuts_down_without_registering_an_instance() {
    let root = TestRoot::new("initialize-cancel");
    let binary = binary(root.path());
    let factory = MockFactory::new(Duration::from_millis(80), Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::valid(), factory, 1);
    let cancellation = CancellationToken::new();
    let acquire_manager = manager.clone();
    let root_path = root.path().to_owned();
    let acquire_cancellation = cancellation.clone();
    let acquire = tokio::spawn(async move {
        acquire_manager
            .acquire_with_cancellation(root_path, acquire_cancellation)
            .await
    });

    for _ in 0..20 {
        if factory_observer.spawn_count() == 1 {
            break;
        }
        sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(factory_observer.spawn_count(), 1);
    cancellation.cancel();

    assert!(matches!(
        acquire.await.expect("initialize task"),
        Err(lsp::manager::ManagerError::Cancelled)
    ));
    assert_eq!(manager.instance_count(), 0);
    assert_eq!(manager.active_lease_count(), 0);
    assert_eq!(
        factory_observer.clients()[0]
            .shutdowns
            .load(Ordering::Acquire),
        1
    );
}

#[tokio::test]
async fn cancelled_schema_probe_does_not_spawn_or_publish_an_instance() {
    let root = TestRoot::new("schema-cancel");
    let binary = binary(root.path());
    let probe = BlockingProbe::new();
    let probe_started = Arc::clone(&probe.started);
    let factory = MockFactory::new(Duration::ZERO, Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = RustAnalyzerManager::with_adapters(options(&binary), probe, factory)
        .expect("create manager");
    let cancellation = CancellationToken::new();
    let acquire_manager = manager.clone();
    let root_path = root.path().to_owned();
    let acquire_cancellation = cancellation.clone();
    let acquire = tokio::spawn(async move {
        acquire_manager
            .acquire_with_cancellation(root_path, acquire_cancellation)
            .await
    });

    probe_started.notified().await;
    cancellation.cancel();

    assert!(matches!(
        acquire.await.expect("schema task"),
        Err(lsp::manager::ManagerError::Cancelled)
    ));
    assert_eq!(factory_observer.spawn_count(), 0);
    assert_eq!(manager.instance_count(), 0);
}

#[tokio::test]
async fn cancelled_start_does_not_poison_an_unrelated_waiter() {
    let root = TestRoot::new("start-waiter-cancel");
    let binary = binary(root.path());
    let factory = MockFactory::new(Duration::from_millis(80), Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::valid(), factory, 1);
    let cancellation = CancellationToken::new();
    let owner_manager = manager.clone();
    let owner_root = root.path().to_owned();
    let owner_cancellation = cancellation.clone();
    let owner = tokio::spawn(async move {
        owner_manager
            .acquire_with_cancellation(owner_root, owner_cancellation)
            .await
    });

    for _ in 0..20 {
        if factory_observer.spawn_count() == 1 {
            break;
        }
        sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(factory_observer.spawn_count(), 1);
    let waiter_manager = manager.clone();
    let waiter_root = root.path().to_owned();
    let waiter = tokio::spawn(async move { waiter_manager.acquire(waiter_root).await });
    sleep(Duration::from_millis(10)).await;
    cancellation.cancel();

    assert!(matches!(
        owner.await.expect("owner task"),
        Err(lsp::manager::ManagerError::Cancelled)
    ));
    assert!(waiter.await.expect("waiter task").is_ok());
    assert_eq!(factory_observer.spawn_count(), 2);
    let _ = manager.close_all().await;
}

#[tokio::test]
async fn factory_receives_the_canonical_workspace_and_fixed_environment() {
    let root = TestRoot::new("factory-spec");
    let binary = binary(root.path());
    let factory = MockFactory::new(Duration::ZERO, Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::valid(), factory, 1);

    manager.acquire(root.path()).await.expect("client");

    let specs = factory_observer.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(
        specs[0].cwd,
        fs::canonicalize(root.path()).expect("canonical root")
    );
    assert!(
        specs[0]
            .env
            .iter()
            .any(|(key, value)| key == "CARGO_TERM_COLOR" && value == "never")
    );
    let _ = manager.close_all().await;
}

#[cfg(target_os = "linux")]
#[test]
fn concrete_factory_uses_the_root_descriptor_protocol_alias() {
    let factory = ConcreteClientFactory;
    assert_eq!(
        factory.protocol_root(Path::new("/lexical/workspace")),
        PathBuf::from("/proc/self/fd/198")
    );
}

#[tokio::test]
async fn protocol_root_is_shared_by_initialize_and_workspace_folder_callback() {
    let root = TestRoot::new("protocol-root-alias");
    let binary = binary(root.path());
    let protocol_root = PathBuf::from("/agz-stable-lsp-root");
    let factory =
        MockFactory::new(Duration::ZERO, Duration::ZERO).with_protocol_root(protocol_root.clone());
    let observer = factory.clone();
    let manager = RustAnalyzerManager::with_authorized_adapters(
        options(&binary),
        MockProbe::valid(),
        factory,
    )
    .expect("authorized manager");
    let authority = workspace::RootGuard::new([root.path().to_owned()], std::iter::empty())
        .expect("authorize workspace")
        .configured_roots()[0]
        .clone();
    manager
        .acquire_authorized(authority, root.path())
        .await
        .expect("start client");
    let client = observer.clients().pop().expect("started mock client");

    let original = root.path().with_extension("original");
    fs::rename(root.path(), &original).expect("move lexical root after initialization");
    fs::create_dir(root.path()).expect("create lexical replacement");
    let initialize = client
        .initialize_params()
        .pop()
        .expect("captured initialize parameters");
    let workspace_folders = client.workspace_folders().await;
    let expected_uri = "file:///agz-stable-lsp-root";
    assert_eq!(initialize["rootUri"], expected_uri);
    assert_eq!(initialize["workspaceFolders"][0]["uri"], expected_uri);
    assert_eq!(workspace_folders[0]["uri"], expected_uri);
    assert_ne!(
        initialize["rootUri"],
        format!("file://{}", root.path().display()),
        "the replacement lexical root must never be sent to the client"
    );

    let _ = manager.close_all().await;
    fs::remove_dir_all(&original).expect("remove original root");
}

#[cfg(unix)]
#[tokio::test]
async fn authorized_root_replacement_cannot_start_the_replacement_analyzer() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestRoot::new("authorized-replacement");
    let authority = workspace::RootGuard::new([root.path().to_owned()], std::iter::empty())
        .expect("authorize original workspace")
        .configured_roots()[0]
        .clone();
    let original = root.path().with_extension("original");
    let marker = root.path().join("replacement-analyzer-ran");
    fs::rename(root.path(), &original).expect("move authorized workspace");
    fs::create_dir(root.path()).expect("create replacement workspace");
    let replacement_analyzer = root.path().join("replacement-rust-analyzer");
    fs::write(
        &replacement_analyzer,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .expect("write replacement analyzer");
    let mut permissions = fs::metadata(&replacement_analyzer)
        .expect("replacement analyzer metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&replacement_analyzer, permissions).expect("make replacement executable");

    let manager = RustAnalyzerManager::with_authorized_adapters(
        options(&replacement_analyzer)
            .with_workspace_code(config::WorkspaceCode::Deny)
            .with_wait_timeout(Duration::from_secs(1)),
        ConcreteBinarySchemaProbe::default(),
        ConcreteClientFactory,
    )
    .expect("create concrete manager");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        manager.acquire_authorized(authority, root.path()),
    )
    .await
    .expect("root replacement failure is bounded");
    assert!(result.is_err(), "replacement root must fail closed");
    assert!(
        !marker.exists(),
        "replacement workspace analyzer must not start"
    );
    let _ = manager.close_all().await;
    fs::remove_dir_all(root.path()).expect("remove replacement workspace");
    fs::remove_dir_all(original).expect("remove original workspace");
}

#[cfg(unix)]
#[tokio::test]
async fn authorized_schema_probe_cancellation_reaps_its_descendant() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestRoot::new("authorized-schema-probe-cleanup");
    let binary = root.path().join("schema-probe");
    let pid_file = root.path().join("schema-descendant.pid");
    let sentinel = root.path().join("schema-descendant-survived");
    fs::write(
        &binary,
        format!(
            "#!/bin/sh\n( trap '' TERM; sleep 2; touch '{}' ) &\necho $! > '{}'\nsleep 5\n",
            sentinel.display(),
            pid_file.display(),
        ),
    )
    .expect("write schema probe fixture");
    let mut permissions = fs::metadata(&binary)
        .expect("schema probe metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).expect("make schema probe executable");

    let authority = workspace::RootGuard::new([root.path().to_owned()], std::iter::empty())
        .expect("authorize workspace")
        .configured_roots()[0]
        .clone();
    let manager = RustAnalyzerManager::new_authorized(
        options(&binary)
            .with_workspace_code(config::WorkspaceCode::Deny)
            .with_wait_timeout(Duration::from_secs(3)),
    )
    .expect("create authorized manager");
    let cancellation = CancellationToken::new();
    let acquire_manager = manager.clone();
    let root_path = root.path().to_owned();
    let acquire_cancellation = cancellation.clone();
    let acquire = tokio::spawn(async move {
        acquire_manager
            .acquire_authorized_with_cancellation(authority, root_path, acquire_cancellation)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !pid_file.exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("schema descendant started");
    cancellation.cancel();

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(4), acquire)
            .await
            .expect("schema cancellation is bounded")
            .expect("schema acquire task"),
        Err(lsp::manager::ManagerError::Cancelled)
    ));
    sleep(Duration::from_millis(700)).await;
    assert!(
        !sentinel.exists(),
        "a cancelled authorized probe must reap its logical descendant"
    );
    assert_eq!(manager.instance_count(), 0);
    let _ = manager.close_all().await;
}

#[tokio::test]
async fn active_lease_blocks_eviction_until_request_finishes() {
    let first = TestRoot::new("active-first");
    let second = TestRoot::new("active-second");
    let binary = binary(&first.path);
    let factory = MockFactory::new(Duration::ZERO, Duration::from_millis(120));
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::valid(), factory, 1);
    let active_manager = manager.clone();
    let active = tokio::spawn(async move {
        active_manager
            .with_client(first.path(), |client| async move {
                client
                    .request("delayed", Value::Null, Duration::from_secs(1))
                    .await
            })
            .await
    });
    sleep(Duration::from_millis(20)).await;
    let pending_manager = manager.clone();
    let pending = tokio::spawn(async move { pending_manager.acquire(second.path()).await });
    sleep(Duration::from_millis(30)).await;
    assert_eq!(manager.instance_count(), 1);
    assert_eq!(manager.active_lease_count(), 1);
    assert!(!factory_observer.clients()[0].is_closed());
    assert_eq!(
        active.await.expect("active task").expect("active request"),
        Value::String("delayed".to_owned())
    );
    let _ = pending
        .await
        .expect("second acquire task")
        .expect("second workspace");
    assert_eq!(manager.instance_count(), 1);
    assert_eq!(manager.active_lease_count(), 0);
    assert!(factory_observer.clients()[0].is_closed());
    let _ = manager.close_all().await;
}

#[tokio::test]
async fn deny_requires_schema_before_factory_spawn_and_allow_is_explicit() {
    let root = TestRoot::new("schema");
    let binary = binary(root.path());
    let factory = MockFactory::new(Duration::ZERO, Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::unavailable(), factory, 2);
    let error = match manager.acquire(root.path()).await {
        Ok(_) => panic!("deny must fail closed"),
        Err(error) => error,
    };
    assert!(error.is_unavailable());
    assert_eq!(factory_observer.spawn_count(), 0);

    let options = options(&binary).with_workspace_code(config::WorkspaceCode::Allow);
    let allow_probe = MockProbe::unavailable();
    let allow_probe_observer = allow_probe.clone();
    let allow_factory = MockFactory::new(Duration::ZERO, Duration::ZERO);
    let allow_factory_observer = allow_factory.clone();
    let allow_manager = RustAnalyzerManager::with_adapters(options, allow_probe, allow_factory)
        .expect("allow manager");
    assert!(allow_manager.acquire(root.path()).await.is_ok());
    assert_eq!(allow_probe_observer.calls.load(Ordering::Acquire), 0);
    assert_eq!(allow_factory_observer.spawn_count(), 1);
    let _ = allow_manager.close_all().await;
    let _ = manager.close_all().await;
}

#[tokio::test]
async fn crash_close_handler_removes_instance_and_next_acquire_restarts() {
    let root = TestRoot::new("crash");
    let binary = binary(root.path());
    let factory = MockFactory::new(Duration::ZERO, Duration::ZERO);
    let factory_observer = factory.clone();
    let manager = manager(&binary, MockProbe::valid(), factory, 2);
    let first = manager.acquire(root.path()).await.expect("first client");
    let first_id = first
        .request("mock/state", Value::Null, Duration::from_secs(1))
        .await
        .expect("first state")["id"]
        .as_u64()
        .expect("first id");
    factory_observer.clients()[0].crash();
    for _ in 0..20 {
        if manager.instance_count() == 0 {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    let second = manager
        .acquire(root.path())
        .await
        .expect("restarted client");
    let second_id = second
        .request("mock/state", Value::Null, Duration::from_secs(1))
        .await
        .expect("second state")["id"]
        .as_u64()
        .expect("second id");
    assert_ne!(first_id, second_id);
    let _ = manager.close_all().await;
}

#[test]
fn root_guard_document_normalization_is_local_bounded_and_utf8() {
    let root = TestRoot::new("document");
    fs::create_dir(root.path().join("src")).expect("create src");
    let source = root.path().join("src/lib.rs");
    fs::write(&source, "fn main() {}\n").expect("write source");
    let guard = workspace::RootGuard::new([root.path().to_owned()], std::iter::empty())
        .expect("root guard");
    let uri = normalize::path_to_file_uri(&source).expect("source uri");
    let document = lsp::documents::normalize_uri_under_guard(&guard, root.path(), &uri, 1024)
        .expect("normalized document");
    assert_eq!(document.relative_path, PathBuf::from("src/lib.rs"));
    assert_eq!(document.text, "fn main() {}\n");
    assert!(
        lsp::documents::normalize_uri_under_guard(
            &guard,
            root.path(),
            "https://example.invalid/source.rs",
            1024,
        )
        .is_err()
    );
}

#[test]
fn document_sync_capabilities_keep_open_close_separate_from_change_kind() {
    assert_eq!(
        normalize::document_sync_options(&json!({
            "capabilities": {"textDocumentSync": {"openClose": true, "change": 0}}
        })),
        DocumentSyncOptions {
            open_close: true,
            change: 0,
        }
    );
    assert_eq!(
        normalize::document_sync_options(&json!({
            "capabilities": {"textDocumentSync": {"openClose": false, "change": 2}}
        })),
        DocumentSyncOptions {
            open_close: false,
            change: 2,
        }
    );
}

#[test]
fn schema_fixture_is_valid() {
    let value: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/lsp/manager/schema-valid.json"
    ))
    .expect("schema fixture");
    let schema = BinaryConfigSchema::from_json(&value).expect("schema keys");
    assert!(schema.supports_workspace_code_deny());
}

#[cfg(unix)]
#[test]
fn document_read_uses_the_open_descriptor_during_a_symlink_swap() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("document-swap");
    let outside = TestRoot::new("document-swap-outside");
    let source = root.path().join("source.rs");
    let outside_source = outside.path().join("secret.rs");
    fs::write(&source, b"fn safe() {}\n").expect("write source");
    fs::write(&outside_source, b"fn secret() {}\n").expect("write outside source");

    let read = read_workspace_file_with_hook(root.path(), Path::new("source.rs"), || {
        fs::remove_file(&source).expect("remove source for swap");
        symlink(&outside_source, &source).expect("replace source with symlink");
    })
    .expect("read opened source");
    assert_eq!(read, "fn safe() {}\n");
    assert!(read_workspace_file(root.path(), Path::new("source.rs")).is_none());
    assert_eq!(
        fs::read(&outside_source).expect("read outside source"),
        b"fn secret() {}\n"
    );
}
