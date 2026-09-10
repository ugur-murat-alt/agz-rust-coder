//! Bounded configuration-matrix planning and execution (`verify`).
//!
//! Planning derives candidate cells from `cargo metadata` plus explicit
//! project policy. Execution routes every runnable cell through
//! [`super::CheckService`] as an ordinary gate request; planning itself never
//! starts a Cargo build. No toolchain or target is downloaded, and no CI
//! workflow is executed as a shell; Cargo may still fetch crates according to
//! the existing network policy.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    config::VerifyConfig,
    gate::{
        GateDetail, GateEvidence, GateRequest, GateSource, GateStatus, GateTargetId,
        ProgressCallback, ProgressEvent, ProgressStage, TestRunner, ValidationOptions,
        validate_toolchain_name,
    },
    workspace::{AuthorizedRoot, ClientRoots, DirectoryEntryKind, WorkspaceSnapshot},
};

use super::CheckService;
use super::check::{resolve_toolchain_cargo, rustup_shim_available};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CI_FILES: usize = 10;
/// Upper bound for authorized reads of workflow and manifest configuration.
const MAX_CONFIG_BYTES: u64 = 65_536;
const CI_SUGGESTION_LIMIT: usize = 8;
const POLICY_FEATURE_GROUP_LIMIT: usize = 16;
const REQUEST_FEATURE_GROUP_LIMIT: usize = 16;
const MAX_GROUP_FEATURES: usize = 32;
const FEATURE_CLOSURE_LIMIT: usize = 256;
const CELL_DIAGNOSTIC_LIMIT: usize = 4;

/// Matrix action selected by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerifyAction {
    #[default]
    MatrixPlan,
    MatrixRun,
}

impl VerifyAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MatrixPlan => "matrix_plan",
            Self::MatrixRun => "matrix_run",
        }
    }
}

/// A development stage executed by one matrix cell.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum VerifyStage {
    Check,
    Clippy,
    Test,
    Doc,
}

impl VerifyStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Doc => "doc",
        }
    }

    pub const fn target(self) -> GateTargetId {
        match self {
            Self::Check => GateTargetId::Check,
            Self::Clippy => GateTargetId::Clippy,
            Self::Test => GateTargetId::Test,
            Self::Doc => GateTargetId::Doc,
        }
    }
}

fn parse_stage(value: &str) -> Option<VerifyStage> {
    match value.trim().to_ascii_lowercase().as_str() {
        "check" => Some(VerifyStage::Check),
        "clippy" => Some(VerifyStage::Clippy),
        "test" => Some(VerifyStage::Test),
        "doc" => Some(VerifyStage::Doc),
        _ => None,
    }
}

/// Test runner used for `test` cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum VerifyRunner {
    #[default]
    Cargo,
    Nextest,
}

impl VerifyRunner {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Nextest => "nextest",
        }
    }

    const fn gate_runner(self) -> TestRunner {
        match self {
            Self::Cargo => TestRunner::Cargo,
            Self::Nextest => TestRunner::Nextest,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Dimensions the caller explicitly requires in the matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequiredConfigurations {
    /// Include the default feature set on the host target and toolchain.
    #[serde(default = "default_true")]
    pub include_default: bool,
    /// Include an explicit `--no-default-features` cell.
    #[serde(default)]
    pub include_no_default: bool,
    /// Include candidate dimensions declared by `[workspace.metadata.agz-verify]`
    /// (or the first workspace member's `[package.metadata.agz-verify]`).
    #[serde(default = "default_true")]
    pub include_policy: bool,
    /// Additional feature groups requested directly by the caller.
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub feature_groups: Vec<Vec<String>>,
    /// Additional target triples requested directly by the caller.
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub targets: Vec<String>,
    /// Include the installed toolchain matching the declared MSRV.
    #[serde(default)]
    pub include_msrv: bool,
    /// Stages to plan. Empty means project policy stages or `check`.
    #[serde(default)]
    #[schemars(length(max = 4))]
    pub stages: Vec<VerifyStage>,
    /// Runner used for `test` cells.
    #[serde(default)]
    pub runner: VerifyRunner,
}

impl Default for RequiredConfigurations {
    fn default() -> Self {
        Self {
            include_default: true,
            include_no_default: false,
            include_policy: true,
            feature_groups: Vec::new(),
            targets: Vec::new(),
            include_msrv: false,
            stages: Vec::new(),
            runner: VerifyRunner::Cargo,
        }
    }
}

/// Bounded cell and wall-clock budget. Configured `verify.*` values are hard
/// ceilings; the request may only narrow them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifyBudget {
    #[serde(default)]
    #[schemars(range(min = 1, max = 64))]
    pub max_cells: Option<u32>,
    #[serde(default)]
    #[schemars(range(min = 1_000, max = 3_600_000))]
    pub max_wall_ms: Option<u64>,
}

/// One matrix request.
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub action: VerifyAction,
    pub directory: Option<PathBuf>,
    pub client_roots: ClientRoots,
    pub root_epoch: u64,
    pub change_id: Option<String>,
    pub required: RequiredConfigurations,
    pub budget: VerifyBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellStatus {
    Planned,
    Pass,
    Fail,
    NotInstalled,
    RunnerUnavailable,
    SkippedBudget,
    SkippedCancelled,
    UnsupportedConfiguration,
    Timeout,
    Cancelled,
    Inconclusive,
    ResourceBlocked,
}

impl CellStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "PLANNED",
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::NotInstalled => "NOT_INSTALLED",
            Self::RunnerUnavailable => "RUNNER_UNAVAILABLE",
            Self::SkippedBudget => "SKIPPED_BUDGET",
            Self::SkippedCancelled => "SKIPPED_CANCELLED",
            Self::UnsupportedConfiguration => "UNSUPPORTED_CONFIGURATION",
            Self::Timeout => "TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::ResourceBlocked => "RESOURCE_BLOCKED",
        }
    }

    const fn is_completed(self) -> bool {
        matches!(self, Self::Pass | Self::Fail)
    }
}

#[derive(Debug, Clone)]
struct MatrixCell {
    id: String,
    package_scope: String,
    features: Vec<String>,
    no_default_features: bool,
    target_triple: Option<String>,
    toolchain: Option<String>,
    msrv: Option<String>,
    stage: VerifyStage,
    runner: VerifyRunner,
    included_because: String,
    compile_only: bool,
}

#[derive(Debug, Clone)]
struct SkippedCell {
    id: String,
    status: CellStatus,
    reason: String,
}

#[derive(Debug, Clone)]
struct MatrixPlan {
    cells: Vec<MatrixCell>,
    skipped: Vec<SkippedCell>,
    policy_source: String,
    resolver: String,
    package_scope: String,
    facts: ToolchainFacts,
    feature_unification: String,
    warnings: Vec<String>,
}

/// A planned or executed matrix cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CellOutcome {
    pub id: String,
    pub package_scope: String,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub target_triple: Option<String>,
    pub toolchain: Option<String>,
    pub msrv: Option<String>,
    pub profile: String,
    pub stage: String,
    pub runner: String,
    pub included_because: String,
    pub status: String,
    pub reason: String,
    pub duration_ms: u64,
    pub job_id: Option<String>,
    pub gate_status: Option<String>,
    pub input_hash: Option<String>,
    pub command_hash: Option<String>,
    pub environment_hash: Option<String>,
    pub strategy: Option<String>,
    pub package_ids: Vec<String>,
    /// True only when Cargo reported at least one executed test case.
    pub test_execution_claimed: bool,
    /// True for non-host compile-only cells; never claims platform execution.
    pub compile_only: bool,
    pub diagnostics: Vec<String>,
}

/// A candidate cell that the plan rejected, never executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkippedCellData {
    pub id: String,
    pub status: String,
    pub reason: String,
}

/// Effective budget accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetOutcome {
    pub max_cells: u64,
    pub max_wall_ms: u64,
    pub planned: u64,
    pub executed: u64,
}

