//! Revision-bound changeset service.
//!
//! Source reads happen only during `create`, from the authorized original
//! workspace. Every later action operates on the server-owned candidate copy.
//! Candidate validation runs through the existing [`CheckService`] with a
//! dedicated `RootGuard` and an isolated server-owned Cargo target directory,
//! so a build never touches the original workspace or target directory.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{ChangeConfig, Config, GateCache},
    gate::{
        GateEvidence, GateRequest, GateStatus, ProgressCallback, SuggestionApplicability,
        SuggestionPackage,
    },
    process::ProcessSupervisor,
    tools::CheckService,
    workspace::{
        AuthorizedRoot, ClientRoots, MetadataService, RootGuard, WorkspaceRoot,
        metadata::MetadataControl, select_in_root,
    },
};

use super::capture::{
    CaptureError, CaptureLimits, CaptureManifest, capture_tree, manifest_hash,
    relative_path_string, sha256_hex,
};
use super::migrate::{
    AnalyzeRequest, AnalyzerError, MigrationAnalyzer, PlanBudgets, compose_patches, plan_migration,
};
use super::model::{
    CHANGE_ID_PREFIX, CHANGE_SCHEMA_VERSION, CaptureSummary, ChangeAction, ChangeCaptureData,
    ChangeData, ChangeDiagnosticData, ChangeEvidenceData, ChangeNewFileData, ChangeOutcome,
    ChangePatchData, ChangeRecord, ChangeRequest, ChangeSourceHashData,
    ChangeSuggestionPackageData, ChangeSuggestionPatchData, MAX_CHANGED_FILES_IN_RECORD,
    MAX_COMMAND_CHARS, MAX_LISTED_CHANGED_FILES, MAX_LISTED_EVIDENCE, MAX_LISTED_HASHES,
    MAX_MIGRATION_OBLIGATIONS, MAX_RECORDED_EVIDENCE, MigrationReportData, NewFileInput,
    PatchInput, RecordState, StoredEvidence, StoredHash, StoredNewFile, StoredPatch,
};
use super::patch::{CandidateLimits, apply_plan, plan_patches};
use super::runtime::{
    RuntimeSnapshotPair, SnapshotError, copy_tree_bounded, expected_baseline_files,
    expected_candidate_files, reverse_apply, verify_and_digest,
};
use super::store::{
    ChangeStore, MAX_CLEANUP_WARNINGS, is_valid_change_id, normalize_lexical, now_ms, truncate,
};

const LOCK_STRIPES: usize = 64;

/// Per-row bounds for compiler feedback persisted in the change record. The
/// package itself comes from `machine_applicable_package_authorized`; these
/// limits only bound how much of it a single evidence row keeps so a record
/// cannot grow without limit. Overruns surface through `truncated` and the
/// `*Total` counters instead of failing the action.
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 2_048;
const MAX_LISTED_SUGGESTION_PATCHES: usize = 32;
const MAX_LISTED_SUGGESTION_SKIPPED: usize = 32;
const MAX_SUGGESTION_PATCH_BYTES: usize = 65_536;
const MAX_SUGGESTION_SKIPPED_CHARS: usize = 256;

static CHANGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Action-specific failure carrying cleanup warnings for the caller.
#[derive(Debug)]
struct ChangeFailure {
    status: &'static str,
    reason: String,
    warnings: Vec<String>,
}

impl ChangeFailure {
    fn new(status: &'static str, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            warnings: Vec::new(),
        }
    }
}

/// Bounded, read-only text snapshot of one revision-bound candidate copy.
///
/// Files are candidate-relative, deterministic, and only drawn from the
/// portable inclusion set. `truncated` is true whenever a file was left out for
/// a bound or safety reason; `skipped` records a bounded sample of those
/// decisions so a caller can surface the portability limit instead of guessing.
#[derive(Debug, Clone, Default)]
pub struct CandidateTree {
    pub files: BTreeMap<String, String>,
    pub skipped: Vec<CandidateTreeSkip>,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateTreeSkip {
    pub file: String,
    pub reason: String,
}

const MAX_TREE_FILES: usize = 2_048;
const MAX_TREE_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TREE_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TREE_DIR_ENTRIES: usize = 20_000;
const MAX_TREE_SKIPS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeFileDecision {
    Include,
    Sensitive,
    Ignore,
}

fn tree_file_decision(relative: &str) -> TreeFileDecision {
    let normalized = relative.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower = name.to_ascii_lowercase();
    if normalized == ".cargo/credentials"
        || normalized == ".cargo/credentials.toml"
        || lower == ".env"
        || lower == ".envrc"
        || has_extension(&lower, "pem")
        || has_extension(&lower, "key")
    {
        return TreeFileDecision::Sensitive;
    }
    if matches!(normalized.as_str(), ".cargo/config" | ".cargo/config.toml")
        || matches!(
            name,
            "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
        )
        || lower.starts_with("license")
        || lower.starts_with("copying")
        || lower.starts_with("notice")
    {
        return TreeFileDecision::Include;
    }
    if has_extension(&normalized, "rs") {
        return TreeFileDecision::Include;
    }
    TreeFileDecision::Ignore
}

fn has_extension(path: &str, expected: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn push_tree_skip(tree: &mut CandidateTree, file: String, reason: &str) {
    if tree.skipped.len() < MAX_TREE_SKIPS {
        tree.skipped.push(CandidateTreeSkip {
            file,
            reason: reason.to_owned(),
        });
    }
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

pub struct ChangeService {
    config: Config,
    roots: Arc<RootGuard>,
    supervisor: ProcessSupervisor,
    store: ChangeStore,
    locks: Vec<Arc<AsyncMutex<()>>>,
    cleanup_warnings: Mutex<Vec<String>>,
    migration_analyzer: Option<Arc<dyn MigrationAnalyzer>>,
}

impl fmt::Debug for ChangeService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChangeService")
            .field("scratch", &self.store.root())
            .field("max_active", &self.store.max_active())
            .finish_non_exhaustive()
    }
}

impl ChangeService {
    /// Creates the service, sweeps orphan/expired scratch entries, and recovers
    /// scratch left behind by an interrupted server process.
    ///
    /// # Errors
    ///
    /// Returns a bounded reason when the scratch root cannot be created or is
    /// a symlink.
    pub fn new(
        config: Config,
        roots: Arc<RootGuard>,
        supervisor: ProcessSupervisor,
    ) -> Result<Self, String> {
        let store = ChangeStore::new(&config.change)?;
        let sweep = store.sweep();
        let recovery = store.recover_interrupted();
        let mut cleanup_warnings = Vec::new();
        for warning in sweep.warnings.iter().chain(recovery.warnings.iter()) {
            if cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
                cleanup_warnings.push(warning.clone());
            }
            tracing::warn!(warning = %warning, "change scratch sweep left cleanup residue");
        }
        if sweep.removed > 0 {
            tracing::info!(
                removed = sweep.removed,
                "change scratch sweep removed entries"
            );
        }
        if recovery.failed > 0 || recovery.removed > 0 {
            tracing::warn!(
                failed = recovery.failed,
                removed = recovery.removed,
                "change scratch recovery handled interrupted work"
            );
        }
        Ok(Self {
            config,
            roots,
            supervisor,
            store,
            locks: (0..LOCK_STRIPES)
                .map(|_| Arc::new(AsyncMutex::new(())))
                .collect(),
            cleanup_warnings: Mutex::new(cleanup_warnings),
            migration_analyzer: None,
        })
    }

    /// Attaches the semantic analyzer used by `action=migrate`. Without one,
    /// migrate returns a typed `ANALYZER_UNAVAILABLE` result and writes nothing.
    pub fn with_migration_analyzer(mut self, analyzer: Arc<dyn MigrationAnalyzer>) -> Self {
        self.migration_analyzer = Some(analyzer);
        self
    }

    pub fn scratch_root(&self) -> &Path {
        self.store.root()
    }

