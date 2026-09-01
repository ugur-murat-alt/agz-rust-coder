#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, BTreeSet, VecDeque, btree_map::Entry};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use cargo_metadata::{Metadata, MetadataCommand};
use sha2::{Digest, Sha256};

use super::graph::{PackageGraph, build_package_graph};
use super::roots::{AuthorizedRoot, RootError, RootGuard};
use super::select::{SelectionError, WorkspaceSelection};

const DEFAULT_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MEMBER_DEPTH: usize = 32;
const MAX_METADATA_INPUT_FILES: usize = 256;
const MAX_METADATA_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_METADATA_CONFIG_DEPTH: usize = 32;

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
    Runner(String),
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
            Self::Runner(message) => write!(formatter, "cargo metadata failed: {message}"),
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

pub trait MetadataRunner: Send + Sync + 'static {
    fn run(&self, command: &MetadataCommandSpec) -> Result<MetadataRun, MetadataError>;
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
        self.check_epoch(selection.epoch())?;
        let mut closure = DependencyClosure::new();
        let mut queue = VecDeque::new();
        let package_root =
            canonical_manifest_directory(selection.authority(), selection.package_root())?;
        queue.push_back(package_root);
        let mut visited = BTreeSet::new();

        while let Some(manifest_dir) = queue.pop_front() {
            if !visited.insert(manifest_dir.clone()) {
                continue;
            }
            let manifest = self.open_manifest(selection, &manifest_dir)?;
            let manifest_path = manifest_dir.join("Cargo.toml");
            closure.manifests.push(manifest_path);
            closure.package_roots.push(manifest_dir.clone());

            let value = parse_manifest(&manifest_dir.join("Cargo.toml"), &manifest)?;
            for candidate in collect_path_values(&value) {
                let candidate =
                    resolve_manifest_path(&manifest_dir, &candidate).ok_or_else(|| {
                        MetadataError::PathDependencyMissing(manifest_dir.join(&candidate))
                    })?;
                self.enqueue_manifest(selection, &mut closure, &mut queue, candidate)?;
            }
            for member in collect_workspace_members(&value) {
                for candidate in expand_workspace_member(selection, &manifest_dir, &member)? {
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
        self.check_epoch(selection.epoch())?;
        let generation = self.generation(selection.package_root())?;
        let cargo = cargo.into();
        let closure = self.preflight(selection)?;
        let Some(graph_fingerprint) = self.cache_fingerprint(selection, &closure) else {
            let snapshot = self.load_uncached(
                selection,
                cargo,
                generation,
                closure,
                MetadataCacheState::Bypass,
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
        };

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

        if !owner {
            let result = wait_for_flight(&flight)?;
            return result.map(|snapshot| MetadataLoad {
                snapshot,
                cache: MetadataCacheState::Hit,
            });
        }

        let result = self.load_uncached(
            selection,
            cargo,
            generation,
            closure,
            MetadataCacheState::Miss,
        );
        {
            let mut slot = flight.result.lock().map_err(|_| MetadataError::Poisoned)?;
            *slot = Some(result.clone());
            flight.ready.notify_all();
        }
        let mut state = self.state.lock().map_err(|_| MetadataError::Poisoned)?;
        state.active.remove(&key);
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
    ) -> Result<Arc<WorkspaceSnapshot>, MetadataError> {
        let command = MetadataCommandSpec::for_selection(selection, cargo);
        if !command.locked {
            return Err(MetadataError::LockedRequired);
        }
        let run = self.runner.run(&command)?;
        self.check_epoch(selection.epoch())?;
        let metadata = validate_metadata(&run.metadata, selection, &closure, &self.guard)?;
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
        Ok(Arc::new(snapshot))
    }

    fn cache_fingerprint(
        &self,
        selection: &WorkspaceSelection,
        closure: &DependencyClosure,
    ) -> Option<[u8; 32]> {
        let mut inputs = BTreeMap::new();
        if closure.manifests.len() > MAX_METADATA_INPUT_FILES {
            return None;
        }
        for manifest in &closure.manifests {
            let bytes = match self.read_cache_input(selection, manifest) {
                Ok(Some(bytes)) => bytes,
                Ok(None) | Err(_) => return None,
            };
            inputs.insert(manifest.clone(), Some(bytes));
        }

        let mut current = selection.package_root().to_owned();
        let mut workspace_root = None;
        let mut reached_authority = false;
        for _ in 0..=MAX_METADATA_CONFIG_DEPTH {
            if !selection.authority().contains(&current) {
                return None;
            }
            let manifest_path = current.join("Cargo.toml");
            if !inputs.contains_key(&manifest_path) {
                let bytes = match self.read_cache_input(selection, &manifest_path) {
                    Ok(bytes) => bytes,
                    Err(_) => return None,
                };
                inputs.insert(manifest_path.clone(), bytes);
            }
            if workspace_root.is_none()
                && let Some(Some(bytes)) = inputs.get(&manifest_path)
            {
                let Ok(value) = parse_manifest(&manifest_path, bytes) else {
                    return None;
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
                    let bytes = match self.read_cache_input(selection, &config) {
                        Ok(bytes) => bytes,
                        Err(_) => return None,
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
            return None;
        }

        let lock_root = workspace_root.unwrap_or_else(|| selection.package_root().to_owned());
        let lock_path = lock_root.join("Cargo.lock");
        if let Entry::Vacant(entry) = inputs.entry(lock_path.clone()) {
            let bytes = match self.read_cache_input(selection, &lock_path) {
                Ok(bytes) => bytes,
                Err(_) => return None,
            };
            entry.insert(bytes);
        }
        if inputs.len() > MAX_METADATA_INPUT_FILES {
            return None;
        }

        let mut hasher = Sha256::new();
        hasher.update(b"agz-rust-coder-metadata-graph-v1");
        let mut total_bytes = 0u64;
        for (path, bytes) in inputs {
            hasher.update(b"path");
            hash_metadata_path(&mut hasher, &path);
            match bytes {
                Some(bytes) => {
                    let length = u64::try_from(bytes.len()).ok()?;
                    total_bytes = total_bytes.checked_add(length)?;
                    if total_bytes > MAX_METADATA_INPUT_BYTES {
                        return None;
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
        Some(hasher.finalize().into())
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
        if selection.authority().contains(directory) {
            return selection
                .authority()
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

fn wait_for_flight(
    flight: &Flight,
) -> Result<Result<Arc<WorkspaceSnapshot>, MetadataError>, MetadataError> {
    let mut result = flight.result.lock().map_err(|_| MetadataError::Poisoned)?;
    while result.is_none() {
        result = flight
            .ready
            .wait(result)
            .map_err(|_| MetadataError::Poisoned)?;
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

fn canonical_manifest_directory(
    root: &AuthorizedRoot,
    path: &Path,
) -> Result<PathBuf, MetadataError> {
    let directory = root.authorize_dir(path)?;
    Ok(directory.path().to_owned())
}

fn canonical_metadata_workspace_root(
    metadata: &Metadata,
    selection: &WorkspaceSelection,
) -> Result<PathBuf, MetadataError> {
    let workspace_root = PathBuf::from(metadata.workspace_root.as_std_path());
    if !selection.authority().contains(&workspace_root) {
        return Err(MetadataError::WorkspaceRootOutside(workspace_root));
    }
    let root = selection.authority().authorize_dir(&workspace_root)?;
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
