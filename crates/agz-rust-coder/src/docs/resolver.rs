//! Exact-version documentation resolution with bounded, fail-open providers.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::workspace::{AuthorizedRoot, RootGuard, WalkLimits, WorkspaceSelection};

use super::{
    cache::{CacheIdentity, DocsCache, GeneratedPage},
    html::{DOCS_MAX_HTML_BYTES, DOCS_MAX_OUTPUT, page_candidates, strip_rustdoc_html},
};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
pub const DOCS_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
pub const DOCS_USER_AGENT: &str = "agz-rust-coder rust.docs (advisory)";
pub const CRATES_IO_REGISTRY: &str = "https://github.com/rust-lang/crates.io-index";
const HTTP_TIMEOUT: Duration = DOCS_FETCH_TIMEOUT;
const MAX_LOCK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 2_000;
const MAX_FINGERPRINT_FILES: usize = 10_000;
const MAX_FINGERPRINT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GIT_CHECKOUT_FILES: usize = 2_000;
const LOCAL_DOC_CLEANUP_INCOMPLETE: &str = "local cargo doc cleanup incomplete";

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocsInput {
    pub dir: String,
    #[serde(rename = "crate")]
    pub crate_name: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub expensive_fallback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocsFallback {
    Auto,
    Local,
    Network,
    Off,
}

#[derive(Debug, Clone)]
pub struct DocsOptions {
    pub timeout_ms: u64,
    pub fallback: DocsFallback,
    pub cache_dir: Option<PathBuf>,
    pub workspace_authority: Option<Arc<AuthorizedRoot>>,
    pub dependency_authorities: Vec<Arc<AuthorizedRoot>>,
    pub cargo_home_authority: Option<Arc<AuthorizedRoot>>,
    pub expensive_fallback: bool,
    pub max_output: usize,
    pub max_html_bytes: usize,
}

impl Default for DocsOptions {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            fallback: DocsFallback::Auto,
            cache_dir: None,
            workspace_authority: None,
            dependency_authorities: Vec::new(),
            cargo_home_authority: None,
            expensive_fallback: false,
            max_output: DOCS_MAX_OUTPUT,
            max_html_bytes: DOCS_MAX_HTML_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocsStatus {
    Found,
    Ambiguous,
    NotFound,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DocsProvider {
    Cache,
    Source,
    Network,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CargoLockSource {
    Registry,
    Git,
    Path,
    Unknown,
}

impl CargoLockSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Git => "git",
            Self::Path => "path",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CargoLockCandidate {
    pub name: String,
    pub version: String,
    pub source: CargoLockSource,
    pub source_url: Option<String>,
    pub raw_source: Option<String>,
    pub registry: Option<String>,
    pub git: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocsResult {
    pub status: DocsStatus,
    pub is_error: bool,
    #[serde(skip, default = "default_true")]
    pub cleanup_complete: bool,
    pub crate_name: String,
    pub version: Option<String>,
    pub provider: Option<DocsProvider>,
    pub text: Option<String>,
    pub page: Option<String>,
    pub warning: Option<String>,
    pub candidates: Vec<String>,
    pub dependency: Option<CargoLockCandidate>,
    pub workspace_root: Option<String>,
    pub manifest_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NetworkRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub max_body_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct NetworkResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub effective_url: String,
}

pub trait NetworkClient: Send + Sync {
    fn fetch(&self, request: &NetworkRequest) -> Result<NetworkResponse, String>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableNetworkClient;

impl NetworkClient for UnavailableNetworkClient {
    fn fetch(&self, _request: &NetworkRequest) -> Result<NetworkResponse, String> {
        Err("no bounded HTTPS client was configured".to_owned())
    }
}

/// Bounded HTTPS adapter for exact-version docs.rs pages.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReqwestNetworkClient;

impl NetworkClient for ReqwestNetworkClient {
    fn fetch(&self, request: &NetworkRequest) -> Result<NetworkResponse, String> {
        if !is_valid_docs_rs_url(&request.url) {
            return Err("documentation request left the fixed docs.rs HTTPS host".to_owned());
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(request.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())?;
        let mut builder = client.get(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let mut response = builder.send().map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let effective_url = response.url().as_str().to_owned();
        if !is_valid_docs_rs_url(&effective_url) {
            return Err("documentation response left the fixed docs.rs HTTPS host".to_owned());
        }
        let mut body = Vec::new();
        response
            .by_ref()
            .take((request.max_body_bytes as u64).saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|error| error.to_string())?;
        if body.len() > request.max_body_bytes {
            return Err("documentation response exceeded the body limit".to_owned());
        }
        Ok(NetworkResponse {
            status,
            body,
            effective_url,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LocalDocRequest {
    pub manifest_path: PathBuf,
    pub package: String,
    pub target_dir: PathBuf,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
}

pub trait LocalDocGenerator: Send + Sync {
    fn generate(&self, request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String>;

    fn generate_authorized(
        &self,
        request: &LocalDocRequest,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<Vec<GeneratedPage>, String> {
        let _ = authority;
        self.generate(request)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableLocalGenerator;

impl LocalDocGenerator for UnavailableLocalGenerator {
    fn generate(&self, _request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String> {
        Err("local cargo doc generation was not configured".to_owned())
    }
}

/// Explicit local fallback using bounded, process-group supervised `cargo doc`.
#[derive(Debug, Clone)]
pub struct CargoDocGenerator {
    supervisor: crate::process::ProcessSupervisor,
}

impl Default for CargoDocGenerator {
    fn default() -> Self {
        Self::new(crate::process::ProcessSupervisor::without_journal())
    }
}

impl CargoDocGenerator {
    pub fn new(supervisor: crate::process::ProcessSupervisor) -> Self {
        Self { supervisor }
    }
}

impl LocalDocGenerator for CargoDocGenerator {
    fn generate(&self, request: &LocalDocRequest) -> Result<Vec<GeneratedPage>, String> {
        self.generate_inner(request, None)
    }

    fn generate_authorized(
        &self,
        request: &LocalDocRequest,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<Vec<GeneratedPage>, String> {
        self.generate_inner(request, Some(authority))
    }
}

impl CargoDocGenerator {
    fn generate_inner(
        &self,
        request: &LocalDocRequest,
        authority: Option<Arc<AuthorizedRoot>>,
    ) -> Result<Vec<GeneratedPage>, String> {
        let remaining = request.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("local documentation deadline elapsed".to_owned());
        }
        let package_root = request
            .manifest_path
            .parent()
            .ok_or_else(|| "manifest has no package directory".to_owned())?;
        fs::create_dir_all(&request.target_dir).map_err(|error| error.to_string())?;
        let target = fs::canonicalize(&request.target_dir).map_err(|error| error.to_string())?;
        let package_root = fs::canonicalize(package_root).map_err(|error| error.to_string())?;
        if target.starts_with(&package_root) {
            return Err("local rustdoc target must remain outside the package source".to_owned());
        }
        // Retain the canonical path for authorization and output reads. Only
        // the Cargo command argument uses a verified equivalent Win32 spelling.
        #[cfg(windows)]
        let target_argument = windows_cargo_target_argument(&target)?;
        #[cfg(not(windows))]
        let target_argument = target.clone();
        let cargo = crate::tools::check::resolve_cargo(None);
        let environment = std::env::vars_os().collect::<Vec<(OsString, OsString)>>();
        let options = crate::process::ProcessRunOptions::new(&package_root)
            .with_timeout(remaining)
            .with_deadline(request.deadline)
            .with_max_output_bytes(1024 * 1024)
            .with_cancellation(request.cancellation.clone())
            .with_environment(environment);
        let arguments = [
            OsString::from("doc"),
            OsString::from("--manifest-path"),
            request.manifest_path.as_os_str().to_owned(),
            OsString::from("--package"),
            OsString::from(&request.package),
            OsString::from("--no-deps"),
            OsString::from("--locked"),
            OsString::from("--target-dir"),
            target_argument.as_os_str().to_owned(),
        ];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let result = runtime
            .block_on(async {
                match authority {
                    Some(authority) => {
                        self.supervisor
                            .run_authorized(cargo, arguments, options, authority)
                            .await
                    }
                    None => self.supervisor.run(cargo, arguments, options).await,
                }
            })
            .map_err(|error| error.to_string())?;
        if !result.drain_complete || !result.cleanup_complete {
            return Err(format!(
                "{LOCAL_DOC_CLEANUP_INCOMPLETE}: drain_complete={} cleanup_complete={}",
                result.drain_complete, result.cleanup_complete
            ));
        }
        if result.timed_out {
            return Err("local cargo doc timed out".to_owned());
        }
        if result.exit_code != 0 {
            return Err(format!(
                "local cargo doc failed: {}",
                result.stderr.chars().take(2_000).collect::<String>()
            ));
        }
        let doc_root = target.join("doc");
        let package_root = super::html::package_folder_names(&request.package)
            .into_iter()
            .map(|folder| doc_root.join(folder))
            .find(|candidate| candidate.is_dir())
            .ok_or_else(|| "local cargo doc did not produce the package pages".to_owned())?;
        collect_generated_pages(&package_root, request.deadline)
    }
}

#[cfg(windows)]
fn windows_cargo_target_argument(target: &Path) -> Result<PathBuf, String> {
    use std::path::{Component, Prefix};

    let mut components = target.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err("canonical Cargo output path has no Windows prefix".to_owned());
    };
    let Prefix::VerbatimDisk(drive) = prefix.kind() else {
        return Ok(target.to_owned());
    };
    if components.next() != Some(Component::RootDir) {
        return Err("canonical Cargo output path is not absolute".to_owned());
    }
    let mut argument = PathBuf::from(format!("{}:\\", char::from(drive)));
    for component in components {
        let Component::Normal(name) = component else {
            return Err("canonical Cargo output path contains traversal".to_owned());
        };
        argument.push(name);
    }
    // Stripping a verbatim prefix is not generally semantics-preserving (for
    // example, trailing dots or reserved names). Require identical resolution.
    let resolved = fs::canonicalize(&argument).map_err(|error| error.to_string())?;
    if resolved != target {
        return Err("Cargo output path has no identity-preserving Win32 spelling".to_owned());
    }
    Ok(argument)
}

fn collect_generated_pages(root: &Path, deadline: Instant) -> Result<Vec<GeneratedPage>, String> {
    const MAX_PAGES: usize = 2_000;
    const MAX_BYTES: u64 = 64 * 1024 * 1024;

    let root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let mut pending = vec![root.clone()];
    let mut pages = Vec::new();
    let mut total_bytes = 0u64;
    while let Some(directory) = pending.pop() {
        if Instant::now() >= deadline {
            return Err("local documentation collection timed out".to_owned());
        }
        let entries = fs::read_dir(&directory).map_err(|error| error.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("local rustdoc output contained a symlink".to_owned());
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("html")
            {
                continue;
            }
            if pages.len() >= MAX_PAGES || metadata.len() > DOCS_MAX_HTML_BYTES as u64 {
                return Err("local rustdoc output exceeded a page bound".to_owned());
            }
            total_bytes = total_bytes.saturating_add(metadata.len());
            if total_bytes > MAX_BYTES {
                return Err("local rustdoc output exceeded the total byte bound".to_owned());
            }
            let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
            if !canonical.starts_with(&root) {
                return Err("local rustdoc output escaped its target directory".to_owned());
            }
            let relative = canonical
                .strip_prefix(&root)
                .map_err(|_| "local rustdoc page escaped its package root".to_owned())?;
            let page = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if !super::html::is_safe_page_path(&page) {
                return Err("local rustdoc page path was unsafe".to_owned());
            }
            let html = fs::read(&canonical).map_err(|error| error.to_string())?;
            pages.push(GeneratedPage { path: page, html });
        }
    }
    pages.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(pages)
}

#[derive(Debug)]
pub enum ResolverError {
    InvalidInput(String),
    Io(String),
}

impl std::fmt::Display for ResolverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(message) | Self::Io(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ResolverError {}

/// URL construction and validation failures remain typed and non-fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocsUrlError {
    InvalidSegment,
    InvalidUrl,
    NonCratesIoSource,
}

impl std::fmt::Display for DocsUrlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSegment => formatter.write_str("docs.rs URL segment is invalid"),
            Self::InvalidUrl => {
                formatter.write_str("generated docs.rs URL failed fixed-host validation")
            }
            Self::NonCratesIoSource => {
                formatter.write_str("docs.rs is only valid for the crates.io registry")
            }
        }
    }
}

impl std::error::Error for DocsUrlError {}

pub struct DocsResolver {
    network: Arc<dyn NetworkClient>,
    local: Arc<dyn LocalDocGenerator>,
    flights: Arc<Mutex<HashMap<String, Arc<LocalFlight>>>>,
    authorized_local_generation: bool,
}

impl std::fmt::Debug for DocsResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DocsResolver")
            .field("flights", &self.flights)
            .finish_non_exhaustive()
    }
}

impl Default for DocsResolver {
    fn default() -> Self {
        Self::new(ReqwestNetworkClient, CargoDocGenerator::default())
    }
}

impl DocsResolver {
    pub fn with_supervisor(supervisor: crate::process::ProcessSupervisor) -> Self {
        Self::new(ReqwestNetworkClient, CargoDocGenerator::new(supervisor))
    }

    pub fn new<N, L>(network: N, local: L) -> Self
    where
        N: NetworkClient + 'static,
        L: LocalDocGenerator + 'static,
    {
        Self {
            network: Arc::new(network),
            local: Arc::new(local),
            flights: Arc::new(Mutex::new(HashMap::new())),
            authorized_local_generation: false,
        }
    }

    pub fn with_clients(
        network: Arc<dyn NetworkClient>,
        local: Arc<dyn LocalDocGenerator>,
    ) -> Self {
        Self {
            network,
            local,
            flights: Arc::new(Mutex::new(HashMap::new())),
            authorized_local_generation: false,
        }
    }

    /// Constructs the resolver used by the MCP server, whose local process
    /// launch receives an explicit workspace authority from request setup.
    pub(crate) fn with_authorized_supervisor(
        supervisor: crate::process::ProcessSupervisor,
    ) -> Self {
        let mut resolver = Self::with_supervisor(supervisor);
        resolver.authorized_local_generation = true;
        resolver
    }

    pub fn resolve(&self, input: &DocsInput, options: &DocsOptions) -> DocsResult {
        self.resolve_with_cancellation(input, options, CancellationToken::new())
    }

    pub fn resolve_with_cancellation(
        &self,
        input: &DocsInput,
        options: &DocsOptions,
        cancellation: CancellationToken,
    ) -> DocsResult {
        self.resolve_inner(input, options, cancellation, None)
    }

    /// Internal MCP entrypoint. The selection owns exact package/worktree
    /// capabilities captured before provider work starts.
    pub(crate) fn resolve_selected_with_cancellation(
        &self,
        input: &DocsInput,
        options: &DocsOptions,
        cancellation: CancellationToken,
        selection: &WorkspaceSelection,
    ) -> DocsResult {
        self.resolve_inner(input, options, cancellation, Some(selection))
    }

    fn resolve_inner(
        &self,
        input: &DocsInput,
        options: &DocsOptions,
        cancellation: CancellationToken,
        selection: Option<&WorkspaceSelection>,
    ) -> DocsResult {
        let deadline = Instant::now() + Duration::from_millis(options.timeout_ms.max(1));
        let prepared = match prepare(input, options, &cancellation, selection) {
            Ok(prepared) => prepared,
            Err(result) => return result,
        };
        if cancellation.is_cancelled() {
            return prepared.error("documentation request was cancelled");
        }
        if Instant::now() >= deadline {
            return prepared.error("documentation deadline elapsed before provider lookup");
        }
        let cache = DocsCache::new(&prepared.cache_root);
        let identity = prepared
            .fingerprint
            .as_ref()
            .map(|fingerprint| CacheIdentity {
                crate_name: prepared.dependency.name.clone(),
                version: prepared.dependency.version.clone(),
                source: prepared
                    .dependency
                    .raw_source
                    .clone()
                    .unwrap_or_else(|| "path".to_owned()),
                fingerprint: fingerprint.clone(),
            });
        let candidates = page_candidates(input.symbol.as_deref());
        let mut complete_cache = false;
        let mut attempts = Vec::new();
        let mut hard_error = false;
        let mut cleanup_complete = true;
        for step in fallback_steps(
            options.fallback,
            input.expensive_fallback || options.expensive_fallback,
        ) {
            if cancellation.is_cancelled() {
                attempts.push("request cancelled".to_owned());
                hard_error = true;
                break;
            }
            if Instant::now() >= deadline {
                attempts.push("deadline elapsed".to_owned());
                hard_error = true;
                break;
            }
            match step {
                Step::Cache => {
                    let Some(identity) = identity.as_ref() else {
                        attempts.push(
                            "cache disabled because the source fingerprint is incomplete"
                                .to_owned(),
                        );
                        continue;
                    };
                    if let Some(page) =
                        cache.read_page(identity, &prepared.dependency.name, &candidates)
                    {
                        return prepared.found(DocsProvider::Cache, page.path, page.text);
                    }
                    complete_cache = cache.is_complete(identity);
                    attempts.push(if complete_cache {
                        "complete cache has no requested page".to_owned()
                    } else {
                        "complete cache unavailable".to_owned()
                    });
                }
                Step::Source => {
                    if let Some(page) = read_source_page(
                        &prepared.source_roots,
                        &prepared.dependency.name,
                        input.symbol.as_deref(),
                    ) {
                        return prepared.found(DocsProvider::Source, page.0, page.1);
                    }
                    attempts.push("bounded source documentation unavailable".to_owned());
                }
                Step::Network => {
                    if let Some(page) =
                        self.network_page(&prepared.dependency, &candidates, deadline, options)
                    {
                        return prepared.found(
                            DocsProvider::Network,
                            PathBuf::from(&page.0),
                            page.1,
                        );
                    }
                    attempts.push("docs.rs unavailable".to_owned());
                }
                Step::Local => {
                    let Some(identity) = identity.as_ref() else {
                        attempts.push(
                            "local generation skipped because the source fingerprint is incomplete"
                                .to_owned(),
                        );
                        continue;
                    };
                    if complete_cache {
                        attempts.push("local regeneration skipped for complete cache".to_owned());
                        continue;
                    }
                    let key = cache.entry_path(identity).display().to_string();
                    let generation_cache = cache.clone();
                    let generation_identity = identity.clone();
                    let generation_manifest = prepared.manifest_path.clone();
                    let generation_package = prepared.dependency.name.clone();
                    let generation_local = Arc::clone(&self.local);
                    let generation_authority = self
                        .authorized_local_generation
                        .then(|| Arc::clone(&prepared.package_authority));
                    let generation = self.local_generation_singleflight(
                        &key,
                        &cancellation,
                        deadline,
                        move |generation_cancellation| {
                            let generation = generation_cache
                                .prepare_generation_bounded(
                                    &generation_identity,
                                    Some(deadline),
                                    Some(&generation_cancellation),
                                )
                                .map_err(|error| {
                                    format!("local cache preparation unavailable: {error}")
                                })?;
                            if generation.is_complete() {
                                return Ok(());
                            }
                            let request = LocalDocRequest {
                                manifest_path: generation_manifest,
                                package: generation_package.clone(),
                                target_dir: generation.target_dir(),
                                deadline,
                                cancellation: generation_cancellation.clone(),
                            };
                            let pages = match generation_authority {
                                Some(authority) => {
                                    generation_local.generate_authorized(&request, authority)?
                                }
                                None => generation_local.generate(&request)?,
                            };
                            generation.validate().map_err(|error| {
                                format!("local cache directory changed during generation: {error}")
                            })?;
                            if generation_cancellation.is_cancelled() {
                                return Err("local documentation request was cancelled".to_owned());
                            }
                            generation_cache
                                .publish_pages(
                                    &generation_identity,
                                    &generation_package,
                                    &pages,
                                    Some(deadline),
                                )
                                .map_err(|error| {
                                    format!("local cache publication unavailable: {error}")
                                })?;
                            Ok(())
                        },
                    );
                    match generation {
                        Ok(()) => {
                            if let Some(page) =
                                cache.read_page(identity, &prepared.dependency.name, &candidates)
                            {
                                return prepared.found(DocsProvider::Local, page.path, page.text);
                            }
                            attempts
                                .push("local pages did not contain the requested item".to_owned());
                        }
                        Err(error) if error.starts_with(LOCAL_DOC_CLEANUP_INCOMPLETE) => {
                            cleanup_complete = false;
                            hard_error = true;
                            attempts.push(format!("local cargo doc unavailable: {error}"));
                            break;
                        }
                        Err(error) if error.starts_with("local cache ") => {
                            hard_error = true;
                            attempts.push(error);
                            break;
                        }
                        Err(error) => {
                            attempts.push(format!("local cargo doc unavailable: {error}"));
                            if cancellation.is_cancelled() {
                                hard_error = true;
                                break;
                            }
                        }
                    }
                }
            }
        }
        let warning = format!(
            "No documentation source was available ({}). Results are advisory; compiler output remains authoritative.",
            attempts.join("; ")
        );
        let mut result = if hard_error {
            prepared.error(warning)
        } else {
            prepared.unavailable(warning)
        };
        result.cleanup_complete = cleanup_complete;
        result
    }

    fn network_page(
        &self,
        dependency: &CargoLockCandidate,
        candidates: &[String],
        deadline: Instant,
        options: &DocsOptions,
    ) -> Option<(String, String)> {
        if dependency.source != CargoLockSource::Registry
            || !is_crates_io_registry(
                dependency
                    .registry
                    .as_deref()
                    .or(dependency.source_url.as_deref()),
            )
        {
            return None;
        }
        for page in candidates {
            if Instant::now() >= deadline {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let url = docs_rs_url(dependency, page).ok()?;
            let request = NetworkRequest {
                url,
                headers: vec![
                    ("accept".to_owned(), "text/html".to_owned()),
                    ("user-agent".to_owned(), DOCS_USER_AGENT.to_owned()),
                ],
                timeout: HTTP_TIMEOUT.min(remaining),
                max_body_bytes: options.max_html_bytes.min(DOCS_MAX_HTML_BYTES),
            };
            let response = self.network.fetch(&request).ok()?;
            if !is_valid_docs_rs_url(&response.effective_url) {
                return None;
            }
            if response.body.len() > request.max_body_bytes {
                return None;
            }
            if response.status == 404 {
                continue;
            }
            if !(200..300).contains(&response.status) {
                return None;
            }
            let html = std::str::from_utf8(&response.body).ok()?;
            let text = strip_rustdoc_html(html);
            if !text.is_empty() {
                return Some((page.clone(), text));
            }
        }
        None
    }

    fn local_generation_singleflight<F>(
        &self,
        key: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
        generate: F,
    ) -> Result<(), String>
    where
        F: FnOnce(CancellationToken) -> Result<(), String> + Send + 'static,
    {
        let (flight, owner) = {
            let mut flights = self
                .flights
                .lock()
                .map_err(|_| "local flight state poisoned".to_owned())?;
            if let Some(flight) = flights.get(key) {
                (flight.clone(), false)
            } else {
                let flight = Arc::new(LocalFlight::new());
                flights.insert(key.to_owned(), flight.clone());
                (flight, true)
            }
        };
        let mut subscription = flight.subscribe()?;
        if owner {
            let worker_flight = Arc::clone(&flight);
            let flights = Arc::clone(&self.flights);
            let key = key.to_owned();
            std::thread::spawn(move || {
                let generation_cancellation = worker_flight.generation_cancellation.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    generate(generation_cancellation)
                }))
                .unwrap_or_else(|_| Err("local cargo doc generator panicked".to_owned()));
                worker_flight.finish(result);
                if let Ok(mut flights) = flights.lock() {
                    flights.remove(&key);
                }
            });
        }
        let result = flight.wait(cancellation, deadline);
        if result.is_err()
            && (cancellation.is_cancelled() || Instant::now() >= deadline)
            && subscription.release()
        {
            flight.wait_for_completion(Duration::from_secs(5));
        }
        result
    }
}

struct LocalFlightState {
    result: Option<Result<(), String>>,
    subscribers: usize,
}

struct LocalFlight {
    state: Mutex<LocalFlightState>,
    ready: Condvar,
    generation_cancellation: CancellationToken,
}

impl std::fmt::Debug for LocalFlight {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalFlight")
            .finish_non_exhaustive()
    }
}

impl LocalFlight {
    fn new() -> Self {
        Self {
            state: Mutex::new(LocalFlightState {
                result: None,
                subscribers: 0,
            }),
            ready: Condvar::new(),
            generation_cancellation: CancellationToken::new(),
        }
    }

    fn subscribe(self: &Arc<Self>) -> Result<LocalSubscription, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "local flight state poisoned".to_owned())?;
        state.subscribers = state.subscribers.saturating_add(1);
        Ok(LocalSubscription {
            flight: Arc::clone(self),
            active: true,
        })
    }

    fn finish(&self, result: Result<(), String>) {
        if let Ok(mut state) = self.state.lock() {
            state.result = Some(result);
            self.ready.notify_all();
        }
    }

    fn wait(&self, cancellation: &CancellationToken, deadline: Instant) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "local flight state poisoned".to_owned())?;
        while state.result.is_none() {
            if cancellation.is_cancelled() {
                return Err("local documentation request was cancelled".to_owned());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("local documentation deadline elapsed".to_owned());
            }
            let wait = remaining.min(Duration::from_millis(25));
            let (guard, _) = self
                .ready
                .wait_timeout(state, wait)
                .map_err(|_| "local flight state poisoned".to_owned())?;
            state = guard;
        }
        state
            .result
            .clone()
            .ok_or_else(|| "local flight completed without a result".to_owned())?
    }

    fn wait_for_completion(&self, timeout: Duration) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while state.result.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let Ok((guard, _)) = self.ready.wait_timeout(state, remaining) else {
                return;
            };
            state = guard;
        }
    }
}

struct LocalSubscription {
    flight: Arc<LocalFlight>,
    active: bool,
}

impl LocalSubscription {
    fn release(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        let Ok(mut state) = self.flight.state.lock() else {
            self.flight.generation_cancellation.cancel();
            return true;
        };
        state.subscribers = state.subscribers.saturating_sub(1);
        let last_active = state.subscribers == 0 && state.result.is_none();
        if last_active {
            self.flight.generation_cancellation.cancel();
        }
        last_active
    }
}

impl Drop for LocalSubscription {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Cache,
    Source,
    Network,
    Local,
}

fn fallback_steps(fallback: DocsFallback, expensive: bool) -> Vec<Step> {
    match fallback {
        DocsFallback::Off => vec![Step::Cache],
        DocsFallback::Local => vec![Step::Cache, Step::Local, Step::Source],
        DocsFallback::Network => vec![Step::Cache, Step::Source, Step::Network],
        DocsFallback::Auto if expensive => {
            vec![Step::Cache, Step::Source, Step::Network, Step::Local]
        }
        DocsFallback::Auto => vec![Step::Cache, Step::Source, Step::Network],
    }
}

struct PreparedRequest {
    workspace_root: PathBuf,
    manifest_path: PathBuf,
    dependency: CargoLockCandidate,
    fingerprint: Option<String>,
    cache_root: PathBuf,
    source_roots: Vec<SourceRoot>,
    package_authority: Arc<AuthorizedRoot>,
}

#[derive(Debug, Clone)]
struct SourceRoot {
    authority: Arc<AuthorizedRoot>,
}

struct SourceRootSet {
    source: Vec<SourceRoot>,
    fingerprint: Vec<SourceRoot>,
}

struct WorkspaceOwner {
    authority: Arc<AuthorizedRoot>,
    root: PathBuf,
    inherited: HashMap<String, PathDependency>,
}

impl PreparedRequest {
    fn context(&self) -> (Option<String>, Option<String>) {
        (
            Some(self.workspace_root.display().to_string()),
            Some(self.manifest_path.display().to_string()),
        )
    }

    fn found(&self, provider: DocsProvider, page: PathBuf, text: String) -> DocsResult {
        let (workspace_root, manifest_path) = self.context();
        DocsResult {
            status: DocsStatus::Found,
            is_error: false,
            cleanup_complete: true,
            crate_name: self.dependency.name.clone(),
            version: Some(self.dependency.version.clone()),
            provider: Some(provider),
            text: Some(text.chars().take(DOCS_MAX_OUTPUT).collect()),
            page: Some(page.display().to_string()),
            warning: None,
            candidates: Vec::new(),
            dependency: Some(self.dependency.clone()),
            workspace_root,
            manifest_path,
        }
    }

    fn unavailable(&self, warning: impl Into<String>) -> DocsResult {
        let (workspace_root, manifest_path) = self.context();
        DocsResult {
            status: DocsStatus::Unavailable,
            is_error: false,
            cleanup_complete: true,
            crate_name: self.dependency.name.clone(),
            version: Some(self.dependency.version.clone()),
            provider: None,
            text: None,
            page: None,
            warning: Some(warning.into()),
            candidates: Vec::new(),
            dependency: Some(self.dependency.clone()),
            workspace_root,
            manifest_path,
        }
    }

    fn error(&self, warning: impl Into<String>) -> DocsResult {
        let mut result = self.unavailable(warning);
        result.is_error = true;
        result
    }
}

pub fn resolve_docs_default(input: &DocsInput, options: &DocsOptions) -> DocsResult {
    DocsResolver::default().resolve(input, options)
}

pub fn resolve_docs(
    input: &DocsInput,
    options: &DocsOptions,
    network: Arc<dyn NetworkClient>,
    local: Arc<dyn LocalDocGenerator>,
) -> DocsResult {
    DocsResolver::with_clients(network, local).resolve(input, options)
}

fn prepare(
    input: &DocsInput,
    options: &DocsOptions,
    cancellation: &CancellationToken,
    selection: Option<&WorkspaceSelection>,
) -> Result<PreparedRequest, DocsResult> {
    let requested = Path::new(input.dir.trim());
    if requested.as_os_str().is_empty() || !requested.is_absolute() {
        return Err(unavailable_input(
            input,
            "rust.docs requires an absolute dir",
        ));
    }
    let Some(requested) = normalize_absolute_path(requested) else {
        return Err(unavailable_input(
            input,
            "documentation dir could not be normalized safely",
        ));
    };
    let authority = match selection {
        Some(selection) => selection.worktree_authority().clone(),
        None => match options.workspace_authority.clone() {
            Some(authority) => authority,
            None => match RootGuard::new([requested.clone()], Vec::<PathBuf>::new()) {
                Ok(guard) => guard.configured_roots()[0].clone(),
                Err(_) => {
                    return Err(unavailable_input(
                        input,
                        "documentation dir is not an accessible authorized directory",
                    ));
                }
            },
        },
    };
    let requested = match selection {
        Some(selection) if requested == selection.requested_dir() => {
            selection.requested_authority().path().to_owned()
        }
        Some(_) => {
            return Err(unavailable_input(
                input,
                "documentation dir no longer matches workspace selection",
            ));
        }
        None => match authority.authorize_dir(&requested) {
            Ok(requested) => requested.path().to_owned(),
            Err(_) => {
                return Err(unavailable_input(
                    input,
                    "documentation dir is outside the authorized workspace root",
                ));
            }
        },
    };
    if cancellation.is_cancelled() {
        return Err(unavailable_input(
            input,
            "documentation request was cancelled",
        ));
    }
    let package_root = match selection {
        Some(selection) => selection.package_authority().path().to_owned(),
        None => match find_package_root(&authority, &requested) {
            Some(root) => root,
            None => {
                return Err(unavailable_input(
                    input,
                    "Cargo.toml was not found within the selected worktree",
                ));
            }
        },
    };
    let package_authority = match selection {
        Some(selection) => selection.package_authority().clone(),
        None => match authority.authorize_dir(&package_root) {
            Ok(authority) => authority,
            Err(_) => {
                return Err(unavailable_input(
                    input,
                    "package root authorization changed before local generation",
                ));
            }
        },
    };
    let workspace_root =
        find_workspace_root(&authority, &package_root).unwrap_or_else(|| package_root.clone());
    let manifest_path = package_root.join("Cargo.toml");
    let lock_path = find_lock_path(&authority, &package_root, &workspace_root);
    let Some(lock_path) = lock_path else {
        return Err(unavailable_context(
            input,
            workspace_root,
            manifest_path,
            "Cargo.lock was not found; exact-version documentation lookup was skipped.",
            false,
        ));
    };
    let lock = match read_authorized_text(&authority, &lock_path, MAX_LOCK_BYTES) {
        Ok(lock) => lock,
        Err(_) => {
            return Err(unavailable_context(
                input,
                workspace_root,
                manifest_path,
                "Cargo.lock could not be read; documentation lookup is advisory and was skipped.",
                false,
            ));
        }
    };
    let dependency = match parse_cargo_lock_dependency(
        &lock,
        &input.crate_name,
        input.version.as_deref(),
        input.source.as_deref(),
    ) {
        Ok(Some(dependency)) => dependency,
        Ok(None) => {
            return Err(unavailable_context(
                input,
                workspace_root,
                manifest_path,
                if input.version.is_some() {
                    "No Cargo.lock package matched the requested crate and exact version."
                } else {
                    "No Cargo.lock package matched the requested crate."
                },
                false,
            ));
        }
        Err(candidates) => {
            return Err(DocsResult {
                status: DocsStatus::Ambiguous,
                is_error: false,
                cleanup_complete: true,
                crate_name: input.crate_name.clone(),
                version: None,
                provider: None,
                text: None,
                page: None,
                warning: Some("Multiple Cargo.lock packages match; pass an exact version and, when needed, a source selector.".to_owned()),
                candidates,
                dependency: None,
                workspace_root: Some(workspace_root.display().to_string()),
                manifest_path: Some(manifest_path.display().to_string()),
            });
        }
    };
    let source_roots = match source_roots(
        &authority,
        &package_root,
        &workspace_root,
        &dependency,
        options,
        cancellation,
    ) {
        Ok(roots) => roots,
        Err(message) => {
            return Err(unavailable_context(
                input,
                workspace_root,
                manifest_path,
                message,
                true,
            ));
        }
    };
    let fingerprint = fingerprint(
        &authority,
        &workspace_root,
        &manifest_path,
        &lock_path,
        &source_roots.fingerprint,
    );
    let cache_root = resolve_cache_root(&workspace_root, options.cache_dir.as_deref());
    Ok(PreparedRequest {
        workspace_root,
        manifest_path,
        dependency,
        fingerprint,
        cache_root,
        source_roots: source_roots.source,
        package_authority,
    })
}

fn unavailable_input(input: &DocsInput, warning: &str) -> DocsResult {
    DocsResult {
        status: DocsStatus::Unavailable,
        is_error: true,
        cleanup_complete: true,
        crate_name: input.crate_name.clone(),
        version: input.version.clone(),
        provider: None,
        text: None,
        page: None,
        warning: Some(warning.to_owned()),
        candidates: Vec::new(),
        dependency: None,
        workspace_root: None,
        manifest_path: None,
    }
}

fn unavailable_context(
    input: &DocsInput,
    workspace_root: PathBuf,
    manifest_path: PathBuf,
    warning: &str,
    is_error: bool,
) -> DocsResult {
    DocsResult {
        status: DocsStatus::Unavailable,
        is_error,
        cleanup_complete: true,
        crate_name: input.crate_name.clone(),
        version: input.version.clone(),
        provider: None,
        text: None,
        page: None,
        warning: Some(warning.to_owned()),
        candidates: Vec::new(),
        dependency: None,
        workspace_root: Some(workspace_root.display().to_string()),
        manifest_path: Some(manifest_path.display().to_string()),
    }
}

fn find_package_root(authority: &AuthorizedRoot, start: &Path) -> Option<PathBuf> {
    let mut current = start.to_owned();
    loop {
        if has_regular_entry(authority, &current, "Cargo.toml") {
            return Some(current);
        }
        if has_entry(authority, &current, ".git") || current == authority.path() {
            return None;
        }
        let parent = current.parent()?;
        if parent == current || !authority.contains(parent) {
            return None;
        }
        current = parent.to_owned();
    }
}

fn find_workspace_root(authority: &AuthorizedRoot, package_root: &Path) -> Option<PathBuf> {
    let mut current = package_root.to_owned();
    loop {
        let manifest = current.join("Cargo.toml");
        if let Ok(text) = read_authorized_text(authority, &manifest, MAX_LOCK_BYTES) {
            if text
                .parse::<toml::Table>()
                .is_ok_and(|table| table.contains_key("workspace"))
            {
                return Some(current);
            }
        }
        if has_entry(authority, &current, ".git") || current == authority.path() {
            break;
        }
        let parent = current.parent()?;
        if parent == current || !authority.contains(parent) {
            break;
        }
        current = parent.to_owned();
    }
    None
}

fn find_lock_path(
    authority: &AuthorizedRoot,
    package_root: &Path,
    workspace_root: &Path,
) -> Option<PathBuf> {
    let workspace = workspace_root.join("Cargo.lock");
    if has_regular_entry(authority, workspace_root, "Cargo.lock") {
        return Some(workspace);
    }
    let package = package_root.join("Cargo.lock");
    has_regular_entry(authority, package_root, "Cargo.lock").then_some(package)
}

fn has_entry(authority: &AuthorizedRoot, directory: &Path, name: &str) -> bool {
    authority.list_directory(directory).is_ok_and(|entries| {
        entries
            .iter()
            .any(|entry| entry.name == std::ffi::OsStr::new(name))
    })
}

fn has_regular_entry(authority: &AuthorizedRoot, directory: &Path, name: &str) -> bool {
    authority.list_directory(directory).is_ok_and(|entries| {
        entries.iter().any(|entry| {
            entry.name == std::ffi::OsStr::new(name)
                && entry.kind == crate::workspace::DirectoryEntryKind::RegularFile
        })
    })
}

fn read_authorized_text(
    authority: &AuthorizedRoot,
    path: &Path,
    max_bytes: u64,
) -> Result<String, ResolverError> {
    let bytes = authority
        .read_file(path, max_bytes)
        .map_err(|error| ResolverError::Io(error.to_string()))?;
    String::from_utf8(bytes)
        .map_err(|_| ResolverError::Io(format!("file is not UTF-8: {}", path.display())))
}

pub fn parse_cargo_lock_dependency(
    lock: &str,
    requested_name: &str,
    requested_version: Option<&str>,
    requested_source: Option<&str>,
) -> Result<Option<CargoLockCandidate>, Vec<String>> {
    let value = match lock.parse::<cargo_lock::Lockfile>() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let packages = value
        .packages
        .iter()
        .map(lock_candidate)
        .collect::<Vec<_>>();
    let exact = packages
        .iter()
        .filter(|candidate| candidate.name == requested_name)
        .cloned()
        .collect::<Vec<_>>();
    let normalized = normalize_name(requested_name);
    let named = if exact.is_empty() {
        packages
            .iter()
            .filter(|candidate| normalize_name(&candidate.name) == normalized)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        exact
    };
    let versioned = requested_version.map_or(named.clone(), |version| {
        named
            .into_iter()
            .filter(|candidate| candidate.version == version.trim())
            .collect()
    });
    let filtered = if let Some(source) = requested_source {
        versioned
            .into_iter()
            .filter(|candidate| candidate_matches_source(candidate, source.trim()))
            .collect()
    } else {
        versioned
    };
    match filtered.as_slice() {
        [] => Ok(None),
        [candidate] => Ok(Some(candidate.clone())),
        candidates => Err(candidates
            .iter()
            .map(|candidate| {
                format!(
                    "{}@{} ({})",
                    candidate.name,
                    candidate.version,
                    candidate.source.as_str()
                )
            })
            .take(100)
            .collect()),
    }
}

fn lock_candidate(package: &cargo_lock::Package) -> CargoLockCandidate {
    let name = package.name.as_str().to_owned();
    let version = package.version.to_string();
    let raw_source = package.source.as_ref().map(ToString::to_string);
    let (source, source_url) = parse_lock_source(raw_source.as_deref());
    CargoLockCandidate {
        name,
        version,
        registry: (source == CargoLockSource::Registry)
            .then(|| source_url.clone())
            .flatten(),
        git: (source == CargoLockSource::Git)
            .then(|| source_url.clone())
            .flatten(),
        path: (source == CargoLockSource::Path)
            .then(|| source_url.clone())
            .flatten(),
        source,
        source_url,
        raw_source,
    }
}

fn parse_lock_source(source: Option<&str>) -> (CargoLockSource, Option<String>) {
    let Some(source) = source else {
        return (CargoLockSource::Path, None);
    };
    for (prefix, kind) in [
        ("registry+", CargoLockSource::Registry),
        ("sparse+", CargoLockSource::Registry),
        ("git+", CargoLockSource::Git),
        ("path+", CargoLockSource::Path),
    ] {
        if let Some(value) = source.strip_prefix(prefix) {
            return (kind, Some(value.to_owned()));
        }
    }
    (CargoLockSource::Unknown, Some(source.to_owned()))
}

fn candidate_matches_source(candidate: &CargoLockCandidate, source: &str) -> bool {
    [
        Some(candidate.source.as_str()),
        candidate.raw_source.as_deref(),
        candidate.source_url.as_deref(),
        candidate.registry.as_deref(),
        candidate.git.as_deref(),
        candidate.path.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value == source)
}

fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

#[derive(Debug, Clone)]
struct PathDependency {
    package_name: String,
    expected_version: Option<PathVersion>,
    root: PathBuf,
}

#[derive(Debug, Clone)]
enum PathVersion {
    Requirement(String),
    Exact(String),
}

fn source_roots(
    workspace_authority: &Arc<AuthorizedRoot>,
    package_root: &Path,
    workspace_root: &Path,
    dependency: &CargoLockCandidate,
    options: &DocsOptions,
    cancellation: &CancellationToken,
) -> Result<SourceRootSet, &'static str> {
    if cancellation.is_cancelled() {
        return Err("documentation request was cancelled");
    }
    match dependency.source {
        CargoLockSource::Registry | CargoLockSource::Unknown => Ok(SourceRootSet {
            source: Vec::new(),
            fingerprint: Vec::new(),
        }),
        CargoLockSource::Git => {
            let Some(cargo_home) = options.cargo_home_authority.as_ref() else {
                return Ok(SourceRootSet {
                    source: Vec::new(),
                    fingerprint: Vec::new(),
                });
            };
            let roots = find_git_sources(cargo_home, dependency, cancellation)?;
            Ok(SourceRootSet {
                source: roots.clone(),
                fingerprint: roots,
            })
        }
        CargoLockSource::Path => {
            let workspace = workspace_authority
                .authorize_dir(workspace_root)
                .map_err(|_| "workspace source root is outside the authorized workspace")?;
            let mut allowed = vec![workspace];
            allowed.extend(options.dependency_authorities.iter().cloned());
            let mut pending = VecDeque::from([package_root.to_owned()]);
            if workspace_root != package_root {
                pending.push_back(workspace_root.to_owned());
            }
            let mut visited = HashSet::new();
            let mut source = Vec::new();
            let mut fingerprint = Vec::new();
            while let Some(package) = pending.pop_front() {
                if cancellation.is_cancelled() {
                    return Err("documentation request was cancelled");
                }
                let Some(authority) = authorize_source_root(&allowed, &package) else {
                    return Err("path dependency is outside the configured dependency roots");
                };
                if !visited.insert(authority.path().to_owned()) {
                    continue;
                }
                if visited.len() > MAX_SOURCE_FILES {
                    return Err("path dependency graph exceeded its manifest limit");
                }
                let manifest = authority.path().join("Cargo.toml");
                let text = read_authorized_text(&authority, &manifest, MAX_LOCK_BYTES)
                    .map_err(|_| "path dependency manifest could not be read safely")?;
                let table = text
                    .parse::<toml::Table>()
                    .map_err(|_| "path dependency manifest could not be parsed")?;
                let current = SourceRoot {
                    authority: authority.clone(),
                };
                fingerprint.push(current.clone());
                if package_identity_from_roots(&allowed, authority.path()).is_some_and(
                    |(_, name, version)| {
                        normalize_name(&name) == normalize_name(&dependency.name)
                            && version == dependency.version
                    },
                ) {
                    source.push(current);
                }
                let base = authority.path();
                let owner = workspace_owner(&allowed, base);
                let empty_inherited = HashMap::new();
                let (inherited, include_overrides) = if let Some(owner) = owner.as_ref() {
                    if base == owner.root {
                        for member in workspace_member_roots(&table, &owner.authority, &owner.root)?
                        {
                            pending.push_back(member);
                        }
                    } else {
                        pending.push_back(owner.root.clone());
                    }
                    (&owner.inherited, base == owner.root)
                } else {
                    (&empty_inherited, false)
                };
                for edge in collect_path_dependencies(&table, base, inherited, include_overrides) {
                    let Some(target) = authorize_source_root(&allowed, &edge.root) else {
                        return Err("path dependency is outside the configured dependency roots");
                    };
                    let Some((target, actual_name, actual_version)) =
                        package_identity_from_roots(&allowed, target.path())
                    else {
                        return Err("path dependency manifest has no package identity");
                    };
                    if normalize_name(&actual_name) != normalize_name(&edge.package_name)
                        || edge.expected_version.as_ref().is_some_and(|expected| {
                            !path_version_matches(expected, &actual_version)
                        })
                    {
                        return Err("path dependency manifest identity does not match its edge");
                    }
                    pending.push_back(target.path().to_owned());
                }
            }
            dedup_source_roots(&mut source);
            dedup_source_roots(&mut fingerprint);
            Ok(SourceRootSet {
                source,
                fingerprint,
            })
        }
    }
}

fn workspace_member_roots(
    table: &toml::Table,
    authority: &AuthorizedRoot,
    base: &Path,
) -> Result<Vec<PathBuf>, &'static str> {
    let Some(workspace) = table.get("workspace").and_then(toml::Value::as_table) else {
        return Ok(Vec::new());
    };
    let members = workspace
        .get("members")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .collect::<Vec<_>>();
    let excludes = workspace
        .get("exclude")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .collect::<Vec<_>>();
    let mut excluded = HashSet::new();
    for pattern in excludes {
        excluded.extend(expand_workspace_pattern(authority, base, pattern)?);
        if excluded.len() > MAX_SOURCE_FILES {
            return Err("workspace exclude expansion exceeded its manifest limit");
        }
    }
    let mut roots = Vec::new();
    for pattern in members {
        for candidate in expand_workspace_pattern(authority, base, pattern)? {
            if excluded.contains(&candidate) || roots.contains(&candidate) {
                continue;
            }
            if has_regular_entry(authority, &candidate, "Cargo.toml") {
                roots.push(candidate);
            }
            if roots.len() > MAX_SOURCE_FILES {
                return Err("workspace member expansion exceeded its manifest limit");
            }
        }
    }
    Ok(roots)
}

