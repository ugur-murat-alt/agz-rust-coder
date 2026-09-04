use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::SystemTime,
};

use agz_rust_coder::docs::{
    CacheIdentity, CargoLockSource, DocsCache, DocsFallback, DocsInput, DocsOptions, DocsProvider,
    DocsResolver, DocsStatus, GeneratedPage, LocalDocGenerator, LocalDocRequest,
    UnavailableNetworkClient, page_candidates, parse_cargo_lock_dependency,
};
use agz_rust_coder::workspace::{AuthorizedRoot, RootGuard};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-coder-docs-{}-{stamp}-{nonce}",
                std::process::id()
            ));
        fs::create_dir(&path).expect("create docs temp root");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn authorized(path: &Path) -> Arc<AuthorizedRoot> {
    RootGuard::new([path.to_owned()], Vec::<PathBuf>::new())
        .expect("authorize root")
        .configured_roots()[0]
        .clone()
}

fn write_workspace(root: &Path, package: &str, lock_package: &str, source: Option<&str>) {
    fs::create_dir_all(root.join("src")).expect("create workspace source");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n"
        ),
    )
    .expect("write workspace manifest");
    fs::write(root.join("src/lib.rs"), "pub fn workspace() {}\n").expect("write source");
    let source = source.map_or_else(String::new, |source| format!("source = \"{source}\"\n"));
    fs::write(
        root.join("Cargo.lock"),
        format!(
            "version = 4\n\n[[package]]\nname = \"{lock_package}\"\nversion = \"0.1.0\"\n{source}"
        ),
    )
    .expect("write lockfile");
}

#[derive(Debug)]
struct RecordingGenerator(Arc<AtomicBool>);

#[derive(Debug)]
struct CountingGenerator(Arc<AtomicUsize>);

impl LocalDocGenerator for RecordingGenerator {
    fn generate(&self, _request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String> {
        self.0.store(true, Ordering::SeqCst);
        Ok(vec![GeneratedPage {
            path: "index.html".to_owned(),
            html: b"<main><p>generated</p></main>".to_vec(),
        }])
    }
}

impl LocalDocGenerator for CountingGenerator {
    fn generate(&self, _request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![GeneratedPage {
            path: "index.html".to_owned(),
            html: b"<main><p>generated</p></main>".to_vec(),
        }])
    }
}

#[derive(Debug)]
struct CancellingGenerator {
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

#[derive(Debug)]
struct BlockingGenerator {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<AtomicBool>,
}

#[derive(Debug)]
struct CleanupFailingGenerator;

impl LocalDocGenerator for CleanupFailingGenerator {
    fn generate(&self, _request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String> {
        Err("local cargo doc cleanup incomplete: injected test failure".to_owned())
    }
}

impl LocalDocGenerator for BlockingGenerator {
    fn generate(&self, request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) && !request.cancellation.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(vec![GeneratedPage {
            path: "index.html".to_owned(),
            html: b"<main><p>singleflight</p></main>".to_vec(),
        }])
    }
}

impl LocalDocGenerator for CancellingGenerator {
    fn generate(&self, request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String> {
        self.started.store(true, Ordering::SeqCst);
        while !request.cancellation.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.finished.store(true, Ordering::SeqCst);
        Err("cancelled".to_owned())
    }
}

#[test]
fn lockfile_resolution_requires_an_exact_unambiguous_package() {
    let lock = r#"
version = 4

[[package]]
name = "demo-crate"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[[package]]
name = "demo-crate"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#;
    let candidates = parse_cargo_lock_dependency(lock, "demo-crate", None, None)
        .expect_err("versionless lookup must be ambiguous");
    assert_eq!(candidates.len(), 2);

    let exact = parse_cargo_lock_dependency(lock, "demo_crate", Some("2.0.0"), None)
        .expect("parse exact package")
        .expect("find exact package");
    assert_eq!(exact.name, "demo-crate");
    assert_eq!(exact.version, "2.0.0");
    assert_eq!(exact.source, CargoLockSource::Registry);
}

#[test]
fn complete_cache_publishes_and_reads_only_safe_bounded_pages() {
    let root = TempRoot::new();
    let cache = DocsCache::new(&root.0);
    let identity = CacheIdentity {
        crate_name: "demo-crate".to_owned(),
        version: "1.2.3".to_owned(),
        source: "registry+https://github.com/rust-lang/crates.io-index".to_owned(),
        fingerprint: "source-fingerprint".to_owned(),
    };
    let page = GeneratedPage {
        path: "struct.Widget.html".to_owned(),
        html: br#"<html><main><section class="docblock"><p>Widget documentation.</p></section></main></html>"#.to_vec(),
    };
    let entry = cache
        .publish_pages(&identity, "demo-crate", &[page], None)
        .expect("publish complete cache entry");
    assert!(entry.starts_with(&root.0));
    assert!(cache.is_complete(&identity));

    let page = cache
        .read_page(&identity, "demo-crate", &page_candidates(Some("Widget")))
        .expect("read cached item page");
    assert!(page.text.contains("Widget documentation"));

    let wrong_identity = CacheIdentity {
        fingerprint: "changed".to_owned(),
        ..identity
    };
    assert!(
        cache
            .read_page(
                &wrong_identity,
                "demo-crate",
                &page_candidates(Some("Widget"))
            )
            .is_none()
    );
}

#[test]
fn generation_lock_files_are_bounded_by_retention_cleanup() {
    let root = TempRoot::new();
    let cache = DocsCache::new(&root.0);
    for index in 0..300 {
        let generation = cache
            .prepare_generation(&CacheIdentity {
                crate_name: "lock-retention".to_owned(),
                version: "0.1.0".to_owned(),
                source: "path".to_owned(),
                fingerprint: format!("fingerprint-{index}"),
            })
            .expect("prepare generation");
        drop(generation);
    }

    let locks = fs::read_dir(&root.0)
        .expect("read cache root")
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".generation.lock")
        })
        .count();
    assert!(locks <= 256, "generation lock count: {locks}");
}