/// Bounded matrix plan or run result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerifyOutcome {
    pub action: String,
    /// `PLANNED`, `FULL_REQUESTED_MATRIX`, or `PARTIAL`.
    pub status: String,
    pub complete: bool,
    pub all_pass: bool,
    /// False for the MVP enumerator: separate cells are not exhaustive
    /// combination proof.
    pub exhaustive: bool,
    pub exhaustive_reason: String,
    pub policy_source: String,
    pub resolver: String,
    pub feature_unification: String,
    pub package_scope: String,
    pub host_target: Option<String>,
    pub selected_compiler: Option<String>,
    pub installed_targets: Vec<String>,
    pub installed_toolchains: Vec<String>,
    pub ci_suggestions: Vec<String>,
    pub change_id: Option<String>,
    pub cells: Vec<CellOutcome>,
    pub skipped: Vec<SkippedCellData>,
    pub completed_cell_ids: Vec<String>,
    pub missing_cell_ids: Vec<String>,
    pub budget: BudgetOutcome,
    pub warnings: Vec<String>,
    pub reason: String,
}

impl VerifyOutcome {
    pub fn failure(
        action: VerifyAction,
        status: &str,
        change_id: Option<String>,
        reason: String,
    ) -> Self {
        Self {
            action: action.as_str().to_owned(),
            status: status.to_owned(),
            complete: false,
            all_pass: false,
            exhaustive: false,
            exhaustive_reason: "no cell was planned".to_owned(),
            policy_source: "unavailable".to_owned(),
            resolver: "unknown".to_owned(),
            feature_unification: String::new(),
            package_scope: "workspace".to_owned(),
            host_target: None,
            selected_compiler: None,
            installed_targets: Vec::new(),
            installed_toolchains: Vec::new(),
            ci_suggestions: Vec::new(),
            change_id,
            cells: Vec::new(),
            skipped: Vec::new(),
            completed_cell_ids: Vec::new(),
            missing_cell_ids: Vec::new(),
            budget: BudgetOutcome {
                max_cells: 0,
                max_wall_ms: 0,
                planned: 0,
                executed: 0,
            },
            warnings: Vec::new(),
            reason,
        }
    }
}

/// Matrix planning and execution service.
#[derive(Debug, Clone)]
pub struct VerifyService {
    check: Arc<CheckService>,
    config: VerifyConfig,
}

impl VerifyService {
    pub fn new(check: Arc<CheckService>, config: VerifyConfig) -> Self {
        Self { check, config }
    }

    pub fn config(&self) -> &VerifyConfig {
        &self.config
    }

    /// Plan or run the requested matrix.
    pub async fn execute(
        &self,
        request: VerifyRequest,
        progress: Option<ProgressCallback>,
        cancellation: Option<CancellationToken>,
    ) -> VerifyOutcome {
        let action = request.action;
        let committed = cancellation.clone().unwrap_or_default();
        let plan_snapshot = match self
            .check
            .plan_snapshot(
                request.directory.clone(),
                request.client_roots.clone(),
                cancellation,
            )
            .await
        {
            Ok(snapshot) => snapshot,
            Err((status, message)) => {
                return VerifyOutcome::failure(
                    action,
                    status.as_str(),
                    request.change_id.clone(),
                    message,
                );
            }
        };
        let mut warnings = Vec::new();
        let facts = discover(&self.check, &plan_snapshot, &committed, &mut warnings).await;
        let policy = parse_policy(&plan_snapshot.snapshot);
        warnings.extend(policy.errors.iter().cloned());
        let resolver = resolver_label(&plan_snapshot.snapshot, &plan_snapshot.workspace_authority);
        let max_cells = effective_max_cells(&request.budget, &self.config);
        let plan = build_plan(
            &request.required,
            &policy,
            &facts,
            &plan_snapshot.snapshot,
            max_cells,
            resolver,
            warnings,
        );
        if action == VerifyAction::MatrixPlan {
            if let Some(callback) = progress.as_ref() {
                emit(
                    callback,
                    ProgressStage::Completed,
                    "matrix plan is ready",
                    0,
                );
            }
            return self.finish_plan(&request, &plan);
        }
        self.run_plan(&request, &plan_snapshot, plan, progress, &committed)
            .await
    }

    fn finish_plan(&self, request: &VerifyRequest, plan: &MatrixPlan) -> VerifyOutcome {
        let mut outcome = self.outcome_header(request, plan);
        outcome.cells = plan.cells.iter().map(planned_cell).collect();
        outcome.missing_cell_ids = plan
            .skipped
            .iter()
            .map(|skipped| skipped.id.clone())
            .collect();
        outcome.status = "PLANNED".to_owned();
        outcome.reason = format!(
            "planned {} cell(s); {} candidate(s) skipped; policy source {}",
            plan.cells.len(),
            plan.skipped.len(),
            plan.policy_source
        );
        outcome
    }

    fn outcome_header(&self, request: &VerifyRequest, plan: &MatrixPlan) -> VerifyOutcome {
        VerifyOutcome {
            action: request.action.as_str().to_owned(),
            status: "PLANNED".to_owned(),
            complete: false,
            all_pass: false,
            exhaustive: false,
            exhaustive_reason: "MVP candidate enumeration: separate feature/target/toolchain \
                 cells are not a full combination proof"
                .to_owned(),
            policy_source: plan.policy_source.clone(),
            resolver: plan.resolver.clone(),
            feature_unification: plan.feature_unification.clone(),
            package_scope: plan.package_scope.clone(),
            host_target: plan.facts.host_target.clone(),
            selected_compiler: plan.facts.selected_compiler(),
            installed_targets: plan.facts.installed_targets.clone(),
            installed_toolchains: plan.facts.installed_toolchains.clone(),
            ci_suggestions: plan.facts.ci_suggestions.clone(),
            change_id: request.change_id.clone(),
            cells: Vec::new(),
            skipped: plan
                .skipped
                .iter()
                .map(|skipped| SkippedCellData {
                    id: skipped.id.clone(),
                    status: skipped.status.as_str().to_owned(),
                    reason: skipped.reason.clone(),
                })
                .collect(),
            completed_cell_ids: Vec::new(),
            missing_cell_ids: Vec::new(),
            budget: BudgetOutcome {
                max_cells: effective_max_cells(&request.budget, &self.config),
                max_wall_ms: effective_max_wall_ms(&request.budget, &self.config),
                planned: plan.cells.len() as u64,
                executed: 0,
            },
            warnings: plan.warnings.clone(),
            reason: String::new(),
        }
    }