fn expand_workspace_pattern(
    authority: &AuthorizedRoot,
    base: &Path,
    pattern: &str,
) -> Result<Vec<PathBuf>, &'static str> {
    let path = Path::new(pattern);
    if path.is_absolute() || pattern.trim().is_empty() {
        return Err("workspace member path is invalid");
    }
    let mut candidates = vec![base.to_owned()];
    for component in path.components() {
        let std::path::Component::Normal(segment) = component else {
            return Err("workspace member path escaped its authorized root");
        };
        let segment = segment
            .to_str()
            .ok_or("workspace member path is not valid UTF-8")?;
        if segment.contains(['?', '[', ']']) || (segment.contains('*') && segment != "*") {
            return Err("workspace member glob is unsupported");
        }
        let mut next = Vec::new();
        for candidate in candidates {
            if segment == "*" {
                let entries = authority
                    .list_directory(&candidate)
                    .map_err(|_| "workspace member directory could not be listed safely")?;
                for entry in entries {
                    if entry.kind == crate::workspace::DirectoryEntryKind::Directory {
                        next.push(candidate.join(entry.name));
                    }
                    if next.len() > MAX_SOURCE_FILES {
                        return Err("workspace member expansion exceeded its manifest limit");
                    }
                }
            } else {
                next.push(candidate.join(segment));
            }
        }
        candidates = next;
    }
    candidates
        .into_iter()
        .map(|candidate| {
            authority
                .authorize_dir(&candidate)
                .map(|root| root.path().to_owned())
                .map_err(|_| "workspace member is outside the authorized root")
        })
        .collect()
}