#[cfg(unix)]
#[test]
fn incomplete_fingerprint_never_reads_a_shared_cache_entry() {
    use std::os::unix::fs::symlink;

    let workspace = TempRoot::new();
    write_workspace(&workspace.0, "workspace", "workspace", None);
    let outside = TempRoot::new();
    symlink(&outside.0, workspace.0.join("unreadable-link")).expect("create fingerprint symlink");
    let cache_root = TempRoot::new();
    let cache = DocsCache::new(&cache_root.0);
    cache
        .publish_pages(
            &CacheIdentity {
                crate_name: "workspace".to_owned(),
                version: "0.1.0".to_owned(),
                source: "path".to_owned(),
                fingerprint: "incomplete".to_owned(),
            },
            "workspace",
            &[GeneratedPage {
                path: "struct.Leak.html".to_owned(),
                html: b"<main><p>other root secret docs</p></main>".to_vec(),
            }],
            None,
        )
        .expect("seed legacy incomplete cache entry");

    let result = DocsResolver::default().resolve(
        &DocsInput {
            dir: workspace.0.display().to_string(),
            crate_name: "workspace".to_owned(),
            symbol: Some("Leak".to_owned()),
            version: Some("0.1.0".to_owned()),
            source: None,
            expensive_fallback: false,
        },
        &DocsOptions {
            fallback: DocsFallback::Off,
            cache_dir: Some(cache_root.0.clone()),
            workspace_authority: Some(authorized(&workspace.0)),
            ..DocsOptions::default()
        },
    );

    assert_eq!(result.status, DocsStatus::Unavailable, "{result:#?}");
    assert!(result.text.is_none());
    assert!(
        result
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("fingerprint is incomplete"))
    );
}

#[test]
fn docs_never_discovers_a_manifest_above_the_authorized_root() {
    let outer = TempRoot::new();
    write_workspace(&outer.0, "outer", "outer", None);
    let allowed = outer.0.join("allowed");
    fs::create_dir(&allowed).expect("create allowed root");
    let input = DocsInput {
        dir: allowed.display().to_string(),
        crate_name: "outer".to_owned(),
        symbol: None,
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: false,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Off,
        workspace_authority: Some(authorized(&allowed)),
        ..DocsOptions::default()
    };

    let result = DocsResolver::default().resolve(&input, &options);

    assert_eq!(result.status, DocsStatus::Unavailable);
    assert!(result.workspace_root.is_none());
    assert!(
        result
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("Cargo.toml was not found"))
    );
}