    async fn run_plan(
        &self,
        request: &VerifyRequest,
        plan_snapshot: &super::check::PlanSnapshot,
        plan: MatrixPlan,
        progress: Option<ProgressCallback>,
        committed: &CancellationToken,
    ) -> VerifyOutcome {
        let mut outcome = self.outcome_header(request, &plan);
        let started = Instant::now();
        let max_cells = outcome.budget.max_cells;
        let wall_deadline = started + Duration::from_millis(outcome.budget.max_wall_ms);
        let directory = request
            .directory
            .clone()
            .unwrap_or_else(|| plan_snapshot.snapshot.workspace_root.clone());
        let mut executed = 0_u64;
        let mut completed = Vec::new();
        let mut missing = Vec::new();
        let mut cells = Vec::with_capacity(plan.cells.len());
        let mut budget_exhausted = false;
        let total = plan.cells.len();
        for (index, cell) in plan.cells.iter().enumerate() {
            let planned = planned_cell(cell);
            if committed.is_cancelled() {
                cells.push(stopped_cell(
                    &planned,
                    CellStatus::SkippedCancelled,
                    "run stopped after client cancellation before this cell started",
                ));
                missing.push(cell.id.clone());
                continue;
            }
            if budget_exhausted || executed >= max_cells || Instant::now() >= wall_deadline {
                budget_exhausted = true;
                cells.push(stopped_cell(
                    &planned,
                    CellStatus::SkippedBudget,
                    if executed >= max_cells {
                        "verify.maxCells budget exhausted before this cell started"
                    } else {
                        "verify.maxWallMs budget exhausted before this cell started"
                    },
                ));
                missing.push(cell.id.clone());
                continue;
            }
            if let Some(callback) = progress.as_ref() {
                emit(
                    callback,
                    ProgressStage::Running,
                    &format!("matrix cell {}/{}: {}", index + 1, total, cell.id),
                    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                );
            }
            let gate = GateRequest::new(directory.clone(), cell.stage.target())
                .with_options(ValidationOptions {
                    features: cell.features.clone(),
                    no_default_features: cell.no_default_features,
                    target_triple: cell.target_triple.clone(),
                    runner: cell.runner.gate_runner(),
                    ..ValidationOptions::default()
                })
                .with_detail(GateDetail::Compact)
                .with_client_roots(request.client_roots.clone())
                .with_root_epoch(request.root_epoch)
                .with_source(GateSource::Explicit)
                .with_toolchain(cell.toolchain.clone());
            let timer_fired = Arc::new(AtomicBool::new(false));
            let cell_token = CancellationToken::new();
            let mut timer = None;
            let remaining = wall_deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                let token = cell_token.clone();
                let fired = Arc::clone(&timer_fired);
                timer = Some(tokio::spawn(async move {
                    tokio::time::sleep(remaining).await;
                    fired.store(true, Ordering::Release);
                    token.cancel();
                }));
            }
            let waiter = tokio::spawn({
                let committed = committed.clone();
                let linked = cell_token.clone();
                async move {
                    committed.cancelled().await;
                    linked.cancel();
                }
            });
            let evidence = self.check.run(gate, None, Some(cell_token.clone())).await;
            if let Some(timer) = timer {
                timer.abort();
            }
            waiter.abort();
            executed = executed.saturating_add(1);
            let (status, reason) = if committed.is_cancelled() {
                (
                    CellStatus::Cancelled,
                    "matrix run was cancelled by the client".to_owned(),
                )
            } else if timer_fired.load(Ordering::Acquire) || Instant::now() >= wall_deadline {
                budget_exhausted = true;
                (
                    CellStatus::SkippedBudget,
                    "verify.maxWallMs budget expired while this cell was running".to_owned(),
                )
            } else {
                (map_gate_status(evidence.status), gate_reason(&evidence))
            };
            let executed_outcome = executed_cell(&planned, status, reason, &evidence);
            if status.is_completed() {
                completed.push(cell.id.clone());
            } else {
                missing.push(cell.id.clone());
            }
            cells.push(executed_outcome);
        }
        outcome.cells = cells;
        outcome.completed_cell_ids = completed;
        outcome.missing_cell_ids = outcome
            .skipped
            .iter()
            .map(|skipped| skipped.id.clone())
            .collect();
        outcome.missing_cell_ids.extend(missing);
        outcome.budget.executed = executed;
        let complete = outcome.missing_cell_ids.is_empty() && !outcome.cells.is_empty();
        outcome.complete = complete;
        outcome.all_pass = complete
            && outcome
                .cells
                .iter()
                .all(|cell| cell.status == CellStatus::Pass.as_str());
        outcome.status = if complete {
            "FULL_REQUESTED_MATRIX".to_owned()
        } else {
            "PARTIAL".to_owned()
        };
        outcome.reason = if complete {
            format!(
                "every requested cell completed: {} pass, {} fail",
                outcome
                    .cells
                    .iter()
                    .filter(|cell| cell.status == CellStatus::Pass.as_str())
                    .count(),
                outcome
                    .cells
                    .iter()
                    .filter(|cell| cell.status == CellStatus::Fail.as_str())
                    .count(),
            )
        } else {
            format!(
                "{} completed cell(s), {} missing cell(s); skipped or incomplete cells never grant a pass",
                outcome.completed_cell_ids.len(),
                outcome.missing_cell_ids.len()
            )
        };
        if let Some(callback) = progress.as_ref() {
            emit(
                callback,
                ProgressStage::Completed,
                &outcome.reason,
                started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            );
        }
        outcome
    }
}

fn planned_cell(cell: &MatrixCell) -> CellOutcome {
    CellOutcome {
        id: cell.id.clone(),
        package_scope: cell.package_scope.clone(),
        features: cell.features.clone(),
        no_default_features: cell.no_default_features,
        target_triple: cell.target_triple.clone(),
        toolchain: cell.toolchain.clone(),
        msrv: cell.msrv.clone(),
        profile: "dev".to_owned(),
        stage: cell.stage.as_str().to_owned(),
        runner: cell.runner.as_str().to_owned(),
        included_because: cell.included_because.clone(),
        status: CellStatus::Planned.as_str().to_owned(),
        reason: "planned; not executed by matrix_plan".to_owned(),
        duration_ms: 0,
        job_id: None,
        gate_status: None,
        input_hash: None,
        command_hash: None,
        environment_hash: None,
        strategy: None,
        package_ids: Vec::new(),
        test_execution_claimed: false,
        compile_only: cell.compile_only,
        diagnostics: Vec::new(),
    }
}

fn stopped_cell(planned: &CellOutcome, status: CellStatus, reason: &str) -> CellOutcome {
    let mut cell = planned.clone();
    cell.status = status.as_str().to_owned();
    cell.reason = reason.to_owned();
    cell
}

fn executed_cell(
    planned: &CellOutcome,
    status: CellStatus,
    reason: String,
    evidence: &GateEvidence,
) -> CellOutcome {
    let mut cell = planned.clone();
    cell.status = status.as_str().to_owned();
    cell.reason = reason;
    cell.duration_ms = evidence.response_ms;
    cell.job_id = Some(evidence.job_id.clone());
    cell.gate_status = Some(evidence.status.as_str().to_owned());
    cell.input_hash = Some(evidence.input_hash.clone());
    cell.command_hash = Some(evidence.command_hash.clone());
    cell.environment_hash = Some(evidence.environment_hash.clone());
    cell.strategy = Some(format!("{:?}", evidence.scope.strategy).to_ascii_lowercase());
    cell.package_ids = evidence.scope.package_ids.clone();
    cell.test_execution_claimed = evidence.steps.iter().any(|step| {
        step.target == GateTargetId::Test && step.evidence.tests_executed.unwrap_or(0) > 0
    });
    cell.diagnostics = evidence
        .steps
        .iter()
        .flat_map(|step| step.diagnostics.iter())
        .take(CELL_DIAGNOSTIC_LIMIT)
        .map(|diagnostic| {
            let code = diagnostic
                .code
                .as_deref()
                .map_or_else(String::new, |code| format!("[{code}] "));
            format!("{code}{}: {}", diagnostic.level, diagnostic.message)
        })
        .collect();
    cell
}

fn gate_reason(evidence: &GateEvidence) -> String {
    let base = evidence
        .message
        .clone()
        .unwrap_or_else(|| evidence.status.as_str().to_owned());
    let failed = evidence
        .steps
        .iter()
        .filter(|step| step.exit_code != 0)
        .count();
    if failed == 0 {
        base
    } else {
        format!("{base}; {failed} failing step(s)")
    }
}

const fn map_gate_status(status: GateStatus) -> CellStatus {
    match status {
        GateStatus::FastPass | GateStatus::FullPass => CellStatus::Pass,
        GateStatus::Fail => CellStatus::Fail,
        GateStatus::Timeout => CellStatus::Timeout,
        GateStatus::Cancelled => CellStatus::Cancelled,
        GateStatus::Unavailable => CellStatus::RunnerUnavailable,
        GateStatus::ResourceBlocked => CellStatus::ResourceBlocked,
        GateStatus::Pending
        | GateStatus::Stale
        | GateStatus::Superseded
        | GateStatus::Inconclusive => CellStatus::Inconclusive,
    }
}