fn authorize_source_root(
    authorities: &[Arc<AuthorizedRoot>],
    path: &Path,
) -> Option<Arc<AuthorizedRoot>> {
    let path = normalize_absolute_path(path)?;
    authorities
        .iter()
        .find_map(|authority| authority.authorize_dir(&path).ok())
}

fn workspace_owner(
    authorities: &[Arc<AuthorizedRoot>],
    package_root: &Path,
) -> Option<WorkspaceOwner> {
    authorities
        .iter()
        .filter_map(|authority| {
            let workspace_root = find_workspace_root(authority, package_root)?;
            let authority = authority.authorize_dir(&workspace_root).ok()?;
            let root = authority.path().to_owned();
            let manifest = root.join("Cargo.toml");
            let text = read_authorized_text(&authority, &manifest, MAX_LOCK_BYTES).ok()?;
            let table = text.parse::<toml::Table>().ok()?;
            Some(WorkspaceOwner {
                inherited: collect_workspace_path_dependencies(&table, &root),
                authority,
                root,
            })
        })
        .max_by(|left, right| {
            left.root
                .components()
                .count()
                .cmp(&right.root.components().count())
                .then_with(|| left.root.cmp(&right.root))
        })
}

fn dedup_source_roots(roots: &mut Vec<SourceRoot>) {
    roots.sort_by(|left, right| left.authority.path().cmp(right.authority.path()));
    roots.dedup_by(|left, right| left.authority.path() == right.authority.path());
}