#[cfg(unix)]
#[test]
fn local_generation_rejects_a_cache_symlink_before_the_generator_runs() {
    use std::os::unix::fs::symlink;

    let workspace = TempRoot::new();
    write_workspace(&workspace.0, "workspace", "workspace", None);
    let cache_parent = TempRoot::new();
    let source_target = workspace.0.join("source-cache");
    let cache_alias = cache_parent.0.join("cache-alias");
    symlink(&source_target, &cache_alias).expect("create cache symlink");
    let called = Arc::new(AtomicBool::new(false));
    let resolver = DocsResolver::with_clients(
        Arc::new(UnavailableNetworkClient),
        Arc::new(RecordingGenerator(Arc::clone(&called))),
    );
    let input = DocsInput {
        dir: workspace.0.display().to_string(),
        crate_name: "workspace".to_owned(),
        symbol: None,
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: true,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Local,
        cache_dir: Some(cache_alias),
        workspace_authority: Some(authorized(&workspace.0)),
        expensive_fallback: true,
        ..DocsOptions::default()
    };

    let result = resolver.resolve(&input, &options);

    assert_eq!(result.status, DocsStatus::Unavailable);
    assert!(!called.load(Ordering::SeqCst));
    assert!(!source_target.exists());
}

#[cfg(unix)]
#[test]
fn prepared_generation_detects_a_parent_swap() {
    use std::os::unix::fs::symlink;

    let cache_root = TempRoot::new();
    let outside = TempRoot::new();
    let identity = CacheIdentity {
        crate_name: "swap-test".to_owned(),
        version: "0.1.0".to_owned(),
        source: "path".to_owned(),
        fingerprint: "fixed".to_owned(),
    };
    let cache = DocsCache::new(&cache_root.0);
    let generation = cache
        .prepare_generation(&identity)
        .expect("prepare generation");
    let entry = cache.entry_path(&identity);
    let moved = cache_root.0.join("moved-entry");
    fs::rename(&entry, &moved).expect("move prepared entry");
    symlink(&outside.0, &entry).expect("replace entry with symlink");

    assert!(generation.validate().is_err());
    assert!(outside.0.read_dir().expect("read outside").next().is_none());
}

#[test]
fn local_generation_observes_cancellation_before_resolution_returns() {
    let workspace = TempRoot::new();
    write_workspace(&workspace.0, "workspace", "workspace", None);
    let cache = TempRoot::new();
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let resolver = DocsResolver::with_clients(
        Arc::new(UnavailableNetworkClient),
        Arc::new(CancellingGenerator {
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
        }),
    );
    let input = DocsInput {
        dir: workspace.0.display().to_string(),
        crate_name: "workspace".to_owned(),
        symbol: None,
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: true,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Local,
        cache_dir: Some(cache.0.clone()),
        workspace_authority: Some(authorized(&workspace.0)),
        expensive_fallback: true,
        ..DocsOptions::default()
    };
    let cancellation = tokio_util::sync::CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::spawn(move || {
        resolver.resolve_with_cancellation(&input, &options, worker_cancellation)
    });
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }

    cancellation.cancel();
    let result = worker.join().expect("join resolver");

    assert_eq!(result.status, DocsStatus::Unavailable);
    assert!(finished.load(Ordering::SeqCst));
    assert!(
        result
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("cancelled"))
    );
}

#[test]
fn local_generation_exposes_incomplete_process_cleanup() {
    let workspace = TempRoot::new();
    write_workspace(&workspace.0, "workspace", "workspace", None);
    fs::write(
        workspace.0.join("src/lib.rs"),
        "/// Source fallback must not hide cleanup failure.\npub struct CleanupDocs;\n",
    )
    .expect("write source fallback docs");
    let cache = TempRoot::new();
    let resolver = DocsResolver::with_clients(
        Arc::new(UnavailableNetworkClient),
        Arc::new(CleanupFailingGenerator),
    );
    let result = resolver.resolve(
        &DocsInput {
            dir: workspace.0.display().to_string(),
            crate_name: "workspace".to_owned(),
            symbol: Some("CleanupDocs".to_owned()),
            version: Some("0.1.0".to_owned()),
            source: None,
            expensive_fallback: true,
        },
        &DocsOptions {
            fallback: DocsFallback::Local,
            cache_dir: Some(cache.0.clone()),
            workspace_authority: Some(authorized(&workspace.0)),
            expensive_fallback: true,
            ..DocsOptions::default()
        },
    );

    assert_eq!(result.status, DocsStatus::Unavailable);
    assert!(result.is_error);
    assert!(!result.cleanup_complete);
}

