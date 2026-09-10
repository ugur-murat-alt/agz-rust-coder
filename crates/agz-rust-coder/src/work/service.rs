//! Bounded work executor service.
//!
//! One work item orchestrates only the existing validated domain APIs: it
//! creates or adopts a revision-bound `change`, stages host candidate patches,
//! and runs the requested acceptance gates as `change(action=validate)` calls.
//! A failed gate produces a bounded [`WorkHandoffData`] decision package with a
//! single-use, revision-bound continuation token instead of an unbounded repair
//! loop. Work records are intentionally in-memory (MVP); the bound change
//! scratch is cleaned by its own TTL sweep.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use schemars::schema_for;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::{
    change::{
        ChangeData, ChangeEvidenceData, ChangeRequest, ChangeService, NewFileInput, PatchInput,
    },
    config::{Config, ToolConfig, WorkConfig},
    gate::{GateDetail, ProgressCallback},
    workspace::WorkspaceRoot,
};

use super::model::{
    MAX_ACCEPTANCE_GATES, MAX_CANDIDATE_NEW_FILES, MAX_CANDIDATE_PATCHES, MAX_CONTRACT_CHARS,
    MAX_LISTED_HANDOFF_DIAGNOSTICS, MAX_LISTED_WORK_EVIDENCE, MAX_REQUIRED_TOOLS,
    MAX_SCOPE_PATH_CHARS, MAX_SCOPE_PATHS, MAX_STOP_CONDITION_CHARS, MAX_UNRESOLVED_OBLIGATIONS,
    MAX_WORK_RECORDS, WORK_ID_PREFIX, WorkAction, WorkBudget, WorkBudgetData, WorkBudgetUsedData,
    WorkCandidateInput, WorkConstraints, WorkData, WorkEvidenceData, WorkGate, WorkHandoffData,
    WorkIntent, WorkOutcome, WorkRequest, WorkTemplate, WorkTool,
};

const LOCK_STRIPES: usize = 64;
const MAX_RECORD_WARNINGS: usize = 16;
const MAX_OBLIGATION_CHARS: usize = 256;

/// Lifecycle state of one work item. Transient states are published so an
/// `inspect` during a long validation observes progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkState {
    Planned,
    Collecting,
    Staging,
    Validating,
    NeedsModel,
    Ready,
    Blocked,
    Failed,
    Cancelled,
}

impl WorkState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Collecting => "collecting",
            Self::Staging => "staging",
            Self::Validating => "validating",
            Self::NeedsModel => "needs_model",
            Self::Ready => "ready",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Ready | Self::Blocked | Self::Failed | Self::Cancelled
        )
    }
}

/// One issued host handoff. Only the token hash is retained after issue.
#[derive(Debug, Clone)]
struct HandoffToken {
    token_hash: String,
    change_id: String,
    revision: u64,
    patch_hash: String,
    expires_at_ms: u64,
    used: bool,
}

/// In-memory work record. Never persisted; bounded by `MAX_WORK_RECORDS`.
#[derive(Debug, Clone)]
struct WorkRecord {
    id: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    workspace_root: PathBuf,
    workspace_epoch: u64,
    state: WorkState,
    intent: WorkIntent,
    budget: WorkBudget,
    change_id: Option<String>,
    base_identity: Option<String>,
    revision: u64,
    patch_hash: Option<String>,
    candidates: u64,
    compiles: u64,
    handoffs: u64,
    tried_fingerprints: Vec<String>,
    last_failure_fingerprint: Option<String>,
    token: Option<HandoffToken>,
    handoff: Option<WorkHandoffData>,
    evidence: Vec<WorkEvidenceData>,
    reason: String,
    warnings: Vec<String>,
    /// Cancelled by `work(action=cancel)` or by a bridged request token.
    cancellation: CancellationToken,
}

impl WorkRecord {
    fn block(&mut self, reason: impl Into<String>) {
        self.state = WorkState::Blocked;
        self.reason = reason.into();
    }

    fn fail(&mut self, reason: impl Into<String>) {
        self.state = WorkState::Failed;
        self.reason = reason.into();
    }

    fn push_warning(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if warning.is_empty() || self.warnings.contains(&warning) {
            return;
        }
        if self.warnings.len() >= MAX_RECORD_WARNINGS {
            self.warnings.remove(0);
        }
        self.warnings.push(warning);
    }

    fn push_evidence(&mut self, row: WorkEvidenceData) {
        self.evidence.push(row);
        if self.evidence.len() > MAX_LISTED_WORK_EVIDENCE {
            self.evidence.remove(0);
        }
    }

    fn deadline_ms(&self) -> u64 {
        self.created_at_ms.saturating_add(self.budget.wall_time_ms)
    }
}

/// Bounded work executor over a `change` service. When the `change` tool is
/// disabled the service is still constructed but every action fails closed.
pub struct WorkService {
    work: WorkConfig,
    tools: ToolConfig,
    change: Option<Arc<ChangeService>>,
    records: Mutex<BTreeMap<String, WorkRecord>>,
    locks: Vec<Arc<AsyncMutex<()>>>,
    sequence: AtomicU64,
}

impl fmt::Debug for WorkService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkService")
            .field("has_change", &self.change.is_some())
            .field("max_active", &self.work.max_active)
            .finish_non_exhaustive()
    }
}

impl WorkService {
    pub fn new(config: &Config, change: Option<Arc<ChangeService>>) -> Self {
        Self {
            work: config.work.clone(),
            tools: config.tools.clone(),
            change,
            records: Mutex::new(BTreeMap::new()),
            locks: (0..LOCK_STRIPES)
                .map(|_| Arc::new(AsyncMutex::new(())))
                .collect(),
            sequence: AtomicU64::new(0),
        }
    }

    /// Executes one work action. `workspace` is the request-authorized root;
    /// all candidate bytes stay in the change service's server-owned scratch.
    pub async fn execute(
        &self,
        request: WorkRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        progress: Option<ProgressCallback>,
    ) -> WorkOutcome {
        match request.action {
            WorkAction::Start => self.start(request, workspace, cancellation, progress).await,
            WorkAction::Resume => {
                self.resume(request, workspace, cancellation, progress)
                    .await
            }
            WorkAction::Inspect => self.inspect(request, workspace),
            WorkAction::Cancel => self.cancel(request, workspace),
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

    fn next_work_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!(
            "{WORK_ID_PREFIX}{:x}-{:x}-{:x}",
            now_ms(),
            std::process::id(),
            sequence
        )
    }

    fn load_record(&self, id: &str) -> Option<WorkRecord> {
        self.records
            .lock()
            .ok()
            .and_then(|records| records.get(id).cloned())
    }

    fn active_count(&self) -> u64 {
        self.records
            .lock()
            .map(|records| {
                records
                    .values()
                    .filter(|record| !record.state.is_terminal())
                    .count()
                    .try_into()
                    .unwrap_or(u64::MAX)
            })
            .unwrap_or(0)
    }

    fn insert(&self, record: WorkRecord) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        if records.len() >= MAX_WORK_RECORDS {
            let victim = records
                .iter()
                .filter(|(_, record)| record.state.is_terminal())
                .min_by_key(|(_, record)| record.updated_at_ms)
                .map(|(id, _)| id.clone());
            if let Some(victim) = victim {
                records.remove(&victim);
            }
        }
        records.insert(record.id.clone(), record);
    }