fn collect_path_dependencies(
    table: &toml::Table,
    base: &Path,
    inherited: &HashMap<String, PathDependency>,
    include_overrides: bool,
) -> Vec<PathDependency> {
    let mut dependencies = Vec::new();
    for key in ["dependencies", "build-dependencies"] {
        collect_dependency_table(table.get(key), base, inherited, &mut dependencies);
    }
    collect_dependency_table(
        table.get("dev-dependencies"),
        base,
        inherited,
        &mut dependencies,
    );
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            for key in ["dependencies", "build-dependencies"] {
                collect_dependency_table(target.get(key), base, inherited, &mut dependencies);
            }
            collect_dependency_table(
                target.get("dev-dependencies"),
                base,
                inherited,
                &mut dependencies,
            );
        }
    }
    if include_overrides {
        if let Some(patches) = table.get("patch").and_then(toml::Value::as_table) {
            for patch in patches.values() {
                collect_dependency_table(Some(patch), base, inherited, &mut dependencies);
            }
        }
        collect_replace_table(table.get("replace"), base, &mut dependencies);
    }
    dependencies
}

fn collect_workspace_path_dependencies(
    table: &toml::Table,
    base: &Path,
) -> HashMap<String, PathDependency> {
    let Some(entries) = table
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
    else {
        return HashMap::new();
    };
    entries
        .iter()
        .filter_map(|(alias, specification)| {
            let specification = specification.as_table()?;
            let path = specification.get("path")?.as_str()?;
            let package_name = specification
                .get("package")
                .and_then(toml::Value::as_str)
                .unwrap_or(alias)
                .to_owned();
            Some((
                alias.clone(),
                PathDependency {
                    package_name,
                    expected_version: specification
                        .get("version")
                        .and_then(toml::Value::as_str)
                        .map(|version| PathVersion::Requirement(version.to_owned())),
                    root: base.join(path),
                },
            ))
        })
        .collect()
}