#[test]
fn local_singleflight_follower_cancellation_does_not_wait_for_the_owner() {
    let workspace = TempRoot::new();
    write_workspace(&workspace.0, "workspace", "workspace", None);
    let cache = TempRoot::new();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let resolver = Arc::new(DocsResolver::with_clients(
        Arc::new(UnavailableNetworkClient),
        Arc::new(BlockingGenerator {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
        }),
    ));
    let input = DocsInput {
        dir: workspace.0.display().to_string(),
        crate_name: "workspace".to_owned(),
        symbol: None,
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: true,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Local,
        cache_dir: Some(cache.0.clone()),
        workspace_authority: Some(authorized(&workspace.0)),
        expensive_fallback: true,
        timeout_ms: 30_000,
        ..DocsOptions::default()
    };
    let owner_resolver = Arc::clone(&resolver);
    let owner_input = input.clone();
    let owner_options = options.clone();
    let owner = std::thread::spawn(move || owner_resolver.resolve(&owner_input, &owner_options));
    while calls.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    let follower_cancellation = tokio_util::sync::CancellationToken::new();
    let follower_token = follower_cancellation.clone();
    let mut follower_input = input;
    follower_input.symbol = Some("different-symbol".to_owned());
    let follower = std::thread::spawn(move || {
        resolver.resolve_with_cancellation(&follower_input, &options, follower_token)
    });
    std::thread::sleep(std::time::Duration::from_millis(20));

    follower_cancellation.cancel();
    let started = std::time::Instant::now();
    let follower_result = follower.join().expect("join follower");

    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(follower_result.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.store(true, Ordering::SeqCst);
    let _ = owner.join().expect("join owner");
}

#[test]
fn local_generation_lock_avoids_duplicate_work_across_resolvers() {
    let workspace = TempRoot::new();
    write_workspace(&workspace.0, "workspace", "workspace", None);
    let cache = TempRoot::new();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let generator: Arc<dyn LocalDocGenerator> = Arc::new(BlockingGenerator {
        calls: Arc::clone(&calls),
        release: Arc::clone(&release),
    });
    let first =
        DocsResolver::with_clients(Arc::new(UnavailableNetworkClient), Arc::clone(&generator));
    let second = DocsResolver::with_clients(Arc::new(UnavailableNetworkClient), generator);
    let input = DocsInput {
        dir: workspace.0.display().to_string(),
        crate_name: "workspace".to_owned(),
        symbol: None,
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: true,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Local,
        cache_dir: Some(cache.0.clone()),
        workspace_authority: Some(authorized(&workspace.0)),
        expensive_fallback: true,
        timeout_ms: 30_000,
        ..DocsOptions::default()
    };
    let first_input = input.clone();
    let first_options = options.clone();
    let owner = std::thread::spawn(move || first.resolve(&first_input, &first_options));
    while calls.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    let follower = std::thread::spawn(move || second.resolve(&input, &options));
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    release.store(true, Ordering::SeqCst);
    let owner = owner.join().expect("join generation owner");
    let follower = follower.join().expect("join generation follower");
    assert_eq!(owner.status, DocsStatus::Found, "{owner:#?}");
    assert_eq!(follower.status, DocsStatus::Found, "{follower:#?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn workspace_path_dependency_source_uses_explicit_capabilities() {
    let parent = TempRoot::new();
    let workspace = parent.0.join("workspace");
    let dependency = parent.0.join("dependency");
    let unrelated = parent.0.join("unrelated");
    write_workspace(&workspace, "workspace", "demo-dependency", None);
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"workspace\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[workspace.dependencies]\ndemo = { package = \"demo-dependency\", path = \"../dependency\", version = \"^0.1\" }\n\n[target.'cfg(unix)'.dependencies]\ndemo = { workspace = true }\n",
    )
    .expect("write workspace dependency");
    fs::create_dir_all(dependency.join("src")).expect("create dependency source");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"demo-dependency\"\nversion.workspace = true\nedition = \"2024\"\n\n[workspace]\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    )
    .expect("write dependency manifest");
    fs::write(
        dependency.join("src/lib.rs"),
        "/// Exact dependency docs.\npub struct Widget;\n",
    )
    .expect("write dependency docs");
    fs::create_dir_all(unrelated.join("src")).expect("create unrelated source");
    fs::write(
        unrelated.join("Cargo.toml"),
        "[package]\nname = \"demo-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write unrelated manifest");
    fs::write(
        unrelated.join("src/lib.rs"),
        "/// Unrelated docs must not escape the graph.\npub struct Widget;\n",
    )
    .expect("write unrelated docs");
    let input = DocsInput {
        dir: workspace.display().to_string(),
        crate_name: "demo-dependency".to_owned(),
        symbol: Some("Widget".to_owned()),
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: false,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Network,
        workspace_authority: Some(authorized(&workspace)),
        dependency_authorities: vec![authorized(&unrelated), authorized(&dependency)],
        ..DocsOptions::default()
    };

    let result = DocsResolver::default().resolve(&input, &options);

    assert_eq!(result.status, DocsStatus::Found, "{result:#?}");
    assert_eq!(result.provider, Some(DocsProvider::Source));
    assert!(
        result
            .text
            .as_deref()
            .is_some_and(|text| text.contains("Exact dependency docs"))
    );
    assert!(
        !result
            .text
            .as_deref()
            .unwrap_or_default()
            .contains("Unrelated")
    );

    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"workspace\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[workspace.dependencies]\ndemo = { package = \"demo-dependency\", path = \"../dependency\", version = \"^9\" }\n\n[target.'cfg(unix)'.dependencies]\ndemo = { workspace = true }\n",
    )
    .expect("change path version requirement");
    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Unavailable, "{result:#?}");
    assert!(result.is_error);
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"workspace\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[workspace.dependencies]\ndemo = { package = \"demo-dependency\", path = \"../dependency\", version = \"^0.1\" }\n\n[target.'cfg(unix)'.dependencies]\ndemo = { workspace = true }\n",
    )
    .expect("restore path version requirement");

    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"demo-dependency\"\nversion.workspace = true\nedition = \"2024\"\n\n[workspace]\n\n[workspace.package]\nversion = \"9.9.9\"\n",
    )
    .expect("change inherited dependency version");
    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Unavailable, "{result:#?}");
    assert!(result.is_error);
}