    /// Reads one candidate-relative regular source file after verifying that
    /// every recorded candidate byte still matches the current revision.
    ///
    /// This is a read-only helper for tools that need revision-bound candidate
    /// evidence (`repair`). The returned errors never include server-owned
    /// absolute paths.
    pub fn read_candidate_source(&self, id: &str, file: &str) -> Result<String, String> {
        if !is_valid_change_id(id) {
            return Err("changeId is not a valid server-issued id".to_owned());
        }
        let record = match self.store.load(id) {
            Ok(Some(record)) => record,
            Ok(None) => return Err("no change record exists".to_owned()),
            Err(_) => return Err("the change record could not be read".to_owned()),
        };
        if record.state != RecordState::Ready {
            return Err("the change is not ready".to_owned());
        }
        let relative = super::patch::normalize_relative(file)
            .map_err(|_| "the requested source path is not a valid candidate path".to_owned())?;
        verify_candidate_bytes(&self.store, id, &record)
            .map_err(|_| "the candidate bytes no longer match the recorded revision".to_owned())?;
        let path = self.store.candidate_dir(id).join(&relative);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "the requested candidate file is unavailable".to_owned())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("the requested candidate path is not a regular file".to_owned());
        }
        let bytes = fs::read(&path)
            .map_err(|_| "the requested candidate file could not be read".to_owned())?;
        if bytes.len() as u64 > crate::diagnostics::MAX_SOURCE_SNAPSHOT_BYTES {
            return Err("the requested candidate file exceeds the bounded snapshot".to_owned());
        }
        String::from_utf8(bytes).map_err(|_| "the candidate source is not valid UTF-8".to_owned())
    }

    /// Current recorded revision for a change id. Read-only: no scratch is
    /// created and the candidate copy is not touched.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::NotFound`] when the change id has no record.
    pub fn current_revision(&self, change_id: &str) -> Result<u64, SnapshotError> {
        if !is_valid_change_id(change_id) {
            return Err(SnapshotError::Invalid(
                "changeId is not a valid server-issued id".to_owned(),
            ));
        }
        match self.store.load(change_id) {
            Ok(Some(record)) => Ok(record.revision),
            Ok(None) => Err(SnapshotError::NotFound),
            Err(reason) => Err(SnapshotError::Invalid(reason)),
        }
    }

    /// Read-only, bounded snapshot of the revision-bound candidate copy.
    ///
    /// Only files a portable reproducer can need are returned: Rust sources,
    /// Cargo manifests and lockfile, toolchain pins, `.cargo` configuration, and
    /// provenance files. Credential, environment, version-control, and build
    /// output is never read. The snapshot is verified against the recorded
    /// revision before any byte is read, so it can never describe a different
    /// candidate than the one the failure evidence belongs to.
    #[allow(clippy::too_many_lines)]
    pub fn read_candidate_tree(&self, id: &str) -> Result<CandidateTree, String> {
        if !is_valid_change_id(id) {
            return Err("changeId is not a valid server-issued id".to_owned());
        }
        let record = match self.store.load(id) {
            Ok(Some(record)) => record,
            Ok(None) => return Err("no change record exists".to_owned()),
            Err(_) => return Err("the change record could not be read".to_owned()),
        };
        if record.state != RecordState::Ready {
            return Err("the change is not ready".to_owned());
        }
        verify_candidate_bytes(&self.store, id, &record)
            .map_err(|_| "the candidate bytes no longer match the recorded revision".to_owned())?;
        let candidate_root = self.store.candidate_dir(id);
        if fs::symlink_metadata(&candidate_root)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("the candidate copy is a symlink and will not be read".to_owned());
        }
        let mut tree = CandidateTree::default();
        let mut stack = vec![candidate_root.clone()];
        let mut visited = 0usize;
        while let Some(directory) = stack.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => {
                    tree.truncated = true;
                    push_tree_skip(
                        &mut tree,
                        relative_display(&candidate_root, &directory),
                        "the candidate directory could not be listed",
                    );
                    continue;
                }
            };
            for entry in entries.flatten() {
                visited = visited.saturating_add(1);
                if visited > MAX_TREE_DIR_ENTRIES {
                    tree.truncated = true;
                    return Ok(tree);
                }
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.file_type().is_symlink() {
                    tree.truncated = true;
                    push_tree_skip(
                        &mut tree,
                        relative_display(&candidate_root, &path),
                        "symlinked entries are never captured",
                    );
                    continue;
                }
                if metadata.is_dir() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == ".git" || name == "target" {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                let relative = relative_display(&candidate_root, &path);
                match tree_file_decision(&relative) {
                    TreeFileDecision::Ignore => {}
                    TreeFileDecision::Sensitive => {
                        push_tree_skip(
                            &mut tree,
                            relative,
                            "credential or environment files are never captured",
                        );
                    }
                    TreeFileDecision::Include => {
                        if tree.files.len() >= MAX_TREE_FILES {
                            tree.truncated = true;
                            push_tree_skip(&mut tree, relative, "file count bound was reached");
                            continue;
                        }
                        if metadata.len() > MAX_TREE_FILE_BYTES {
                            tree.truncated = true;
                            push_tree_skip(&mut tree, relative, "file exceeds the per-file bound");
                            continue;
                        }
                        if tree.total_bytes.saturating_add(metadata.len()) > MAX_TREE_TOTAL_BYTES {
                            tree.truncated = true;
                            push_tree_skip(
                                &mut tree,
                                relative,
                                "snapshot total byte bound was reached",
                            );
                            continue;
                        }
                        match fs::read(&path).and_then(|bytes| {
                            String::from_utf8(bytes)
                                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
                        }) {
                            Ok(text) => {
                                tree.total_bytes = tree.total_bytes.saturating_add(metadata.len());
                                tree.files.insert(relative, text);
                            }
                            Err(_) => {
                                tree.truncated = true;
                                push_tree_skip(
                                    &mut tree,
                                    relative,
                                    "the file is not valid UTF-8 text",
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(tree)
    }

    /// Full configuration shared with the change engine (verify reuses the
    /// candidate guard and isolated-cache contract).
    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    /// Bounded raw record for server-internal verification flows.
    pub(crate) fn record(&self, id: &str) -> Result<Option<ChangeRecord>, String> {
        self.store.load(id)
    }

    pub(crate) fn candidate_dir(&self, id: &str) -> PathBuf {
        self.store.candidate_dir(id)
    }

    pub(crate) fn cache_dir(&self, id: &str) -> PathBuf {
        self.store.cache_dir(id)
    }

    /// Executes one change action. `workspace` is the request-authorized root
    /// used only to capture the original or to compare the authorization epoch.
    pub async fn execute(
        &self,
        request: ChangeRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        progress: Option<ProgressCallback>,
    ) -> ChangeOutcome {
        match request.action {
            ChangeAction::Create => self.create(workspace, cancellation).await,
            ChangeAction::Stage => self.stage(request, workspace, cancellation).await,
            ChangeAction::Migrate => {
                Box::pin(self.migrate(request, workspace, cancellation, progress)).await
            }
            ChangeAction::Inspect => self.inspect(request),
            ChangeAction::Validate => {
                self.validate(request, workspace, cancellation, progress)
                    .await
            }
            ChangeAction::Export => self.export(request, workspace),
            ChangeAction::Discard => self.discard(request).await,
        }
    }

    fn stripe(&self, id: &str) -> Arc<AsyncMutex<()>> {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in id.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        let index = usize::try_from(hash % LOCK_STRIPES as u64).unwrap_or(0);
        Arc::clone(&self.locks[index])
    }

    fn take_cleanup_warnings(&self) -> Vec<String> {
        self.cleanup_warnings
            .lock()
            .map(|warnings| warnings.clone())
            .unwrap_or_default()
    }

    fn push_cleanup_warnings(&self, warnings: &[String]) {
        if warnings.is_empty() {
            return;
        }
        if let Ok(mut current) = self.cleanup_warnings.lock() {
            for warning in warnings {
                if current.len() >= MAX_CLEANUP_WARNINGS {
                    break;
                }
                if !current.contains(warning) {
                    current.push(warning.clone());
                }
            }
        }
    }

    fn error_outcome(
        &self,
        action: ChangeAction,
        status: &'static str,
        summary: impl Into<String>,
        reason: impl Into<String>,
        change_id: Option<&str>,
    ) -> ChangeOutcome {
        let mut data = ChangeData {
            action: action.as_str().to_owned(),
            change_id: change_id.map(str::to_owned),
            state: "absent".to_owned(),
            ..ChangeData::default()
        };
        data.reason = reason.into();
        data.cleanup_warnings = self.take_cleanup_warnings();
        ChangeOutcome {
            status,
            summary: summary.into(),
            is_error: true,
            data,
        }
    }

    fn finish(
        &self,
        status: &'static str,
        summary: impl Into<String>,
        is_error: bool,
        mut data: ChangeData,
    ) -> ChangeOutcome {
        let mut warnings = self.take_cleanup_warnings();
        for warning in &data.cleanup_warnings {
            if !warnings.contains(warning) && warnings.len() < MAX_CLEANUP_WARNINGS {
                warnings.push(warning.clone());
            }
        }
        data.cleanup_warnings = warnings;
        ChangeOutcome {
            status,
            summary: summary.into(),
            is_error,
            data,
        }
    }

    /// Re-reads the pinned record while the per-change file lock is held.
    ///
    /// `validate` and `export` read the record before taking the lock so that a
    /// missing or terminal change is refused without creating scratch. A stage
    /// in another server process can publish a new revision in that window, so
    /// the state candidate bytes are verified against must be re-read and
    /// compared with the pinned record under the lock.
    fn reload_pinned_record(
        &self,
        action: ChangeAction,
        id: &str,
        pinned: &ChangeRecord,
    ) -> Result<ChangeRecord, Box<ChangeOutcome>> {
        let record = match self.store.load(id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(Box::new(self.error_outcome(
                    action,
                    "NOT_FOUND",
                    "The change id has no server-owned scratch.",
                    "no change record exists",
                    Some(id),
                )));
            }
            Err(reason) => {
                return Err(Box::new(self.error_outcome(
                    action,
                    "INVALID",
                    "The change record could not be read.",
                    reason,
                    Some(id),
                )));
            }
        };
        let unchanged = record.revision == pinned.revision
            && record.state == pinned.state
            && record.base_identity == pinned.base_identity
            && record.workspace_epoch == pinned.workspace_epoch
            && record.applying_revision == pinned.applying_revision;
        if unchanged {
            return Ok(record);
        }
        let status = match record.state {
            RecordState::FailedInconsistent => "FAILED_INCONSISTENT",
            RecordState::Discarded => "DISCARDED",
            RecordState::Applying | RecordState::Capturing => "FAILED_INCONSISTENT",
            RecordState::Ready => "STALE",
        };
        let reason = format!(
            "the change moved from {} revision {} to {} revision {} before its bytes could be verified",
            pinned.state.as_str(),
            pinned.revision,
            record.state.as_str(),
            record.revision
        );
        Err(Box::new(self.finish(
            status,
            "The change changed before verification; no evidence was recorded.",
            true,
            refused_data(action, &record, reason),
        )))
    }

    fn failure_outcome(
        &self,
        action: ChangeAction,
        change_id: Option<&str>,
        failure: ChangeFailure,
    ) -> ChangeOutcome {
        self.push_cleanup_warnings(&failure.warnings);
        let mut data = ChangeData {
            action: action.as_str().to_owned(),
            change_id: change_id.map(str::to_owned),
            state: "absent".to_owned(),
            ..ChangeData::default()
        };
        data.reason = failure.reason;
        let summary = match failure.status {
            "INCOMPLETE_INPUTS" => "The workspace could not be captured completely.",
            "CANCELLED" => "The action was cancelled before completion.",
            "TIMEOUT" => "The action timed out before completion.",
            "RESOURCE_BLOCKED" => "The active change limit was reached.",
            "PATCH_REJECTED" => "No patch was applied; a patch failed pre-validation.",
            "STALE" => "The change revision/base identity is stale.",
            "FAILED_INCONSISTENT" => {
                "The candidate copy is inconsistent and further work is refused."
            }
            _ => "The change action did not complete.",
        };
        let is_error = true;
        self.finish(failure.status, summary, is_error, data)
    }

    async fn create(
        &self,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
    ) -> ChangeOutcome {
        if self.store.active_count() >= self.store.max_active() {
            return self.error_outcome(
                ChangeAction::Create,
                "RESOURCE_BLOCKED",
                "The active change limit was reached.",
                format!(
                    "at most {} active changes are allowed; discard or let one expire",
                    self.store.max_active()
                ),
                None,
            );
        }
        let roots = Arc::clone(&self.roots);
        let config = self.config.change.clone();
        let cargo = crate::tools::check::resolve_cargo(self.config.cargo.path.as_deref());
        let store = self.store.clone();
        let workspace = workspace.clone();
        let cancellation_for_task = cancellation.clone();
        let deadline = Instant::now() + Duration::from_millis(self.config.gate.hard_timeout_ms);
        let control = MetadataControl::new(
            deadline,
            cancellation.clone(),
            self.supervisor.clone(),
            tokio::runtime::Handle::current(),
        );
        let joined = tokio::task::spawn_blocking(move || {
            create_blocking(
                roots,
                &config,
                &cargo,
                &store,
                &workspace,
                &cancellation_for_task,
                &control,
            )
        })
        .await;
        match joined {
            Ok(Ok(record)) => {
                let data = data_for_record(ChangeAction::Create, &record, None);
                self.finish(
                    "CREATED",
                    "Captured the complete workspace into server-owned scratch.",
                    false,
                    data,
                )
            }
            Ok(Err(failure)) => self.failure_outcome(ChangeAction::Create, None, failure),
            Err(error) => self.error_outcome(
                ChangeAction::Create,
                "UNAVAILABLE",
                "The bounded capture worker did not complete.",
                error.to_string(),
                None,
            ),
        }
    }

    async fn stage(
        &self,
        request: ChangeRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
    ) -> ChangeOutcome {
        let Some(id) = request.change_id.clone() else {
            return self.error_outcome(
                ChangeAction::Stage,
                "INVALID",
                "action=stage requires changeId.",
                "changeId is required",
                None,
            );
        };
        if !is_valid_change_id(&id) {
            return self.error_outcome(
                ChangeAction::Stage,
                "INVALID",
                "The change id is not valid.",
                "changeId is not a valid server-issued id",
                Some(&id),
            );
        }
        let _stripe = self.stripe(&id).lock_owned().await;
        let store = self.store.clone();
        let config = self.config.change.clone();
        let expected = request.expected_revision;
        let base = request.base_identity.clone();
        let patches = request.patches.clone();
        let new_files = request.new_files.clone();
        let cancellation_for_task = cancellation.clone();
        let id_for_task = id.clone();
        let workspace_epoch = workspace.epoch();
        let joined = tokio::task::spawn_blocking(move || {
            stage_blocking(
                &store,
                &config,
                &id_for_task,
                workspace_epoch,
                expected,
                base.as_deref(),
                &patches,
                &new_files,
                &cancellation_for_task,
            )
        })
        .await;
        match joined {
            Ok(Ok(record)) => {
                let data = data_for_record(ChangeAction::Stage, &record, None);
                self.finish(
                    "STAGED",
                    "Patches were validated and applied to the candidate copy.",
                    false,
                    data,
                )
            }
            Ok(Err(failure)) => self.failure_outcome(ChangeAction::Stage, Some(&id), failure),
            Err(error) => self.error_outcome(
                ChangeAction::Stage,
                "UNAVAILABLE",
                "The bounded patch worker did not complete.",
                error.to_string(),
                Some(&id),
            ),
        }
    }

    /// Plans a structural migration from advisory analysis, applies it to the
    /// candidate copy, and validates the new revision with real Cargo.
    async fn migrate(
        &self,
        request: ChangeRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        progress: Option<ProgressCallback>,
    ) -> ChangeOutcome {
        let Some(id) = request.change_id.clone() else {
            return self.error_outcome(
                ChangeAction::Migrate,
                "INVALID",
                "action=migrate requires changeId.",
                "changeId is required",
                None,
            );
        };
        if !is_valid_change_id(&id) {
            return self.error_outcome(
                ChangeAction::Migrate,
                "INVALID",
                "The change id is not valid.",
                "changeId is not a valid server-issued id",
                Some(&id),
            );
        }
        let Some(migration) = request.migration.clone() else {
            return self.error_outcome(
                ChangeAction::Migrate,
                "INVALID",
                "action=migrate requires anchor and transformation.",
                "migration inputs are required",
                Some(&id),
            );
        };
        if let Some(scope) = migration.consumer_scope.as_deref()
            && scope != "workspace"
        {
            return self.error_outcome(
                ChangeAction::Migrate,
                "MIGRATION_REFUSED",
                "Only workspace-wide consumer migration is supported.",
                format!("consumerScope '{scope}' is unsupported; only 'workspace' is available"),
                Some(&id),
            );
        }
        let Some(analyzer) = self.migration_analyzer.clone() else {
            return self.error_outcome(
                ChangeAction::Migrate,
                "ANALYZER_UNAVAILABLE",
                "Migration analysis requires a configured semantic analyzer.",
                "no migration analyzer is configured; rust-analyzer analysis is unavailable",
                Some(&id),
            );
        };
        let Some(expected_revision) = request.expected_revision else {
            return self.error_outcome(
                ChangeAction::Migrate,
                "INVALID",
                "action=migrate requires expectedRevision.",
                "expectedRevision is required",
                Some(&id),
            );
        };
        let Some(base_identity) = request.base_identity.clone() else {
            return self.error_outcome(
                ChangeAction::Migrate,
                "INVALID",
                "action=migrate requires baseIdentity.",
                "baseIdentity is required",
                Some(&id),
            );
        };

        // Plan and apply under the change lock. The analyzer runs against the
        // server-owned candidate copy, never the original workspace. The
        // planning future is boxed so it does not inflate the server future.
        let prepared = {
            let _stripe = self.stripe(&id).lock_owned().await;
            let planning = self.migrate_prepare(
                &id,
                &migration,
                analyzer,
                expected_revision,
                &base_identity,
                workspace,
                &cancellation,
            );
            Box::pin(planning).await
        };
        let (record, report) = match prepared {
            Ok(prepared) => prepared,
            Err(outcome) => return *outcome,
        };

        // Real Cargo validation on the transformed candidate revision.
        let validate_request = ChangeRequest {
            action: ChangeAction::Validate,
            change_id: Some(id.clone()),
            expected_revision: Some(record.revision),
            base_identity: Some(record.base_identity.clone()),
            patches: Vec::new(),
            new_files: Vec::new(),
            migration: None,
            target: request.target,
            options: request.options.clone(),
            detail: request.detail,
            timings: request.timings,
        };
        let validation = {
            let validating = self.validate(validate_request, workspace, cancellation, progress);
            Box::pin(validating).await
        };
        let mut data = validation.data;
        data.action = ChangeAction::Migrate.as_str().to_owned();
        data.migration = Some(report.clone());
        let complete = report.complete;
        let (status, is_error, summary) = if complete {
            match validation.status {
                "PASS" => (
                    "MIGRATED",
                    false,
                    "The candidate revision was migrated and passed real Cargo validation.",
                ),
                "FAIL" => (
                    "MIGRATION_FAILED",
                    true,
                    "The migrated candidate failed real Cargo validation.",
                ),
                _ => (
                    "MIGRATION_INCONCLUSIVE",
                    true,
                    "The migrated candidate could not be validated to a terminal result.",
                ),
            }
        } else {
            (
                "MIGRATION_PARTIAL",
                true,
                "The migration is incomplete; unresolved sites and budget omissions are reported.",
            )
        };
        data.reason = format!(
            "{} gate={} obligations={} complete={}",
            data.reason, validation.status, report.obligations_total, complete
        );
        self.finish(status, summary, is_error, data)
    }

    #[allow(clippy::too_many_arguments)]
    async fn migrate_prepare(
        &self,
        id: &str,
        migration: &super::model::MigrateRequest,
        analyzer: Arc<dyn MigrationAnalyzer>,
        expected_revision: u64,
        base_identity: &str,
        workspace: &WorkspaceRoot,
        cancellation: &CancellationToken,
    ) -> Result<(ChangeRecord, MigrationReportData), Box<ChangeOutcome>> {
        let _file_lock = match self.store.try_lock(id) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Err(Box::new(self.error_outcome(
                    ChangeAction::Migrate,
                    "CHANGE_BUSY",
                    "The change scratch is locked by another process.",
                    "another server process holds this change lock",
                    Some(id),
                )));
            }
            Err(reason) => {
                return Err(Box::new(self.error_outcome(
                    ChangeAction::Migrate,
                    "UNAVAILABLE",
                    "The change lock could not be opened.",
                    reason,
                    Some(id),
                )));
            }
        };
        let Some(record) = (match self.store.load(id) {
            Ok(record) => record,
            Err(reason) => {
                return Err(Box::new(self.error_outcome(
                    ChangeAction::Migrate,
                    "INVALID",
                    "The change record could not be read.",
                    reason,
                    Some(id),
                )));
            }
        }) else {
            return Err(Box::new(self.error_outcome(
                ChangeAction::Migrate,
                "NOT_FOUND",
                "The change id has no server-owned scratch.",
                "no change record exists",
                Some(id),
            )));
        };
        match record.state {
            RecordState::Ready => {}
            RecordState::FailedInconsistent | RecordState::Applying | RecordState::Capturing => {
                return Err(Box::new(self.finish(
                    "FAILED_INCONSISTENT",
                    "An inconsistent candidate cannot be migrated.",
                    true,
                    refused_data(
                        ChangeAction::Migrate,
                        &record,
                        "the candidate may be partially applied; migration is refused",
                    ),
                )));
            }
            RecordState::Discarded => {
                return Err(Box::new(self.finish(
                    "DISCARDED",
                    "The change scratch was discarded.",
                    true,
                    refused_data(
                        ChangeAction::Migrate,
                        &record,
                        "the candidate copy was discarded",
                    ),
                )));
            }
        }
        if record.workspace_epoch != workspace.epoch() {
            return Err(Box::new(self.finish(
                "STALE",
                "The authorization epoch changed; migration is refused.",
                true,
                refused_data(
                    ChangeAction::Migrate,
                    &record,
                    format!(
                        "authorization root epoch changed from {} to {}",
                        record.workspace_epoch,
                        workspace.epoch()
                    ),
                ),
            )));
        }
        if expected_revision != record.revision {
            return Err(Box::new(self.finish(
                "STALE",
                "The requested revision is not current; nothing was migrated.",
                true,
                refused_data(
                    ChangeAction::Migrate,
                    &record,
                    format!(
                        "expectedRevision {expected_revision} does not match current revision {}",
                        record.revision
                    ),
                ),
            )));
        }
        if base_identity != record.base_identity {
            return Err(Box::new(self.finish(
                "STALE",
                "The requested base identity is stale; nothing was migrated.",
                true,
                refused_data(
                    ChangeAction::Migrate,
                    &record,
                    "baseIdentity does not match the captured base",
                ),
            )));
        }
        if cancellation.is_cancelled() {
            return Err(Box::new(self.finish(
                "CANCELLED",
                "Migration was cancelled before analysis started.",
                true,
                refused_data(
                    ChangeAction::Migrate,
                    &record,
                    "the migration request was cancelled before analysis",
                ),
            )));
        }
        if record.revision.saturating_add(1) > self.config.change.max_revisions {
            return Err(Box::new(self.error_outcome(
                ChangeAction::Migrate,
                "REVISION_LIMIT",
                "The change reached its configured revision limit.",
                format!(
                    "the change reached the configured revision limit {}",
                    self.config.change.max_revisions
                ),
                Some(id),
            )));
        }
        let candidate_root = self.store.candidate_dir(id);
        if fs::symlink_metadata(&candidate_root)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(Box::new(self.error_outcome(
                ChangeAction::Migrate,
                "FAILED_INCONSISTENT",
                "The candidate copy is a symlink and will not be analyzed.",
                "candidate copy was replaced by a symlink",
                Some(id),
            )));
        }
        if let Err(reason) = verify_candidate_bytes(&self.store, id, &record) {
            return Err(Box::new(self.finish(
                "FAILED_INCONSISTENT",
                "The candidate bytes do not match the recorded revision; nothing was migrated.",
                true,
                refused_data(ChangeAction::Migrate, &record, reason),
            )));
        }
        let max_references = u64::from(
            migration
                .constraints
                .max_references
                .unwrap_or(super::model::MAX_MIGRATION_REFERENCES as u32),
        );
        let max_identity_checks = u64::from(
            migration
                .constraints
                .max_identity_checks
                .unwrap_or(super::model::MAX_MIGRATION_IDENTITY_CHECKS as u32),
        );
        let analysis = analyzer
            .analyze(AnalyzeRequest {
                root: candidate_root.clone(),
                anchor_file: migration.anchor.file.clone(),
                anchor_symbol: migration.anchor.symbol.clone(),
                anchor_line: migration.anchor.line,
                max_references,
                max_identity_checks,
                timeout: Duration::from_millis(self.config.rust_analyzer.timeout_ms),
            })
            .await;
        let analysis = match analysis {
            Ok(analysis) => analysis,
            Err(error) => {
                let (status, summary) = match error {
                    AnalyzerError::Unavailable(_) | AnalyzerError::Unsupported(_) => (
                        "ANALYZER_UNAVAILABLE",
                        "Migration analysis is unavailable; nothing was migrated.",
                    ),
                    AnalyzerError::NotFound(_) | AnalyzerError::Ambiguous(_) => (
                        "MIGRATION_REFUSED",
                        "The anchor could not be resolved unambiguously; nothing was migrated.",
                    ),
                    AnalyzerError::Invalid(_) => (
                        "MIGRATION_REFUSED",
                        "The migration analysis input was rejected; nothing was migrated.",
                    ),
                };
                return Err(Box::new(self.error_outcome(
                    ChangeAction::Migrate,
                    status,
                    summary,
                    error.to_string(),
                    Some(id),
                )));
            }
        };
        let read_root = candidate_root.clone();
        let read_file = move |file: &str| -> Option<String> {
            let relative = super::patch::normalize_relative(file).ok()?;
            crate::tools::symbol::read_workspace_file(&read_root, &read_root.join(relative))
        };
        let mut plan = plan_migration(
            migration,
            &analysis,
            &read_file,
            PlanBudgets::from_request(migration),
        );
        if let Some((status, reason)) = plan.refused.take() {
            return Err(Box::new(self.error_outcome(
                ChangeAction::Migrate,
                status,
                "The migration plan was refused before touching the candidate.",
                reason,
                Some(id),
            )));
        }
        let (patches, compose_obligations) = compose_patches(&plan.edits, &read_file);
        if !compose_obligations.is_empty() {
            let extra = u64::try_from(compose_obligations.len()).unwrap_or(u64::MAX);
            plan.report.obligations_total = plan.report.obligations_total.saturating_add(extra);
            let room = MAX_MIGRATION_OBLIGATIONS.saturating_sub(plan.report.obligations.len());
            plan.report
                .obligations
                .extend(compose_obligations.into_iter().take(room));
            plan.report.complete = false;
            plan.report.notes.push(
                "one or more planned edits could not be composed into candidate patches".to_owned(),
            );
        }
        if patches.is_empty() {
            let status = if plan.report.complete {
                "MIGRATION_NOOP"
            } else {
                "MIGRATION_PARTIAL"
            };
            let mut data = data_for_record(ChangeAction::Migrate, &record, None);
            data.migration = Some(plan.report.clone());
            data.reason = if plan.report.complete {
                "the plan produced no candidate edits".to_owned()
            } else {
                format!(
                    "the plan produced no applicable edits and {} obligation(s) remain",
                    plan.report.obligations_total
                )
            };
            let is_error = !plan.report.complete;
            return Err(Box::new(self.finish(
                status,
                if is_error {
                    "No edit was applied because the migration is incomplete."
                } else {
                    "No candidate edit was required."
                },
                is_error,
                data,
            )));
        }
        let mut record = match stage_locked(
            &self.store,
            &self.config.change,
            record,
            Some(expected_revision),
            Some(base_identity),
            &patches,
            &[],
            cancellation,
        ) {
            Ok(record) => record,
            Err(failure) => {
                return Err(Box::new(self.failure_outcome(
                    ChangeAction::Migrate,
                    Some(id),
                    failure,
                )));
            }
        };
        let mut report = plan.report;
        report.candidate_revision = record.revision;
        record.migration = Some(report.clone());
        if let Err(reason) = self.store.save(&mut record) {
            self.push_cleanup_warnings(&[format!(
                "the migration report could not be persisted: {reason}"
            )]);
        }
        Ok((record, report))
    }

    fn inspect(&self, request: ChangeRequest) -> ChangeOutcome {
        let Some(id) = request.change_id.clone() else {
            return self.error_outcome(
                ChangeAction::Inspect,
                "INVALID",
                "action=inspect requires changeId.",
                "changeId is required",
                None,
            );
        };
        let record = match self.store.load(&id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return self.error_outcome(
                    ChangeAction::Inspect,
                    "NOT_FOUND",
                    "The change id has no server-owned scratch.",
                    "no change record exists",
                    Some(&id),
                );
            }
            Err(reason) => {
                return self.error_outcome(
                    ChangeAction::Inspect,
                    "INVALID",
                    "The change record could not be read.",
                    reason,
                    Some(&id),
                );
            }
        };
        let data = data_for_record(ChangeAction::Inspect, &record, None);
        self.finish(
            "INSPECTED",
            "Returned the bounded change record.",
            false,
            data,
        )
    }

    async fn validate(
        &self,
        request: ChangeRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        progress: Option<ProgressCallback>,
    ) -> ChangeOutcome {
        let Some(id) = request.change_id.clone() else {
            return self.error_outcome(
                ChangeAction::Validate,
                "INVALID",
                "action=validate requires changeId.",
                "changeId is required",
                None,
            );
        };
        if !is_valid_change_id(&id) {
            return self.error_outcome(
                ChangeAction::Validate,
                "INVALID",
                "The change id is not valid.",
                "changeId is not a valid server-issued id",
                Some(&id),
            );
        }
        let _stripe = self.stripe(&id).lock_owned().await;
        let record = match self.store.load(&id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return self.error_outcome(
                    ChangeAction::Validate,
                    "NOT_FOUND",
                    "The change id has no server-owned scratch.",
                    "no change record exists",
                    Some(&id),
                );
            }
            Err(reason) => {
                return self.error_outcome(
                    ChangeAction::Validate,
                    "INVALID",
                    "The change record could not be read.",
                    reason,
                    Some(&id),
                );
            }
        };
        match record.state {
            RecordState::FailedInconsistent => {
                return self.finish(
                    "FAILED_INCONSISTENT",
                    "A failed partial apply cannot be validated.",
                    true,
                    refused_data(
                        ChangeAction::Validate,
                        &record,
                        "the candidate may be partially applied; validation is refused",
                    ),
                );
            }
            RecordState::Discarded => {
                return self.finish(
                    "DISCARDED",
                    "The change scratch was discarded.",
                    true,
                    refused_data(
                        ChangeAction::Validate,
                        &record,
                        "the candidate copy was discarded",
                    ),
                );
            }
            RecordState::Applying | RecordState::Capturing => {
                let reason = format!(
                    "the change is still {}; no completed revision was recorded",
                    record.state.as_str()
                );
                return self.finish(
                    "FAILED_INCONSISTENT",
                    "An unrecorded candidate state cannot be validated.",
                    true,
                    refused_data(ChangeAction::Validate, &record, reason),
                );
            }
            RecordState::Ready => {}
        }
        // Pinning is required for validate: the returned evidence must identify
        // the exact revision and base it was produced for.
        let Some(expected_revision) = request.expected_revision else {
            return self.error_outcome(
                ChangeAction::Validate,
                "INVALID",
                "action=validate requires expectedRevision.",
                "expectedRevision is required",
                Some(&id),
            );
        };
        if expected_revision != record.revision {
            let reason = format!(
                "expectedRevision {} does not match current revision {}",
                expected_revision, record.revision
            );
            return self.finish(
                "STALE",
                "The requested revision is not current; no fresh evidence was recorded.",
                true,
                refused_data(ChangeAction::Validate, &record, reason),
            );
        }
        let Some(base_identity) = request.base_identity.as_deref() else {
            return self.error_outcome(
                ChangeAction::Validate,
                "INVALID",
                "action=validate requires baseIdentity.",
                "baseIdentity is required",
                Some(&id),
            );
        };
        if base_identity != record.base_identity {
            return self.finish(
                "STALE",
                "The requested base identity is stale; no fresh evidence was recorded.",
                true,
                refused_data(
                    ChangeAction::Validate,
                    &record,
                    "baseIdentity does not match the captured base",
                ),
            );
        }
        if record.workspace_epoch != workspace.epoch() {
            let reason = format!(
                "authorization root epoch changed from {} to {}; no fresh evidence was recorded",
                record.workspace_epoch,
                workspace.epoch()
            );
            return self.finish(
                "STALE",
                "The authorization epoch changed; evidence is not usable.",
                true,
                refused_data(ChangeAction::Validate, &record, reason),
            );
        }
        if cancellation.is_cancelled() {
            return self.finish(
                "CANCELLED",
                "Validation was cancelled before any Cargo process started.",
                true,
                refused_data(
                    ChangeAction::Validate,
                    &record,
                    "the validation request was cancelled before it started",
                ),
            );
        }
        let _file_lock = match self.store.try_lock(&id) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return self.finish(
                    "CHANGE_BUSY",
                    "The change scratch is locked by another process.",
                    true,
                    refused_data(
                        ChangeAction::Validate,
                        &record,
                        "another server process holds this change lock",
                    ),
                );
            }
            Err(reason) => {
                return self.error_outcome(
                    ChangeAction::Validate,
                    "UNAVAILABLE",
                    "The change lock could not be opened.",
                    reason,
                    Some(&id),
                );
            }
        };
        // The record was read before the file lock; a stage in another server
        // process may have published a new revision since. Re-read under the
        // lock so verification and evidence bind to the locked revision.
        let record = match self.reload_pinned_record(ChangeAction::Validate, &id, &record) {
            Ok(record) => record,
            Err(outcome) => return *outcome,
        };

        let candidate_root = self.store.candidate_dir(&id);
        if fs::symlink_metadata(&candidate_root)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return self.error_outcome(
                ChangeAction::Validate,
                "FAILED_INCONSISTENT",
                "The candidate copy is a symlink and will not be compiled.",
                "candidate copy was replaced by a symlink",
                Some(&id),
            );
        }
        if record.applying_revision.is_some() {
            return self.finish(
                "FAILED_INCONSISTENT",
                "An interrupted stage cannot be validated.",
                true,
                refused_data(
                    ChangeAction::Validate,
                    &record,
                    "an interrupted stage left the candidate state unrecorded",
                ),
            );
        }
        let mut activity = record.clone();
        if let Err(error) = self.store.save(&mut activity) {
            self.push_cleanup_warnings(&[error]);
        }
        if let Err(reason) = verify_candidate_bytes(&self.store, &id, &record) {
            let mut failed = record.clone();
            failed.state = RecordState::FailedInconsistent;
            failed.applying_revision = None;
            failed
                .evidence
                .iter_mut()
                .for_each(StoredEvidence::supersede);
            if failed.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
                failed.cleanup_warnings.push(truncate(&reason, 512));
            }
            let _ = self.store.save(&mut failed);
            return self.finish(
                "FAILED_INCONSISTENT",
                "The candidate bytes do not match the recorded revision; no evidence was recorded.",
                true,
                refused_data(ChangeAction::Validate, &failed, reason),
            );
        }
        for root in &record.external_paths {
            if !root.is_dir() {
                return self.error_outcome(
                    ChangeAction::Validate,
                    "UNVERIFIABLE_DEPENDENCY",
                    "An authorized external path dependency disappeared.",
                    format!(
                        "external path dependency {} is no longer an existing directory",
                        root.display()
                    ),
                    Some(&id),
                );
            }
        }
        let mut dependency_roots = record
            .dependency_roots
            .iter()
            .filter(|root| root.is_dir())
            .cloned()
            .collect::<Vec<_>>();
        for root in &record.external_paths {
            if !dependency_roots.contains(root) {
                dependency_roots.push(root.clone());
            }
        }
        let guard = match RootGuard::new([candidate_root.clone()], dependency_roots) {
            Ok(guard) => Arc::new(guard),
            Err(error) => {
                return self.error_outcome(
                    ChangeAction::Validate,
                    "UNVERIFIABLE_DEPENDENCY",
                    "External path dependencies could not be authorized for the candidate.",
                    format!("candidate root guard failed: {error}"),
                    Some(&id),
                );
            }
        };
        let mut config = self.config.clone();
        config.gate.cache = GateCache::Isolated;
        config.gate.cache_dir = self.store.cache_dir(&id);
        let service = CheckService::new(config, Arc::clone(&guard));
        let gate_request = GateRequest::new(candidate_root.clone(), request.target)
            .with_options(request.options.clone())
            .with_detail(request.detail)
            .with_timings(request.timings)
            .with_root_epoch(0);
        let evidence = service
            .run(gate_request, progress, Some(cancellation.clone()))
            .await;
        service.close().await;

        let updated = match self.store.load(&id) {
            Ok(Some(updated)) if updated.revision == record.revision => updated,
            _ => {
                let mut stale = record.clone();
                stale
                    .evidence
                    .iter_mut()
                    .for_each(StoredEvidence::supersede);
                stale
            }
        };
        let fresh = evidence_freshness(&evidence, &cancellation)
            && updated.revision == record.revision
            && workspace.epoch() == record.workspace_epoch;
        let rows = evidence_rows(record.revision, fresh, &evidence);
        let mut updated = updated;
        updated.evidence.extend(rows);
        if updated.evidence.len() > MAX_RECORDED_EVIDENCE {
            let excess = updated.evidence.len() - MAX_RECORDED_EVIDENCE;
            updated.evidence.drain(0..excess);
        }
        let mut save_error = None;
        if let Err(error) = self.store.save(&mut updated) {
            save_error = Some(error);
        }
        self.push_cleanup_warnings(&save_error.iter().cloned().collect::<Vec<_>>());
        let status = if fresh {
            match evidence.status {
                GateStatus::FastPass | GateStatus::FullPass => "PASS",
                GateStatus::Fail => "FAIL",
                other => other.as_str(),
            }
        } else {
            match evidence.status {
                GateStatus::Cancelled => "CANCELLED",
                GateStatus::Timeout => "TIMEOUT",
                _ => "INCONCLUSIVE",
            }
        };
        let verified = updated.current_revision_fresh_pass();
        let mut data = data_for_record(ChangeAction::Validate, &updated, None);
        data.verified = verified;
        data.reason = match (&save_error, fresh) {
            (Some(error), _) => format!("evidence could not be persisted: {error}"),
            (None, true) => format!("{status} with fresh candidate evidence"),
            (None, false) => "evidence is not usable for the current revision".to_owned(),
        };
        self.finish(
            status,
            "Candidate validation finished against the isolated copy.",
            !matches!(status, "PASS" | "FAIL"),
            data,
        )
    }

    /// Materialize a verified baseline/candidate snapshot pair from
    /// server-owned change scratch for `profile(action=runtime_compare)`.
    ///
    /// The candidate side is a bounded copy of the recorded candidate revision.
    /// The baseline side starts from the same copy, reverse-applies the
    /// recorded patch log, and re-hashes both sides against the capture
    /// manifest and the recorded revision hashes. The original workspace is
    /// never read. `destination` must be a fresh server-owned directory.
    ///
    /// # Errors
    ///
    /// Returns a typed [`SnapshotError`]; a revision, hash, or extra-file
    /// mismatch is `Incomparable`/`Stale` instead of a guessed measurement.
    pub async fn materialize_runtime_snapshots(
        &self,
        change_id: &str,
        baseline_revision: u64,
        candidate_revision: u64,
        destination: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RuntimeSnapshotPair, SnapshotError> {
        if !is_valid_change_id(change_id) {
            return Err(SnapshotError::Invalid(
                "changeId is not a valid server-issued id".to_owned(),
            ));
        }
        if cancellation.is_cancelled() {
            return Err(SnapshotError::Cancelled);
        }
        if baseline_revision != 0 {
            return Err(SnapshotError::Incomparable(format!(
                "only revision 0 is materializable as the baseline; requested {baseline_revision}"
            )));
        }
        let _stripe = self.stripe(change_id).lock_owned().await;
        let record = match self.store.load(change_id) {
            Ok(Some(record)) => record,
            Ok(None) => return Err(SnapshotError::NotFound),
            Err(reason) => return Err(SnapshotError::Invalid(reason)),
        };
        match record.state {
            RecordState::Ready => {}
            RecordState::FailedInconsistent => {
                return Err(SnapshotError::Incomparable(
                    "the candidate may be partially applied; runtime comparison is refused"
                        .to_owned(),
                ));
            }
            RecordState::Discarded => return Err(SnapshotError::NotFound),
            RecordState::Applying | RecordState::Capturing => {
                return Err(SnapshotError::Stale(format!(
                    "the change is still {}; no completed revision was recorded",
                    record.state.as_str()
                )));
            }
        }
        if candidate_revision != record.revision {
            return Err(SnapshotError::Stale(format!(
                "candidateRevision {candidate_revision} does not match current revision {}",
                record.revision
            )));
        }
        let _file_lock = match self.store.try_lock(change_id) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Err(SnapshotError::Stale(
                    "the change scratch is locked by another process".to_owned(),
                ));
            }
            Err(reason) => return Err(SnapshotError::Unavailable(reason)),
        };
        let record = match self.reload_pinned_record(ChangeAction::Validate, change_id, &record) {
            Ok(record) => record,
            Err(outcome) => return Err(SnapshotError::Stale(outcome.data.reason.clone())),
        };
        if cancellation.is_cancelled() {
            return Err(SnapshotError::Cancelled);
        }
        let recorded_candidate = self.store.candidate_dir(change_id);
        if fs::symlink_metadata(&recorded_candidate)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(SnapshotError::Invalid(
                "the candidate copy is a symlink and cannot be measured".to_owned(),
            ));
        }
        if record.applying_revision.is_some() {
            return Err(SnapshotError::Stale(
                "an interrupted stage left the candidate state unrecorded".to_owned(),
            ));
        }
        verify_candidate_bytes(&self.store, change_id, &record)
            .map_err(SnapshotError::Incomparable)?;
        let manifest_bytes = fs::read(self.store.manifest_path(change_id)).map_err(|error| {
            SnapshotError::Unavailable(format!("capture manifest is unavailable: {error}"))
        })?;
        let manifest: CaptureManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|error| {
                SnapshotError::Invalid(format!("capture manifest is malformed: {error}"))
            })?;
        // Both expected-file maps are computed before any copy so a recorded
        // path that can never be materialized fails before scratch is created.
        let expected_candidate = expected_candidate_files(&record, &manifest)?;
        let expected_baseline = expected_baseline_files(&record, &manifest)?;
        if destination.exists() {
            return Err(SnapshotError::Invalid(
                "the runtime snapshot destination already exists".to_owned(),
            ));
        }
        fs::create_dir_all(destination).map_err(|error| {
            SnapshotError::Unavailable(format!(
                "could not create {}: {error}",
                destination.display()
            ))
        })?;
        let baseline_root = destination.join("baseline");
        let candidate_root = destination.join("candidate");
        let limits = CaptureLimits {
            max_files: self.config.change.max_files,
            max_bytes: self.config.change.max_bytes,
        };
        let copy_error = |reason: String| {
            if cancellation.is_cancelled() {
                SnapshotError::Cancelled
            } else {
                SnapshotError::Unavailable(reason)
            }
        };
        let excluded = copy_tree_bounded(
            &recorded_candidate,
            &candidate_root,
            limits.max_files,
            limits.max_bytes,
            cancellation,
        )
        .map_err(copy_error)?;
        let candidate_source_digest = verify_and_digest(&candidate_root, &expected_candidate)?;
        copy_tree_bounded(
            &recorded_candidate,
            &baseline_root,
            limits.max_files,
            limits.max_bytes,
            cancellation,
        )
        .map_err(copy_error)?;
        reverse_apply(&baseline_root, &record, cancellation)?;
        let baseline_source_digest = verify_and_digest(&baseline_root, &expected_baseline)?;
        Ok(RuntimeSnapshotPair {
            change_id: change_id.to_owned(),
            baseline_revision,
            candidate_revision: record.revision,
            base_identity: record.base_identity.clone(),
            patch_hash: record.patch_hash.clone(),
            manifest_hash: record.capture.manifest_hash.clone(),
            workspace_root: record.workspace_root.clone(),
            workspace_epoch: record.workspace_epoch,
            dependency_roots: record.dependency_roots.clone(),
            baseline_root,
            candidate_root,
            baseline_source_digest,
            candidate_source_digest,
            changed_files: record.changed_files.clone(),
            excluded,
        })
    }

    fn export(&self, request: ChangeRequest, workspace: &WorkspaceRoot) -> ChangeOutcome {
        let Some(id) = request.change_id.clone() else {
            return self.error_outcome(
                ChangeAction::Export,
                "INVALID",
                "action=export requires changeId.",
                "changeId is required",
                None,
            );
        };
        if !is_valid_change_id(&id) {
            return self.error_outcome(
                ChangeAction::Export,
                "INVALID",
                "The change id is not valid.",
                "changeId is not a valid server-issued id",
                Some(&id),
            );
        }
        let record = match self.store.load(&id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return self.error_outcome(
                    ChangeAction::Export,
                    "NOT_FOUND",
                    "The change id has no server-owned scratch.",
                    "no change record exists",
                    Some(&id),
                );
            }
            Err(reason) => {
                return self.error_outcome(
                    ChangeAction::Export,
                    "INVALID",
                    "The change record could not be read.",
                    reason,
                    Some(&id),
                );
            }
        };
        match record.state {
            RecordState::FailedInconsistent => {
                return self.finish(
                    "FAILED_INCONSISTENT",
                    "A failed partial apply cannot be exported as a verified package.",
                    true,
                    refused_data(
                        ChangeAction::Export,
                        &record,
                        "the candidate may be partially applied; export is refused",
                    ),
                );
            }
            RecordState::Discarded => {
                return self.finish(
                    "DISCARDED",
                    "The change scratch was discarded.",
                    true,
                    refused_data(
                        ChangeAction::Export,
                        &record,
                        "the candidate copy was discarded",
                    ),
                );
            }
            RecordState::Applying | RecordState::Capturing => {
                let reason = format!(
                    "the change is still {}; no completed revision was recorded",
                    record.state.as_str()
                );
                return self.finish(
                    "FAILED_INCONSISTENT",
                    "An unrecorded candidate state cannot be exported.",
                    true,
                    refused_data(ChangeAction::Export, &record, reason),
                );
            }
            RecordState::Ready => {}
        }
        if record.workspace_epoch != workspace.epoch() {
            let reason = format!(
                "authorization root epoch changed from {} to {}; the recorded candidate is stale",
                record.workspace_epoch,
                workspace.epoch()
            );
            return self.finish(
                "STALE",
                "The authorization epoch changed; the change record is not exportable.",
                true,
                refused_data(ChangeAction::Export, &record, reason),
            );
        }
        let _file_lock = match self.store.try_lock(&id) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return self.finish(
                    "CHANGE_BUSY",
                    "The change scratch is locked by another process.",
                    true,
                    refused_data(
                        ChangeAction::Export,
                        &record,
                        "another server process holds this change lock",
                    ),
                );
            }
            Err(reason) => {
                return self.error_outcome(
                    ChangeAction::Export,
                    "UNAVAILABLE",
                    "The change lock could not be opened.",
                    reason,
                    Some(&id),
                );
            }
        };
        // The record was read before the file lock; a stage in another server
        // process may have published a new revision since. Re-read under the
        // lock so verification and the exported package bind to the locked
        // revision.
        let record = match self.reload_pinned_record(ChangeAction::Export, &id, &record) {
            Ok(record) => record,
            Err(outcome) => return *outcome,
        };
        if record.applying_revision.is_some() {
            return self.finish(
                "FAILED_INCONSISTENT",
                "An interrupted stage cannot be exported.",
                true,
                refused_data(
                    ChangeAction::Export,
                    &record,
                    "an interrupted stage left the candidate state unrecorded",
                ),
            );
        }
        if let Err(reason) = verify_candidate_bytes(&self.store, &id, &record) {
            let mut failed = record.clone();
            failed.state = RecordState::FailedInconsistent;
            failed.applying_revision = None;
            failed
                .evidence
                .iter_mut()
                .for_each(StoredEvidence::supersede);
            if failed.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
                failed.cleanup_warnings.push(truncate(&reason, 512));
            }
            let _ = self.store.save(&mut failed);
            return self.finish(
                "FAILED_INCONSISTENT",
                "The candidate bytes do not match the recorded revision; export is refused.",
                true,
                refused_data(ChangeAction::Export, &failed, reason),
            );
        }
        let budget = usize::try_from(self.config.limits.tool_output_bytes).unwrap_or(49_152);
        let content_budget = budget.saturating_div(2);
        let mut data = data_for_record(ChangeAction::Export, &record, Some(content_budget));
        let verified = record.current_revision_fresh_pass();
        data.verified = verified;
        self.finish(
            if verified {
                "EXPORTED"
            } else {
                "EXPORTED_UNVERIFIED"
            },
            "Returned a revision-bound change package without writing the workspace.",
            false,
            data,
        )
    }

    async fn discard(&self, request: ChangeRequest) -> ChangeOutcome {
        let Some(id) = request.change_id.clone() else {
            return self.error_outcome(
                ChangeAction::Discard,
                "INVALID",
                "action=discard requires changeId.",
                "changeId is required",
                None,
            );
        };
        if !is_valid_change_id(&id) {
            return self.error_outcome(
                ChangeAction::Discard,
                "INVALID",
                "The change id is not valid.",
                "changeId is not a valid server-issued id",
                Some(&id),
            );
        }
        let _stripe = self.stripe(&id).lock_owned().await;
        let Some(mut record) = self.store.load(&id).ok().flatten() else {
            let mut data = ChangeData {
                action: ChangeAction::Discard.as_str().to_owned(),
                change_id: Some(id.clone()),
                discarded: true,
                state: "discarded".to_owned(),
                ..ChangeData::default()
            };
            data.reason = "no scratch exists; discard is already complete".to_owned();
            return self.finish(
                "DISCARDED",
                "The change scratch is already absent.",
                false,
                data,
            );
        };
        if record.state == RecordState::Discarded {
            let data = data_for_record(ChangeAction::Discard, &record, None);
            return self.finish(
                "DISCARDED",
                "The change scratch was already discarded.",
                false,
                data,
            );
        }
        let _file_lock = match self.store.try_lock(&id) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                let mut data = data_for_record(ChangeAction::Discard, &record, None);
                data.reason = "another server process holds this change lock".to_owned();
                return self.finish(
                    "CHANGE_BUSY",
                    "The change scratch is locked by another process.",
                    true,
                    data,
                );
            }
            Err(reason) => {
                return self.error_outcome(
                    ChangeAction::Discard,
                    "UNAVAILABLE",
                    "The change lock could not be opened.",
                    reason,
                    Some(&id),
                );
            }
        };
        let warnings = self.store.discard_worktree(&id);
        record.state = RecordState::Discarded;
        record
            .evidence
            .iter_mut()
            .for_each(StoredEvidence::supersede);
        for warning in &warnings {
            if record.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
                record.cleanup_warnings.push(warning.clone());
            }
        }
        if let Err(error) = self.store.save(&mut record) {
            self.push_cleanup_warnings(&[error]);
        }
        self.push_cleanup_warnings(&warnings);
        let mut data = data_for_record(ChangeAction::Discard, &record, None);
        data.discarded = true;
        data.reason = if warnings.is_empty() {
            "server-owned scratch removed".to_owned()
        } else {
            "server-owned scratch removal was incomplete; see cleanupWarnings".to_owned()
        };
        self.finish(
            "DISCARDED",
            "The candidate copy was removed and the record was marked discarded.",
            false,
            data,
        )
    }
}