fn collect_dependency_table(
    value: Option<&toml::Value>,
    base: &Path,
    inherited: &HashMap<String, PathDependency>,
    dependencies: &mut Vec<PathDependency>,
) {
    let Some(entries) = value.and_then(toml::Value::as_table) else {
        return;
    };
    for (name, specification) in entries {
        let Some(specification) = specification.as_table() else {
            continue;
        };
        if specification
            .get("workspace")
            .and_then(toml::Value::as_bool)
            == Some(true)
        {
            if let Some(dependency) = inherited.get(name) {
                dependencies.push(dependency.clone());
            }
            continue;
        }
        let Some(path) = specification.get("path").and_then(toml::Value::as_str) else {
            continue;
        };
        let package_name = specification
            .get("package")
            .and_then(toml::Value::as_str)
            .unwrap_or(name)
            .to_owned();
        dependencies.push(PathDependency {
            package_name,
            expected_version: specification
                .get("version")
                .and_then(toml::Value::as_str)
                .map(|version| PathVersion::Requirement(version.to_owned())),
            root: base.join(path),
        });
    }
}

fn collect_replace_table(
    value: Option<&toml::Value>,
    base: &Path,
    dependencies: &mut Vec<PathDependency>,
) {
    let Some(entries) = value.and_then(toml::Value::as_table) else {
        return;
    };
    for (package_id, specification) in entries {
        let Some(specification) = specification.as_table() else {
            continue;
        };
        let Some(path) = specification.get("path").and_then(toml::Value::as_str) else {
            continue;
        };
        let Some((package_name, version)) = package_id.rsplit_once(':') else {
            continue;
        };
        dependencies.push(PathDependency {
            package_name: package_name.to_owned(),
            expected_version: Some(PathVersion::Exact(version.to_owned())),
            root: base.join(path),
        });
    }
}