#[test]
fn transitive_external_build_dependency_changes_cache_identity() {
    let parent = TempRoot::new();
    let workspace = parent.0.join("workspace");
    let external = parent.0.join("external");
    let member = external.join("member");
    let helper = external.join("helper");
    let wrong = parent.0.join("wrong-helper");
    write_workspace(&workspace, "workspace", "direct", None);
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"workspace\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[workspace.dependencies]\nhelper = { package = \"wrong-helper\", path = \"../wrong-helper\", version = \"0.1.0\" }\n\n[dependencies]\ndirect = { path = \"../external/member\", version = \"0.1.0\" }\n",
    )
    .expect("write main workspace manifest");
    fs::write(
        workspace.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"workspace\"\nversion = \"0.1.0\"\ndependencies = [\"direct\"]\n\n[[package]]\nname = \"direct\"\nversion = \"0.1.0\"\ndependencies = [\"external-helper\"]\n\n[[package]]\nname = \"external-helper\"\nversion = \"0.1.0\"\n",
    )
    .expect("write main workspace lockfile");
    fs::create_dir_all(member.join("src")).expect("create external member");
    fs::write(
        external.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nhelper = { package = \"external-helper\", path = \"helper\", version = \"0.1.0\" }\n",
    )
    .expect("write external workspace manifest");
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"direct\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nhelper = { workspace = true }\n\n[build-dependencies]\nhelper = { workspace = true }\n\n[dev-dependencies]\nhelper = { workspace = true }\n",
    )
    .expect("write external member manifest");
    fs::write(member.join("src/lib.rs"), "pub struct Direct;\n")
        .expect("write external member source");
    fs::create_dir_all(helper.join("src")).expect("create external helper");
    fs::write(
        helper.join("Cargo.toml"),
        "[package]\nname = \"external-helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write external helper manifest");
    fs::write(helper.join("src/lib.rs"), "pub fn helper() {}\n")
        .expect("write external helper source");
    fs::create_dir_all(wrong.join("src")).expect("create wrong helper");
    fs::write(
        wrong.join("Cargo.toml"),
        "[package]\nname = \"wrong-helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write wrong helper manifest");
    fs::write(wrong.join("src/lib.rs"), "pub fn helper() {}\n").expect("write wrong helper source");

    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = DocsResolver::with_clients(
        Arc::new(UnavailableNetworkClient),
        Arc::new(CountingGenerator(Arc::clone(&calls))),
    );
    let input = DocsInput {
        dir: workspace.display().to_string(),
        crate_name: "direct".to_owned(),
        symbol: None,
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: false,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Local,
        cache_dir: Some(parent.0.join("docs-cache")),
        workspace_authority: Some(authorized(&workspace)),
        dependency_authorities: vec![authorized(&external), authorized(&wrong)],
        timeout_ms: 30_000,
        ..DocsOptions::default()
    };

    let first = resolver.resolve(&input, &options);
    assert_eq!(first.status, DocsStatus::Found, "{first:#?}");
    assert_eq!(first.provider, Some(DocsProvider::Local));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    fs::write(
        helper.join("src/lib.rs"),
        "pub fn helper() {}\npub fn changed_build_input() {}\n",
    )
    .expect("change external build dependency source");
    let second = resolver.resolve(&input, &options);
    assert_eq!(second.status, DocsStatus::Found, "{second:#?}");
    assert_eq!(second.provider, Some(DocsProvider::Local));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn transitive_dev_entries_are_graph_edges_but_overrides_are_not() {
    let parent = TempRoot::new();
    let workspace = parent.0.join("workspace");
    let direct = parent.0.join("direct");
    let fake_dev = parent.0.join("fake-dev");
    let fake_override = parent.0.join("fake-override");
    write_workspace(&workspace, "workspace", "fake-dependency", None);
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"workspace\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\ndirect = { path = \"../direct\", version = \"^0.1\" }\n",
    )
    .expect("write direct dependency edge");
    fs::create_dir_all(direct.join("src")).expect("create direct dependency");
    fs::write(
        direct.join("Cargo.toml"),
        "[package]\nname = \"direct\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dev-dependencies]\nfake = { package = \"fake-dependency\", path = \"../fake-dev\" }\n\n[replace]\n\"fake-override:0.1.0\" = { path = \"../fake-override\" }\n",
    )
    .expect("write transitive dev and override entries");
    fs::write(
        workspace.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"workspace\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"direct\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"fake-dependency\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"fake-override\"\nversion = \"0.1.0\"\n",
    )
    .expect("write dependency graph lockfile");
    fs::write(direct.join("src/lib.rs"), "pub struct Direct;\n").expect("write direct source");
    fs::create_dir_all(fake_dev.join("src")).expect("create fake dev dependency");
    fs::write(
        fake_dev.join("Cargo.toml"),
        "[package]\nname = \"fake-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write fake dev manifest");
    fs::write(
        fake_dev.join("src/lib.rs"),
        "/// Reachable through the transitive dev dependency graph.\npub struct Fake;\n",
    )
    .expect("write fake dev source");
    fs::create_dir_all(fake_override.join("src")).expect("create fake override dependency");
    fs::write(
        fake_override.join("Cargo.toml"),
        "[package]\nname = \"fake-override\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write fake override manifest");
    fs::write(
        fake_override.join("src/lib.rs"),
        "/// Must not be reachable through a transitive replace entry.\npub struct Override;\n",
    )
    .expect("write fake override source");

    let result = DocsResolver::default().resolve(
        &DocsInput {
            dir: workspace.display().to_string(),
            crate_name: "fake-dependency".to_owned(),
            symbol: Some("Fake".to_owned()),
            version: Some("0.1.0".to_owned()),
            source: None,
            expensive_fallback: false,
        },
        &DocsOptions {
            fallback: DocsFallback::Network,
            workspace_authority: Some(authorized(&workspace)),
            dependency_authorities: vec![
                authorized(&direct),
                authorized(&fake_dev),
                authorized(&fake_override),
            ],
            ..DocsOptions::default()
        },
    );

    assert_eq!(result.status, DocsStatus::Found, "{result:#?}");
    assert_eq!(result.provider, Some(DocsProvider::Source));

    let result = DocsResolver::default().resolve(
        &DocsInput {
            dir: workspace.display().to_string(),
            crate_name: "fake-override".to_owned(),
            symbol: Some("Override".to_owned()),
            version: Some("0.1.0".to_owned()),
            source: None,
            expensive_fallback: false,
        },
        &DocsOptions {
            fallback: DocsFallback::Network,
            workspace_authority: Some(authorized(&workspace)),
            dependency_authorities: vec![
                authorized(&direct),
                authorized(&fake_dev),
                authorized(&fake_override),
            ],
            ..DocsOptions::default()
        },
    );

    assert_eq!(result.status, DocsStatus::Unavailable, "{result:#?}");
    assert!(result.text.is_none());
}