fn effective_max_cells(budget: &VerifyBudget, config: &VerifyConfig) -> u64 {
    u64::from(budget.max_cells.unwrap_or(u32::MAX)).min(config.max_cells)
}

fn effective_max_wall_ms(budget: &VerifyBudget, config: &VerifyConfig) -> u64 {
    budget
        .max_wall_ms
        .unwrap_or(u64::MAX)
        .min(config.max_wall_ms)
}

fn emit(callback: &ProgressCallback, stage: ProgressStage, message: &str, elapsed_ms: u64) {
    callback(ProgressEvent {
        stage,
        target: None,
        progress: 0.0,
        total: None,
        message: message.to_owned(),
        heartbeat: false,
        elapsed_ms,
    });
}

#[derive(Debug, Clone, Default)]
struct ProjectPolicy {
    source: String,
    feature_groups: Vec<Vec<String>>,
    mutually_exclusive: Vec<Vec<String>>,
    targets: Vec<String>,
    msrv: Option<String>,
    stages: Vec<VerifyStage>,
    errors: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct PolicyValue {
    feature_groups: Vec<Vec<String>>,
    mutually_exclusive_features: Vec<Vec<String>>,
    targets: Vec<String>,
    msrv: Option<String>,
    stages: Vec<String>,
}

fn parse_policy(snapshot: &WorkspaceSnapshot) -> ProjectPolicy {
    if let Some(value) = snapshot.metadata.workspace_metadata.get("agz-verify") {
        return policy_from_value("workspace.metadata.agz-verify", value);
    }
    for package in &snapshot.metadata.packages {
        if !snapshot.metadata.workspace_members.contains(&package.id) {
            continue;
        }
        if let Some(value) = package.metadata.get("agz-verify") {
            return policy_from_value("package.metadata.agz-verify", value);
        }
    }
    ProjectPolicy {
        source: "defaults".to_owned(),
        ..ProjectPolicy::default()
    }
}

fn policy_from_value(source: &str, value: &serde_json::Value) -> ProjectPolicy {
    let mut policy = ProjectPolicy {
        source: source.to_owned(),
        ..ProjectPolicy::default()
    };
    let parsed = match serde_json::from_value::<PolicyValue>(value.clone()) {
        Ok(parsed) => parsed,
        Err(error) => {
            policy
                .errors
                .push(format!("{source} policy is malformed: {error}"));
            return policy;
        }
    };
    if parsed.feature_groups.len() > POLICY_FEATURE_GROUP_LIMIT {
        policy.errors.push(format!(
            "{source} declares more than {POLICY_FEATURE_GROUP_LIMIT} feature groups; excess groups were ignored"
        ));
    }
    policy.feature_groups = parsed
        .feature_groups
        .into_iter()
        .take(POLICY_FEATURE_GROUP_LIMIT)
        .collect();
    policy.mutually_exclusive = parsed.mutually_exclusive_features;
    policy.targets = parsed.targets;
    policy.msrv = parsed.msrv;
    for stage in parsed.stages {
        match parse_stage(&stage) {
            Some(stage) => policy.stages.push(stage),
            None => policy.errors.push(format!(
                "{source} stage `{stage}` is not check/clippy/test/doc"
            )),
        }
    }
    policy
}

#[derive(Debug, Clone, Default)]
struct ToolchainFacts {
    host_target: Option<String>,
    rustc_version: Option<String>,
    rustc_release: Option<String>,
    cargo_version: Option<String>,
    installed_targets: Vec<String>,
    installed_toolchains: Vec<String>,
    rustup_available: bool,
    rustup_shim_available: bool,
    ci_suggestions: Vec<String>,
    notes: Vec<String>,
}

impl ToolchainFacts {
    fn selected_compiler(&self) -> Option<String> {
        match (&self.rustc_version, &self.rustc_release) {
            (Some(version), _) => Some(version.clone()),
            (None, Some(release)) => Some(format!("rustc {release}")),
            (None, None) => None,
        }
    }

    fn target_installed(&self, triple: &str) -> bool {
        self.installed_targets
            .iter()
            .any(|installed| installed == triple)
    }

    fn toolchain_for(&self, requested: &str) -> Option<String> {
        if let Some(exact) = self
            .installed_toolchains
            .iter()
            .find(|toolchain| toolchain.as_str() == requested)
        {
            return Some(exact.clone());
        }
        self.installed_toolchains.iter().find_map(|toolchain| {
            let name_version = toolchain.split('-').next().unwrap_or(toolchain.as_str());
            version_matches(name_version, requested).then(|| toolchain.clone())
        })
    }
}

fn version_matches(installed: &str, requested: &str) -> bool {
    installed == requested
        || installed
            .strip_prefix(requested)
            .is_some_and(|rest| rest.starts_with('.'))
        || requested
            .strip_prefix(installed)
            .is_some_and(|rest| rest.starts_with('.'))
}

async fn discover(
    check: &CheckService,
    plan: &super::check::PlanSnapshot,
    cancellation: &CancellationToken,
    warnings: &mut Vec<String>,
) -> ToolchainFacts {
    let mut facts = ToolchainFacts::default();
    let cwd = plan.snapshot.workspace_root.as_path();
    let authority = plan.workspace_authority.clone();
    let cargo = check.cargo_path().to_owned();
    let rustup = resolve_program("rustup");
    facts.rustup_available = rustup.is_some();
    facts.rustup_shim_available = rustup_shim_available(&cargo);
    let rustc = resolve_rustc(&cargo);
    if let Some(rustc) = rustc.as_deref() {
        match check
            .auxiliary_output(
                cwd,
                rustc,
                &[OsString::from("-vV")],
                DISCOVERY_TIMEOUT,
                cancellation,
                authority.clone(),
            )
            .await
        {
            Ok(output) => {
                for line in output.lines() {
                    if let Some(host) = line.strip_prefix("host: ") {
                        facts.host_target = Some(host.trim().to_owned());
                    } else if let Some(release) = line.strip_prefix("release: ") {
                        facts.rustc_release = Some(release.trim().to_owned());
                    }
                }
                facts.rustc_version = output.lines().next().map(str::to_owned);
            }
            Err(error) => facts
                .notes
                .push(format!("rustc discovery unavailable: {error}")),
        }
    } else {
        facts
            .notes
            .push("rustc was not found; host target is unknown".to_owned());
    }
    match check
        .auxiliary_output(
            cwd,
            &cargo,
            &[OsString::from("--version")],
            DISCOVERY_TIMEOUT,
            cancellation,
            authority.clone(),
        )
        .await
    {
        Ok(output) => facts.cargo_version = Some(output.trim().to_owned()),
        Err(error) => facts
            .notes
            .push(format!("cargo discovery unavailable: {error}")),
    }
    if let Some(rustup) = rustup.as_deref() {
        match check
            .auxiliary_output(
                cwd,
                rustup,
                &[
                    OsString::from("target"),
                    OsString::from("list"),
                    OsString::from("--installed"),
                ],
                DISCOVERY_TIMEOUT,
                cancellation,
                authority.clone(),
            )
            .await
        {
            Ok(output) => facts.installed_targets = parse_installed_targets(&output),
            Err(error) => facts
                .notes
                .push(format!("rustup target discovery unavailable: {error}")),
        }
        match check
            .auxiliary_output(
                cwd,
                rustup,
                &[OsString::from("toolchain"), OsString::from("list")],
                DISCOVERY_TIMEOUT,
                cancellation,
                authority,
            )
            .await
        {
            Ok(output) => facts.installed_toolchains = parse_installed_toolchains(&output),
            Err(error) => facts
                .notes
                .push(format!("rustup toolchain discovery unavailable: {error}")),
        }
    }
    if facts.installed_targets.is_empty() {
        facts.installed_targets = rustlib_targets(&cargo);
    }
    if facts.installed_toolchains.is_empty() {
        facts.installed_toolchains = rustup_home_toolchains();
    }
    if let Some(host) = facts.host_target.clone()
        && !facts.target_installed(&host)
    {
        facts.installed_targets.push(host);
    }
    facts.installed_targets.sort();
    facts.installed_targets.dedup();
    facts.installed_toolchains.sort();
    facts.installed_toolchains.dedup();
    facts.ci_suggestions = ci_suggestions(
        &plan.snapshot.workspace_root,
        &plan.workspace_authority,
        warnings,
    );
    warnings.append(&mut facts.notes);
    facts
}

fn resolve_program(name: &str) -> Option<PathBuf> {
    let executable = executable_name(name);
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(&executable);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME")?;
    let candidate = PathBuf::from(home)
        .join(".cargo")
        .join("bin")
        .join(executable);
    candidate.is_file().then_some(candidate)
}

fn resolve_rustc(cargo: &Path) -> Option<PathBuf> {
    if let Some(parent) = cargo.parent() {
        let candidate = parent.join(executable_name("rustc"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    resolve_program("rustc")
}

fn parse_installed_targets(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let token = line.split_whitespace().next()?;
            looks_like_triple(token).then(|| token.to_owned())
        })
        .collect()
}

fn parse_installed_toolchains(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|token| !token.is_empty() && *token != "no")
        .map(str::to_owned)
        .collect()
}

fn looks_like_triple(value: &str) -> bool {
    value.matches('-').count() >= 2
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
}

fn rustlib_targets(cargo: &Path) -> Vec<String> {
    let Some(parent) = cargo.parent() else {
        return Vec::new();
    };
    let Some(rustlib) = parent
        .parent()
        .map(|toolchain| toolchain.join("lib").join("rustlib"))
    else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(rustlib) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| looks_like_triple(name))
        .collect()
}