fn path_version_matches(expected: &PathVersion, actual: &str) -> bool {
    match expected {
        PathVersion::Exact(expected) => expected == actual,
        PathVersion::Requirement(expected) => {
            let Ok(requirement) = cargo_metadata::semver::VersionReq::parse(expected) else {
                return false;
            };
            let Ok(actual) = cargo_metadata::semver::Version::parse(actual) else {
                return false;
            };
            requirement.matches(&actual)
        }
    }
}

fn package_name(table: &toml::Table) -> Option<String> {
    table
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

fn package_identity(
    table: &toml::Table,
    inherited_version: Option<&str>,
) -> Option<(String, String)> {
    let package = table.get("package")?.as_table()?;
    let version = package.get("version")?;
    let version = if let Some(version) = version.as_str() {
        version
    } else {
        if !version.as_table()?.get("workspace")?.as_bool()? {
            return None;
        }
        inherited_version?
    };
    Some((
        package.get("name")?.as_str()?.to_owned(),
        version.to_owned(),
    ))
}

fn package_identity_from_roots(
    allowed: &[Arc<AuthorizedRoot>],
    package_root: &Path,
) -> Option<(Arc<AuthorizedRoot>, String, String)> {
    for authority in allowed {
        let Ok(root) = authority.authorize_dir(package_root) else {
            continue;
        };
        let Ok(text) = read_authorized_text(&root, &root.path().join("Cargo.toml"), MAX_LOCK_BYTES)
        else {
            continue;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            continue;
        };
        let inherited_version = workspace_package_version(authority, package_root);
        if let Some((name, version)) = package_identity(&table, inherited_version.as_deref()) {
            return Some((root, name, version));
        }
    }
    None
}

fn workspace_package_version(authority: &AuthorizedRoot, package_root: &Path) -> Option<String> {
    let workspace = find_workspace_root(authority, package_root)?;
    let text =
        read_authorized_text(authority, &workspace.join("Cargo.toml"), MAX_LOCK_BYTES).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    table
        .get("workspace")?
        .as_table()?
        .get("package")?
        .as_table()?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

fn find_git_sources(
    cargo_home: &Arc<AuthorizedRoot>,
    dependency: &CargoLockCandidate,
    cancellation: &CancellationToken,
) -> Result<Vec<SourceRoot>, &'static str> {
    let checkout_path = cargo_home.path().join("git").join("checkouts");
    let Ok(checkouts) = cargo_home.authorize_dir(&checkout_path) else {
        return Ok(Vec::new());
    };
    let mut roots = bounded_manifests(&checkouts, MAX_GIT_CHECKOUT_FILES)?
        .into_iter()
        .take_while(|_| !cancellation.is_cancelled())
        .filter_map(|manifest| {
            let text = read_authorized_text(&checkouts, &manifest, MAX_LOCK_BYTES).ok()?;
            let table = text.parse::<toml::Table>().ok()?;
            let matches = package_name(&table).as_deref() == Some(&dependency.name)
                && table
                    .get("package")
                    .and_then(toml::Value::as_table)
                    .and_then(|package| package.get("version"))
                    .and_then(toml::Value::as_str)
                    == Some(&dependency.version);
            let package_root = matches
                .then(|| manifest.parent().map(Path::to_owned))
                .flatten()?;
            verify_git_checkout(cargo_home, &checkouts, &package_root, dependency).then(|| {
                checkouts
                    .authorize_dir(&package_root)
                    .ok()
                    .map(|authority| SourceRoot { authority })
            })?
        })
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| left.authority.path().cmp(right.authority.path()));
    roots.dedup_by(|left, right| left.authority.path() == right.authority.path());
    Ok(roots)
}