fn evidence_freshness(evidence: &GateEvidence, cancellation: &CancellationToken) -> bool {
    let terminal = matches!(
        evidence.status,
        GateStatus::FastPass | GateStatus::FullPass | GateStatus::Fail
    );
    let clean = evidence.steps.iter().all(|step| {
        step.drain_complete && step.cleanup_complete && !step.cancelled && !step.timed_out
    });
    terminal && clean && !cancellation.is_cancelled()
}

fn evidence_rows(revision: u64, fresh: bool, evidence: &GateEvidence) -> Vec<StoredEvidence> {
    let status = evidence.status;
    let recorded_at = now_ms();
    if evidence.steps.is_empty() {
        return vec![StoredEvidence {
            revision,
            target: status.as_str().to_owned(),
            command: String::new(),
            status: status.as_str().to_owned(),
            exit_code: None,
            first_diagnostic_ms: evidence.first_diagnostic_ms,
            total_ms: evidence.response_ms,
            fresh,
            authoritative: fresh,
            recorded_at_ms: recorded_at,
            diagnostics: Vec::new(),
            diagnostics_total: 0,
            diagnostics_omitted: 0,
            suggestion_package: None,
            stats: crate::diagnostics::EvidenceStats::default(),
        }];
    }
    let root = evidence.workspace_root.as_deref();
    evidence
        .steps
        .iter()
        .take(16)
        .map(|step| {
            // Only a fresh run may persist compiler feedback; a cancelled,
            // timed-out, or already-superseded run keeps its counters and
            // stats but no diagnostics or suggestions.
            let diagnostics = if fresh {
                step.diagnostics
                    .iter()
                    .map(|diagnostic| change_diagnostic(diagnostic, root))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let diagnostics_total = if fresh {
                u64::try_from(diagnostics.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(step.diagnostics_omitted)
            } else {
                0
            };
            StoredEvidence {
                revision,
                target: step.target.as_str().to_owned(),
                command: truncate(&step.command, MAX_COMMAND_CHARS),
                status: status.as_str().to_owned(),
                exit_code: Some(step.exit_code),
                first_diagnostic_ms: step.first_diagnostic_ms,
                total_ms: step.duration_ms,
                fresh,
                authoritative: fresh
                    && matches!(
                        status,
                        GateStatus::FastPass | GateStatus::FullPass | GateStatus::Fail
                    ),
                recorded_at_ms: recorded_at,
                diagnostics,
                diagnostics_total,
                diagnostics_omitted: if fresh { step.diagnostics_omitted } else { 0 },
                // Suggestions are offered only for a fresh compile failure; a
                // pass, a stale run, or a refused request must never carry a
                // patch that assumes a broken candidate.
                suggestion_package: if fresh && status == GateStatus::Fail {
                    step.suggestion_package.as_ref().map(|package| {
                        bounded_suggestion_package(
                            package,
                            unsupported_suggestions(&step.diagnostics),
                        )
                    })
                } else {
                    None
                },
                stats: step.evidence.clone(),
            }
        })
        .collect()
}

/// Converts one gate diagnostic into candidate-relative, bounded evidence.
fn change_diagnostic(
    diagnostic: &crate::gate::GateDiagnostic,
    root: Option<&Path>,
) -> ChangeDiagnosticData {
    ChangeDiagnosticData {
        code: diagnostic.code.clone(),
        level: diagnostic.level.clone(),
        file: relative_diagnostic_file(diagnostic.file.as_deref(), root),
        line: diagnostic.line,
        message: truncate(&diagnostic.message, MAX_DIAGNOSTIC_MESSAGE_BYTES),
    }
}

/// Keeps compiler paths workspace-relative when the compiler emitted an
/// absolute path inside the compiled candidate root.
fn relative_diagnostic_file(file: Option<&str>, root: Option<&Path>) -> Option<String> {
    let file = file?;
    let normalized = file.replace('\\', "/");
    let root = root?;
    let path = Path::new(&normalized);
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(root) {
            if let Some(relative) = relative.to_str() {
                return Some(relative.replace('\\', "/"));
            }
        }
    }
    Some(normalized)
}

/// Counts suggestions that cannot become a write-free patch because the
/// compiler did not mark them machine-applicable or supplied no edits.
fn unsupported_suggestions(diagnostics: &[crate::gate::GateDiagnostic]) -> u64 {
    diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.suggestions.iter())
        .filter(|suggestion| {
            suggestion.applicability != SuggestionApplicability::MachineApplicable
                || suggestion.edits.is_empty()
        })
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Applies the per-row persistence limits to a machine-applicable package.
/// A missing patch or skipped reason is only ever a *visible* truncation: the
/// `*Total` fields keep the pre-limit counts and `truncated` is set.
fn bounded_suggestion_package(
    package: &SuggestionPackage,
    unsupported: u64,
) -> ChangeSuggestionPackageData {
    let mut used = 0usize;
    let mut patches = Vec::new();
    for patch in package.patches.iter().take(MAX_LISTED_SUGGESTION_PATCHES) {
        let size = patch
            .old_string
            .len()
            .saturating_add(patch.new_string.len());
        if used.saturating_add(size) > MAX_SUGGESTION_PATCH_BYTES {
            // Skip the oversized patch instead of hiding every later, small
            // patch behind it; truncation stays visible through `truncated`.
            continue;
        }
        used = used.saturating_add(size);
        patches.push(ChangeSuggestionPatchData {
            file: patch.file.clone(),
            old_string: patch.old_string.clone(),
            new_string: patch.new_string.clone(),
        });
    }
    let skipped = package
        .skipped
        .iter()
        .take(MAX_LISTED_SUGGESTION_SKIPPED)
        .map(|reason| truncate(reason, MAX_SUGGESTION_SKIPPED_CHARS))
        .collect::<Vec<_>>();
    let truncated = patches.len() < package.patches.len() || skipped.len() < package.skipped.len();
    ChangeSuggestionPackageData {
        patches,
        skipped,
        unsupported,
        patches_total: package.patches.len().try_into().unwrap_or(u64::MAX),
        skipped_total: package.skipped.len().try_into().unwrap_or(u64::MAX),
        truncated,
    }
}

fn data_for_record(
    action: ChangeAction,
    record: &ChangeRecord,
    content_budget: Option<usize>,
) -> ChangeData {
    let changed_files = record
        .changed_files
        .iter()
        .take(MAX_LISTED_CHANGED_FILES)
        .cloned()
        .collect::<Vec<_>>();
    let source_hashes = record
        .source_hashes
        .iter()
        .take(MAX_LISTED_HASHES)
        .map(|(file, hash)| ChangeSourceHashData {
            file: file.clone(),
            sha256: hash.sha256.clone(),
            bytes: hash.bytes,
        })
        .collect::<Vec<_>>();
    let mut evidence = record
        .evidence
        .iter()
        .rev()
        .take(MAX_LISTED_EVIDENCE)
        .map(|evidence| {
            // Compiler feedback describes the candidate bytes of the revision
            // it was produced for. Any other row stays historical and carries
            // no diagnostics or suggestions.
            let current = evidence.fresh && evidence.revision == record.revision;
            ChangeEvidenceData {
                revision: evidence.revision,
                target: evidence.target.clone(),
                command: evidence.command.clone(),
                status: evidence.status.clone(),
                exit_code: evidence.exit_code,
                first_diagnostic_ms: evidence.first_diagnostic_ms,
                total_ms: evidence.total_ms,
                fresh: evidence.fresh,
                authoritative: evidence.authoritative,
                diagnostics: if current {
                    evidence.diagnostics.clone()
                } else {
                    Vec::new()
                },
                diagnostics_total: if current {
                    evidence.diagnostics_total
                } else {
                    0
                },
                diagnostics_omitted: if current {
                    evidence.diagnostics_omitted
                } else {
                    0
                },
                suggestion_package: if current {
                    evidence.suggestion_package.clone()
                } else {
                    None
                },
                stats: evidence.stats.clone(),
            }
        })
        .collect::<Vec<_>>();
    evidence.reverse();
    let patches = record
        .patches
        .iter()
        .take(MAX_LISTED_HASHES)
        .map(|patch| ChangePatchData {
            file: patch.file.clone(),
            old_string: patch.old_string.clone(),
            new_string: patch.new_string.clone(),
        })
        .collect::<Vec<_>>();
    let mut used = 0usize;
    let mut omitted = false;
    let mut new_files = Vec::new();
    for file in record.new_files.iter().take(MAX_LISTED_HASHES) {
        let content = content_budget.and_then(|budget| {
            if used.saturating_add(file.content.len()) <= budget {
                used = used.saturating_add(file.content.len());
                Some(file.content.clone())
            } else {
                omitted = true;
                None
            }
        });
        new_files.push(ChangeNewFileData {
            file: file.file.clone(),
            sha256: file.sha256.clone(),
            bytes: file.bytes,
            content,
        });
    }
    if record.new_files.len() > MAX_LISTED_HASHES {
        omitted = true;
    }
    ChangeData {
        action: action.as_str().to_owned(),
        change_id: Some(record.id.clone()),
        base_identity: Some(record.base_identity.clone()),
        revision: record.revision,
        patch_hash: Some(record.patch_hash.clone()),
        state: record.state.as_str().to_owned(),
        verified: record.current_revision_fresh_pass(),
        discarded: record.state == RecordState::Discarded,
        changed_files,
        changed_files_total: record.changed_files.len().try_into().unwrap_or(u64::MAX),
        source_hashes,
        source_hashes_total: record.source_hashes.len().try_into().unwrap_or(u64::MAX),
        evidence,
        evidence_total: record.evidence.len().try_into().unwrap_or(u64::MAX),
        capture: Some(ChangeCaptureData {
            files: record.capture.files,
            bytes: record.capture.bytes,
            manifest_hash: record.capture.manifest_hash.clone(),
            complete: record.capture.complete,
            excluded: record.capture.excluded.clone(),
        }),
        patches_total: record.patches.len().try_into().unwrap_or(u64::MAX),
        patches,
        new_files_total: record.new_files.len().try_into().unwrap_or(u64::MAX),
        new_files,
        new_files_content_omitted: omitted,
        migration: record.migration.clone(),
        cleanup_warnings: record.cleanup_warnings.clone(),
        reason: String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn create_blocking(
    roots: Arc<RootGuard>,
    config: &ChangeConfig,
    cargo: &Path,
    store: &ChangeStore,
    workspace: &WorkspaceRoot,
    cancellation: &CancellationToken,
    control: &MetadataControl,
) -> Result<ChangeRecord, ChangeFailure> {
    if cancellation.is_cancelled() {
        return Err(ChangeFailure::new("CANCELLED", "capture was cancelled"));
    }
    let selection = select_in_root(workspace).map_err(|error| {
        ChangeFailure::new(
            "INCOMPLETE_INPUTS",
            format!("workspace selection failed: {error}"),
        )
    })?;
    let configured_dependencies = roots
        .dependency_roots()
        .iter()
        .map(|root| root.path().to_owned())
        .collect::<Vec<_>>();
    let metadata = MetadataService::new(Arc::clone(&roots));
    let load = metadata
        .acquire_controlled(&selection, cargo, control)
        .map_err(|error| match error {
            crate::workspace::MetadataError::Cancelled => {
                ChangeFailure::new("CANCELLED", "capture was cancelled during metadata")
            }
            crate::workspace::MetadataError::TimedOut => ChangeFailure::new(
                "TIMEOUT",
                "workspace metadata exceeded the configured deadline",
            ),
            error => ChangeFailure::new(
                "INCOMPLETE_INPUTS",
                format!("workspace metadata could not be captured: {error}"),
            ),
        })?;
    let snapshot = load.snapshot;
    if !snapshot.dependency_closure.complete {
        return Err(ChangeFailure::new(
            "INCOMPLETE_INPUTS",
            "the dependency closure is incomplete; capture would be unsafe",
        ));
    }
    let capture_root = snapshot.workspace_root.clone();
    let capture_authority = if selection.worktree_authority().path() == capture_root {
        selection.worktree_authority().clone()
    } else {
        selection
            .worktree_authority()
            .authorize_dir(&capture_root)
            .map_err(|error| {
                ChangeFailure::new(
                    "INCOMPLETE_INPUTS",
                    format!("workspace capture could not be authorized: {error}"),
                )
            })?
    };

    let scratch_root = store.root();
    if path_is_within(scratch_root, &capture_root) || path_is_within(&capture_root, scratch_root) {
        return Err(ChangeFailure::new(
            "INCOMPLETE_INPUTS",
            "server-owned scratch overlaps the captured workspace",
        ));
    }
    reject_escaping_cargo_target(&capture_root)?;
    let mut excluded = BTreeSet::new();
    // Cargo reports the target directory in the ordinary spelling while the
    // capture root is canonical; normalize before computing the exclusion.
    let cargo_target = crate::workspace::canonical_spelling(&snapshot.target_directory);
    if let Ok(relative) = cargo_target.strip_prefix(&capture_root) {
        excluded.insert(relative.to_owned());
    }
    if let Ok(relative) = scratch_root.strip_prefix(&capture_root) {
        excluded.insert(relative.to_owned());
    }
    reject_external_path_dependencies(&snapshot, &capture_root)?;

    let id = next_change_id();
    let change_dir = store.change_dir(&id);
    // Create and lock the fresh change before writing any candidate byte. The
    // open lock file is the durable ownership marker that keeps a concurrent
    // sweep in another process from deleting a live capture.
    let lock = match store.try_lock(&id) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return Err(ChangeFailure::new(
                "CHANGE_BUSY",
                "another process holds the fresh change lock",
            ));
        }
        Err(reason) => return Err(ChangeFailure::new("RESOURCE_BLOCKED", reason)),
    };
    let mut placeholder = placeholder_record(&id, &capture_root, workspace.epoch());
    if let Err(error) = store.save(&mut placeholder) {
        drop(lock);
        let _ = store.remove_path(&change_dir);
        return Err(ChangeFailure::new(
            "RESOURCE_BLOCKED",
            format!("the capture marker could not be published: {error}"),
        ));
    }
    match capture_into(
        store,
        &id,
        &capture_root,
        &capture_authority,
        &excluded,
        config,
        configured_dependencies,
        workspace.epoch(),
        cancellation,
    ) {
        Ok(record) => Ok(record),
        Err(mut failure) => {
            drop(lock);
            if let Err(error) = store.remove_path(&change_dir) {
                failure.warnings.push(truncate(&error, 512));
            }
            Err(failure)
        }
    }
}

fn placeholder_record(id: &str, capture_root: &Path, workspace_epoch: u64) -> ChangeRecord {
    let timestamp = now_ms();
    ChangeRecord {
        schema_version: CHANGE_SCHEMA_VERSION,
        id: id.to_owned(),
        created_at_ms: timestamp,
        updated_at_ms: timestamp,
        workspace_root: capture_root.to_owned(),
        capture_root: capture_root.to_owned(),
        workspace_epoch,
        candidate_epoch: 0,
        base_identity: String::new(),
        state: RecordState::Capturing,
        capture: CaptureSummary {
            files: 0,
            bytes: 0,
            manifest_hash: String::new(),
            complete: false,
            excluded: Vec::new(),
        },
        external_paths: Vec::new(),
        dependency_roots: Vec::new(),
        revision: 0,
        applying_revision: None,
        patches: Vec::new(),
        new_files: Vec::new(),
        changed_files: Vec::new(),
        candidate_files: 0,
        candidate_bytes: 0,
        patch_hash: String::new(),
        source_hashes: BTreeMap::new(),
        evidence: Vec::new(),
        migration: None,
        cleanup_warnings: Vec::new(),
    }
}

/// Path dependencies outside the captured workspace are not materialized into
/// the candidate scratch copy, so their relative `path = "..."` references
/// cannot resolve there. Such a capture can never be validated honestly, so
/// `create` fails closed and lists the paths it cannot reproduce.
fn reject_external_path_dependencies(
    snapshot: &crate::workspace::WorkspaceSnapshot,
    capture_root: &Path,
) -> Result<(), ChangeFailure> {
    let mut roots = Vec::new();
    for root in snapshot
        .external_paths
        .iter()
        .chain(snapshot.dependency_closure.package_roots.iter())
    {
        if path_is_within(capture_root, root) {
            continue;
        }
        if !root.is_dir() {
            return Err(ChangeFailure::new(
                "INCOMPLETE_INPUTS",
                format!(
                    "external path dependency {} is not an existing directory",
                    root.display()
                ),
            ));
        }
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    if roots.is_empty() {
        return Ok(());
    }
    let listed = roots
        .iter()
        .take(8)
        .map(|root| truncate(&root.display().to_string(), 256))
        .collect::<Vec<_>>()
        .join(", ");
    let more = roots.len().saturating_sub(8);
    let suffix = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    Err(ChangeFailure::new(
        "INCOMPLETE_INPUTS",
        format!(
            "path dependencies outside the captured workspace cannot be reproduced in the server-owned candidate copy ({listed}{suffix}); create is refused so an incomplete capture cannot be validated"
        ),
    ))
}

#[allow(clippy::too_many_arguments)]
fn capture_into(
    store: &ChangeStore,
    id: &str,
    capture_root: &Path,
    capture_authority: &AuthorizedRoot,
    excluded: &BTreeSet<PathBuf>,
    config: &ChangeConfig,
    dependency_roots: Vec<PathBuf>,
    workspace_epoch: u64,
    cancellation: &CancellationToken,
) -> Result<ChangeRecord, ChangeFailure> {
    let candidate_root = store.candidate_dir(id);
    let limits = CaptureLimits {
        max_files: config.max_files,
        max_bytes: config.max_bytes,
    };
    let manifest = match capture_tree(
        capture_authority,
        &candidate_root,
        excluded,
        limits,
        cancellation,
    ) {
        Ok(manifest) => manifest,
        Err(CaptureError::Cancelled) => {
            return Err(ChangeFailure::new("CANCELLED", "capture was cancelled"));
        }
        Err(CaptureError::Incomplete(reason)) => {
            return Err(ChangeFailure::new(
                "INCOMPLETE_INPUTS",
                format!("capture is incomplete: {reason}"),
            ));
        }
    };
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|error| {
        ChangeFailure::new(
            "INCOMPLETE_INPUTS",
            format!("capture manifest could not be serialized: {error}"),
        )
    })?;
    store
        .write_manifest(id, &manifest_bytes)
        .map_err(|error| ChangeFailure::new("INCOMPLETE_INPUTS", error))?;
    let manifest_digest = manifest_hash(&manifest);
    let base_identity = base_identity(workspace_epoch, &manifest_digest, capture_root);
    let candidate_epoch = match RootGuard::new([candidate_root.clone()], dependency_roots.clone())
        .and_then(|guard| guard.snapshot(ClientRoots::unsupported()))
    {
        Ok(snapshot) => snapshot.epoch(),
        Err(error) => {
            return Err(ChangeFailure::new(
                "INCOMPLETE_INPUTS",
                format!("candidate root could not be authorized: {error}"),
            ));
        }
    };
    let candidate_files = manifest.files.len().try_into().unwrap_or(u64::MAX);
    let mut record = ChangeRecord {
        schema_version: CHANGE_SCHEMA_VERSION,
        id: id.to_owned(),
        created_at_ms: now_ms(),
        updated_at_ms: now_ms(),
        workspace_root: capture_root.to_owned(),
        capture_root: capture_root.to_owned(),
        workspace_epoch,
        candidate_epoch,
        base_identity,
        state: RecordState::Ready,
        capture: CaptureSummary {
            files: candidate_files,
            bytes: manifest.total_bytes,
            manifest_hash: manifest_digest,
            complete: true,
            excluded: manifest.excluded.clone(),
        },
        external_paths: Vec::new(),
        dependency_roots,
        revision: 0,
        applying_revision: None,
        patches: Vec::new(),
        new_files: Vec::new(),
        changed_files: Vec::new(),
        candidate_files,
        candidate_bytes: manifest.total_bytes,
        patch_hash: String::new(),
        source_hashes: BTreeMap::new(),
        evidence: Vec::new(),
        migration: None,
        cleanup_warnings: Vec::new(),
    };
    store
        .save(&mut record)
        .map_err(|error| ChangeFailure::new("INCOMPLETE_INPUTS", error))?;
    Ok(record)
}

fn base_identity(epoch: u64, manifest_digest: &str, capture_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-mcp-change-base\0");
    hasher.update(epoch.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(manifest_digest.as_bytes());
    hasher.update(b"\0");
    hasher.update(capture_root.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn next_change_id() -> String {
    let sequence = CHANGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{CHANGE_ID_PREFIX}{:x}-{:x}-{:x}",
        now_ms(),
        std::process::id(),
        sequence
    )
}

fn path_is_within(root: &Path, candidate: &Path) -> bool {
    // Cargo and config may spell the same Windows path with or without the
    // verbatim prefix; compare one canonical spelling so containment stays
    // fail-closed regardless of the caller's spelling.
    let root = crate::workspace::canonical_spelling(root);
    let candidate = crate::workspace::canonical_spelling(candidate);
    candidate == root
        || candidate
            .strip_prefix(&root)
            .is_ok_and(|relative| !relative.is_absolute())
}

fn reject_escaping_cargo_target(candidate_root: &Path) -> Result<(), ChangeFailure> {
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let path = candidate_root.join(relative);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ChangeFailure::new(
                    "INCOMPLETE_INPUTS",
                    format!("could not read {}: {error}", path.display()),
                ));
            }
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            ChangeFailure::new(
                "INCOMPLETE_INPUTS",
                format!("{} is not valid UTF-8", path.display()),
            )
        })?;
        let value: toml::Value = toml::from_str(text).map_err(|error| {
            ChangeFailure::new(
                "INCOMPLETE_INPUTS",
                format!("{} could not be parsed: {error}", path.display()),
            )
        })?;
        for candidate in cargo_target_candidates(&value) {
            let resolved = if Path::new(&candidate).is_absolute() {
                normalize_lexical(Path::new(&candidate))
            } else {
                normalize_lexical(&candidate_root.join(&candidate))
            };
            if !path_is_within(candidate_root, &resolved) {
                return Err(ChangeFailure::new(
                    "INCOMPLETE_INPUTS",
                    format!(
                        "{} redirects the Cargo target directory outside the candidate copy",
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn cargo_target_candidates(value: &toml::Value) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(target_dir) = value
        .get("build")
        .and_then(|build| build.get("target-dir"))
        .and_then(toml::Value::as_str)
    {
        candidates.push(target_dir.to_owned());
    }
    if let Some(target_dir) = value
        .get("env")
        .and_then(|env| env.get("CARGO_TARGET_DIR"))
        .and_then(toml::Value::as_str)
    {
        candidates.push(target_dir.to_owned());
    }
    candidates
}

#[allow(clippy::too_many_arguments)]
fn stage_blocking(
    store: &ChangeStore,
    config: &ChangeConfig,
    id: &str,
    workspace_epoch: u64,
    expected_revision: Option<u64>,
    base_identity: Option<&str>,
    patches: &[PatchInput],
    new_files: &[NewFileInput],
    cancellation: &CancellationToken,
) -> Result<ChangeRecord, ChangeFailure> {
    let Some(_lock) = store
        .try_lock(id)
        .map_err(|reason| ChangeFailure::new("UNAVAILABLE", reason))?
    else {
        return Err(ChangeFailure::new(
            "CHANGE_BUSY",
            "another server process holds this change lock",
        ));
    };
    let Some(record) = store
        .load(id)
        .map_err(|reason| ChangeFailure::new("INVALID", reason))?
    else {
        return Err(ChangeFailure::new(
            "NOT_FOUND",
            "no change record exists for this change id",
        ));
    };
    if record.state != RecordState::Ready {
        return Err(ChangeFailure::new(
            match record.state {
                RecordState::FailedInconsistent => "FAILED_INCONSISTENT",
                RecordState::Discarded => "DISCARDED",
                RecordState::Capturing | RecordState::Applying => "FAILED_INCONSISTENT",
                RecordState::Ready => "INVALID",
            },
            format!("change state is {}", record.state.as_str()),
        ));
    }
    if record.workspace_epoch != workspace_epoch {
        return Err(ChangeFailure::new(
            "STALE",
            format!(
                "authorization root epoch changed from {} to {}; the change is stale",
                record.workspace_epoch, workspace_epoch
            ),
        ));
    }
    stage_locked(
        store,
        config,
        record,
        expected_revision,
        base_identity,
        patches,
        new_files,
        cancellation,
    )
}

/// Applies a pre-validated patch plan to the candidate while the caller holds
/// the change lock. Shared by `stage` and `migrate`.
#[allow(clippy::too_many_arguments)]
fn stage_locked(
    store: &ChangeStore,
    config: &ChangeConfig,
    mut record: ChangeRecord,
    expected_revision: Option<u64>,
    base_identity: Option<&str>,
    patches: &[PatchInput],
    new_files: &[NewFileInput],
    cancellation: &CancellationToken,
) -> Result<ChangeRecord, ChangeFailure> {
    let id = record.id.clone();
    let expected_revision = expected_revision
        .ok_or_else(|| ChangeFailure::new("INVALID", "expectedRevision is required"))?;
    if expected_revision != record.revision {
        return Err(ChangeFailure::new(
            "STALE",
            format!(
                "expectedRevision {} does not match current revision {}",
                expected_revision, record.revision
            ),
        ));
    }
    let base_identity =
        base_identity.ok_or_else(|| ChangeFailure::new("INVALID", "baseIdentity is required"))?;
    if base_identity != record.base_identity {
        return Err(ChangeFailure::new(
            "STALE",
            "baseIdentity does not match the captured base",
        ));
    }
    if cancellation.is_cancelled() {
        return Err(ChangeFailure::new("CANCELLED", "stage was cancelled"));
    }
    if record.revision.saturating_add(1) > config.max_revisions {
        return Err(ChangeFailure::new(
            "REVISION_LIMIT",
            format!(
                "the change reached the configured revision limit {}",
                config.max_revisions
            ),
        ));
    }
    let candidate_root = store.candidate_dir(&id);
    let limits = CandidateLimits {
        current_files: record.candidate_files,
        current_bytes: record.candidate_bytes,
        max_files: config.max_files,
        max_bytes: config.max_bytes,
    };
    let plan = plan_patches(&candidate_root, patches, new_files, limits)
        .map_err(|reason| ChangeFailure::new("PATCH_REJECTED", reason))?;
    if cancellation.is_cancelled() {
        return Err(ChangeFailure::new("CANCELLED", "stage was cancelled"));
    }
    // Publish the durable applying marker before the first candidate write.
    // A crash or a failed final publish leaves this marker (or a `FailedInconsistent`
    // record) on disk, so `validate`/`export` can never treat bytes from an
    // unrecorded revision as verified.
    record.state = RecordState::Applying;
    record.applying_revision = Some(record.revision.saturating_add(1));
    if let Err(reason) = store.save(&mut record) {
        record.state = RecordState::Ready;
        record.applying_revision = None;
        return Err(ChangeFailure::new(
            "UNAVAILABLE",
            format!(
                "the applying marker could not be published; no candidate byte was written: {reason}"
            ),
        ));
    }
    if let Err(error) = apply_plan(&candidate_root, &plan) {
        record.state = RecordState::FailedInconsistent;
        record.applying_revision = None;
        if record.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
            record.cleanup_warnings.push(truncate(&error, 512));
        }
        let _ = store.save(&mut record);
        return Err(ChangeFailure::new(
            "FAILED_INCONSISTENT",
            format!("a candidate write failed after validation: {error}"),
        ));
    }
    // Record the hashes of the bytes actually on disk. A read-back mismatch
    // means the candidate no longer matches the plan and must never be
    // reported as a completed revision.
    let mut applied_hashes = BTreeMap::new();
    for file in &plan.files {
        let relative = relative_path_string(&file.relative);
        let bytes = match fs::read(candidate_root.join(&file.relative)) {
            Ok(bytes) => bytes,
            Err(error) => {
                record.state = RecordState::FailedInconsistent;
                record.applying_revision = None;
                if record.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
                    record.cleanup_warnings.push(format!(
                        "candidate file {relative} could not be re-read after apply: {error}"
                    ));
                }
                let _ = store.save(&mut record);
                return Err(ChangeFailure::new(
                    "FAILED_INCONSISTENT",
                    format!("candidate file {relative} could not be re-read after apply"),
                ));
            }
        };
        if bytes != file.contents {
            record.state = RecordState::FailedInconsistent;
            record.applying_revision = None;
            if record.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
                record.cleanup_warnings.push(format!(
                    "candidate file {relative} does not match the planned bytes"
                ));
            }
            let _ = store.save(&mut record);
            return Err(ChangeFailure::new(
                "FAILED_INCONSISTENT",
                format!("candidate file {relative} does not match the planned bytes"),
            ));
        }
        applied_hashes.insert(
            relative,
            StoredHash {
                sha256: sha256_hex(&bytes),
                bytes: bytes.len().try_into().unwrap_or(u64::MAX),
            },
        );
    }
    let inserted_new_files = plan
        .new_files
        .iter()
        .map(|file| StoredNewFile {
            file: relative_path_string(&file.relative),
            content: file.content.clone(),
            sha256: sha256_hex(file.content.as_bytes()),
            bytes: file.content.len().try_into().unwrap_or(u64::MAX),
        })
        .collect::<Vec<_>>();
    let inserted_patches = plan
        .patches
        .iter()
        .map(|patch| StoredPatch {
            file: normalize_patch_file(&patch.file),
            old_string: patch.old_string.clone(),
            new_string: patch.new_string.clone(),
        })
        .collect::<Vec<_>>();
    let mut candidate_bytes = i128::from(record.candidate_bytes);
    for file in &plan.files {
        candidate_bytes -= i128::from(file.previous_bytes);
        candidate_bytes += i128::from(file.contents.len() as u64);
    }
    if candidate_bytes < 0 {
        record.state = RecordState::FailedInconsistent;
        record.applying_revision = None;
        if record.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
            record.cleanup_warnings.push(
                "candidate byte accounting became negative after applying patches".to_owned(),
            );
        }
        let _ = store.save(&mut record);
        return Err(ChangeFailure::new(
            "FAILED_INCONSISTENT",
            "candidate byte accounting failed after apply",
        ));
    }
    record.source_hashes.extend(applied_hashes);
    record.candidate_files = record
        .candidate_files
        .saturating_add(inserted_new_files.len() as u64);
    record.candidate_bytes = u64::try_from(candidate_bytes).unwrap_or(u64::MAX);
    record.revision = record.revision.saturating_add(1);
    record.patches.extend(inserted_patches);
    record.new_files.extend(inserted_new_files);
    for file in &plan.files {
        let relative = relative_path_string(&file.relative);
        if !record.changed_files.contains(&relative) {
            record.changed_files.push(relative);
        }
    }
    if record.changed_files.len() > MAX_CHANGED_FILES_IN_RECORD {
        record.changed_files.truncate(MAX_CHANGED_FILES_IN_RECORD);
    }
    record.patch_hash = compute_patch_hash(record.revision, &record.patches, &record.new_files);
    // Superseding a revision invalidates its compiler feedback; dropping the
    // diagnostics here also keeps the durable record bounded across stages.
    record
        .evidence
        .iter_mut()
        .for_each(StoredEvidence::supersede);
    record.state = RecordState::Ready;
    record.applying_revision = None;
    if let Err(reason) = store.save(&mut record) {
        let mut failed = record.clone();
        failed.state = RecordState::FailedInconsistent;
        failed.applying_revision = None;
        if failed.cleanup_warnings.len() < MAX_CLEANUP_WARNINGS {
            failed.cleanup_warnings.push(truncate(&reason, 512));
        }
        let _ = store.save(&mut failed);
        return Err(ChangeFailure::new(
            "FAILED_INCONSISTENT",
            format!("the change record could not be published: {reason}"),
        ));
    }
    Ok(record)
}