    /// Publishes a transient state without touching any other field, so an
    /// `inspect` during a long validation sees progress. A cancelled record is
    /// never revived.
    fn set_state(&self, id: &str, state: WorkState) {
        if let Ok(mut records) = self.records.lock()
            && let Some(record) = records.get_mut(id)
        {
            if record.state == WorkState::Cancelled {
                return;
            }
            record.state = state;
            record.updated_at_ms = now_ms();
        }
    }

    /// Writes the local record back. A concurrently cancelled record wins: the
    /// in-flight completion never overwrites `CANCELLED`.
    fn commit(&self, record: &WorkRecord) {
        if let Ok(mut records) = self.records.lock()
            && let Some(current) = records.get_mut(&record.id)
        {
            if current.state == WorkState::Cancelled {
                return;
            }
            let mut stored = record.clone();
            stored.cancellation = current.cancellation.clone();
            stored.updated_at_ms = now_ms();
            *current = stored;
        }
    }

    fn bridge(&self, request: CancellationToken, work: CancellationToken) -> Bridge {
        let combined = CancellationToken::new();
        let target = combined.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                () = request.cancelled() => target.cancel(),
                () = work.cancelled() => target.cancel(),
            }
        });
        Bridge {
            token: combined,
            handle,
        }
    }

    fn check_constraints(&self, constraints: &WorkConstraints) -> Result<(), String> {
        if constraints.required_tools.len() > MAX_REQUIRED_TOOLS {
            return Err(format!(
                "requiredTools accepts at most {MAX_REQUIRED_TOOLS} entries"
            ));
        }
        for tool in &constraints.required_tools {
            let enabled = match tool {
                WorkTool::Change => self.change.is_some() && self.tools.change,
                WorkTool::Check => true,
                WorkTool::Docs => self.tools.docs,
                WorkTool::Lsp => self.tools.lsp,
                WorkTool::Audit => self.tools.audit,
                WorkTool::CrateLookup => self.tools.crate_lookup,
            };
            if !enabled {
                return Err(format!(
                    "required tool '{}' is not enabled; work failed closed",
                    tool.as_str()
                ));
            }
            if constraints.offline && matches!(tool, WorkTool::Docs | WorkTool::CrateLookup) {
                return Err(format!(
                    "offline constraint conflicts with required network tool '{}'",
                    tool.as_str()
                ));
            }
        }
        Ok(())
    }

    async fn start(
        &self,
        request: WorkRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        progress: Option<ProgressCallback>,
    ) -> WorkOutcome {
        let Some(change) = self.change.clone() else {
            return self.refuse(
                WorkAction::Start,
                "BLOCKED",
                "The work executor cannot start.",
                "required tool 'change' is not enabled; no plan node is executed and the workspace is untouched",
                None,
            );
        };
        if request.work_id.is_some() || request.continuation_token.is_some() {
            return self.refuse(
                WorkAction::Start,
                "INVALID",
                "action=start does not accept workId or continuationToken.",
                "workId and continuationToken apply only to resume/inspect/cancel",
                None,
            );
        }
        let Some(intent) = request.intent.clone() else {
            return self.refuse(
                WorkAction::Start,
                "INVALID",
                "action=start requires intent.",
                "intent is required for action=start",
                None,
            );
        };
        if let Err(reason) = validate_intent(&intent) {
            return self.refuse(
                WorkAction::Start,
                "INVALID",
                "The work intent is invalid.",
                reason,
                None,
            );
        }
        if let Some(reason) = candidate_scope_error(&intent, &request.patches, &request.new_files) {
            return self.refuse(
                WorkAction::Start,
                "BLOCKED",
                "The host candidate is outside the declared intent scope.",
                reason,
                None,
            );
        }
        if let Err(reason) = self.check_constraints(&request.constraints) {
            return self.refuse(
                WorkAction::Start,
                "BLOCKED",
                "The work constraints cannot be satisfied.",
                reason,
                None,
            );
        }
        if self.active_count() >= self.work.max_active {
            return self.refuse(
                WorkAction::Start,
                "RESOURCE_BLOCKED",
                "The active work limit was reached.",
                format!(
                    "at most {} active work items are allowed; wait for a handoff or cancel one",
                    self.work.max_active
                ),
                None,
            );
        }
        if cancellation.is_cancelled() {
            return self.refuse(
                WorkAction::Start,
                "CANCELLED",
                "The work start was cancelled before any change was created.",
                "request cancellation arrived before the plan started",
                None,
            );
        }

        let id = self.next_work_id();
        let work_token = CancellationToken::new();
        let mut record = WorkRecord {
            id: id.clone(),
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            workspace_root: workspace.root().path().to_owned(),
            workspace_epoch: workspace.epoch(),
            state: WorkState::Planned,
            intent,
            budget: request.budget,
            change_id: None,
            base_identity: None,
            revision: 0,
            patch_hash: None,
            candidates: 0,
            compiles: 0,
            handoffs: 0,
            tried_fingerprints: Vec::new(),
            last_failure_fingerprint: None,
            token: None,
            handoff: None,
            evidence: Vec::new(),
            reason: String::new(),
            warnings: Vec::new(),
            cancellation: work_token.clone(),
        };
        self.insert(record.clone());
        let bridge = self.bridge(cancellation.clone(), work_token.clone());

        let mut current: Option<ChangeData> = None;
        match request.change_id.clone() {
            Some(change_id) => {
                record.state = WorkState::Collecting;
                self.set_state(&id, WorkState::Collecting);
                let inspected = change
                    .execute(
                        inspect_request(&change_id),
                        workspace,
                        bridge.token.clone(),
                        None,
                    )
                    .await;
                if inspected.status == "INSPECTED" {
                    let data = inspected.data;
                    if data.state == "ready" {
                        record.change_id = Some(change_id);
                        record.revision = data.revision;
                        record.base_identity = data.base_identity.clone();
                        record.patch_hash = data.patch_hash.clone();
                        current = Some(data);
                    } else {
                        record.fail(format!(
                            "the provided changeId is in state {} and cannot be extended",
                            data.state
                        ));
                    }
                } else {
                    record.fail(format!(
                        "the provided changeId could not be adopted: {} ({})",
                        inspected.status, inspected.data.reason
                    ));
                }
            }
            None => {
                record.state = WorkState::Collecting;
                self.set_state(&id, WorkState::Collecting);
                let created = change
                    .execute(create_request(), workspace, bridge.token.clone(), None)
                    .await;
                record
                    .warnings
                    .extend(created.data.cleanup_warnings.iter().cloned());
                match created.status {
                    "CREATED" => {
                        record.change_id = created.data.change_id.clone();
                        record.base_identity = created.data.base_identity.clone();
                        record.patch_hash = created.data.patch_hash.clone();
                    }
                    "RESOURCE_BLOCKED" => record.block(created.data.reason.clone()),
                    "CANCELLED" => {
                        record.state = WorkState::Cancelled;
                        record.reason = "work was cancelled during change capture".to_owned();
                    }
                    other => record.fail(format!(
                        "change capture failed with status {other}: {}",
                        created.data.reason
                    )),
                }
            }
        }

        if !matches!(
            record.state,
            WorkState::Failed | WorkState::Blocked | WorkState::Cancelled
        ) {
            self.drive(
                &mut record,
                &change,
                workspace,
                &bridge.token,
                progress,
                current.as_ref(),
                &request.patches,
                &request.new_files,
            )
            .await;
        }
        self.finalize(&mut record, &work_token);
        self.outcome_for_record(record, WorkAction::Start)
    }

    #[allow(clippy::too_many_arguments)]
    async fn resume(
        &self,
        request: WorkRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        progress: Option<ProgressCallback>,
    ) -> WorkOutcome {
        let Some(change) = self.change.clone() else {
            return self.refuse(
                WorkAction::Resume,
                "BLOCKED",
                "The work executor cannot resume.",
                "required tool 'change' is not enabled; the continuation token was not consumed",
                request.work_id.as_deref(),
            );
        };
        let Some(work_id) = request.work_id.clone() else {
            return self.refuse(
                WorkAction::Resume,
                "INVALID",
                "action=resume requires workId.",
                "workId is required for action=resume",
                None,
            );
        };
        if !is_valid_work_id(&work_id) {
            return self.refuse(
                WorkAction::Resume,
                "INVALID",
                "The work id is not valid.",
                "workId is not a valid server-issued id",
                Some(&work_id),
            );
        }
        let Some(token) = request.continuation_token.clone() else {
            return self.refuse(
                WorkAction::Resume,
                "INVALID",
                "action=resume requires continuationToken.",
                "continuationToken is required for action=resume",
                Some(&work_id),
            );
        };
        let _stripe = self.stripe(&work_id).lock_owned().await;
        let Some(mut record) = self.load_record(&work_id) else {
            return self.refuse(
                WorkAction::Resume,
                "NOT_FOUND",
                "The work id is unknown.",
                "no in-memory work record exists (records are not persisted across restarts)",
                Some(&work_id),
            );
        };
        if record.workspace_root != workspace.root().path()
            || record.workspace_epoch != workspace.epoch()
        {
            return self.finish_error(
                record,
                WorkAction::Resume,
                "STALE",
                "The work belongs to a different workspace or authorization epoch.",
                "workspace identity or root epoch changed; the work was not resumed",
            );
        }
        if record.state != WorkState::NeedsModel {
            return self.finish_error(
                record,
                WorkAction::Resume,
                "STALE",
                "The work has no active handoff.",
                "the continuation token was already consumed or superseded",
            );
        }
        let Some(stored) = record.token.as_ref() else {
            return self.finish_error(
                record,
                WorkAction::Resume,
                "STALE",
                "The work has no active continuation token.",
                "no handoff token is recorded for this work",
            );
        };
        let stored_hash = stored.token_hash.clone();
        let stored_revision = stored.revision;
        let stored_patch_hash = stored.patch_hash.clone();
        let stored_change_id = stored.change_id.clone();
        let expires_at_ms = stored.expires_at_ms;
        let used = stored.used;
        if used {
            return self.reject(
                &record,
                WorkAction::Resume,
                "STALE",
                "The continuation token was already used.",
                "continuation tokens are single-use; run action=inspect for the current state",
            );
        }
        if sha256_hex(token.as_bytes()) != stored_hash {
            return self.reject(
                &record,
                WorkAction::Resume,
                "STALE",
                "The continuation token is not valid for this work.",
                "the continuation token does not match the recorded handoff",
            );
        }
        if now_ms() > expires_at_ms {
            if let Some(stored) = record.token.as_mut() {
                stored.used = true;
            }
            record.block("the continuation token expired before a candidate was provided");
            self.commit(&record);
            return self.outcome_for_record(record, WorkAction::Resume);
        }
        // Consume the token before any staging; a failed candidate attempt never
        // revives it.
        if let Some(stored) = record.token.as_mut() {
            stored.used = true;
        }

        // Re-read the bound change: an external stage moves the revision and
        // makes this continuation stale instead of verifying newer bytes.
        let inspected = change
            .execute(
                inspect_request(&stored_change_id),
                workspace,
                cancellation.clone(),
                None,
            )
            .await;
        let binding_current = inspected.status == "INSPECTED"
            && inspected.data.revision == stored_revision
            && inspected.data.patch_hash.as_deref() == Some(stored_patch_hash.as_str())
            && record.change_id.as_deref() == Some(stored_change_id.as_str());
        if !binding_current {
            return self.finish_error(
                record,
                WorkAction::Resume,
                "STALE",
                "The bound change moved since the handoff was issued.",
                "the continuation token is revision-bound; the change revision no longer matches",
            );
        }
        let current = inspected.data;
        if request.intent.is_some()
            || !request.constraints.required_tools.is_empty()
            || request.constraints.offline
        {
            record.push_warning(
                "resume uses the recorded intent and constraints; the provided values were ignored",
            );
        }
        if request.patches.is_empty() && request.new_files.is_empty() {
            record.block(
                "a resume must provide at least one candidate patch or new file; the token was consumed",
            );
            self.commit(&record);
            return self.outcome_for_record(record, WorkAction::Resume);
        }
        if let Some(reason) = self.stop_before_candidate(&record) {
            record.block(reason);
            self.commit(&record);
            return self.outcome_for_record(record, WorkAction::Resume);
        }

        let bridge = self.bridge(cancellation.clone(), record.cancellation.clone());
        self.drive(
            &mut record,
            &change,
            workspace,
            &bridge.token,
            progress,
            Some(&current),
            &request.patches,
            &request.new_files,
        )
        .await;
        let work_token = record.cancellation.clone();
        self.finalize(&mut record, &work_token);
        self.outcome_for_record(record, WorkAction::Resume)
    }

    fn inspect(&self, request: WorkRequest, workspace: &WorkspaceRoot) -> WorkOutcome {
        let record = match (request.work_id.as_deref(), request.change_id.as_deref()) {
            (Some(id), _) if !id.is_empty() => match self.load_record(id) {
                Some(record) => record,
                None => {
                    return self.refuse(
                        WorkAction::Inspect,
                        "NOT_FOUND",
                        "The work id is unknown.",
                        "no in-memory work record exists",
                        Some(id),
                    );
                }
            },
            (_, Some(change_id)) if !change_id.is_empty() => {
                let found = {
                    let records = self.records.lock().ok();
                    records.as_ref().and_then(|records| {
                        records
                            .values()
                            .filter(|record| record.change_id.as_deref() == Some(change_id))
                            .max_by_key(|record| record.updated_at_ms)
                            .cloned()
                    })
                };
                match found {
                    Some(record) => record,
                    None => {
                        return self.refuse(
                            WorkAction::Inspect,
                            "NOT_FOUND",
                            "No work is bound to the change id.",
                            "no in-memory work record references this changeId",
                            None,
                        );
                    }
                }
            }
            _ => {
                return self.refuse(
                    WorkAction::Inspect,
                    "INVALID",
                    "action=inspect requires workId or changeId.",
                    "workId or changeId is required",
                    None,
                );
            }
        };
        if record.workspace_root != workspace.root().path()
            || record.workspace_epoch != workspace.epoch()
        {
            return self.finish_error(
                record,
                WorkAction::Inspect,
                "STALE",
                "The work belongs to a different workspace or authorization epoch.",
                "workspace identity or root epoch changed; the record was not returned",
            );
        }
        let data = self.data_for_record("inspect", &record);
        WorkOutcome {
            status: "INSPECTED",
            summary: "Returned the bounded work record.".to_owned(),
            is_error: false,
            data,
        }
    }

    fn cancel(&self, request: WorkRequest, workspace: &WorkspaceRoot) -> WorkOutcome {
        let Some(work_id) = request.work_id.clone() else {
            return self.refuse(
                WorkAction::Cancel,
                "INVALID",
                "action=cancel requires workId.",
                "workId is required for action=cancel",
                None,
            );
        };
        let Some(mut record) = ({
            let Ok(records) = self.records.lock() else {
                return self.refuse(
                    WorkAction::Cancel,
                    "UNAVAILABLE",
                    "The work record store is unavailable.",
                    "the in-memory record lock could not be acquired",
                    Some(&work_id),
                );
            };
            records.get(&work_id).cloned()
        }) else {
            return self.refuse(
                WorkAction::Cancel,
                "NOT_FOUND",
                "The work id is unknown.",
                "no in-memory work record exists",
                Some(&work_id),
            );
        };
        if record.workspace_root != workspace.root().path()
            || record.workspace_epoch != workspace.epoch()
        {
            let reason = "workspace identity or root epoch changed; cancel was refused".to_owned();
            return self.finish_error(
                record,
                WorkAction::Cancel,
                "STALE",
                "The work belongs to a different workspace or authorization epoch.",
                reason,
            );
        }
        if record.state.is_terminal() {
            if record.reason.is_empty() {
                record.reason = "the work was already terminal; cancel is idempotent".to_owned();
            }
        } else {
            // Cancelling the stored token aborts any in-flight change
            // validation for this work; the change scratch itself is left to
            // the change TTL sweep.
            record.cancellation.cancel();
            record.state = WorkState::Cancelled;
            record.reason = "work cancelled by the host; the bound change validation was cancelled and its scratch is left for TTL cleanup".to_owned();
        }
        self.commit(&record);
        let data = self.data_for_record("cancel", &record);
        WorkOutcome {
            status: "CANCELLED",
            summary: "The work was cancelled.".to_owned(),
            is_error: false,
            data,
        }
    }

    /// Runs one host candidate: stage, then every acceptance gate in order.
    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        record: &mut WorkRecord,
        change: &Arc<ChangeService>,
        workspace: &WorkspaceRoot,
        cancellation: &CancellationToken,
        progress: Option<ProgressCallback>,
        current: Option<&ChangeData>,
        patches: &[PatchInput],
        new_files: &[NewFileInput],
    ) {
        if patches.is_empty() && new_files.is_empty() {
            self.handoff_from_current(record, current);
            return;
        }
        if let Some(reason) = candidate_scope_error(&record.intent, patches, new_files) {
            record.block(reason);
            return;
        }
        if let Some(reason) = self.stop_before_candidate(record) {
            record.block(reason);
            return;
        }
        let fingerprint = candidate_fingerprint(patches, new_files);
        if record
            .tried_fingerprints
            .iter()
            .any(|tried| tried == &fingerprint)
        {
            record
                .block("no progress: the host candidate is byte-identical to an earlier candidate");
            return;
        }
        let Some(change_id) = record.change_id.clone() else {
            record.fail("the work has no bound change to stage into");
            return;
        };
        let base = record.base_identity.clone().unwrap_or_default();
        record.state = WorkState::Staging;
        self.set_state(&record.id, WorkState::Staging);
        let staged = change
            .execute(
                stage_request(
                    &change_id,
                    record.revision,
                    &base,
                    patches.to_vec(),
                    new_files.to_vec(),
                ),
                workspace,
                cancellation.clone(),
                None,
            )
            .await;
        record
            .warnings
            .extend(staged.data.cleanup_warnings.iter().cloned());
        match staged.status {
            "STAGED" => {}
            "CANCELLED" => {
                record.state = WorkState::Cancelled;
                record.reason = "work was cancelled during candidate staging".to_owned();
                return;
            }
            other => {
                record.fail(format!(
                    "candidate staging failed with status {other}: {}",
                    staged.data.reason
                ));
                return;
            }
        }
        record.tried_fingerprints.push(fingerprint);
        record.candidates = record.candidates.saturating_add(1);
        record.revision = staged.data.revision;
        record.patch_hash = staged.data.patch_hash.clone();
        if let Some(base_identity) = staged.data.base_identity.clone() {
            record.base_identity = Some(base_identity);
        }

        record.state = WorkState::Validating;
        self.set_state(&record.id, WorkState::Validating);
        for gate in unique_gates(&record.intent) {
            if cancellation.is_cancelled() {
                record.state = WorkState::Cancelled;
                record.reason = "work was cancelled during gate validation".to_owned();
                return;
            }
            if let Some(reason) = self.stop_before_compile(record) {
                record.block(reason);
                return;
            }
            record.compiles = record.compiles.saturating_add(1);
            let outcome = change
                .execute(
                    validate_request(
                        &change_id,
                        record.revision,
                        record.base_identity.as_deref().unwrap_or_default(),
                        gate,
                    ),
                    workspace,
                    cancellation.clone(),
                    progress.clone(),
                )
                .await;
            record
                .warnings
                .extend(outcome.data.cleanup_warnings.iter().cloned());
            if let Some(row) = select_row(&outcome.data, gate) {
                record.push_evidence(evidence_row(gate, row));
            }
            match outcome.status {
                "PASS" => {}
                "FAIL" => {
                    let Some(row) = select_row(&outcome.data, gate).cloned() else {
                        record.fail(format!(
                            "gate {} failed without a fresh evidence row",
                            gate.as_str()
                        ));
                        return;
                    };
                    self.handoff_after_failure(record, gate, &row);
                    return;
                }
                "CANCELLED" => {
                    record.state = WorkState::Cancelled;
                    record.reason = "work was cancelled during gate validation".to_owned();
                    return;
                }
                "TIMEOUT" => {
                    record.block(format!(
                        "gate {} timed out before producing usable evidence",
                        gate.as_str()
                    ));
                    return;
                }
                other => {
                    record.fail(format!(
                        "gate {} returned {other}: {}",
                        gate.as_str(),
                        outcome.data.reason
                    ));
                    return;
                }
            }
        }
        record.state = WorkState::Ready;
        record.reason = format!(
            "all {} requested acceptance gate(s) passed on revision {}; this is requested-gate evidence on the current revision, not behavior proof",
            record.intent.acceptance_gates.len(),
            record.revision
        );
    }

    /// No host candidate was provided. Reuse current-revision fresh failure
    /// evidence when the adopted change already carries it, then issue a
    /// bounded handoff instead of guessing or running anything.
    fn handoff_from_current(&self, record: &mut WorkRecord, current: Option<&ChangeData>) {
        let failure = current.and_then(|data| {
            data.evidence
                .iter()
                .rev()
                .find(|row| row.revision == data.revision && row.fresh && row.status == "FAIL")
        });
        if let Some(data) = current {
            record.revision = data.revision;
            record.patch_hash = data.patch_hash.clone();
            if data.base_identity.is_some() {
                record.base_identity = data.base_identity.clone();
            }
        }
        if let Some(reason) = self.stop_before_handoff(record) {
            record.block(reason);
            return;
        }
        record.handoffs = record.handoffs.saturating_add(1);
        let handoff = match failure {
            Some(row) => self.issue_token(
                record,
                "compile",
                row.diagnostics.clone(),
                row.diagnostics_total,
                row.diagnostics_omitted,
                row.suggestion_package.clone(),
            ),
            None => self.issue_token(record, "no_candidate", Vec::new(), 0, 0, None),
        };
        record.state = WorkState::NeedsModel;
        record.reason = if failure.is_some() {
            "the current revision has a fresh compile failure; a bounded repair handoff was issued"
                .to_owned()
        } else {
            "the intent declares a contract but no host candidate was provided; a bounded candidate handoff was issued"
                .to_owned()
        };
        record.handoff = Some(handoff);
    }

    fn handoff_after_failure(
        &self,
        record: &mut WorkRecord,
        gate: WorkGate,
        row: &ChangeEvidenceData,
    ) {
        if let Some(fingerprint) = failure_fingerprint(&row.diagnostics, row.diagnostics_total)
            && record.last_failure_fingerprint.as_deref() == Some(fingerprint.as_str())
        {
            record.block(
                "no progress: the new candidate failed with the same diagnostics as the previous handoff",
            );
            return;
        }
        if let Some(fingerprint) = failure_fingerprint(&row.diagnostics, row.diagnostics_total) {
            record.last_failure_fingerprint = Some(fingerprint);
        }
        if let Some(reason) = self.stop_before_handoff(record) {
            record.block(reason);
            return;
        }
        record.handoffs = record.handoffs.saturating_add(1);
        let failure_kind = if matches!(gate, WorkGate::Test) {
            "test"
        } else {
            "compile"
        };
        let handoff = self.issue_token(
            record,
            failure_kind,
            row.diagnostics.clone(),
            row.diagnostics_total,
            row.diagnostics_omitted,
            row.suggestion_package.clone(),
        );
        record.state = WorkState::NeedsModel;
        record.reason = format!(
            "gate {} failed on revision {}; a bounded decision package was issued",
            gate.as_str(),
            record.revision
        );
        record.handoff = Some(handoff);
    }

    fn issue_token(
        &self,
        record: &mut WorkRecord,
        failure_kind: &str,
        diagnostics: Vec<crate::change::ChangeDiagnosticData>,
        diagnostics_total: u64,
        diagnostics_omitted: u64,
        suggestion_package: Option<crate::change::ChangeSuggestionPackageData>,
    ) -> WorkHandoffData {
        let change_id = record.change_id.clone().unwrap_or_default();
        let patch_hash = record.patch_hash.clone().unwrap_or_default();
        let issued_at_ms = now_ms();
        let expires_at_ms = issued_at_ms.saturating_add(self.work.continuation_ttl_ms);
        let token = generate_token(
            &record.id,
            record.revision,
            &patch_hash,
            self.sequence.fetch_add(1, Ordering::Relaxed),
            issued_at_ms,
        );
        record.token = Some(HandoffToken {
            token_hash: sha256_hex(token.as_bytes()),
            change_id,
            revision: record.revision,
            patch_hash,
            expires_at_ms,
            used: false,
        });
        let diagnostics = diagnostics
            .into_iter()
            .take(MAX_LISTED_HANDOFF_DIAGNOSTICS)
            .collect::<Vec<_>>();
        let obligations = unresolved_obligations(record, suggestion_package.as_ref());
        WorkHandoffData {
            continuation_token: token,
            expires_at_ms,
            failure_kind: failure_kind.to_owned(),
            diagnostics,
            diagnostics_total,
            diagnostics_omitted,
            suggestion_package,
            unresolved_obligations: obligations,
            candidate_input_schema: candidate_input_schema().clone(),
        }
    }

    fn stop_before_candidate(&self, record: &WorkRecord) -> Option<String> {
        if now_ms() >= record.deadline_ms() {
            return Some(format!(
                "wall time budget exhausted (wallTimeMs={}) before a candidate could be staged",
                record.budget.wall_time_ms
            ));
        }
        if record.candidates >= record.budget.max_candidates {
            return Some(format!(
                "candidate budget exhausted (maxCandidates={}); stop condition: {}",
                record.budget.max_candidates,
                truncate(&record.intent.stop_condition, MAX_OBLIGATION_CHARS)
            ));
        }
        if record.compiles >= record.budget.max_compiles {
            return Some(format!(
                "compile budget exhausted (maxCompiles={}); stop condition: {}",
                record.budget.max_compiles,
                truncate(&record.intent.stop_condition, MAX_OBLIGATION_CHARS)
            ));
        }
        None
    }

    fn stop_before_compile(&self, record: &WorkRecord) -> Option<String> {
        if now_ms() >= record.deadline_ms() {
            return Some(format!(
                "wall time budget exhausted (wallTimeMs={}) before gate validation",
                record.budget.wall_time_ms
            ));
        }
        if record.compiles >= record.budget.max_compiles {
            return Some(format!(
                "compile budget exhausted (maxCompiles={}); stop condition: {}",
                record.budget.max_compiles,
                truncate(&record.intent.stop_condition, MAX_OBLIGATION_CHARS)
            ));
        }
        None
    }

    fn stop_before_handoff(&self, record: &WorkRecord) -> Option<String> {
        if record.handoffs >= record.budget.max_handoffs {
            return Some(format!(
                "handoff budget exhausted (maxHandoffs={}); stop condition: {}",
                record.budget.max_handoffs,
                truncate(&record.intent.stop_condition, MAX_OBLIGATION_CHARS)
            ));
        }
        if record.candidates >= record.budget.max_candidates {
            return Some(format!(
                "candidate budget exhausted (maxCandidates={}); no host candidate can be staged",
                record.budget.max_candidates
            ));
        }
        if record.compiles >= record.budget.max_compiles {
            return Some(format!(
                "compile budget exhausted (maxCompiles={}); no gate could be run for a new candidate",
                record.budget.max_compiles
            ));
        }
        if now_ms() >= record.deadline_ms() {
            return Some(format!(
                "wall time budget exhausted (wallTimeMs={}); no further handoff was issued",
                record.budget.wall_time_ms
            ));
        }
        None
    }

    /// A cancelled or bridged request always wins over a locally computed
    /// terminal state.
    fn finalize(&self, record: &mut WorkRecord, work_token: &CancellationToken) {
        if work_token.is_cancelled() && record.state != WorkState::Cancelled {
            record.state = WorkState::Cancelled;
            record.reason = "work was cancelled; no further plan nodes were executed".to_owned();
        }
        self.commit(record);
    }

    fn data_for_record(&self, action: &str, record: &WorkRecord) -> WorkData {
        let evidence = record
            .evidence
            .iter()
            .rev()
            .take(MAX_LISTED_WORK_EVIDENCE)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        let handoff = if record.state == WorkState::NeedsModel {
            record.handoff.clone()
        } else {
            None
        };
        WorkData {
            action: action.to_owned(),
            work_id: Some(record.id.clone()),
            state: record.state.as_str().to_owned(),
            template: Some(record.intent.template.as_str().to_owned()),
            change_id: record.change_id.clone(),
            revision: record.revision,
            patch_hash: record.patch_hash.clone(),
            acceptance_gates: unique_gates(&record.intent)
                .into_iter()
                .map(|gate| gate.as_str().to_owned())
                .collect(),
            scope_paths: record.intent.scope_paths.clone(),
            budget: WorkBudgetData {
                max_compiles: record.budget.max_compiles,
                max_candidates: record.budget.max_candidates,
                max_handoffs: record.budget.max_handoffs,
                wall_time_ms: record.budget.wall_time_ms,
            },
            used: WorkBudgetUsedData {
                candidates: record.candidates,
                compiles: record.compiles,
                handoffs: record.handoffs,
                elapsed_ms: now_ms().saturating_sub(record.created_at_ms),
            },
            handoff,
            evidence_total: record.evidence.len().try_into().unwrap_or(u64::MAX),
            evidence,
            reason: record.reason.clone(),
            warnings: record.warnings.clone(),
        }
    }

    fn outcome_for_record(&self, record: WorkRecord, action: WorkAction) -> WorkOutcome {
        let data = self.data_for_record(action.as_str(), &record);
        match record.state {
            WorkState::NeedsModel => WorkOutcome {
                status: "NEEDS_MODEL",
                summary: "A bounded host decision package was issued for the current revision."
                    .to_owned(),
                is_error: false,
                data,
            },
            WorkState::Ready => WorkOutcome {
                status: "READY",
                summary:
                    "All requested acceptance gates passed for the current revision; this is requested-gate evidence, not behavior proof."
                        .to_owned(),
                is_error: false,
                data,
            },
            WorkState::Blocked => WorkOutcome {
                status: "BLOCKED",
                summary: "The work stopped under its explicit policy or budget.".to_owned(),
                is_error: true,
                data,
            },
            WorkState::Cancelled => WorkOutcome {
                status: "CANCELLED",
                summary: "The work was cancelled; no further plan nodes were executed.".to_owned(),
                is_error: true,
                data,
            },
            _ => WorkOutcome {
                status: "FAILED",
                summary: "The work could not produce usable evidence.".to_owned(),
                is_error: true,
                data,
            },
        }
    }

    fn finish_error(
        &self,
        mut record: WorkRecord,
        action: WorkAction,
        status: &'static str,
        summary: impl Into<String>,
        reason: impl Into<String>,
    ) -> WorkOutcome {
        if !record.state.is_terminal() {
            record.state = if status == "BLOCKED" {
                WorkState::Blocked
            } else {
                WorkState::Failed
            };
        }
        record.reason = reason.into();
        self.commit(&record);
        WorkOutcome {
            status,
            summary: summary.into(),
            is_error: true,
            data: self.data_for_record(action.as_str(), &record),
        }
    }

    fn refuse(
        &self,
        action: WorkAction,
        status: &'static str,
        summary: impl Into<String>,
        reason: impl Into<String>,
        work_id: Option<&str>,
    ) -> WorkOutcome {
        let data = WorkData {
            action: action.as_str().to_owned(),
            work_id: work_id.map(str::to_owned),
            state: "absent".to_owned(),
            reason: reason.into(),
            ..WorkData::default()
        };
        WorkOutcome {
            status,
            summary: summary.into(),
            is_error: true,
            data,
        }
    }

    /// Rejects one request without changing the recorded work state. A stale or
    /// reused token must not invalidate the live handoff it failed to match.
    fn reject(
        &self,
        record: &WorkRecord,
        action: WorkAction,
        status: &'static str,
        summary: impl Into<String>,
        reason: impl Into<String>,
    ) -> WorkOutcome {
        let mut data = self.data_for_record(action.as_str(), record);
        data.reason = reason.into();
        WorkOutcome {
            status,
            summary: summary.into(),
            is_error: true,
            data,
        }
    }
}