fn verify_git_checkout(
    cargo_home: &AuthorizedRoot,
    checkouts: &AuthorizedRoot,
    package_root: &Path,
    dependency: &CargoLockCandidate,
) -> bool {
    let Some((expected_url, expected_revision)) = expected_git_identity(dependency) else {
        return false;
    };
    let mut checkout_root = package_root.to_owned();
    while checkout_root != checkouts.path() && !has_regular_entry(checkouts, &checkout_root, ".git")
    {
        let Some(parent) = checkout_root.parent() else {
            return false;
        };
        if !checkouts.contains(parent) {
            return false;
        }
        checkout_root = parent.to_owned();
    }
    if checkout_root == checkouts.path() {
        return false;
    }
    let Ok(git_file) = read_authorized_text(checkouts, &checkout_root.join(".git"), 64 * 1024)
    else {
        return false;
    };
    let Some(git_dir) = git_file.trim().strip_prefix("gitdir:").map(str::trim) else {
        return false;
    };
    let git_dir = Path::new(git_dir);
    let git_dir = if git_dir.is_absolute() {
        normalize_absolute_path(git_dir)
    } else {
        normalize_absolute_path(&checkout_root.join(git_dir))
    };
    let Some(git_dir) = git_dir else {
        return false;
    };
    let Ok(worktree) = cargo_home.authorize_dir(&git_dir) else {
        return false;
    };
    let Ok(head) = read_authorized_text(&worktree, &git_dir.join("HEAD"), 256) else {
        return false;
    };
    if !head.trim().eq_ignore_ascii_case(expected_revision) {
        return false;
    }
    let Some(common_dir) = git_dir.parent().and_then(Path::parent) else {
        return false;
    };
    let Ok(common) = cargo_home.authorize_dir(common_dir) else {
        return false;
    };
    let Ok(config) = read_authorized_text(&common, &common_dir.join("config"), 1024 * 1024) else {
        return false;
    };
    git_origin_url(&config)
        .is_some_and(|actual| normalize_git_url(actual) == normalize_git_url(expected_url))
}

fn expected_git_identity(dependency: &CargoLockCandidate) -> Option<(&str, &str)> {
    let source = dependency.git.as_deref()?;
    let (url_and_query, revision) = source.rsplit_once('#')?;
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let url = url_and_query
        .split_once('?')
        .map_or(url_and_query, |(url, _)| url);
    Some((url, revision))
}

fn git_origin_url(config: &str) -> Option<&str> {
    let mut origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            origin = line == "[remote \"origin\"]";
            continue;
        }
        if origin
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "url"
        {
            return Some(value.trim());
        }
    }
    None
}

fn normalize_git_url(url: &str) -> &str {
    url.trim().trim_end_matches('/').trim_end_matches(".git")
}

fn normalize_absolute_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR));
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            std::path::Component::Normal(name) => normalized.push(name),
        }
    }
    Some(normalized)
}

fn bounded_manifests(
    authority: &Arc<AuthorizedRoot>,
    limit: usize,
) -> Result<Vec<PathBuf>, &'static str> {
    let walked = authority
        .walk_files_matching(
            WalkLimits {
                max_files: limit,
                max_file_bytes: MAX_LOCK_BYTES,
                max_total_bytes: MAX_FINGERPRINT_BYTES,
                max_depth: 64,
                skip_directories: [".git", "target", "node_modules"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
            },
            |path| path.file_name().is_some_and(|name| name == "Cargo.toml"),
        )
        .map_err(|_| "source root could not be traversed safely")?;
    if !walked.issues.is_empty() {
        return Err("source root traversal was incomplete");
    }
    Ok(walked
        .files
        .into_iter()
        .map(|file| authority.path().join(file.path))
        .collect())
}

fn read_source_page(
    roots: &[SourceRoot],
    crate_name: &str,
    symbol: Option<&str>,
) -> Option<(PathBuf, String)> {
    let mut files = Vec::new();
    for root in roots {
        files.extend(bounded_rust_sources(root, MAX_SOURCE_FILES));
    }
    files.sort_by_key(|(_, path)| {
        let rank = if path.ends_with("lib.rs") {
            0
        } else if path.ends_with("mod.rs") {
            1
        } else {
            2
        };
        (rank, path.clone())
    });
    for (authority, path) in files {
        let Ok(source) = read_authorized_text(&authority, &path, 2 * 1024 * 1024) else {
            continue;
        };
        if let Some(text) = extract_source_documentation(&source, symbol) {
            return Some((path, text));
        }
    }
    let _ = crate_name;
    None
}