fn rustup_home_toolchains() -> Vec<String> {
    let home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")));
    let Some(home) = home else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(home.join("toolchains")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect()
}

#[cfg(windows)]
fn executable_name(name: &str) -> String {
    format!("{name}.exe")
}

#[cfg(not(windows))]
fn executable_name(name: &str) -> String {
    name.to_owned()
}

fn ci_suggestions(
    workspace_root: &Path,
    authority: &AuthorizedRoot,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let workflows = workspace_root.join(".github").join("workflows");
    let Ok(entries) = authority.list_directory(&workflows) else {
        return Vec::new();
    };
    let mut files = entries
        .iter()
        .filter(|entry| entry.kind == DirectoryEntryKind::RegularFile)
        .map(|entry| workflows.join(&entry.name))
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension == "yml" || extension == "yaml")
        })
        .collect::<Vec<_>>();
    files.sort();
    files.truncate(MAX_CI_FILES);
    if files.is_empty() {
        return Vec::new();
    }
    warnings.push(
        "CI workflow files were read as literal configuration suggestions only; no workflow command or shell was executed"
            .to_owned(),
    );
    let mut suggestions = Vec::new();
    let mut seen = BTreeSet::new();
    for file in files {
        let text = match authority.read_file(&file, MAX_CONFIG_BYTES) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    warnings.push(format!(
                        "CI file {} is not UTF-8 text and was ignored",
                        file.display()
                    ));
                    continue;
                }
            },
            Err(error) => {
                warnings.push(format!(
                    "CI file {} was ignored by the bounded authorized read: {error}",
                    file.display()
                ));
                continue;
            }
        };
        let relative = file
            .strip_prefix(workspace_root)
            .unwrap_or(file.as_path())
            .display()
            .to_string();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            for token in line.split_whitespace() {
                let candidate = token
                    .trim_matches(|character: char| {
                        character == '"' || character == '\'' || character == ','
                    })
                    .trim_start_matches("--target=");
                if looks_like_triple(candidate) && seen.insert(candidate.to_owned()) {
                    suggestions.push(format!(
                        "{candidate} (CI suggestion from {relative}; workflow was not executed)"
                    ));
                }
                if suggestions.len() >= CI_SUGGESTION_LIMIT {
                    return suggestions;
                }
            }
        }
    }
    suggestions
}

#[derive(Debug, Clone)]
struct FeatureSet {
    label: String,
    features: Vec<String>,
    no_default_features: bool,
    because: String,
}

fn build_plan(
    required: &RequiredConfigurations,
    policy: &ProjectPolicy,
    facts: &ToolchainFacts,
    snapshot: &WorkspaceSnapshot,
    max_cells: u64,
    resolver: String,
    mut warnings: Vec<String>,
) -> MatrixPlan {
    let features_map = workspace_features(snapshot);
    let mut cells = Vec::new();
    let mut skipped = Vec::new();
    let stages = effective_stages(required, policy);
    let package_scope = "workspace".to_owned();
    let feature_sets = feature_sets(required, policy, &features_map, &mut skipped);
    let mut used_ids = BTreeSet::new();
    for features in &feature_sets {
        for stage in &stages {
            let cell = MatrixCell {
                id: String::new(),
                package_scope: package_scope.clone(),
                features: features.features.clone(),
                no_default_features: features.no_default_features,
                target_triple: None,
                toolchain: None,
                msrv: None,
                stage: *stage,
                runner: runner_for(*stage, required.runner),
                included_because: features.because.clone(),
                compile_only: false,
            };
            push_cell(
                &mut cells,
                &mut used_ids,
                cell,
                spec_id(&features.label, "host", "default", stage.as_str()),
            );
        }
    }
    plan_targets(
        required,
        policy,
        facts,
        &stages,
        &mut cells,
        &mut skipped,
        &mut used_ids,
    );
    plan_msrv(
        required,
        policy,
        facts,
        &mut cells,
        &mut skipped,
        &mut used_ids,
        snapshot,
    );
    if cells.len() as u64 > max_cells {
        let excess = cells.split_off(usize::try_from(max_cells).unwrap_or(usize::MAX));
        for cell in excess {
            skipped.push(SkippedCell {
                id: cell.id,
                status: CellStatus::SkippedBudget,
                reason: "verify.maxCells budget exhausted during planning".to_owned(),
            });
        }
    }
    if warnings.is_empty() {
        warnings.push(
            "verify reads CI workflow files as literal configuration input only and never executes them"
                .to_owned(),
        );
    }
    MatrixPlan {
        cells,
        skipped,
        policy_source: policy.source.clone(),
        resolver: resolver.clone(),
        package_scope,
        facts: facts.clone(),
        feature_unification: format!(
            "resolver {resolver}; selected features apply with workspace scope and Cargo feature unification decides the effective set"
        ),
        warnings,
    }
}

fn effective_stages(required: &RequiredConfigurations, policy: &ProjectPolicy) -> Vec<VerifyStage> {
    let mut stages = if required.stages.is_empty() {
        policy.stages.clone()
    } else {
        required.stages.clone()
    };
    if stages.is_empty() {
        stages.push(VerifyStage::Check);
    }
    let mut seen = BTreeSet::new();
    stages.retain(|stage| seen.insert(*stage));
    stages
}

const fn runner_for(stage: VerifyStage, runner: VerifyRunner) -> VerifyRunner {
    if matches!(stage, VerifyStage::Test) {
        runner
    } else {
        VerifyRunner::Cargo
    }
}