/// Re-reads every candidate file whose bytes were recorded for the current
/// revision and fails when the on-disk bytes differ. This is the read-side of
/// the applying marker: a `Ready` record can never be validated or exported
/// against bytes that belong to an unrecorded revision.
/// Builds refusal data whose `verified` flag and evidence rows never claim a
/// fresh verification for a request that was rejected before re-checking bytes.
fn refused_data(
    action: ChangeAction,
    record: &ChangeRecord,
    reason: impl Into<String>,
) -> ChangeData {
    let mut data = data_for_record(action, record, None);
    data.verified = false;
    data.evidence.iter_mut().for_each(|evidence| {
        evidence.fresh = false;
        evidence.authoritative = false;
        // A refused request must never surface compiler feedback as if the
        // candidate had just been verified for it.
        evidence.diagnostics.clear();
        evidence.diagnostics_total = 0;
        evidence.diagnostics_omitted = 0;
        evidence.suggestion_package = None;
    });
    data.reason = reason.into();
    data
}

fn verify_candidate_bytes(
    store: &ChangeStore,
    id: &str,
    record: &ChangeRecord,
) -> Result<(), String> {
    let candidate_root = store.candidate_dir(id);
    for (file, recorded) in &record.source_hashes {
        let relative = super::patch::normalize_relative(file)
            .map_err(|error| format!("recorded candidate path {file} is invalid: {error}"))?;
        let path = candidate_root.join(&relative);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("recorded candidate file {file} is unavailable: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "recorded candidate file {file} is not a regular file"
            ));
        }
        let bytes = fs::read(&path).map_err(|error| {
            format!("recorded candidate file {file} could not be read: {error}")
        })?;
        if sha256_hex(&bytes) != recorded.sha256 || bytes.len() as u64 != recorded.bytes {
            return Err(format!(
                "candidate file {file} does not match the recorded revision bytes"
            ));
        }
    }
    Ok(())
}