fn bounded_rust_sources(root: &SourceRoot, limit: usize) -> Vec<(Arc<AuthorizedRoot>, PathBuf)> {
    root.authority
        .walk_files_matching(
            WalkLimits {
                max_files: limit,
                max_file_bytes: 2 * 1024 * 1024,
                max_total_bytes: MAX_FINGERPRINT_BYTES,
                max_depth: 64,
                skip_directories: [".git", "target", "vendor", "node_modules"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
            },
            |path| path.extension().is_some_and(|extension| extension == "rs"),
        )
        .ok()
        .filter(|walked| walked.issues.is_empty())
        .into_iter()
        .flat_map(|walked| walked.files)
        .map(|file| {
            (
                root.authority.clone(),
                root.authority.path().join(file.path),
            )
        })
        .collect()
}

fn extract_source_documentation(source: &str, symbol: Option<&str>) -> Option<String> {
    let lines = source.lines().collect::<Vec<_>>();
    let Some(symbol) = symbol.map(str::trim).filter(|symbol| !symbol.is_empty()) else {
        let docs = lines
            .iter()
            .filter(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("//!") || trimmed.starts_with("///")
            })
            .take(80)
            .map(|line| strip_doc_prefix(line))
            .collect::<Vec<_>>();
        return (!docs.is_empty()).then(|| docs.join("\n").trim().to_owned());
    };
    if !is_identifier(symbol) {
        return None;
    }
    let kinds = [
        "struct", "enum", "trait", "union", "fn", "type", "const", "static", "mod",
    ];
    let index = lines.iter().position(|line| {
        kinds
            .iter()
            .any(|kind| contains_item_name(line, kind, symbol))
    })?;
    let mut start = index;
    while start > 0 {
        let trimmed = lines[start - 1].trim_start();
        if trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("#")
            || trimmed.starts_with("/**")
            || trimmed.starts_with("*")
        {
            start -= 1;
        } else {
            break;
        }
    }
    let excerpt = lines[start..lines.len().min(index + 8)]
        .iter()
        .map(|line| strip_doc_prefix(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    (!excerpt.is_empty()).then_some(excerpt)
}

fn contains_item_name(line: &str, kind: &str, symbol: &str) -> bool {
    let Some(index) = line.find(kind) else {
        return false;
    };
    let tail = &line[index + kind.len()..];
    let tail = tail.trim_start();
    tail.strip_prefix(symbol).is_some_and(|tail| {
        tail.chars()
            .next()
            .is_none_or(|character| !(character.is_ascii_alphanumeric() || character == '_'))
    })
}

fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn strip_doc_prefix(line: &str) -> String {
    line.trim_start()
        .strip_prefix("///")
        .or_else(|| line.trim_start().strip_prefix("//!"))
        .or_else(|| line.trim_start().strip_prefix("/**"))
        .or_else(|| line.trim_start().strip_prefix("*"))
        .unwrap_or(line)
        .trim_start()
        .strip_suffix("*/")
        .unwrap_or_else(|| {
            line.trim_start()
                .strip_prefix("///")
                .unwrap_or(line.trim_start())
        })
        .trim()
        .to_owned()
}

fn fingerprint(
    workspace_authority: &Arc<AuthorizedRoot>,
    workspace_root: &Path,
    manifest_path: &Path,
    lock_path: &Path,
    fingerprint_roots: &[SourceRoot],
) -> Option<String> {
    let workspace = workspace_authority.authorize_dir(workspace_root).ok()?;
    let mut files = BTreeMap::new();
    files.insert(manifest_path.to_owned(), workspace_authority.clone());
    files.insert(lock_path.to_owned(), workspace_authority.clone());
    let remaining = MAX_FINGERPRINT_FILES.saturating_sub(files.len());
    for path in bounded_fingerprint_files(&workspace, remaining)? {
        files.insert(path, workspace.clone());
    }
    for root in fingerprint_roots {
        let remaining = MAX_FINGERPRINT_FILES.saturating_sub(files.len());
        if remaining == 0 {
            return None;
        }
        for path in bounded_fingerprint_files(&root.authority, remaining)? {
            files.insert(path, root.authority.clone());
        }
    }
    if files.len() > MAX_FINGERPRINT_FILES {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-coder-docs-v1\0");
    let mut total = 0u64;
    for (path, authority) in files {
        let bytes = authority.read_file(&path, 2 * 1024 * 1024).ok()?;
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_FINGERPRINT_BYTES {
            return None;
        }
        hasher.update(path.as_os_str().to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0xff]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn bounded_fingerprint_files(root: &Arc<AuthorizedRoot>, limit: usize) -> Option<Vec<PathBuf>> {
    let walked = root
        .walk_files_matching(
            WalkLimits {
                max_files: limit,
                max_file_bytes: 2 * 1024 * 1024,
                max_total_bytes: MAX_FINGERPRINT_BYTES,
                max_depth: 64,
                skip_directories: [".git", "target", "vendor", "node_modules"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
            },
            |path| {
                path.file_name().is_some_and(|name| {
                    name == "Cargo.toml" || name == "Cargo.lock" || name == "build.rs"
                }) || path.extension().is_some_and(|extension| extension == "rs")
            },
        )
        .ok()?;
    if !walked.issues.is_empty() {
        return None;
    }
    Some(
        walked
            .files
            .into_iter()
            .map(|file| root.path().join(file.path))
            .collect(),
    )
}

fn resolve_cache_root(workspace_root: &Path, requested: Option<&Path>) -> PathBuf {
    let default = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("agz-rust-coder")
        .join("docs");
    let candidate = requested.map_or_else(|| default.clone(), Path::to_owned);
    let candidate = absolute_path(&candidate);
    if !candidate.starts_with(workspace_root) {
        return candidate;
    }
    let fallback = absolute_path(&std::env::temp_dir().join("agz-rust-coder").join("docs"));
    if fallback.starts_with(workspace_root) {
        absolute_path(
            &std::env::temp_dir().join(format!("agz-rust-coder-docs-{}", std::process::id())),
        )
    } else {
        fallback
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |directory| directory.join(path))
    }
}

pub fn is_crates_io_registry(registry: Option<&str>) -> bool {
    let Some(registry) = registry else {
        return false;
    };
    let registry = registry.trim().trim_end_matches('/').to_ascii_lowercase();
    matches!(
        registry.as_str(),
        CRATES_IO_REGISTRY | "https://index.crates.io"
    )
}

/// Builds the fixed docs.rs URL for one exact crates.io registry candidate.
pub fn build_docs_rs_url(
    crate_name: &str,
    version: &str,
    page: &str,
) -> Result<String, DocsUrlError> {
    if !valid_url_segment(crate_name) || !valid_url_segment(version) {
        return Err(DocsUrlError::InvalidSegment);
    }
    let parts = validate_page(page)?;
    let rustdoc_name = crate_name.replace('-', "_");
    let encoded_page = parts
        .iter()
        .map(|part| encode_url_segment(part))
        .collect::<Vec<_>>()
        .join("/");
    let url = format!(
        "https://docs.rs/{}/{}/{}/{}",
        encode_url_segment(crate_name),
        encode_url_segment(version),
        encode_url_segment(&rustdoc_name),
        encoded_page
    );
    is_valid_docs_rs_url(&url)
        .then_some(url)
        .ok_or(DocsUrlError::InvalidUrl)
}

/// Builds a docs.rs URL only for the public crates.io registry.
pub fn docs_rs_url(dependency: &CargoLockCandidate, page: &str) -> Result<String, DocsUrlError> {
    if dependency.source != CargoLockSource::Registry
        || !is_crates_io_registry(
            dependency
                .registry
                .as_deref()
                .or(dependency.source_url.as_deref()),
        )
    {
        return Err(DocsUrlError::NonCratesIoSource);
    }
    build_docs_rs_url(&dependency.name, &dependency.version, page)
}

/// Accepts only HTTPS URLs whose authority is exactly `docs.rs`.
pub fn is_valid_docs_rs_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return false;
    };
    if !authority.eq_ignore_ascii_case("docs.rs")
        || authority.contains('@')
        || authority.contains(':')
        || authority.bytes().any(|byte| byte.is_ascii_control())
    {
        return false;
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains('\\')
                && !part.bytes().any(|byte| byte.is_ascii_control())
        })
}

fn valid_url_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', '?', '#', '%'])
        && !value.chars().any(char::is_control)
}

fn validate_page(page: &str) -> Result<Vec<&str>, DocsUrlError> {
    let parts = page.split('/').collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| !valid_url_segment(part)) {
        return Err(DocsUrlError::InvalidSegment);
    }
    Ok(parts)
}

fn encode_url_segment(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~') {
            result.push(byte as char);
        } else {
            result.push('%');
            result.push(hex(byte >> 4));
            result.push(hex(byte & 0x0f));
        }
    }
    result
}

fn hex(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + value - 10) as char,
    }
}