#[test]
fn virtual_workspace_members_are_bounded_source_graph_edges() {
    let workspace = TempRoot::new();
    let member = workspace.0.join("crates/member");
    fs::create_dir_all(member.join("src")).expect("create virtual workspace member");
    fs::write(
        workspace.0.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
    )
    .expect("write virtual workspace manifest");
    fs::write(
        workspace.0.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"virtual-member\"\nversion = \"0.1.0\"\n",
    )
    .expect("write virtual workspace lockfile");
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"virtual-member\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write member manifest");
    fs::write(
        member.join("src/lib.rs"),
        "/// Documentation from an authorized virtual workspace member.\npub struct VirtualItem;\n",
    )
    .expect("write member source");

    let result = DocsResolver::default().resolve(
        &DocsInput {
            dir: workspace.0.display().to_string(),
            crate_name: "virtual-member".to_owned(),
            symbol: Some("VirtualItem".to_owned()),
            version: Some("0.1.0".to_owned()),
            source: None,
            expensive_fallback: false,
        },
        &DocsOptions {
            fallback: DocsFallback::Network,
            workspace_authority: Some(authorized(&workspace.0)),
            ..DocsOptions::default()
        },
    );

    assert_eq!(result.status, DocsStatus::Found, "{result:#?}");
    assert_eq!(result.provider, Some(DocsProvider::Source));
    assert!(
        result
            .text
            .as_deref()
            .is_some_and(|text| text.contains("authorized virtual workspace member"))
    );
}

