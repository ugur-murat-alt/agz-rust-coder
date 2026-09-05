#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, BTreeSet, VecDeque, btree_map::Entry};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cargo_metadata::{Metadata, MetadataCommand};
use sha2::{Digest, Sha256};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use super::graph::{PackageGraph, build_package_graph};
use super::roots::{AuthorizedRoot, RootError, RootGuard};
use super::select::{SelectionError, WorkspaceSelection};
use crate::process::{ProcessRunOptions, ProcessSupervisor, root_bound::RootBoundCommand};

const DEFAULT_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MEMBER_DEPTH: usize = 32;
const MAX_METADATA_INPUT_FILES: usize = 256;
const MAX_METADATA_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_METADATA_CONFIG_DEPTH: usize = 32;
const FLIGHT_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MetadataCacheState {
    Hit,
    Miss,
    Bypass,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MetadataError {
    Root(RootError),
    Selection(SelectionError),
    InvalidManifest(PathBuf),
    ManifestTooLarge(PathBuf),
    ManifestParse { path: PathBuf, message: String },
    PathDependencyBlocked(PathBuf),
    PathDependencyMissing(PathBuf),
    UnexpectedExternalPath(PathBuf),
    WorkspaceRootOutside(PathBuf),
    LockedRequired,
    Cancelled,
    TimedOut,
    Runner(String),
    RootBinding(String),
    RootEpochChanged { expected: u64, actual: u64 },
    Poisoned,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(error) => error.fmt(formatter),
            Self::Selection(error) => error.fmt(formatter),
            Self::InvalidManifest(path) => {
                write!(formatter, "invalid Cargo.toml: {}", path.display())
            }
            Self::ManifestTooLarge(path) => write!(
                formatter,
                "Cargo.toml exceeds the bounded input size: {}",
                path.display()
            ),
            Self::ManifestParse { path, message } => {
                write!(formatter, "failed to parse {}: {message}", path.display())
            }
            Self::PathDependencyBlocked(path) => write!(
                formatter,
                "path dependency is outside the authorized dependency roots: {}",
                path.display()
            ),
            Self::PathDependencyMissing(path) => write!(
                formatter,
                "path dependency directory was not found: {}",
                path.display()
            ),
            Self::UnexpectedExternalPath(path) => write!(
                formatter,
                "metadata reported an unexpected external path: {}",
                path.display()
            ),
            Self::WorkspaceRootOutside(path) => write!(
                formatter,
                "metadata workspace root is outside the authorized root: {}",
                path.display()
            ),
            Self::LockedRequired => formatter.write_str("metadata execution must use --locked"),
            Self::Cancelled => formatter.write_str("cargo metadata was cancelled"),
            Self::TimedOut => formatter.write_str("cargo metadata deadline elapsed"),
            Self::Runner(message) => write!(formatter, "cargo metadata failed: {message}"),
            Self::RootBinding(message) => {
                write!(formatter, "cargo metadata root binding failed: {message}")
            }
            Self::RootEpochChanged { expected, actual } => {
                write!(formatter, "root epoch changed from {expected} to {actual}")
            }
            Self::Poisoned => formatter.write_str("workspace metadata state was poisoned"),
        }
    }
}

impl std::error::Error for MetadataError {}

impl From<RootError> for MetadataError {
    fn from(error: RootError) -> Self {
        Self::Root(error)
    }
}

impl From<SelectionError> for MetadataError {
    fn from(error: SelectionError) -> Self {
        Self::Selection(error)
    }
}

#[derive(Debug, Clone)]
pub struct MetadataCommandSpec {
    pub cargo: PathBuf,
    pub manifest_path: PathBuf,
    pub current_dir: PathBuf,
    pub args: Vec<OsString>,
    pub locked: bool,
}