/// Aborts the cancellation bridge when an action completes.
struct Bridge {
    token: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn create_request() -> ChangeRequest {
    ChangeRequest {
        action: crate::change::ChangeAction::Create,
        change_id: None,
        expected_revision: None,
        base_identity: None,
        patches: Vec::new(),
        new_files: Vec::new(),
        migration: None,
        target: crate::gate::GateTargetId::Check,
        options: crate::gate::ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn inspect_request(change_id: &str) -> ChangeRequest {
    ChangeRequest {
        action: crate::change::ChangeAction::Inspect,
        change_id: Some(change_id.to_owned()),
        ..create_request()
    }
}

fn stage_request(
    change_id: &str,
    revision: u64,
    base_identity: &str,
    patches: Vec<PatchInput>,
    new_files: Vec<NewFileInput>,
) -> ChangeRequest {
    ChangeRequest {
        action: crate::change::ChangeAction::Stage,
        change_id: Some(change_id.to_owned()),
        expected_revision: Some(revision),
        base_identity: Some(base_identity.to_owned()),
        patches,
        new_files,
        ..create_request()
    }
}

fn validate_request(
    change_id: &str,
    revision: u64,
    base_identity: &str,
    gate: WorkGate,
) -> ChangeRequest {
    ChangeRequest {
        action: crate::change::ChangeAction::Validate,
        change_id: Some(change_id.to_owned()),
        expected_revision: Some(revision),
        base_identity: Some(base_identity.to_owned()),
        target: gate.target(),
        detail: GateDetail::Standard,
        ..create_request()
    }
}

fn validate_intent(intent: &WorkIntent) -> Result<(), String> {
    if intent.scope_paths.is_empty() {
        return Err("intent.scopePaths must declare the candidate target scope".to_owned());
    }
    if intent.scope_paths.len() > MAX_SCOPE_PATHS {
        return Err(format!(
            "intent.scopePaths accepts at most {MAX_SCOPE_PATHS} paths"
        ));
    }
    for path in &intent.scope_paths {
        validate_scope_path(path)?;
    }
    if intent.contract.trim().is_empty() {
        return Err("intent.contract cannot be empty".to_owned());
    }
    if intent.contract.chars().count() > MAX_CONTRACT_CHARS {
        return Err(format!(
            "intent.contract accepts at most {MAX_CONTRACT_CHARS} characters"
        ));
    }
    if intent.stop_condition.trim().is_empty() {
        return Err("intent.stopCondition cannot be empty".to_owned());
    }
    if intent.stop_condition.chars().count() > MAX_STOP_CONDITION_CHARS {
        return Err(format!(
            "intent.stopCondition accepts at most {MAX_STOP_CONDITION_CHARS} characters"
        ));
    }
    if intent.acceptance_gates.is_empty() {
        return Err("intent.acceptanceGates must not be empty".to_owned());
    }
    if intent.acceptance_gates.len() > MAX_ACCEPTANCE_GATES {
        return Err(format!(
            "intent.acceptanceGates accepts at most {MAX_ACCEPTANCE_GATES} gates"
        ));
    }
    if intent.change_budget.max_patches == 0 || intent.change_budget.max_patches > 512 {
        return Err("intent.changeBudget.maxPatches must be between 1 and 512".to_owned());
    }
    if intent.change_budget.max_new_files > 512 {
        return Err("intent.changeBudget.maxNewFiles must be between 0 and 512".to_owned());
    }
    if intent.template == WorkTemplate::RepairCompileFailure
        && !intent
            .acceptance_gates
            .iter()
            .any(|gate| !matches!(gate, WorkGate::Fmt))
    {
        return Err(
            "repair_compile_failure requires at least one compiler-producing acceptance gate"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_scope_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("intent.scopePaths entries cannot be empty".to_owned());
    }
    if path.chars().count() > MAX_SCOPE_PATH_CHARS {
        return Err(format!(
            "intent.scopePaths entries accept at most {MAX_SCOPE_PATH_CHARS} characters"
        ));
    }
    if path.chars().any(char::is_control) {
        return Err("intent.scopePaths entries cannot contain control characters".to_owned());
    }
    let parsed = Path::new(path);
    if parsed.is_absolute()
        || parsed.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || parsed.components().next().is_none()
    {
        return Err(format!(
            "intent.scopePaths entry '{path}' must be a workspace-relative path without parent traversal"
        ));
    }
    Ok(())
}

fn candidate_scope_error(
    intent: &WorkIntent,
    patches: &[PatchInput],
    new_files: &[NewFileInput],
) -> Option<String> {
    if patches.len() > MAX_CANDIDATE_PATCHES || new_files.len() > MAX_CANDIDATE_NEW_FILES {
        return Some(
            "candidate exceeds the executor's bounded patch/new-file input limit".to_owned(),
        );
    }
    let scopes = intent
        .scope_paths
        .iter()
        .filter_map(|scope| normalize_relative(scope))
        .collect::<Vec<_>>();
    let report = |file: &str| -> Option<String> {
        let Some(normalized) = normalize_relative(file) else {
            return Some(format!(
                "candidate file '{file}' is not a valid in-workspace relative path; work failed closed"
            ));
        };
        let covered = scopes.iter().any(|scope| {
            normalized == *scope
                || normalized
                    .strip_prefix(scope)
                    .is_ok_and(|relative| !relative.is_absolute())
        });
        (!covered).then(|| {
            format!(
                "candidate file '{file}' is outside the declared scopePaths; work failed closed instead of widening the plan"
            )
        })
    };
    for patch in patches {
        if let Some(reason) = report(&patch.file) {
            return Some(reason);
        }
    }
    for file in new_files {
        if let Some(reason) = report(&file.file) {
            return Some(reason);
        }
    }
    None
}

fn normalize_relative(path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in parsed.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn unique_gates(intent: &WorkIntent) -> Vec<WorkGate> {
    let mut gates = Vec::new();
    for gate in &intent.acceptance_gates {
        if !gates.contains(gate) {
            gates.push(*gate);
        }
    }
    gates
}

fn select_row<'a>(data: &'a ChangeData, gate: WorkGate) -> Option<&'a ChangeEvidenceData> {
    data.evidence.iter().rev().find(|row| {
        row.revision == data.revision
            && row.fresh
            && (gate == WorkGate::All || row.target == gate.as_str())
    })
}

fn evidence_row(gate: WorkGate, row: &ChangeEvidenceData) -> WorkEvidenceData {
    let status = match row.status.as_str() {
        "FAST_PASS" | "FULL_PASS" | "PASS" => "PASS",
        "FAIL" => "FAIL",
        other => other,
    };
    WorkEvidenceData {
        revision: row.revision,
        gate: gate.as_str().to_owned(),
        status: status.to_owned(),
        fresh: row.fresh,
        exit_code: row.exit_code,
        total_ms: row.total_ms,
        diagnostics_total: row.diagnostics_total,
    }
}

fn unresolved_obligations(
    record: &WorkRecord,
    package: Option<&crate::change::ChangeSuggestionPackageData>,
) -> Vec<String> {
    let mut obligations = Vec::new();
    obligations.push(format!(
        "contract: {}",
        truncate(&record.intent.contract, MAX_OBLIGATION_CHARS)
    ));
    obligations.push(format!(
        "stopCondition: {}",
        truncate(&record.intent.stop_condition, MAX_OBLIGATION_CHARS)
    ));
    for gate in unique_gates(&record.intent) {
        obligations.push(format!(
            "gate {} has no fresh pass on revision {}",
            gate.as_str(),
            record.revision
        ));
        if obligations.len() >= MAX_UNRESOLVED_OBLIGATIONS {
            return obligations;
        }
    }
    if let Some(package) = package {
        for skipped in package.skipped.iter().take(4) {
            if obligations.len() >= MAX_UNRESOLVED_OBLIGATIONS {
                break;
            }
            obligations.push(format!(
                "suggestion skipped: {}",
                truncate(skipped, MAX_OBLIGATION_CHARS)
            ));
        }
    }
    obligations
}

fn candidate_fingerprint(patches: &[PatchInput], new_files: &[NewFileInput]) -> String {
    let mut entries = patches
        .iter()
        .map(|patch| {
            (
                patch.file.clone(),
                patch.old_string.clone(),
                patch.new_string.clone(),
            )
        })
        .collect::<Vec<_>>();
    entries.sort();
    let mut files = new_files
        .iter()
        .map(|file| (file.file.clone(), sha256_hex(file.content.as_bytes())))
        .collect::<Vec<_>>();
    files.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-coder-work-candidate\0");
    for (file, old_string, new_string) in entries {
        hasher.update(file.as_bytes());
        hasher.update(b"\0");
        hasher.update(old_string.as_bytes());
        hasher.update(b"\0");
        hasher.update(new_string.as_bytes());
        hasher.update(b"\0");
    }
    for (file, digest) in files {
        hasher.update(file.as_bytes());
        hasher.update(b"\0");
        hasher.update(digest.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

fn failure_fingerprint(
    diagnostics: &[crate::change::ChangeDiagnosticData],
    diagnostics_total: u64,
) -> Option<String> {
    if diagnostics.is_empty() && diagnostics_total == 0 {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-coder-work-failure\0");
    hasher.update(diagnostics_total.to_string().as_bytes());
    hasher.update(b"\0");
    for diagnostic in diagnostics {
        hasher.update(diagnostic.code.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\0");
        hasher.update(diagnostic.file.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\0");
        hasher.update(diagnostic.line.unwrap_or(0).to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(diagnostic.message.as_bytes());
        hasher.update(b"\0");
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn candidate_input_schema() -> &'static serde_json::Value {
    static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::to_value(schema_for!(WorkCandidateInput)).unwrap_or(serde_json::Value::Null)
    })
}

fn generate_token(
    work_id: &str,
    revision: u64,
    patch_hash: &str,
    sequence: u64,
    issued_at_ms: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-coder-work-token\0");
    hasher.update(work_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(revision.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(patch_hash.as_bytes());
    hasher.update(b"\0");
    hasher.update(sequence.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(issued_at_ms.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn is_valid_work_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.starts_with(WORK_ID_PREFIX)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeDiagnosticData;

    fn intent() -> WorkIntent {
        WorkIntent {
            template: WorkTemplate::RepairCompileFailure,
            scope_paths: vec!["src".to_owned()],
            contract: "value() returns 2".to_owned(),
            stop_condition: "all gates pass".to_owned(),
            acceptance_gates: vec![WorkGate::Check],
            change_budget: super::super::model::WorkChangeBudget {
                max_patches: 8,
                max_new_files: 4,
            },
        }
    }

    #[test]
    fn scope_and_budget_validation_fail_closed() {
        let mut invalid = intent();
        invalid.scope_paths = vec!["../escape".to_owned()];
        assert!(validate_intent(&invalid).is_err());
        let mut invalid = intent();
        invalid.scope_paths = vec!["/etc".to_owned()];
        assert!(validate_intent(&invalid).is_err());
        let mut invalid = intent();
        invalid.contract = "   ".to_owned();
        assert!(validate_intent(&invalid).is_err());
        let mut invalid = intent();
        invalid.acceptance_gates = vec![WorkGate::Fmt];
        assert!(
            validate_intent(&invalid).is_err(),
            "repair requires a compiler-producing gate"
        );
        let mut invalid = intent();
        invalid.change_budget.max_patches = 0;
        assert!(validate_intent(&invalid).is_err());
        assert!(validate_intent(&intent()).is_ok());
    }

    #[test]
    fn candidate_scope_rejects_out_of_scope_files() {
        let patch = |file: &str| PatchInput {
            file: file.to_owned(),
            old_string: "a".to_owned(),
            new_string: "b".to_owned(),
        };
        assert!(candidate_scope_error(&intent(), &[patch("src/lib.rs")], &[]).is_none());
        assert!(candidate_scope_error(&intent(), &[patch("tests/it.rs")], &[]).is_some());
        assert!(candidate_scope_error(&intent(), &[patch("../escape.rs")], &[]).is_some());
    }

    #[test]
    fn fingerprints_are_deterministic_and_order_independent() {
        let first = PatchInput {
            file: "src/a.rs".to_owned(),
            old_string: "a".to_owned(),
            new_string: "b".to_owned(),
        };
        let second = PatchInput {
            file: "src/b.rs".to_owned(),
            old_string: "c".to_owned(),
            new_string: "d".to_owned(),
        };
        assert_eq!(
            candidate_fingerprint(&[first.clone(), second.clone()], &[]),
            candidate_fingerprint(&[second, first.clone()], &[])
        );
        assert_ne!(
            candidate_fingerprint(&[first], &[]),
            candidate_fingerprint(&[], &[])
        );
        let diagnostics = vec![ChangeDiagnosticData {
            code: Some("E0308".to_owned()),
            level: "error".to_owned(),
            file: Some("src/a.rs".to_owned()),
            line: Some(1),
            message: "mismatched types".to_owned(),
        }];
        assert_eq!(
            failure_fingerprint(&diagnostics, 1),
            failure_fingerprint(&diagnostics, 1)
        );
        assert!(failure_fingerprint(&[], 0).is_none());
    }

    #[test]
    fn tokens_are_single_use_and_bound_to_the_work() {
        let first = generate_token("wk-1-1-1", 1, "hash", 0, 10);
        let second = generate_token("wk-1-1-1", 1, "hash", 1, 10);
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_ne!(
            first,
            generate_token("wk-1-1-1", 2, "hash", 0, 10),
            "the token must bind the revision"
        );
    }
}