fn feature_sets(
    required: &RequiredConfigurations,
    policy: &ProjectPolicy,
    features_map: &BTreeMap<String, Vec<String>>,
    skipped: &mut Vec<SkippedCell>,
) -> Vec<FeatureSet> {
    let mut sets = Vec::new();
    if required.include_default {
        sets.push(FeatureSet {
            label: "default".to_owned(),
            features: Vec::new(),
            no_default_features: false,
            because: "default feature set on the host target and toolchain".to_owned(),
        });
    }
    if required.include_no_default {
        sets.push(FeatureSet {
            label: "no-default".to_owned(),
            features: Vec::new(),
            no_default_features: true,
            because: "explicitly requested --no-default-features cell".to_owned(),
        });
    }
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    if required.include_policy {
        for (index, group) in policy.feature_groups.iter().enumerate() {
            groups.push((format!("policy-{}", index + 1), group.clone()));
        }
    }
    for (index, group) in required.feature_groups.iter().enumerate() {
        if index >= REQUEST_FEATURE_GROUP_LIMIT {
            skipped.push(SkippedCell {
                id: format!("requested-group-{}", index + 1),
                status: CellStatus::UnsupportedConfiguration,
                reason: format!(
                    "at most {REQUEST_FEATURE_GROUP_LIMIT} requested feature groups are accepted"
                ),
            });
            continue;
        }
        groups.push((format!("requested-{}", index + 1), group.clone()));
    }
    let mut seen_groups = BTreeSet::new();
    for (label, group) in groups {
        if group.is_empty() || group.len() > MAX_GROUP_FEATURES {
            skipped.push(SkippedCell {
                id: label,
                status: CellStatus::UnsupportedConfiguration,
                reason: format!(
                    "feature group must contain 1..={MAX_GROUP_FEATURES} feature names"
                ),
            });
            continue;
        }
        let mut normalized = group.clone();
        normalized.sort();
        normalized.dedup();
        if !seen_groups.insert(normalized.clone()) {
            continue;
        }
        if let Some(feature) = normalized
            .iter()
            .find(|feature| !valid_feature_name(feature))
        {
            skipped.push(SkippedCell {
                id: label,
                status: CellStatus::UnsupportedConfiguration,
                reason: format!("feature name `{feature}` is not a bounded Cargo feature name"),
            });
            continue;
        }
        if let Some(feature) = normalized
            .iter()
            .find(|feature| !feature_is_declared(features_map, feature))
        {
            skipped.push(SkippedCell {
                id: label,
                status: CellStatus::UnsupportedConfiguration,
                reason: format!(
                    "feature `{feature}` is not declared by any workspace package; unsupported configuration is not a product regression"
                ),
            });
            continue;
        }
        let closure = feature_closure(features_map, &normalized, true);
        if let Some(conflict) = feature_conflict(policy, &closure) {
            skipped.push(SkippedCell {
                id: label,
                status: CellStatus::UnsupportedConfiguration,
                reason: conflict,
            });
            continue;
        }
        sets.push(FeatureSet {
            label: label.clone(),
            features: normalized,
            no_default_features: false,
            because: format!(
                "explicitly supported feature group `{label}` from project policy ({})",
                policy.source
            ),
        });
    }
    sets
}

fn workspace_features(snapshot: &WorkspaceSnapshot) -> BTreeMap<String, Vec<String>> {
    let mut features = BTreeMap::new();
    for package in &snapshot.metadata.packages {
        if !snapshot.metadata.workspace_members.contains(&package.id) {
            continue;
        }
        for (name, values) in &package.features {
            features
                .entry(name.clone())
                .or_insert_with(Vec::new)
                .extend(values.iter().cloned());
        }
    }
    features
}

fn feature_is_declared(features: &BTreeMap<String, Vec<String>>, feature: &str) -> bool {
    if features.contains_key(feature) {
        return true;
    }
    let Some((dependency, _)) = feature.split_once('/') else {
        return false;
    };
    let dependency = dependency.strip_suffix('?').unwrap_or(dependency);
    !dependency.is_empty() && features.contains_key(dependency)
}

fn feature_closure(
    features: &BTreeMap<String, Vec<String>>,
    roots: &[String],
    include_default: bool,
) -> BTreeSet<String> {
    let mut queue = roots.to_vec();
    if include_default && let Some(default) = features.get("default") {
        queue.extend(default.iter().cloned());
    }
    let mut seen = BTreeSet::new();
    while let Some(feature) = queue.pop() {
        if seen.len() >= FEATURE_CLOSURE_LIMIT {
            break;
        }
        if !seen.insert(feature.clone()) {
            continue;
        }
        if let Some(dependencies) = features.get(&feature) {
            queue.extend(dependencies.iter().cloned());
        }
    }
    seen
}