impl MetadataCommandSpec {
    pub fn for_selection(selection: &WorkspaceSelection, cargo: impl Into<PathBuf>) -> Self {
        let cargo = cargo.into();
        let manifest_path = selection.manifest_path().to_owned();
        let current_dir = selection.package_root().to_owned();
        let args = vec![
            OsString::from("metadata"),
            OsString::from("--format-version"),
            OsString::from("1"),
            OsString::from("--manifest-path"),
            manifest_path.clone().into_os_string(),
            OsString::from("--locked"),
        ];
        Self {
            cargo,
            manifest_path,
            current_dir,
            args,
            locked: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetadataRun {
    pub metadata: Metadata,
}

/// Request-scoped execution limits for the internal asynchronous metadata path.
/// The public synchronous metadata façade intentionally does not require it.
#[derive(Debug, Clone)]
pub struct MetadataControl {
    deadline: Instant,
    cancellation: CancellationToken,
    supervisor: ProcessSupervisor,
    runtime: Handle,
    supervised_sccache: bool,
}

impl MetadataControl {
    #[must_use]
    pub(crate) fn new(
        deadline: Instant,
        cancellation: CancellationToken,
        supervisor: ProcessSupervisor,
        runtime: Handle,
    ) -> Self {
        Self {
            deadline,
            cancellation,
            supervisor,
            runtime,
            supervised_sccache: false,
        }
    }

    #[must_use]
    pub(crate) fn with_supervised_sccache(mut self, enabled: bool) -> Self {
        self.supervised_sccache = enabled;
        self
    }

    pub(crate) fn checkpoint(&self) -> Result<(), MetadataError> {
        if self.cancellation.is_cancelled() {
            return Err(MetadataError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(MetadataError::TimedOut);
        }
        Ok(())
    }
}

pub trait MetadataRunner: Send + Sync + 'static {
    fn run(&self, command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError>;

    fn run_authorized(
        &self,
        command: &MetadataCommandSpec,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<MetadataRun, MetadataError> {
        let _ = authority;
        self.run(command)
    }

    fn run_with_control(
        &self,
        command: &MetadataCommandSpec,
        authority: Arc<AuthorizedRoot>,
        control: &MetadataControl,
    ) -> Result<MetadataRun, MetadataError> {
        control.checkpoint()?;
        self.run_authorized(command, authority)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CargoMetadataRunner;

impl MetadataRunner for CargoMetadataRunner {
    fn run(&self, command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError> {
        if !command.locked || !command.args.iter().any(|arg| arg == OsStr::new("--locked")) {
            return Err(MetadataError::LockedRequired);
        }
        let mut metadata = MetadataCommand::new();
        metadata
            .cargo_path(command.cargo.clone())
            .manifest_path(command.manifest_path.clone())
            .current_dir(command.current_dir.clone())
            .other_options(vec!["--locked".to_owned()]);
        metadata
            .exec()
            .map(|metadata| MetadataRun { metadata })
            .map_err(|error| MetadataError::Runner(error.to_string()))
    }

    fn run_authorized(
        &self,
        command: &MetadataCommandSpec,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<MetadataRun, MetadataError> {
        if !command.locked || !command.args.iter().any(|arg| arg == OsStr::new("--locked")) {
            return Err(MetadataError::LockedRequired);
        }
        let mut args = command.args.clone();
        if let Some(index) = args.iter().position(|arg| arg == "--manifest-path")
            && let Some(path) = args.get_mut(index.saturating_add(1))
        {
            *path = OsString::from("Cargo.toml");
        }
        let bound = RootBoundCommand::new(
            &authority,
            &command.current_dir,
            &command.cargo,
            &args,
            &std::env::vars_os().collect(),
        )
        .map_err(|error| MetadataError::RootBinding(error.to_string()))?;
        let output = Command::new(bound.executable)
            .args(bound.args)
            .env_clear()
            .envs(bound.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| MetadataError::RootBinding(error.to_string()))?;
        // Keep the exact directory handle live through child completion. This
        // is the Windows replacement barrier; Unix uses the verified cwd.
        drop(authority);
        if !output.status.success() {
            return Err(MetadataError::Runner(
                String::from_utf8_lossy(&output.stderr).into(),
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map(|metadata| MetadataRun { metadata })
            .map_err(|error| MetadataError::Runner(error.to_string()))
    }

    fn run_with_control(
        &self,
        command: &MetadataCommandSpec,
        authority: Arc<AuthorizedRoot>,
        control: &MetadataControl,
    ) -> Result<MetadataRun, MetadataError> {
        if !command.locked || !command.args.iter().any(|arg| arg == OsStr::new("--locked")) {
            return Err(MetadataError::LockedRequired);
        }
        control.checkpoint()?;
        // An explicitly supervised sccache session starts only after metadata.
        // Metadata's rustc information probes must not start an ambient daemon.
        // Empty RUSTC_WRAPPER overrides Cargo config for this subprocess only;
        // compilation still uses the validated, owned wrapper session.
        let mut environment = std::env::vars_os().collect::<BTreeMap<_, _>>();
        if control.supervised_sccache {
            environment.insert("RUSTC_WRAPPER".into(), "".into());
        }
        let result = control
            .runtime
            .block_on(
                control.supervisor.run_authorized(
                    command.cargo.clone(),
                    command.args.clone(),
                    ProcessRunOptions::new(command.current_dir.clone())
                        .with_deadline(control.deadline)
                        .with_cancellation(control.cancellation.clone())
                        .with_environment(environment),
                    authority,
                ),
            )
            .map_err(|error| match error {
                crate::process::ProcessError::Cancelled => MetadataError::Cancelled,
                crate::process::ProcessError::TimedOut => MetadataError::TimedOut,
                error => MetadataError::Runner(error.to_string()),
            })?;
        if result.cancelled {
            return Err(MetadataError::Cancelled);
        }
        if result.timed_out {
            return Err(MetadataError::TimedOut);
        }
        if !result.drain_complete || !result.cleanup_complete {
            return Err(MetadataError::Runner(
                "cargo metadata cleanup did not complete".to_owned(),
            ));
        }
        control.checkpoint()?;
        if result.exit_code != 0 {
            return Err(MetadataError::Runner(result.stderr));
        }
        serde_json::from_str(&result.stdout)
            .map(|metadata| MetadataRun { metadata })
            .map_err(|error| MetadataError::Runner(error.to_string()))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DependencyClosure {
    pub manifests: Vec<PathBuf>,
    pub package_roots: Vec<PathBuf>,
    pub external_roots: Vec<PathBuf>,
    pub complete: bool,
}

impl DependencyClosure {
    fn new() -> Self {
        Self {
            manifests: Vec::new(),
            package_roots: Vec::new(),
            external_roots: Vec::new(),
            complete: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub requested_dir: PathBuf,
    pub canonical_worktree: PathBuf,
    pub package_root: PathBuf,
    pub workspace_root: PathBuf,
    pub manifest_path: PathBuf,
    pub generation: u64,
    pub epoch: u64,
    pub metadata: Arc<Metadata>,
    pub graph: PackageGraph,
    pub target_directory: PathBuf,
    pub external_paths: Vec<PathBuf>,
    pub dependency_closure: DependencyClosure,
    pub metadata_cache: MetadataCacheState,
}

#[derive(Debug, Clone)]
pub struct MetadataLoad {
    pub snapshot: Arc<WorkspaceSnapshot>,
    pub cache: MetadataCacheState,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct MetadataKey {
    package_root: PathBuf,
    epoch: u64,
    generation: u64,
    graph_fingerprint: [u8; 32],
    supervised_sccache: bool,
}

#[derive(Debug)]
struct Flight {
    result: Mutex<Option<Result<Arc<WorkspaceSnapshot>, MetadataError>>>,
    ready: Condvar,
}

impl Flight {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

#[derive(Debug, Default)]
struct MetadataState {
    generations: BTreeMap<PathBuf, u64>,
    cache: BTreeMap<MetadataKey, Arc<WorkspaceSnapshot>>,
    active: BTreeMap<MetadataKey, Arc<Flight>>,
}

pub struct MetadataService<R: MetadataRunner = CargoMetadataRunner> {
    guard: Arc<RootGuard>,
    runner: Arc<R>,
    state: Mutex<MetadataState>,
}

impl<R: MetadataRunner> fmt::Debug for MetadataService<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MetadataService")
            .field("guard", &self.guard)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl MetadataService<CargoMetadataRunner> {
    pub fn new(guard: Arc<RootGuard>) -> Self {
        Self::with_runner(guard, CargoMetadataRunner)
    }
}

impl<R: MetadataRunner> MetadataService<R> {
    pub fn with_runner(guard: Arc<RootGuard>, runner: R) -> Self {
        Self {
            guard,
            runner: Arc::new(runner),
            state: Mutex::new(MetadataState::default()),
        }
    }

    pub fn preflight(
        &self,
        selection: &WorkspaceSelection,
    ) -> Result<DependencyClosure, MetadataError> {
        self.preflight_inner(selection, None)
    }

    fn preflight_inner(
        &self,
        selection: &WorkspaceSelection,
        control: Option<&MetadataControl>,
    ) -> Result<DependencyClosure, MetadataError> {
        checkpoint(control)?;
        self.check_epoch(selection.epoch())?;
        let mut closure = DependencyClosure::new();
        let mut queue = VecDeque::new();
        // The package was opened while selecting the workspace. Do not reopen
        // its lexical path through the configured parent after that boundary.
        let package_root = selection.package_authority().path().to_owned();
        queue.push_back(package_root);
        let mut visited = BTreeSet::new();

        while let Some(manifest_dir) = queue.pop_front() {
            checkpoint(control)?;
            if !visited.insert(manifest_dir.clone()) {
                continue;
            }
            let manifest = self.open_manifest(selection, &manifest_dir)?;
            checkpoint(control)?;
            let manifest_path = manifest_dir.join("Cargo.toml");
            closure.manifests.push(manifest_path);
            closure.package_roots.push(manifest_dir.clone());

            let value = parse_manifest(&manifest_dir.join("Cargo.toml"), &manifest)?;
            for candidate in collect_path_values(&value) {
                checkpoint(control)?;
                let candidate =
                    resolve_manifest_path(&manifest_dir, &candidate).ok_or_else(|| {
                        MetadataError::PathDependencyMissing(manifest_dir.join(&candidate))
                    })?;
                self.enqueue_manifest(selection, &mut closure, &mut queue, candidate)?;
            }
            for member in collect_workspace_members(&value) {
                checkpoint(control)?;
                for candidate in expand_workspace_member(selection, &manifest_dir, &member)? {
                    checkpoint(control)?;
                    self.enqueue_manifest(selection, &mut closure, &mut queue, candidate)?;
                }
            }
        }
        closure.manifests.sort();
        closure.manifests.dedup();
        closure.package_roots.sort();
        closure.package_roots.dedup();
        closure.external_roots.sort();
        closure.external_roots.dedup();
        Ok(closure)
    }

    fn enqueue_manifest(
        &self,
        selection: &WorkspaceSelection,
        closure: &mut DependencyClosure,
        queue: &mut VecDeque<PathBuf>,
        candidate: PathBuf,
    ) -> Result<(), MetadataError> {
        let dependency_dir = match self.authorize_manifest_dir(selection, &candidate) {
            Ok(directory) => directory,
            Err(
                MetadataError::Root(RootError::PathOutsideRoot(_))
                | MetadataError::PathDependencyBlocked(_),
            ) => {
                return Err(MetadataError::PathDependencyBlocked(candidate));
            }
            Err(MetadataError::Root(RootError::PathNotFound(_))) => {
                return Err(MetadataError::PathDependencyMissing(candidate));
            }
            Err(error) => return Err(error),
        };
        let dependency_path = dependency_dir.path().to_owned();
        if !selection.authority().contains(&dependency_path)
            && !closure.external_roots.contains(&dependency_path)
        {
            closure.external_roots.push(dependency_path.clone());
        }
        queue.push_back(dependency_path);
        Ok(())
    }

    pub fn acquire(
        &self,
        selection: &WorkspaceSelection,
        cargo: impl Into<PathBuf>,
    ) -> Result<MetadataLoad, MetadataError> {
        self.acquire_inner(selection, cargo.into(), None, false)
    }

    pub(crate) fn acquire_controlled(
        &self,
        selection: &WorkspaceSelection,
        cargo: impl Into<PathBuf>,
        control: &MetadataControl,
    ) -> Result<MetadataLoad, MetadataError> {
        self.acquire_inner(selection, cargo.into(), Some(control), true)
    }

    fn acquire_inner(
        &self,
        selection: &WorkspaceSelection,
        cargo: PathBuf,
        control: Option<&MetadataControl>,
        authorized_execution: bool,
    ) -> Result<MetadataLoad, MetadataError> {
        checkpoint(control)?;
        self.check_epoch(selection.epoch())?;
        let generation = self.generation(selection.package_root())?;
        let closure = self.preflight_inner(selection, control)?;
        let Some(graph_fingerprint) = self.cache_fingerprint_inner(selection, &closure, control)?
        else {
            let snapshot = self.load_uncached(
                selection,
                cargo,
                generation,
                closure,
                MetadataCacheState::Bypass,
                control,
                authorized_execution,
            )?;
            return Ok(MetadataLoad {
                snapshot,
                cache: MetadataCacheState::Bypass,
            });
        };
        let key = MetadataKey {
            package_root: selection.package_root().to_owned(),
            epoch: selection.epoch(),
            generation,
            graph_fingerprint,
            supervised_sccache: control.is_some_and(|control| control.supervised_sccache),
        };

        let (flight, _) = loop {
            checkpoint(control)?;
            let (flight, owner) = {
                let mut state = self.state.lock().map_err(|_| MetadataError::Poisoned)?;
                if let Some(snapshot) = state.cache.get(&key) {
                    return Ok(MetadataLoad {
                        snapshot: snapshot.clone(),
                        cache: MetadataCacheState::Hit,
                    });
                }
                if let Some(flight) = state.active.get(&key) {
                    (flight.clone(), false)
                } else {
                    let flight = Arc::new(Flight::new());
                    state.active.insert(key.clone(), flight.clone());
                    (flight, true)
                }
            };
            if owner {
                break (flight, true);
            }

            match wait_for_flight(&flight, control)? {
                // Cancellation and deadline are request-local outcomes. The
                // completed owner flight has already been removed from
                // `active`, so a healthy follower may claim a fresh flight.
                Err(MetadataError::Cancelled | MetadataError::TimedOut) => continue,
                result => {
                    return result.map(|snapshot| MetadataLoad {
                        snapshot,
                        cache: MetadataCacheState::Hit,
                    });
                }
            }
        };

        let mut result = self.load_uncached(
            selection,
            cargo,
            generation,
            closure,
            MetadataCacheState::Miss,
            control,
            authorized_execution,
        );
        if result.is_ok() {
            if let Err(error) = checkpoint(control) {
                result = Err(error);
            }
        }
        let mut state = self.state.lock().map_err(|_| MetadataError::Poisoned)?;
        state.active.remove(&key);
        // Remove the completed flight before wake-up. Otherwise a healthy
        // follower can observe the request-local terminal result repeatedly
        // while `active` still points at this flight.
        {
            let mut slot = flight.result.lock().map_err(|_| MetadataError::Poisoned)?;
            *slot = Some(result.clone());
            flight.ready.notify_all();
        }
        match result {
            Ok(snapshot) => {
                if state
                    .generations
                    .get(selection.package_root())
                    .copied()
                    .unwrap_or(0)
                    == generation
                {
                    state.cache.insert(key, snapshot.clone());
                }
                Ok(MetadataLoad {
                    snapshot,
                    cache: MetadataCacheState::Miss,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn mark_dirty(&self, package_root: &Path) -> Result<u64, MetadataError> {
        let package_root = package_root.to_owned();
        let mut state = self.state.lock().map_err(|_| MetadataError::Poisoned)?;
        let generation = {
            let generation = state.generations.entry(package_root.clone()).or_insert(0);
            *generation = generation.saturating_add(1);
            *generation
        };
        state.cache.retain(|key, snapshot| {
            key.package_root != package_root && snapshot.package_root != package_root
        });
        Ok(generation)
    }

    pub fn generation(&self, package_root: &Path) -> Result<u64, MetadataError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| MetadataError::Poisoned)?
            .generations
            .get(package_root)
            .copied()
            .unwrap_or(0))
    }

    pub fn is_current(&self, snapshot: &WorkspaceSnapshot) -> Result<bool, MetadataError> {
        Ok(
            self.generation(&snapshot.package_root)? == snapshot.generation
                && self.guard.current_epoch()? == snapshot.epoch,
        )
    }

    pub fn clear(&self) -> Result<(), MetadataError> {
        let mut state = self.state.lock().map_err(|_| MetadataError::Poisoned)?;
        state.cache.clear();
        state.generations.clear();
        Ok(())
    }

    fn load_uncached(
        &self,
        selection: &WorkspaceSelection,
        cargo: PathBuf,
        generation: u64,
        closure: DependencyClosure,
        metadata_cache: MetadataCacheState,
        control: Option<&MetadataControl>,
        authorized_execution: bool,
    ) -> Result<Arc<WorkspaceSnapshot>, MetadataError> {
        checkpoint(control)?;
        let command = MetadataCommandSpec::for_selection(selection, cargo);
        if !command.locked {
            return Err(MetadataError::LockedRequired);
        }
        let run = if let Some(control) = control {
            let authority = selection.package_authority().clone();
            self.runner.run_with_control(&command, authority, control)?
        } else if authorized_execution {
            let authority = selection.package_authority().clone();
            self.runner.run_authorized(&command, authority)?
        } else {
            self.runner.run(&command)?
        };
        checkpoint(control)?;
        self.check_epoch(selection.epoch())?;
        let metadata = validate_metadata(&run.metadata, selection, &closure, &self.guard)?;
        checkpoint(control)?;
        let workspace_root = canonical_metadata_workspace_root(&run.metadata, selection)?;
        let target_directory = PathBuf::from(run.metadata.target_directory.as_std_path());
        let external_paths = metadata
            .packages
            .iter()
            .filter_map(|package| {
                let is_member = run
                    .metadata
                    .workspace_members
                    .iter()
                    .any(|member| member == &package.id);
                (!is_member && package.source.is_none()).then(|| {
                    PathBuf::from(
                        package
                            .manifest_path
                            .as_std_path()
                            .parent()
                            .unwrap_or(package.manifest_path.as_std_path()),
                    )
                })
            })
            .collect::<Vec<_>>();
        checkpoint(control)?;
        let snapshot = WorkspaceSnapshot {
            requested_dir: selection.requested_dir().to_owned(),
            canonical_worktree: selection.canonical_worktree().to_owned(),
            package_root: selection.package_root().to_owned(),
            workspace_root,
            manifest_path: selection.manifest_path().to_owned(),
            generation,
            epoch: selection.epoch(),
            graph: build_package_graph(&metadata),
            metadata: Arc::new(metadata),
            target_directory,
            external_paths,
            dependency_closure: closure,
            metadata_cache,
        };
        checkpoint(control)?;
        Ok(Arc::new(snapshot))
    }

    fn cache_fingerprint_inner(
        &self,
        selection: &WorkspaceSelection,
        closure: &DependencyClosure,
        control: Option<&MetadataControl>,
    ) -> Result<Option<[u8; 32]>, MetadataError> {
        checkpoint(control)?;
        let mut inputs = BTreeMap::new();
        if closure.manifests.len() > MAX_METADATA_INPUT_FILES {
            return Ok(None);
        }
        for manifest in &closure.manifests {
            checkpoint(control)?;
            let bytes = match self.read_cache_input(selection, manifest) {
                Ok(Some(bytes)) => bytes,
                Ok(None) | Err(_) => return Ok(None),
            };
            inputs.insert(manifest.clone(), Some(bytes));
        }

        let mut current = selection.package_root().to_owned();
        let mut workspace_root = None;
        let mut reached_authority = false;
        for _ in 0..=MAX_METADATA_CONFIG_DEPTH {
            checkpoint(control)?;
            if !selection.authority().contains(&current) {
                return Ok(None);
            }
            let manifest_path = current.join("Cargo.toml");
            if !inputs.contains_key(&manifest_path) {
                let bytes = match self.read_cache_input(selection, &manifest_path) {
                    Ok(bytes) => bytes,
                    Err(_) => return Ok(None),
                };
                inputs.insert(manifest_path.clone(), bytes);
            }
            if workspace_root.is_none()
                && let Some(Some(bytes)) = inputs.get(&manifest_path)
            {
                let Ok(value) = parse_manifest(&manifest_path, bytes) else {
                    return Ok(None);
                };
                if value
                    .get("workspace")
                    .and_then(toml::Value::as_table)
                    .is_some()
                {
                    workspace_root = Some(current.clone());
                }
            }

            for config in [
                current.join(".cargo/config"),
                current.join(".cargo/config.toml"),
            ] {
                if let Entry::Vacant(entry) = inputs.entry(config.clone()) {
                    checkpoint(control)?;
                    let bytes = match self.read_cache_input(selection, &config) {
                        Ok(bytes) => bytes,
                        Err(_) => return Ok(None),
                    };
                    entry.insert(bytes);
                }
            }

            if current == selection.authority().path() {
                reached_authority = true;
                break;
            }
            let Some(parent) = current.parent() else {
                reached_authority = true;
                break;
            };
            if !selection.authority().contains(parent) {
                reached_authority = true;
                break;
            }
            current = parent.to_owned();
        }
        if !reached_authority {
            return Ok(None);
        }

        let lock_root = workspace_root.unwrap_or_else(|| selection.package_root().to_owned());
        let lock_path = lock_root.join("Cargo.lock");
        if let Entry::Vacant(entry) = inputs.entry(lock_path.clone()) {
            let bytes = match self.read_cache_input(selection, &lock_path) {
                Ok(bytes) => bytes,
                Err(_) => return Ok(None),
            };
            entry.insert(bytes);
        }
        if inputs.len() > MAX_METADATA_INPUT_FILES {
            return Ok(None);
        }

        let mut hasher = Sha256::new();
        hasher.update(b"agz-rust-coder-metadata-graph-v1");
        let mut total_bytes = 0u64;
        for (path, bytes) in inputs {
            checkpoint(control)?;
            hasher.update(b"path");
            hash_metadata_path(&mut hasher, &path);
            match bytes {
                Some(bytes) => {
                    let Some(length) = u64::try_from(bytes.len()).ok() else {
                        return Ok(None);
                    };
                    let Some(next_total) = total_bytes.checked_add(length) else {
                        return Ok(None);
                    };
                    total_bytes = next_total;
                    if total_bytes > MAX_METADATA_INPUT_BYTES {
                        return Ok(None);
                    }
                    hasher.update(b"present");
                    hasher.update(length.to_le_bytes());
                    hasher.update(bytes);
                }
                None => {
                    hasher.update(b"missing");
                }
            }
        }
        Ok(Some(hasher.finalize().into()))
    }

    fn read_cache_input(
        &self,
        selection: &WorkspaceSelection,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, RootError> {
        let root = if selection.authority().contains(path) {
            selection.authority().clone()
        } else {
            self.guard.resolve_dependency(path)?.root
        };
        match root.read_file(path, DEFAULT_MANIFEST_BYTES) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(RootError::PathNotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn check_epoch(&self, expected: u64) -> Result<(), MetadataError> {
        let actual = self.guard.current_epoch()?;
        if actual != expected {
            return Err(MetadataError::RootEpochChanged { expected, actual });
        }
        Ok(())
    }

    fn open_manifest(
        &self,
        selection: &WorkspaceSelection,
        directory: &Path,
    ) -> Result<Vec<u8>, MetadataError> {
        let root = self.authorize_manifest_dir(selection, directory)?;
        root.read_file(Path::new("Cargo.toml"), DEFAULT_MANIFEST_BYTES)
            .map_err(MetadataError::from)
    }

    fn authorize_manifest_dir(
        &self,
        selection: &WorkspaceSelection,
        directory: &Path,
    ) -> Result<Arc<AuthorizedRoot>, MetadataError> {
        if directory == selection.package_authority().path() {
            return Ok(selection.package_authority().clone());
        }
        if selection.worktree_authority().contains(directory) {
            return selection
                .worktree_authority()
                .authorize_dir(directory)
                .map_err(MetadataError::from);
        }
        match self.guard.resolve_dependency(directory) {
            Ok(resolved) => resolved
                .root
                .authorize_dir(&resolved.canonical)
                .map_err(MetadataError::from),
            Err(RootError::PathOutsideRoot(_)) => {
                Err(MetadataError::PathDependencyBlocked(directory.to_owned()))
            }
            Err(error) => Err(MetadataError::Root(error)),
        }
    }
}

fn checkpoint(control: Option<&MetadataControl>) -> Result<(), MetadataError> {
    control.map_or(Ok(()), MetadataControl::checkpoint)
}

fn wait_for_flight(
    flight: &Flight,
    control: Option<&MetadataControl>,
) -> Result<Result<Arc<WorkspaceSnapshot>, MetadataError>, MetadataError> {
    let mut result = flight.result.lock().map_err(|_| MetadataError::Poisoned)?;
    while result.is_none() {
        checkpoint(control)?;
        let Some(control) = control else {
            result = flight
                .ready
                .wait(result)
                .map_err(|_| MetadataError::Poisoned)?;
            continue;
        };
        let remaining = control.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(MetadataError::TimedOut);
        }
        let (next, _) = flight
            .ready
            .wait_timeout(result, remaining.min(FLIGHT_WAIT_POLL_INTERVAL))
            .map_err(|_| MetadataError::Poisoned)?;
        result = next;
    }
    result.clone().ok_or(MetadataError::Poisoned)
}

fn parse_manifest(path: &Path, bytes: &[u8]) -> Result<toml::Value, MetadataError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| MetadataError::InvalidManifest(path.to_owned()))?;
    text.parse::<toml::Table>()
        .map(toml::Value::Table)
        .map_err(|error| MetadataError::ManifestParse {
            path: path.to_owned(),
            message: error.to_string(),
        })
}

fn collect_path_values(value: &toml::Value) -> Vec<String> {
    let mut values = Vec::new();
    let Some(root) = value.as_table() else {
        return values;
    };
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        collect_dependency_paths(root.get(key), &mut values);
    }
    if let Some(workspace) = root.get("workspace").and_then(toml::Value::as_table) {
        collect_dependency_paths(workspace.get("dependencies"), &mut values);
    }
    if let Some(targets) = root.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                collect_dependency_paths(target.get(key), &mut values);
            }
        }
    }
    collect_nested_dependency_paths(root.get("patch"), &mut values);
    collect_dependency_paths(root.get("replace"), &mut values);
    values
}

fn collect_dependency_paths(value: Option<&toml::Value>, values: &mut Vec<String>) {
    let Some(dependencies) = value.and_then(toml::Value::as_table) else {
        return;
    };
    for dependency in dependencies.values() {
        if let Some(path) = dependency
            .as_table()
            .and_then(|table| table.get("path"))
            .and_then(toml::Value::as_str)
        {
            values.push(path.to_owned());
        }
    }
}

fn collect_nested_dependency_paths(value: Option<&toml::Value>, values: &mut Vec<String>) {
    let Some(tables) = value.and_then(toml::Value::as_table) else {
        return;
    };
    for table in tables.values() {
        collect_dependency_paths(Some(table), values);
    }
}

fn collect_workspace_members(value: &toml::Value) -> Vec<String> {
    value
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn resolve_manifest_path(base: &Path, value: &str) -> Option<PathBuf> {
    let candidate = Path::new(value);
    let candidate = if candidate.is_absolute() {
        candidate.to_owned()
    } else {
        base.join(candidate)
    };
    lexical_absolute(&candidate)
}

fn expand_workspace_member(
    selection: &WorkspaceSelection,
    base: &Path,
    pattern: &str,
) -> Result<Vec<PathBuf>, MetadataError> {
    let pattern = Path::new(pattern);
    if pattern.is_absolute() {
        return Err(MetadataError::PathDependencyBlocked(pattern.to_owned()));
    }
    let components = pattern.components().collect::<Vec<_>>();
    if components.len() > DEFAULT_MEMBER_DEPTH {
        return Err(MetadataError::PathDependencyBlocked(base.join(pattern)));
    }
    let mut candidates = vec![base.to_owned()];
    for component in components {
        let Component::Normal(component) = component else {
            return Err(MetadataError::PathDependencyBlocked(base.join(pattern)));
        };
        let component = component.to_string_lossy();
        let wildcard = component.contains('*') || component.contains('?');
        if !wildcard {
            candidates = candidates
                .into_iter()
                .map(|candidate| candidate.join(component.as_ref()))
                .collect();
            continue;
        }
        let mut expanded = Vec::new();
        for candidate in candidates {
            for entry in selection.authority().list_directory(&candidate)? {
                if entry.kind != super::roots::DirectoryEntryKind::Directory {
                    if wildcard_matches(&component, &entry.name.to_string_lossy()) {
                        return Err(MetadataError::PathDependencyBlocked(
                            candidate.join(entry.name),
                        ));
                    }
                    continue;
                }
                if wildcard_matches(&component, &entry.name.to_string_lossy()) {
                    expanded.push(candidate.join(entry.name));
                }
            }
        }
        candidates = expanded;
    }

    let mut resolved = Vec::new();
    for candidate in candidates {
        let Some(candidate) = lexical_absolute(&candidate) else {
            return Err(MetadataError::PathDependencyBlocked(candidate));
        };
        let directory =
            selection
                .authority()
                .authorize_dir(&candidate)
                .map_err(|error| match error {
                    RootError::PathNotFound(_) => {
                        MetadataError::PathDependencyMissing(candidate.clone())
                    }
                    RootError::PathOutsideRoot(_) | RootError::Symlink(_) => {
                        MetadataError::PathDependencyBlocked(candidate.clone())
                    }
                    other => MetadataError::Root(other),
                })?;
        directory
            .read_file(Path::new("Cargo.toml"), DEFAULT_MANIFEST_BYTES)
            .map_err(|error| match error {
                RootError::PathNotFound(_) => {
                    MetadataError::PathDependencyMissing(candidate.clone())
                }
                other => MetadataError::Root(other),
            })?;
        resolved.push(directory.path().to_owned());
    }
    resolved.sort();
    resolved.dedup();
    Ok(resolved)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut star = None;
    let mut star_value = 0;
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == value[value_index] || pattern[pattern_index] == b'?')
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value += 1;
            value_index = star_value;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn lexical_absolute(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => result.push(prefix.as_os_str()),
            Component::RootDir => result.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::Normal(name) => result.push(name),
        }
    }
    Some(result)
}

fn hash_metadata_path(hasher: &mut Sha256, path: &Path) {
    let path = path.to_string_lossy();
    let length = u64::try_from(path.len()).unwrap_or(u64::MAX);
    hasher.update(length.to_le_bytes());
    hasher.update(path.as_bytes());
}

fn canonical_metadata_workspace_root(
    metadata: &Metadata,
    selection: &WorkspaceSelection,
) -> Result<PathBuf, MetadataError> {
    let workspace_root = PathBuf::from(metadata.workspace_root.as_std_path());
    if workspace_root == selection.package_authority().path() {
        return Ok(selection.package_authority().path().to_owned());
    }
    if !selection.worktree_authority().contains(&workspace_root) {
        return Err(MetadataError::WorkspaceRootOutside(workspace_root));
    }
    let root = selection
        .worktree_authority()
        .authorize_dir(&workspace_root)?;
    Ok(root.path().to_owned())
}

fn validate_metadata(
    metadata: &Metadata,
    selection: &WorkspaceSelection,
    closure: &DependencyClosure,
    guard: &RootGuard,
) -> Result<Metadata, MetadataError> {
    let workspace_members: BTreeSet<String> = metadata
        .workspace_members
        .iter()
        .map(|package_id| package_id.repr.clone())
        .collect();
    let mut expected_external = closure
        .external_roots
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut validated = metadata.clone();
    for package in &metadata.packages {
        if package.source.is_some() || workspace_members.contains(&package.id.repr) {
            continue;
        }
        let root = PathBuf::from(
            package
                .manifest_path
                .as_std_path()
                .parent()
                .unwrap_or(package.manifest_path.as_std_path()),
        );
        let canonical = if selection.authority().contains(&root) {
            selection
                .authority()
                .authorize_dir(&root)?
                .path()
                .to_owned()
        } else {
            let resolved = guard
                .resolve_dependency(&root)
                .map_err(|_| MetadataError::UnexpectedExternalPath(root.clone()))?;
            resolved.canonical
        };
        if selection.authority().contains(&canonical) {
            continue;
        }
        if !expected_external.remove(&canonical) {
            return Err(MetadataError::UnexpectedExternalPath(canonical));
        }
    }
    if let Some(unused) = expected_external.into_iter().next() {
        return Err(MetadataError::UnexpectedExternalPath(unused));
    }
    validated
        .packages
        .sort_by(|left, right| left.id.repr.cmp(&right.id.repr));
    Ok(validated)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use cargo_metadata::MetadataCommand;

    use super::*;
    use crate::workspace::{ClientRoots, WorkspaceSelection, select_workspace};

    #[derive(Debug)]
    struct TestWorkspace(PathBuf);

    impl TestWorkspace {
        fn new(label: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let root = std::fs::canonicalize(std::env::temp_dir())
                .expect("canonical temp directory")
                .join(format!(
                    "agz-rust-coder-metadata-control-{label}-{}-{stamp}",
                    std::process::id()
                ));
            std::fs::create_dir_all(root.join("src")).expect("create workspace");
            std::fs::write(root.join(".git"), "fixture worktree marker").expect("write marker");
            std::fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"metadata-control-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .expect("write manifest");
            std::fs::write(
                root.join("Cargo.lock"),
                "version = 4\n\n[[package]]\nname = \"metadata-control-fixture\"\nversion = \"0.1.0\"\n",
            )
            .expect("write lockfile");
            std::fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").expect("write source");
            Self(root)
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug, Clone)]
    struct LateSuccessRunner {
        metadata: MetadataRun,
        calls: Arc<AtomicUsize>,
        started: Arc<AtomicBool>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl MetadataRunner for LateSuccessRunner {
        fn run(&self, _command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.metadata.clone())
        }

        fn run_with_control(
            &self,
            _command: &MetadataCommandSpec,
            _authority: Arc<AuthorizedRoot>,
            _control: &MetadataControl,
        ) -> Result<MetadataRun, MetadataError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.store(true, Ordering::Release);
            let (released, ready) = &*self.release;
            let mut released = released.lock().expect("release lock");
            while !*released {
                released = ready.wait(released).expect("release wait");
            }
            Ok(self.metadata.clone())
        }
    }

    #[derive(Debug, Clone)]
    struct DeadlineSuccessRunner {
        metadata: MetadataRun,
        calls: Arc<AtomicUsize>,
    }

    impl MetadataRunner for DeadlineSuccessRunner {
        fn run(&self, _command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.metadata.clone())
        }

        fn run_with_control(
            &self,
            _command: &MetadataCommandSpec,
            _authority: Arc<AuthorizedRoot>,
            control: &MetadataControl,
        ) -> Result<MetadataRun, MetadataError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let lock = Mutex::new(());
            let guard = lock.lock().expect("deadline lock");
            let _ = Condvar::new()
                .wait_timeout(
                    guard,
                    control.deadline.saturating_duration_since(Instant::now()),
                )
                .expect("deadline wait");
            Ok(self.metadata.clone())
        }
    }

    fn fixture() -> (
        TestWorkspace,
        Arc<RootGuard>,
        WorkspaceSelection,
        MetadataRun,
    ) {
        let workspace = TestWorkspace::new("late-cache");
        let guard = Arc::new(
            RootGuard::new([workspace.0.clone()], std::iter::empty()).expect("root guard"),
        );
        let snapshot = guard
            .snapshot(ClientRoots::unsupported())
            .expect("root snapshot");
        let selection = select_workspace(&snapshot, Some(&workspace.0)).expect("selection");
        let metadata = MetadataCommand::new()
            .manifest_path(workspace.0.join("Cargo.toml"))
            .exec()
            .expect("fixture metadata");
        (workspace, guard, selection, MetadataRun { metadata })
    }

    fn control(deadline: Instant, cancellation: CancellationToken) -> MetadataControl {
        MetadataControl::new(
            deadline,
            cancellation,
            ProcessSupervisor::without_journal(),
            Handle::current(),
        )
    }

    #[tokio::test]
    async fn cancelled_owner_late_success_is_not_published_to_cache() {
        let (_workspace, guard, selection, metadata) = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let service = Arc::new(MetadataService::with_runner(
            guard,
            LateSuccessRunner {
                metadata,
                calls: Arc::clone(&calls),
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            },
        ));
        let cancellation = CancellationToken::new();
        let owner = {
            let service = Arc::clone(&service);
            let selection = selection.clone();
            let control = control(
                Instant::now() + Duration::from_secs(2),
                cancellation.clone(),
            );
            tokio::task::spawn_blocking(move || {
                service.acquire_controlled(&selection, "cargo", &control)
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owner reached runner");
        cancellation.cancel();
        let (released, ready) = &*release;
        *released.lock().expect("release lock") = true;
        ready.notify_all();

        assert!(matches!(
            owner.await.expect("owner join"),
            Err(MetadataError::Cancelled)
        ));
        assert!(
            service
                .state
                .lock()
                .expect("metadata state")
                .cache
                .is_empty()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "late result was not cached"
        );
    }

    #[tokio::test]
    async fn healthy_follower_retries_after_owner_local_cancellation() {
        let (_workspace, guard, selection, metadata) = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let service = Arc::new(MetadataService::with_runner(
            guard,
            LateSuccessRunner {
                metadata,
                calls: Arc::clone(&calls),
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            },
        ));
        let owner_cancellation = CancellationToken::new();
        let owner = {
            let service = Arc::clone(&service);
            let selection = selection.clone();
            let control = control(
                Instant::now() + Duration::from_secs(2),
                owner_cancellation.clone(),
            );
            tokio::task::spawn_blocking(move || {
                service.acquire_controlled(&selection, "cargo", &control)
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owner reached runner");
        let follower = {
            let service = Arc::clone(&service);
            let selection = selection.clone();
            let control = control(
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            );
            tokio::task::spawn_blocking(move || {
                service.acquire_controlled(&selection, "cargo", &control)
            })
        };

        owner_cancellation.cancel();
        let (released, ready) = &*release;
        *released.lock().expect("release lock") = true;
        ready.notify_all();

        assert!(matches!(
            owner.await.expect("owner join"),
            Err(MetadataError::Cancelled)
        ));
        assert!(
            follower
                .await
                .expect("follower join")
                .expect("healthy follower refreshes metadata")
                .snapshot
                .metadata
                .packages
                .iter()
                .any(|package| package.name == "metadata-control-fixture")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            service.state.lock().expect("metadata state").cache.len(),
            1,
            "only the request-independent success is cached"
        );
    }

    #[tokio::test]
    async fn timed_out_owner_late_success_is_not_published_to_cache() {
        let (_workspace, guard, selection, metadata) = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = MetadataService::with_runner(
            guard,
            DeadlineSuccessRunner {
                metadata,
                calls: Arc::clone(&calls),
            },
        );
        let control = control(
            Instant::now() + Duration::from_millis(20),
            CancellationToken::new(),
        );

        assert!(matches!(
            service.acquire_controlled(&selection, "cargo", &control),
            Err(MetadataError::TimedOut)
        ));
        assert!(
            service
                .state
                .lock()
                .expect("metadata state")
                .cache
                .is_empty()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "late result was not cached"
        );
    }

    #[tokio::test]
    async fn singleflight_follower_honors_its_cancellation_and_deadline() {
        let flight = Flight::new();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = control(Instant::now() + Duration::from_secs(1), cancellation);
        assert!(matches!(
            wait_for_flight(&flight, Some(&cancelled)),
            Err(MetadataError::Cancelled)
        ));

        let timed_out = control(Instant::now(), CancellationToken::new());
        assert!(matches!(
            wait_for_flight(&flight, Some(&timed_out)),
            Err(MetadataError::TimedOut)
        ));
    }
}