#[test]
fn replace_path_dependency_requires_the_exact_package_id_version() {
    let parent = TempRoot::new();
    let workspace = parent.0.join("workspace");
    let replacement = parent.0.join("replacement");
    write_workspace(&workspace, "workspace", "replace-demo", None);
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"workspace\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[replace]\n\"replace-demo:0.1.0\" = { path = \"../replacement\" }\n",
    )
    .expect("write replace edge");
    fs::create_dir_all(replacement.join("src")).expect("create replacement source");
    fs::write(
        replacement.join("Cargo.toml"),
        "[package]\nname = \"replace-demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write replacement manifest");
    fs::write(
        replacement.join("src/lib.rs"),
        "/// Replacement docs.\npub struct Replacement;\n",
    )
    .expect("write replacement docs");
    let input = DocsInput {
        dir: workspace.display().to_string(),
        crate_name: "replace-demo".to_owned(),
        symbol: Some("Replacement".to_owned()),
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: false,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Network,
        workspace_authority: Some(authorized(&workspace)),
        dependency_authorities: vec![authorized(&replacement)],
        ..DocsOptions::default()
    };

    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Found, "{result:#?}");

    fs::write(
        replacement.join("Cargo.toml"),
        "[package]\nname = \"replace-demo\"\nversion = \"9.9.9\"\nedition = \"2024\"\n",
    )
    .expect("replace manifest version");
    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Unavailable, "{result:#?}");
    assert!(result.is_error);
}

#[test]
fn git_source_requires_the_exact_locked_revision_and_origin() {
    let workspace = TempRoot::new();
    let revision = "0123456789abcdef0123456789abcdef01234567";
    let source = format!("git+https://example.com/demo.git?rev=main#{revision}");
    write_workspace(&workspace.0, "workspace", "git-demo", Some(&source));
    let cargo_home = TempRoot::new();
    let checkout = cargo_home.0.join("git/checkouts/demo-abc/rev123");
    let git_dir = cargo_home.0.join("git/db/demo-abc/worktrees/rev123");
    fs::create_dir_all(checkout.join("src")).expect("create checkout source");
    fs::create_dir_all(&git_dir).expect("create git worktree metadata");
    fs::write(
        checkout.join("Cargo.toml"),
        "[package]\nname = \"git-demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write git manifest");
    fs::write(
        checkout.join("src/lib.rs"),
        "/// Exact git docs.\npub struct GitWidget;\n",
    )
    .expect("write git docs");
    fs::write(
        checkout.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .expect("write gitdir pointer");
    fs::write(git_dir.join("HEAD"), format!("{revision}\n")).expect("write git head");
    fs::write(
        cargo_home.0.join("git/db/demo-abc/config"),
        "[remote \"origin\"]\n\turl = https://example.com/demo.git\n",
    )
    .expect("write git config");
    let input = DocsInput {
        dir: workspace.0.display().to_string(),
        crate_name: "git-demo".to_owned(),
        symbol: Some("GitWidget".to_owned()),
        version: Some("0.1.0".to_owned()),
        source: Some(source),
        expensive_fallback: false,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Network,
        workspace_authority: Some(authorized(&workspace.0)),
        cargo_home_authority: Some(authorized(&cargo_home.0)),
        timeout_ms: 30_000,
        ..DocsOptions::default()
    };

    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Found, "{result:#?}");
    assert_eq!(result.provider, Some(DocsProvider::Source));

    fs::write(
        git_dir.join("HEAD"),
        "ffffffffffffffffffffffffffffffffffffffff\n",
    )
    .expect("replace git head");
    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Unavailable, "{result:#?}");

    fs::write(git_dir.join("HEAD"), format!("{revision}\n")).expect("restore git head");
    fs::write(
        cargo_home.0.join("git/db/demo-abc/config"),
        "[remote \"origin\"]\n\turl = https://example.com/wrong.git\n",
    )
    .expect("replace git origin");
    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Unavailable, "{result:#?}");
}