fn feature_conflict(policy: &ProjectPolicy, active: &BTreeSet<String>) -> Option<String> {
    for group in &policy.mutually_exclusive {
        let hits = group
            .iter()
            .filter(|feature| active.contains(*feature))
            .collect::<Vec<_>>();
        if hits.len() >= 2 {
            return Some(format!(
                "mutually exclusive features enabled together by project policy: {}",
                hits.iter()
                    .map(|feature| feature.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    None
}

/// Valid Cargo feature selection: a plain feature name, `dep/feat`, or the
/// weak `dep?/feat` form. Leading separators and empty segments are rejected so
/// malformed input becomes a typed unsupported configuration, never a flag.
fn valid_feature_name(feature: &str) -> bool {
    if feature.is_empty() || feature.len() > 128 {
        return false;
    }
    let (dependency, selector) = match feature.split_once('/') {
        Some((dependency, selector)) => {
            if selector.is_empty() || selector.contains('/') {
                return false;
            }
            (
                dependency.strip_suffix('?').unwrap_or(dependency),
                Some(selector),
            )
        }
        None => (feature, None),
    };
    valid_feature_segment(dependency) && selector.is_none_or(valid_feature_segment)
}

fn valid_feature_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphanumeric() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
}

fn push_cell(
    cells: &mut Vec<MatrixCell>,
    used_ids: &mut BTreeSet<String>,
    mut cell: MatrixCell,
    id: String,
) {
    cell.id = unique_id(used_ids, &id);
    cells.push(cell);
}

fn unique_id(used: &mut BTreeSet<String>, base: &str) -> String {
    let base = sanitize_id(base);
    let mut candidate = base.clone();
    let mut suffix = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn sanitize_id(value: &str) -> String {
    let mut id = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            id.push(character.to_ascii_lowercase());
        } else {
            id.push('-');
        }
    }
    if id.is_empty() { "cell".to_owned() } else { id }
}

fn spec_id(feature_label: &str, target: &str, toolchain: &str, stage: &str) -> String {
    format!("{feature_label}--{target}--{toolchain}--{stage}")
}

fn plan_targets(
    required: &RequiredConfigurations,
    policy: &ProjectPolicy,
    facts: &ToolchainFacts,
    stages: &[VerifyStage],
    cells: &mut Vec<MatrixCell>,
    skipped: &mut Vec<SkippedCell>,
    used_ids: &mut BTreeSet<String>,
) {
    let mut targets = Vec::new();
    if required.include_policy {
        targets.extend(policy.targets.iter().cloned());
    }
    targets.extend(required.targets.iter().cloned());
    for suggestion in &facts.ci_suggestions {
        if let Some(triple) = suggestion.split_whitespace().next()
            && facts.target_installed(triple)
        {
            targets.push(triple.to_owned());
        }
    }
    let mut seen = BTreeSet::new();
    for target in targets {
        if !looks_like_triple(&target) {
            skipped.push(SkippedCell {
                id: format!("target-{}", sanitize_id(&target)),
                status: CellStatus::UnsupportedConfiguration,
                reason: format!("`{target}` is not a bounded built-in target triple"),
            });
            continue;
        }
        if !seen.insert(target.clone()) {
            continue;
        }
        if facts.host_target.as_deref() == Some(target.as_str()) {
            skipped.push(SkippedCell {
                id: format!("target-{}", sanitize_id(&target)),
                status: CellStatus::UnsupportedConfiguration,
                reason: format!("target {target} is the host target and is already covered"),
            });
            continue;
        }
        if !facts.target_installed(&target) {
            skipped.push(SkippedCell {
                id: format!("target-{}", sanitize_id(&target)),
                status: CellStatus::NotInstalled,
                reason: format!(
                    "target {target} is not installed; no download is attempted and compile-only plans cannot run"
                ),
            });
            continue;
        }
        if !stages.contains(&VerifyStage::Check) {
            skipped.push(SkippedCell {
                id: format!("target-{}", sanitize_id(&target)),
                status: CellStatus::UnsupportedConfiguration,
                reason: format!(
                    "target {target} is compile-only and `check` was not among the requested stages"
                ),
            });
            continue;
        }
        let id = spec_id("default", &sanitize_id(&target), "host", "check");
        push_cell(
            cells,
            used_ids,
            MatrixCell {
                id: String::new(),
                package_scope: "workspace".to_owned(),
                features: Vec::new(),
                no_default_features: false,
                target_triple: Some(target.clone()),
                toolchain: None,
                msrv: None,
                stage: VerifyStage::Check,
                runner: VerifyRunner::Cargo,
                included_because: format!(
                    "installed target {target}; compile-only cell, never claims test execution on that platform"
                ),
                compile_only: true,
            },
            id,
        );
    }
    if required
        .stages
        .iter()
        .any(|stage| !matches!(stage, VerifyStage::Check))
        && !required.targets.is_empty()
    {
        skipped.push(SkippedCell {
            id: "target-non-check-stages".to_owned(),
            status: CellStatus::UnsupportedConfiguration,
            reason: "non-host targets are compile-only in the MVP; test/clippy/doc stages on a \
                     foreign target cannot claim platform execution"
                .to_owned(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_msrv(
    required: &RequiredConfigurations,
    policy: &ProjectPolicy,
    facts: &ToolchainFacts,
    cells: &mut Vec<MatrixCell>,
    skipped: &mut Vec<SkippedCell>,
    used_ids: &mut BTreeSet<String>,
    snapshot: &WorkspaceSnapshot,
) {
    let declared_version = declared_rust_version(snapshot);
    let requested = policy.msrv.clone().or_else(|| declared_version.clone());
    let include = required.include_msrv
        || (required.include_policy && (policy.msrv.is_some() || declared_version.is_some()));
    if !include {
        return;
    }
    let Some(requested) = requested else {
        skipped.push(SkippedCell {
            id: "msrv".to_owned(),
            status: CellStatus::UnsupportedConfiguration,
            reason: "no MSRV was requested and no workspace package declares rust-version"
                .to_owned(),
        });
        return;
    };
    let Some(toolchain) = facts.toolchain_for(&requested) else {
        skipped.push(SkippedCell {
            id: "msrv".to_owned(),
            status: CellStatus::NotInstalled,
            reason: format!(
                "MSRV toolchain matching {requested} is not installed; no download is attempted"
            ),
        });
        return;
    };
    if resolve_toolchain_cargo(&toolchain).is_none() && !facts.rustup_shim_available {
        skipped.push(SkippedCell {
            id: "msrv".to_owned(),
            status: CellStatus::UnsupportedConfiguration,
            reason: format!(
                "MSRV toolchain {toolchain} has no direct cargo and the configured cargo is not a rustup shim; no toolchain download is attempted"
            ),
        });
        return;
    }
    if let Err(message) = validate_toolchain_name(&toolchain) {
        skipped.push(SkippedCell {
            id: "msrv".to_owned(),
            status: CellStatus::UnsupportedConfiguration,
            reason: message,
        });
        return;
    }
    let id = spec_id("default", "host", &sanitize_id(&toolchain), "check");
    push_cell(
        cells,
        used_ids,
        MatrixCell {
            id: String::new(),
            package_scope: "workspace".to_owned(),
            features: Vec::new(),
            no_default_features: false,
            target_triple: None,
            toolchain: Some(toolchain.clone()),
            msrv: Some(requested.clone()),
            stage: VerifyStage::Check,
            runner: VerifyRunner::Cargo,
            included_because: format!(
                "installed MSRV toolchain {toolchain} selected for rust-version {requested}; the gate pins RUSTC/RUSTUP_TOOLCHAIN/PATH (or uses the rustup +toolchain shim) so the selected compiler is actually invoked and bound by the command and environment hashes"
            ),
            compile_only: false,
        },
        id,
    );
}

fn declared_rust_version(snapshot: &WorkspaceSnapshot) -> Option<String> {
    snapshot
        .metadata
        .packages
        .iter()
        .filter(|package| snapshot.metadata.workspace_members.contains(&package.id))
        .filter_map(|package| package.rust_version.as_ref())
        .map(ToString::to_string)
        .max_by(|left, right| compare_versions(left, right))
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |value: &str| {
        value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    parse(left).cmp(&parse(right))
}

fn resolver_label(snapshot: &WorkspaceSnapshot, authority: &AuthorizedRoot) -> String {
    if let Ok(bytes) = authority.read_file(&snapshot.manifest_path, MAX_CONFIG_BYTES)
        && let Ok(text) = String::from_utf8(bytes)
        && let Ok(value) = text.parse::<toml::Value>()
        && let Some(resolver) = value
            .get("workspace")
            .and_then(|workspace| workspace.get("resolver"))
            .and_then(toml::Value::as_str)
    {
        return resolver.to_owned();
    }
    let resolver = snapshot
        .metadata
        .packages
        .iter()
        .filter(|package| snapshot.metadata.workspace_members.contains(&package.id))
        .map(|package| resolver_for_edition(package.edition))
        .max()
        .unwrap_or(0);
    match resolver {
        0 => "unknown".to_owned(),
        other => format!("edition-default ({other})"),
    }
}

/// Cargo resolver defaults by edition: 2015 -> 1, 2018/2021 -> 2, 2024 -> 3.
fn resolver_for_edition(edition: cargo_metadata::Edition) -> u8 {
    match edition {
        cargo_metadata::Edition::E2024 => 3,
        cargo_metadata::Edition::E2021 | cargo_metadata::Edition::E2018 => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(name, values)| {
                (
                    (*name).to_owned(),
                    values.iter().map(|value| (*value).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn feature_groups_reject_undeclared_and_conflicting_features() {
        let map = features(&[
            ("default", &["std"]),
            ("std", &[]),
            ("tls-native", &[]),
            ("tls-rustls", &[]),
        ]);
        let policy = ProjectPolicy {
            source: "test".to_owned(),
            feature_groups: vec![
                vec!["missing".to_owned()],
                vec!["tls-native".to_owned(), "tls-rustls".to_owned()],
            ],
            mutually_exclusive: vec![vec!["tls-native".to_owned(), "tls-rustls".to_owned()]],
            ..ProjectPolicy::default()
        };
        let mut skipped = Vec::new();
        let sets = feature_sets(
            &RequiredConfigurations::default(),
            &policy,
            &map,
            &mut skipped,
        );
        assert_eq!(sets.len(), 1);
        assert_eq!(skipped.len(), 2);
        assert!(
            skipped
                .iter()
                .all(|cell| cell.status == CellStatus::UnsupportedConfiguration)
        );
        assert!(
            skipped
                .iter()
                .any(|cell| cell.reason.contains("not declared"))
        );
        assert!(
            skipped
                .iter()
                .any(|cell| cell.reason.contains("mutually exclusive"))
        );
    }

    #[test]
    fn feature_closure_detects_default_conflict() {
        let map = features(&[("default", &["native"]), ("native", &[]), ("rustls", &[])]);
        let policy = ProjectPolicy {
            mutually_exclusive: vec![vec!["native".to_owned(), "rustls".to_owned()]],
            ..ProjectPolicy::default()
        };
        let mut skipped = Vec::new();
        let sets = feature_sets(
            &RequiredConfigurations {
                feature_groups: vec![vec!["rustls".to_owned()]],
                ..RequiredConfigurations::default()
            },
            &policy,
            &map,
            &mut skipped,
        );
        assert_eq!(sets.len(), 1);
        assert!(
            skipped
                .iter()
                .any(|cell| cell.reason.contains("mutually exclusive"))
        );
    }

    #[test]
    fn version_matching_accepts_prefix_forms() {
        assert!(version_matches("1.88.0", "1.88"));
        assert!(version_matches("1.88", "1.88.0"));
        assert!(version_matches("1.88.0", "1.88.0"));
        assert!(!version_matches("1.89.0", "1.88"));
        assert!(!version_matches("stable", "1.88"));
    }

    #[test]
    fn ci_scan_reads_literal_suggestions_only() {
        let root = std::env::temp_dir().join(format!(
            "agz-verify-ci-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |value| value.as_nanos())
        ));
        let workflows = root.join(".github").join("workflows");
        fs::create_dir_all(&workflows).expect("create workflow directory");
        fs::write(
            workflows.join("ci.yml"),
            "matrix:\n  target: x86_64-unknown-linux-gnu\n  other: ${{ matrix.target }}\nrun: rustup target add aarch64-unknown-linux-gnu\n",
        )
        .expect("write workflow");
        let authority = test_authority(&root);
        let mut warnings = Vec::new();
        let suggestions = ci_suggestions(&root, &authority, &mut warnings);
        assert!(
            suggestions
                .iter()
                .any(|value| value.starts_with("x86_64-unknown-linux-gnu"))
        );
        assert!(
            suggestions
                .iter()
                .any(|value| value.starts_with("aarch64-unknown-linux-gnu"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ci_scan_is_bounded_and_never_follows_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "agz-verify-ci-bounded-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |value| value.as_nanos())
        ));
        let workflows = root.join(".github").join("workflows");
        fs::create_dir_all(&workflows).expect("create workflow directory");
        fs::write(
            workflows.join("huge.yml"),
            "target: x86_64-unknown-linux-gnu\n".repeat(4_000),
        )
        .expect("write oversized workflow");
        let outside = root.join("outside.yml");
        fs::write(&outside, "target: aarch64-unknown-linux-gnu\n").expect("write outside file");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workflows.join("linked.yml"))
            .expect("create workflow symlink");
        let authority = test_authority(&root);
        let mut warnings = Vec::new();
        let suggestions = ci_suggestions(&root, &authority, &mut warnings);
        assert!(
            !suggestions
                .iter()
                .any(|value| value.starts_with("x86_64-unknown-linux-gnu"))
        );
        #[cfg(unix)]
        assert!(
            !suggestions
                .iter()
                .any(|value| value.starts_with("aarch64-unknown-linux-gnu"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("bounded authorized read")),
            "{warnings:#?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolver_defaults_follow_the_cargo_edition_matrix() {
        use cargo_metadata::Edition;
        assert_eq!(resolver_for_edition(Edition::E2015), 1);
        assert_eq!(resolver_for_edition(Edition::E2018), 2);
        assert_eq!(resolver_for_edition(Edition::E2021), 2);
        assert_eq!(resolver_for_edition(Edition::E2024), 3);
    }

    #[test]
    fn feature_selection_handles_dependency_and_weak_forms() {
        let map = features(&[("std", &[]), ("serde", &[]), ("derive", &[])]);
        assert!(valid_feature_name("std"));
        assert!(valid_feature_name("serde/derive"));
        assert!(valid_feature_name("serde?/derive"));
        assert!(feature_is_declared(&map, "serde/derive"));
        assert!(feature_is_declared(&map, "serde?/derive"));
        assert!(!feature_is_declared(&map, "missing?/derive"));
        for invalid in [
            "",
            "/std",
            "+std",
            "std/",
            "std//x",
            "std/derive?/x",
            ".std",
            "-std",
            "std?",
            "a b",
            "std?/",
        ] {
            assert!(!valid_feature_name(invalid), "{invalid:?} must be rejected");
        }
    }

    #[test]
    fn cross_target_check_cells_are_only_planned_when_check_is_requested() {
        let facts = ToolchainFacts {
            host_target: Some("x86_64-unknown-linux-gnu".to_owned()),
            installed_targets: vec![
                "x86_64-unknown-linux-gnu".to_owned(),
                "wasm32-unknown-unknown".to_owned(),
            ],
            ..ToolchainFacts::default()
        };
        let policy = ProjectPolicy::default();
        let target = "wasm32-unknown-unknown".to_owned();
        let mut cells = Vec::new();
        let mut skipped = Vec::new();
        let mut used_ids = BTreeSet::new();
        plan_targets(
            &RequiredConfigurations {
                include_default: false,
                include_policy: false,
                targets: vec![target.clone()],
                stages: vec![VerifyStage::Test],
                ..RequiredConfigurations::default()
            },
            &policy,
            &facts,
            &[VerifyStage::Test],
            &mut cells,
            &mut skipped,
            &mut used_ids,
        );
        assert!(cells.is_empty(), "{cells:#?}");
        assert!(skipped.iter().any(|cell| {
            cell.status == CellStatus::UnsupportedConfiguration
                && cell.reason.contains("was not among the requested stages")
        }));

        let mut cells = Vec::new();
        let mut skipped = Vec::new();
        let mut used_ids = BTreeSet::new();
        plan_targets(
            &RequiredConfigurations {
                include_default: false,
                include_policy: false,
                targets: vec![target.clone()],
                stages: vec![VerifyStage::Check],
                ..RequiredConfigurations::default()
            },
            &policy,
            &facts,
            &[VerifyStage::Check],
            &mut cells,
            &mut skipped,
            &mut used_ids,
        );
        assert_eq!(cells.len(), 1, "{cells:#?}");
        assert_eq!(cells[0].target_triple.as_deref(), Some(target.as_str()));
        assert!(cells[0].compile_only);
    }

    #[test]
    fn bounded_reads_reject_oversized_and_symlinked_manifests() {
        let root = std::env::temp_dir().join(format!(
            "agz-verify-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |value| value.as_nanos())
        ));
        fs::create_dir_all(&root).expect("create fixture root");
        let authority = test_authority(&root);
        let oversized = root.join("Cargo.toml");
        fs::write(&oversized, "[workspace]\n".repeat(8_000)).expect("write oversized manifest");
        assert!(authority.read_file(&oversized, MAX_CONFIG_BYTES).is_err());
        #[cfg(unix)]
        {
            let real = root.join("real.toml");
            fs::write(&real, "[workspace]\nresolver = \"2\"\n").expect("write real manifest");
            let link = root.join("linked.toml");
            std::os::unix::fs::symlink(&real, &link).expect("create manifest symlink");
            assert!(authority.read_file(&link, MAX_CONFIG_BYTES).is_err());
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn test_authority(root: &Path) -> Arc<AuthorizedRoot> {
        let guard = crate::workspace::RootGuard::new([root.to_owned()], std::iter::empty())
            .expect("create test root guard");
        Arc::clone(&guard.configured_roots()[0])
    }

    #[test]
    fn gate_status_mapping_keeps_skips_distinct() {
        assert_eq!(map_gate_status(GateStatus::FastPass), CellStatus::Pass);
        assert_eq!(map_gate_status(GateStatus::Fail), CellStatus::Fail);
        assert_eq!(
            map_gate_status(GateStatus::Unavailable),
            CellStatus::RunnerUnavailable
        );
        assert_eq!(map_gate_status(GateStatus::Timeout), CellStatus::Timeout);
        assert!(CellStatus::Pass.is_completed());
        assert!(!CellStatus::SkippedBudget.is_completed());
        assert!(!CellStatus::NotInstalled.is_completed());
    }

    #[test]
    fn effective_budget_is_clamped_by_configuration() {
        let config = VerifyConfig {
            max_cells: 4,
            max_wall_ms: 10_000,
        };
        let budget = VerifyBudget {
            max_cells: Some(16),
            max_wall_ms: Some(600_000),
        };
        assert_eq!(effective_max_cells(&budget, &config), 4);
        assert_eq!(effective_max_wall_ms(&budget, &config), 10_000);
        let narrow = VerifyBudget {
            max_cells: Some(2),
            max_wall_ms: Some(3_000),
        };
        assert_eq!(effective_max_cells(&narrow, &config), 2);
        assert_eq!(effective_max_wall_ms(&narrow, &config), 3_000);
    }
}