fn normalize_patch_file(file: &str) -> String {
    super::patch::normalize_relative(file)
        .map(|path| relative_path_string(&path))
        .unwrap_or_else(|_| file.to_owned())
}

fn compute_patch_hash(
    revision: u64,
    patches: &[StoredPatch],
    new_files: &[StoredNewFile],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-mcp-change-patches\0");
    hasher.update(revision.to_string().as_bytes());
    hasher.update(b"\0");
    for patch in patches {
        hasher.update(patch.file.as_bytes());
        hasher.update(b"\0");
        hasher.update(patch.old_string.as_bytes());
        hasher.update(b"\0");
        hasher.update(patch.new_string.as_bytes());
        hasher.update(b"\0");
    }
    for file in new_files {
        hasher.update(file.file.as_bytes());
        hasher.update(b"\0");
        hasher.update(file.sha256.as_bytes());
        hasher.update(b"\0");
        hasher.update(file.bytes.to_string().as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GateTargetId;

    #[test]
    fn change_id_and_patch_hash_are_deterministic_and_bounded() {
        let first = next_change_id();
        let second = next_change_id();
        assert_ne!(first, second);
        assert!(is_valid_change_id(&first));
        assert!(is_valid_change_id(&second));

        let patches = vec![StoredPatch {
            file: "src/lib.rs".to_owned(),
            old_string: "a".to_owned(),
            new_string: "b".to_owned(),
        }];
        let new_files = vec![StoredNewFile {
            file: "src/new.rs".to_owned(),
            content: "pub fn new() {}".to_owned(),
            sha256: sha256_hex(b"pub fn new() {}"),
            bytes: 15,
        }];
        assert_eq!(
            compute_patch_hash(1, &patches, &new_files),
            compute_patch_hash(1, &patches, &new_files)
        );
        assert_ne!(
            compute_patch_hash(1, &patches, &new_files),
            compute_patch_hash(2, &patches, &new_files)
        );
    }

    #[test]
    fn pinned_record_reload_detects_a_cross_process_revision_bump() {
        let base = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-change-reload-{}-{}",
                std::process::id(),
                CHANGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&base).expect("create reload base");
        let mut config = Config::defaults_at(&base);
        config.change.scratch_dir = base.join("scratch");
        config.gate.cache_dir = base.join("gate");
        config.gate.lease_dir = base.join("leases");
        config.docs.cache_dir = base.join("docs");
        config.telemetry.enabled = false;
        config.telemetry.path = base.join("activity.jsonl");
        let guard =
            Arc::new(RootGuard::new([base.clone()], std::iter::empty()).expect("root guard"));
        let service = ChangeService::new(config, guard, ProcessSupervisor::without_journal())
            .expect("change service");
        let id = "ch-reload-1";
        let mut record = placeholder_record(id, &base, 1);
        record.state = RecordState::Ready;
        record.base_identity = "base".to_owned();
        record.revision = 1;
        service.store.save(&mut record).expect("save record");
        let pinned = service.store.load(id).expect("load").expect("present");

        let unchanged = service
            .reload_pinned_record(ChangeAction::Validate, id, &pinned)
            .expect("unchanged record");
        assert_eq!(unchanged.revision, 1);

        let mut bumped = pinned.clone();
        bumped.revision = 2;
        service.store.save(&mut bumped).expect("save bump");
        let outcome = service
            .reload_pinned_record(ChangeAction::Validate, id, &pinned)
            .expect_err("a bumped revision must not be verified as pinned");
        assert_eq!(outcome.status, "STALE");
        assert!(!outcome.data.verified);
        // Windows: cap-std opens an authorized root without FILE_SHARE_DELETE,
        // so the guard's directory handle (owned by the service) must be closed
        // before the scratch tree can be removed.
        drop(service);
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn cargo_target_config_candidates_are_detected() {
        let value: toml::Value = toml::from_str(
            "[build]\ntarget-dir = \"/outside/target\"\n\n[env]\nCARGO_TARGET_DIR = \"relative\"\n",
        )
        .expect("parse toml");
        let candidates = cargo_target_candidates(&value);
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|value| value == "/outside/target"));
    }

    #[test]
    fn evidence_freshness_requires_terminal_clean_uncancelled_steps() {
        let mut evidence = GateEvidence::pending(
            "test-job",
            &GateRequest::without_directory(GateTargetId::Check),
        );
        evidence.status = GateStatus::FastPass;
        evidence.steps.push(crate::gate::GateStepResult {
            evidence: crate::diagnostics::EvidenceStats::default(),
            diagnostics_omitted: 0,
            contexts: Vec::new(),
            target: GateTargetId::Check,
            command: "cargo check".to_owned(),
            exit_code: 0,
            signal: None,
            timed_out: false,
            cancelled: false,
            duration_ms: 1,
            first_diagnostic_ms: None,
            diagnostics: Vec::new(),
            suggestion_package: None,
            tail: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            output_truncated: false,
            drain_complete: true,
            cleanup_complete: true,
            build: None,
        });
        let token = CancellationToken::new();
        assert!(evidence_freshness(&evidence, &token));
        token.cancel();
        assert!(!evidence_freshness(&evidence, &token));
        evidence.steps[0].drain_complete = false;
        assert!(!evidence_freshness(&evidence, &CancellationToken::new()));
    }

    fn feedback_row(revision: u64, fresh: bool) -> StoredEvidence {
        StoredEvidence {
            revision,
            target: "check".to_owned(),
            command: "cargo check".to_owned(),
            status: "FAIL".to_owned(),
            exit_code: Some(1),
            first_diagnostic_ms: None,
            total_ms: 1,
            fresh,
            authoritative: fresh,
            recorded_at_ms: 1,
            diagnostics: vec![ChangeDiagnosticData {
                code: Some("E0384".to_owned()),
                level: "error".to_owned(),
                file: Some("src/lib.rs".to_owned()),
                line: Some(1),
                message: "cannot assign twice".to_owned(),
            }],
            diagnostics_total: 1,
            diagnostics_omitted: 0,
            suggestion_package: Some(ChangeSuggestionPackageData {
                patches: vec![ChangeSuggestionPatchData {
                    file: "src/lib.rs".to_owned(),
                    old_string: "let x".to_owned(),
                    new_string: "let mut x".to_owned(),
                }],
                skipped: Vec::new(),
                unsupported: 1,
                patches_total: 1,
                skipped_total: 0,
                truncated: false,
            }),
            stats: crate::diagnostics::EvidenceStats {
                build_success: Some(false),
                ..crate::diagnostics::EvidenceStats::default()
            },
        }
    }

    #[test]
    fn only_current_revision_fresh_rows_expose_compiler_feedback() {
        let mut record = placeholder_record("ch-feedback-1", Path::new("/tmp/agz-feedback"), 1);
        record.state = RecordState::Ready;
        record.revision = 2;
        record.evidence.push(feedback_row(1, false));
        record.evidence.push(feedback_row(2, true));
        // A hypothetical old row that is still flagged fresh must be hidden by
        // the revision gate, not by the storage cleanup alone.
        record.evidence.push(feedback_row(1, true));

        let data = data_for_record(ChangeAction::Inspect, &record, None);
        let rows = &data.evidence;

        assert!(rows[0].diagnostics.is_empty());
        assert!(rows[0].suggestion_package.is_none());
        assert_eq!(rows[0].diagnostics_total, 0);
        assert_eq!(rows[0].stats.build_success, Some(false));

        assert_eq!(rows[1].diagnostics.len(), 1);
        assert_eq!(rows[1].diagnostics_total, 1);
        let package = rows[1]
            .suggestion_package
            .as_ref()
            .expect("current-row package");
        assert_eq!(package.unsupported, 1);
        assert_eq!(package.patches.len(), 1);

        assert!(rows[2].fresh);
        assert!(rows[2].revision != record.revision);
        assert!(
            rows[2].diagnostics.is_empty() && rows[2].suggestion_package.is_none(),
            "feedback is bound to the current revision: {:#?}",
            rows[2]
        );

        let mut superseded = feedback_row(2, true);
        superseded.supersede();
        assert!(!superseded.fresh);
        assert!(superseded.diagnostics.is_empty());
        assert_eq!(superseded.diagnostics_total, 0);
        assert!(superseded.suggestion_package.is_none());
        assert_eq!(superseded.stats.build_success, Some(false));
    }

    #[test]
    fn suggestion_package_limits_surface_as_visible_truncation() {
        let patch = |old: String, new: String| crate::gate::SuggestionPatch {
            file: "src/lib.rs".to_owned(),
            old_string: old,
            new_string: new,
        };
        let many = SuggestionPackage {
            patches: (0..40)
                .map(|_| patch("a".repeat(64), "b".repeat(64)))
                .collect(),
            skipped: (0..40).map(|index| format!("reason {index}")).collect(),
        };
        let bounded = bounded_suggestion_package(&many, 7);
        assert_eq!(bounded.patches.len(), MAX_LISTED_SUGGESTION_PATCHES);
        assert_eq!(bounded.skipped.len(), MAX_LISTED_SUGGESTION_SKIPPED);
        assert_eq!(bounded.patches_total, 40);
        assert_eq!(bounded.skipped_total, 40);
        assert_eq!(bounded.unsupported, 7);
        assert!(bounded.truncated);

        let oversized = SuggestionPackage {
            patches: vec![
                patch("x".repeat(40_000), "y".repeat(40_000)),
                patch("a".to_owned(), "b".to_owned()),
            ],
            skipped: Vec::new(),
        };
        let bounded = bounded_suggestion_package(&oversized, 0);
        assert_eq!(
            bounded.patches.len(),
            1,
            "the per-row byte budget must drop only oversized patches: {bounded:#?}"
        );
        assert_eq!(bounded.patches[0].old_string, "a");
        assert_eq!(bounded.patches_total, 2);
        assert!(bounded.truncated);
    }
}