#[test]
#[ignore = "requires docs.rs network access"]
fn real_docs_rs_adapter_resolves_the_exact_lockfile_version() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cache = TempRoot::new();
    let input = DocsInput {
        dir: workspace.display().to_string(),
        crate_name: "reqwest".to_owned(),
        symbol: Some("Client".to_owned()),
        version: Some("0.13.2".to_owned()),
        source: None,
        expensive_fallback: false,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Network,
        cache_dir: Some(cache.0.clone()),
        timeout_ms: 30_000,
        ..DocsOptions::default()
    };

    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Found, "{result:#?}");
    assert_eq!(result.provider, Some(DocsProvider::Network));
    assert_eq!(result.version.as_deref(), Some("0.13.2"));
    assert!(
        result
            .text
            .as_deref()
            .is_some_and(|text| text.contains("Client"))
    );
}

#[test]
#[ignore = "runs a real bounded cargo doc process"]
fn real_local_generator_writes_only_to_the_external_cache() {
    let workspace =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stage7/clean");
    let _ = fs::remove_dir_all(workspace.join("target"));
    let before = fs::read(workspace.join("src/lib.rs")).expect("read source before");
    let cache = TempRoot::new();
    let input = DocsInput {
        dir: workspace.display().to_string(),
        crate_name: "stage7-clean".to_owned(),
        symbol: Some("answer".to_owned()),
        version: Some("0.1.0".to_owned()),
        source: None,
        expensive_fallback: true,
    };
    let options = DocsOptions {
        fallback: DocsFallback::Local,
        cache_dir: Some(cache.0.clone()),
        timeout_ms: 60_000,
        expensive_fallback: true,
        ..DocsOptions::default()
    };

    let result = DocsResolver::default().resolve(&input, &options);
    assert_eq!(result.status, DocsStatus::Found, "{result:#?}");
    assert_eq!(result.provider, Some(DocsProvider::Local));
    assert_eq!(
        fs::read(workspace.join("src/lib.rs")).expect("read source after"),
        before
    );
    assert!(!workspace.join("target").exists());
}

#[cfg(unix)]
#[test]
fn selected_child_replacement_rejects_local_doc_generation_before_cargo_runs() {
    use std::time::{Duration, Instant};

    use agz_rust_coder::workspace::ClientRoots;

    let sandbox = TempRoot::new();
    let root = sandbox.0.join("workspace");
    fs::create_dir(&root).expect("create original workspace root");
    fs::create_dir(root.join("src")).expect("create original source directory");
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"original\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write original manifest");
    fs::write(root.join("src/lib.rs"), b"pub fn value() -> u8 { 1 }\n")
        .expect("write original source");

    let guard = RootGuard::new([sandbox.0.clone()], Vec::<PathBuf>::new())
        .expect("authorize configured parent");
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot configured parent");
    let authority = snapshot
        .select(Some(&root))
        .expect("select child workspace")
        .requested_authority()
        .clone();
    let marker = sandbox.0.join("replacement-doc-generator-ran");
    let original = sandbox.0.join("workspace-original");
    fs::rename(&root, &original).expect("rename authorized root");
    fs::create_dir(&root).expect("create replacement workspace root");
    fs::create_dir(root.join("src")).expect("create replacement source directory");
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"replacement\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
    )
    .expect("write replacement manifest");
    fs::write(
        root.join("Cargo.lock"),
        b"version = 4\n\n[[package]]\nname = \"replacement\"\nversion = \"0.1.0\"\n",
    )
    .expect("write replacement lockfile");
    fs::write(root.join("src/lib.rs"), b"pub fn value() -> u8 { 2 }\n")
        .expect("write replacement source");
    fs::write(
        root.join("build.rs"),
        format!(
            "use std::{{env, fs, path::PathBuf}};\nfn main() {{\n    let manifest = PathBuf::from(env::var_os(\"CARGO_MANIFEST_DIR\").unwrap());\n    let marker = manifest.parent().unwrap().join(\"{}\");\n    fs::write(marker, b\"replacement doc generator ran\").unwrap();\n}}\n",
            marker.file_name().expect("marker file name").to_string_lossy(),
        ),
    )
    .expect("write replacement build script");

    let request = LocalDocRequest {
        manifest_path: root.join("Cargo.toml"),
        package: "replacement".to_owned(),
        target_dir: sandbox.0.join("doc-target"),
        deadline: Instant::now() + Duration::from_secs(5),
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let result =
        agz_rust_coder::docs::CargoDocGenerator::default().generate_authorized(&request, authority);

    assert!(
        result.is_err(),
        "replacement root must fail before local cargo runs: {result:?}"
    );
    assert!(!marker.exists(), "replacement build script must not run");
    assert!(original.join("Cargo.toml").is_file());
}
