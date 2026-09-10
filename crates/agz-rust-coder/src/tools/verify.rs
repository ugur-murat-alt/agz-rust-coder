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
    change::{
        ChangeAction, ChangeRequest, ChangeService, NewFileInput, PatchInput,
        model::{ChangeRecord, RecordState},
    },
    config::{GateCache, VerifyConfig},
    gate::{
        GateDetail, GateEvidence, GateRequest, GateSource, GateStatus, GateTargetId,
        ProgressCallback, ProgressEvent, ProgressStage, TestRunner, ValidationOptions,
        validate_toolchain_name,
    },
    workspace::{
        AuthorizedRoot, ClientRoots, DirectoryEntryKind, RootGuard, WorkspaceRoot,
        WorkspaceSnapshot,
    },
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
    TestPlan,
    TestRun,
    TestCandidate,
}

impl VerifyAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MatrixPlan => "matrix_plan",
            Self::MatrixRun => "matrix_run",
            Self::TestPlan => "test_plan",
            Self::TestRun => "test_run",
            Self::TestCandidate => "test_candidate",
        }
    }

    pub const fn is_test_action(self) -> bool {
        matches!(self, Self::TestPlan | Self::TestRun | Self::TestCandidate)
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
    /// Maximum planned/runnable test items for test actions. Only narrows the
    /// configured `verify.max_tests` ceiling.
    #[serde(default)]
    #[schemars(range(min = 1, max = 64))]
    pub max_tests: Option<u32>,
    /// Repeated baseline/candidate runs used to observe flakiness. Only
    /// narrows the configured `verify.repeats` ceiling.
    #[serde(default)]
    #[schemars(range(min = 1, max = 5))]
    pub repeats: Option<u32>,
}

/// One explicit user mapping from a changed path, package, or target to a test
/// scope. Mappings are hints; they never hide the graph-derived plan entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestMapping {
    /// Workspace-relative file or directory the mapping applies to.
    #[serde(default)]
    pub path: Option<String>,
    /// Workspace package name the mapping applies to.
    #[serde(default)]
    pub package: Option<String>,
    /// Target (integration test binary, lib, or bin) the mapping applies to.
    #[serde(default)]
    pub target: Option<String>,
    /// Exact test function name to run.
    #[serde(default)]
    pub test_name: Option<String>,
}

/// A caller-provided semantic reference hint (for example from the `references`
/// tool). The referenced file joins the changed set as an advisory input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticReference {
    /// Workspace-relative file containing the referenced symbol.
    pub file: String,
    /// Optional symbol name for display only.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// Bounded regression-test patch supplied with `test_candidate`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestPatchInput {
    #[serde(default)]
    pub patches: Vec<PatchInput>,
    #[serde(default)]
    pub new_files: Vec<NewFileInput>,
}

/// Behavior contract the regression test must demonstrate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BehaviorContract {
    /// Exact test function name that must fail on the baseline snapshot.
    pub test_name: String,
    /// Workspace package that hosts the regression test.
    #[serde(default)]
    pub package: Option<String>,
    /// Test binary or target name that hosts the regression test.
    #[serde(default)]
    pub target: Option<String>,
    /// Substring that the baseline failure output must contain (the expected
    /// assertion message). A failure without this text is not evidence.
    pub expected_failure: String,
}

/// One matrix or test request.
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub action: VerifyAction,
    pub directory: Option<PathBuf>,
    pub client_roots: ClientRoots,
    pub root_epoch: u64,
    pub change_id: Option<String>,
    pub required: RequiredConfigurations,
    pub budget: VerifyBudget,
    /// Test runner/features for test actions.
    pub test_configuration: ValidationOptions,
    pub test_mappings: Vec<TestMapping>,
    pub changed_paths: Vec<String>,
    pub semantic_references: Vec<SemanticReference>,
    pub test_patch: Option<TestPatchInput>,
    pub behavior_contract: Option<BehaviorContract>,
    /// Request-authorized workspace root; required for `test_candidate`.
    pub workspace: Option<WorkspaceRoot>,
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

/// One planned test scope with its full identity and selection reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestPlanItemData {
    pub id: String,
    /// Lower runs first: cheapest, most directly relevant scopes.
    pub rank: u64,
    /// `unit`, `integration`, `doctest`, `mapping`, or `workspace`.
    pub scope: String,
    pub package: String,
    pub package_id: String,
    pub target: String,
    pub target_kind: String,
    /// Bounded test-name filter recorded exactly as it is applied.
    pub filter: Option<String>,
    /// Exact test name that must appear in executed results for this item to
    /// count as evidence.
    pub exact_test: Option<String>,
    pub features: Vec<String>,
    pub no_default_features: bool,
    /// True when the item is planned as its own feature-gated scope.
    pub feature_gated: bool,
    /// Runner recorded for this scope (`cargo` or `nextest`). Doctest scopes
    /// always record `cargo`.
    pub runner: String,
    pub reason: String,
}

/// Bounded test plan with every include/skip reason visible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestPlanData {
    pub items: Vec<TestPlanItemData>,
    pub skipped: Vec<SkippedCellData>,
    pub sources: Vec<String>,
    /// Conservative widening reasons; a narrow plan is never silently trusted.
    pub widened_because: Vec<String>,
    /// True only when the plan covers the whole workspace test inventory.
    pub full: bool,
    pub max_tests: u64,
}

/// One repeated baseline or candidate observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestRepeatData {
    pub run: u64,
    /// Classified run status (`PASS`, `FAIL`, `ZERO_MATCH`, `IGNORED_ONLY`,
    /// `COMPILE_FAIL`, `TIMEOUT`, `CANCELLED`, `INCONCLUSIVE`, ...).
    pub status: String,
    pub gate_status: String,
    pub exact_test_seen: bool,
    pub expected_failure_seen: bool,
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub tests_executed: u64,
    pub duration_ms: u64,
    pub input_hash: Option<String>,
    pub command_hash: Option<String>,
    pub environment_hash: Option<String>,
    pub reason: String,
}

/// Aggregated observations for one baseline/candidate side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestCandidateSideData {
    pub repeats: Vec<TestRepeatData>,
    pub pass_observed: u64,
    pub fail_observed: u64,
    pub other_observed: u64,
    pub exact_test_seen: bool,
}

/// `test_candidate` comparison result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestCandidateData {
    pub change_id: String,
    pub change_revision: u64,
    pub change_base_identity: String,
    /// Server-owned probe change id used for both snapshots.
    pub probe_change_id: Option<String>,
    /// True when the candidate snapshot was reconstructed from the recorded
    /// change patches on a fresh capture of the same base identity.
    pub reconstructed_candidate: bool,
    pub test_name: String,
    pub expected_failure: String,
    pub repeats: u64,
    pub baseline: TestCandidateSideData,
    pub candidate: TestCandidateSideData,
    /// `SATISFIED`, `VIOLATED`, `BASELINE_INCOMPATIBLE`, `REJECTED`, or
    /// `INCONCLUSIVE`.
    pub contract_status: String,
    pub compatible: bool,
    pub flaky: bool,
    /// Visible cheat findings (test deletion, ignore addition, assertion
    /// weakening, scope narrowing) detected before or during the comparison.
    pub detections: Vec<String>,
}

/// One executed test scope with its exact identity and evidence binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestRunItemData {
    pub item: TestPlanItemData,
    pub status: String,
    pub reason: String,
    /// True only when the exact requested test name was observed as executed.
    pub exact_test_seen: bool,
    pub tests_executed: u64,
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    /// Bounded list of executed test names parsed from libtest/nextest output.
    pub executed_names: Vec<String>,
    pub duration_ms: u64,
    pub job_id: Option<String>,
    pub gate_status: Option<String>,
    pub input_hash: Option<String>,
    pub command_hash: Option<String>,
    pub environment_hash: Option<String>,
    pub command: Option<String>,
    pub diagnostics: Vec<String>,
}

/// `test_run` result. `suite` is `FULL_REQUESTED_SUITE` only when the plan is
/// the full workspace test inventory and every item completed with a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestRunData {
    pub full: bool,
    pub requested: u64,
    pub completed: u64,
    pub missing: Vec<String>,
    pub suite: String,
    /// Doctest gate summary: nextest never removes the separate doctest scope.
    pub doctest_gate: String,
    pub items: Vec<TestRunItemData>,
}

/// Bounded matrix plan or run result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerifyOutcome {
    pub action: String,
    /// `PLANNED`, `FULL_REQUESTED_MATRIX`, `PARTIAL`, `TESTED_SUBSET`,
    /// `FULL_REQUESTED_SUITE`, `SATISFIED`, `VIOLATED`, ... .
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_plan: Option<TestPlanData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_run: Option<TestRunData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_candidate: Option<TestCandidateData>,
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
            test_plan: None,
            test_run: None,
            test_candidate: None,
        }
    }
}

/// Matrix planning and execution service.
#[derive(Debug, Clone)]
pub struct VerifyService {
    check: Arc<CheckService>,
    change: Option<Arc<ChangeService>>,
    config: VerifyConfig,
}

impl VerifyService {
    pub fn new(check: Arc<CheckService>, config: VerifyConfig) -> Self {
        Self {
            check,
            change: None,
            config,
        }
    }

    /// Binds the revision-bound change engine for `test_candidate` snapshots.
    pub fn with_change(mut self, change: Option<Arc<ChangeService>>) -> Self {
        self.change = change;
        self
    }

    pub fn config(&self) -> &VerifyConfig {
        &self.config
    }

    /// Plan or run the requested matrix or test action.
    pub async fn execute(
        &self,
        request: VerifyRequest,
        progress: Option<ProgressCallback>,
        cancellation: Option<CancellationToken>,
    ) -> VerifyOutcome {
        let action = request.action;
        let committed = cancellation.clone().unwrap_or_default();
        if action.is_test_action() {
            return Box::pin(self.execute_test(request, progress, committed)).await;
        }
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
            test_plan: None,
            test_run: None,
            test_candidate: None,
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

const MAX_EXECUTED_NAMES: usize = 64;
const TEST_SCAN_MAX_FILES: usize = 4_000;
const TEST_SCAN_MAX_FILE_BYTES: u64 = 1_048_576;
const TEST_DETECTION_LIMIT: usize = 16;

/// Internal plan item before protocol conversion.
#[derive(Debug, Clone)]
struct TestPlanItem {
    id: String,
    rank: u64,
    scope: String,
    package: String,
    package_id: String,
    target: String,
    target_kind: String,
    filter: Option<String>,
    exact_test: Option<String>,
    features: Vec<String>,
    no_default_features: bool,
    feature_gated: bool,
    runner: VerifyRunner,
    reason: String,
}

#[derive(Debug, Clone, Default)]
struct TestPlanBuild {
    items: Vec<TestPlanItem>,
    skipped: Vec<SkippedCell>,
    sources: Vec<String>,
    widened_because: Vec<String>,
    full: bool,
    max_tests: u64,
    changed_packages: Vec<String>,
}

impl TestPlanBuild {
    fn plan_data(&self) -> TestPlanData {
        TestPlanData {
            items: self.items.iter().map(test_plan_item_data).collect(),
            skipped: self
                .skipped
                .iter()
                .map(|skipped| SkippedCellData {
                    id: skipped.id.clone(),
                    status: skipped.status.as_str().to_owned(),
                    reason: skipped.reason.clone(),
                })
                .collect(),
            sources: self.sources.clone(),
            widened_because: self.widened_because.clone(),
            full: self.full,
            max_tests: self.max_tests,
        }
    }
}

fn test_plan_item_data(item: &TestPlanItem) -> TestPlanItemData {
    TestPlanItemData {
        id: item.id.clone(),
        rank: item.rank,
        scope: item.scope.clone(),
        package: item.package.clone(),
        package_id: item.package_id.clone(),
        target: item.target.clone(),
        target_kind: item.target_kind.clone(),
        filter: item.filter.clone(),
        exact_test: item.exact_test.clone(),
        features: item.features.clone(),
        no_default_features: item.no_default_features,
        feature_gated: item.feature_gated,
        runner: item.runner.as_str().to_owned(),
        reason: item.reason.clone(),
    }
}

fn effective_max_tests(budget: &VerifyBudget, config: &VerifyConfig) -> u64 {
    u64::from(budget.max_tests.unwrap_or(u32::MAX)).min(config.max_tests)
}

fn effective_repeats(budget: &VerifyBudget, config: &VerifyConfig) -> u64 {
    u64::from(budget.repeats.unwrap_or(u32::MAX)).min(config.repeats)
}

fn test_outcome_header(
    config: &VerifyConfig,
    request: &VerifyRequest,
    status: &str,
    reason: String,
) -> VerifyOutcome {
    let mut outcome =
        VerifyOutcome::failure(request.action, status, request.change_id.clone(), reason);
    outcome.policy_source = "test-plan".to_owned();
    outcome.resolver = "cargo-metadata".to_owned();
    outcome.exhaustive_reason =
        "test planning is a prioritization heuristic, not a complete test-impact oracle".to_owned();
    outcome.budget = BudgetOutcome {
        max_cells: effective_max_cells(&request.budget, config),
        max_wall_ms: effective_max_wall_ms(&request.budget, config),
        planned: 0,
        executed: 0,
    };
    outcome
}

/// Resolves the changed set from the change record, explicit paths, and
/// semantic reference hints. Every ambiguity becomes a visible widening reason.
fn resolve_changed_set(
    change: Option<&Arc<ChangeService>>,
    change_id: Option<&str>,
    request_paths: &[String],
    references: &[SemanticReference],
    snapshot: &WorkspaceSnapshot,
    widened_because: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let root = snapshot.canonical_worktree.clone();
    let mut changed_from_record = false;
    if let Some(id) = change_id {
        match change {
            Some(change) => match change.record(id) {
                Ok(Some(record)) if !record.changed_files.is_empty() => {
                    changed_from_record = true;
                    for raw in &record.changed_files {
                        push_changed_path(raw, &mut paths, widened_because, &root);
                    }
                }
                Ok(Some(_)) => widened_because
                    .push("the change record has no staged files; widened to workspace".to_owned()),
                Ok(None) => widened_because.push(
                    "the change id has no server-owned record; the changed set is unknown and \
                     was widened to workspace"
                        .to_owned(),
                ),
                Err(error) => widened_because.push(format!(
                    "the change record could not be read ({error}); widened to workspace"
                )),
            },
            None => widened_because.push(
                "changeId was supplied but the change tool is disabled; widened to workspace"
                    .to_owned(),
            ),
        }
    }
    if !changed_from_record {
        for raw in request_paths {
            push_changed_path(raw, &mut paths, widened_because, &root);
        }
    }
    for reference in references {
        push_changed_path(&reference.file, &mut paths, widened_because, &root);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn push_changed_path(
    raw: &str,
    paths: &mut Vec<PathBuf>,
    widened_because: &mut Vec<String>,
    root: &Path,
) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        widened_because.push("an empty changed path was ignored".to_owned());
        return;
    }
    let candidate = Path::new(trimmed);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        widened_because.push(format!(
            "changed path `{trimmed}` is not a bounded workspace-relative path"
        ));
        return;
    }
    paths.push(root.join(candidate));
}

fn owner_node<'a>(
    snapshot: &'a WorkspaceSnapshot,
    absolute: &Path,
) -> Option<&'a crate::workspace::graph::PackageNode> {
    snapshot
        .graph
        .nodes()
        .values()
        .filter(|node| node.workspace_member)
        .filter(|node| absolute == node.root || absolute.starts_with(&node.root))
        .max_by_key(|node| node.root.as_os_str().len())
}

fn global_cargo_input(path: &Path) -> Option<&'static str> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("Cargo.toml") => Some("a Cargo manifest"),
        Some("Cargo.lock") => Some("the lockfile"),
        Some("build.rs") => Some("a build script"),
        Some("rust-toolchain" | "rust-toolchain.toml") => Some("the toolchain file"),
        _ if path
            .components()
            .any(|component| component.as_os_str() == ".cargo") =>
        {
            Some("Cargo configuration")
        }
        _ => None,
    }
}

fn package_has_proc_macro(package: &cargo_metadata::Package) -> bool {
    package
        .targets
        .iter()
        .any(|target| target.kind.iter().any(is_proc_macro_kind))
}

fn is_proc_macro_kind(kind: &cargo_metadata::TargetKind) -> bool {
    matches!(kind, cargo_metadata::TargetKind::ProcMacro)
}

fn lib_root_target(package: &cargo_metadata::Package, absolute: &Path) -> bool {
    package.targets.iter().any(|target| {
        target.kind.iter().any(|kind| {
            matches!(
                kind,
                cargo_metadata::TargetKind::Lib | cargo_metadata::TargetKind::RLib
            )
        }) && target.src_path.as_std_path() == absolute
    })
}

fn is_test_bearing_kind(kind: &cargo_metadata::TargetKind) -> bool {
    matches!(
        kind,
        cargo_metadata::TargetKind::Lib
            | cargo_metadata::TargetKind::RLib
            | cargo_metadata::TargetKind::Bin
            | cargo_metadata::TargetKind::Test
            | cargo_metadata::TargetKind::ProcMacro
    )
}

fn target_kind_name(target: &cargo_metadata::Target) -> &'static str {
    for kind in &target.kind {
        match kind {
            cargo_metadata::TargetKind::Lib => return "lib",
            cargo_metadata::TargetKind::RLib => return "rlib",
            cargo_metadata::TargetKind::Bin => return "bin",
            cargo_metadata::TargetKind::Test => return "test",
            cargo_metadata::TargetKind::ProcMacro => return "proc-macro",
            cargo_metadata::TargetKind::Bench => return "bench",
            cargo_metadata::TargetKind::Example => return "example",
            _ => {}
        }
    }
    "unknown"
}

fn merged_features(base: &[String], required: &[String]) -> Vec<String> {
    let mut merged = base.to_vec();
    merged.extend(required.iter().cloned());
    merged.sort();
    merged.dedup();
    merged
}

struct PackagePlanContext<'a> {
    base_features: &'a [String],
    no_default_features: bool,
    runner: VerifyRunner,
    package_scope: &'a str,
    rank_base: u64,
    reason_prefix: &'a str,
}

fn package_test_items(
    package: &cargo_metadata::Package,
    context: &PackagePlanContext<'_>,
    skipped: &mut Vec<SkippedCell>,
) -> Vec<TestPlanItem> {
    let mut items = Vec::new();
    for target in &package.targets {
        let kind = target_kind_name(target);
        if matches!(kind, "bench" | "example") {
            skipped.push(SkippedCell {
                id: format!(
                    "{}--{kind}--{}",
                    sanitize_id(&package.name),
                    sanitize_id(&target.name)
                ),
                status: CellStatus::UnsupportedConfiguration,
                reason: format!(
                    "`{kind}` target {} is not executed by the default test suite and is not \
                     selected by this plan",
                    target.name
                ),
            });
            continue;
        }
        if !is_test_bearing_kind_target(target) {
            continue;
        }
        let feature_gated = !target.required_features.is_empty();
        let gate_note = if feature_gated {
            format!(
                " and is planned as its own feature-gated scope ({})",
                target.required_features.join(", ")
            )
        } else {
            String::new()
        };
        if target.test {
            items.push(TestPlanItem {
                id: String::new(),
                rank: context.rank_base,
                scope: context.package_scope.to_owned(),
                package: package.name.to_string(),
                package_id: package.id.repr.clone(),
                target: target.name.clone(),
                target_kind: kind.to_owned(),
                filter: None,
                exact_test: None,
                features: merged_features(context.base_features, &target.required_features),
                no_default_features: context.no_default_features,
                feature_gated,
                runner: context.runner,
                reason: format!(
                    "{}: {kind} target `{}` executes unit/integration tests{gate_note}",
                    context.reason_prefix, target.name
                ),
            });
        }
        let doctestable = target.doctest
            && target.kind.iter().any(|kind| {
                matches!(
                    kind,
                    cargo_metadata::TargetKind::Lib | cargo_metadata::TargetKind::RLib
                )
            });
        if doctestable {
            items.push(TestPlanItem {
                id: String::new(),
                rank: context.rank_base + 2,
                scope: "doctest".to_owned(),
                package: package.name.to_string(),
                package_id: package.id.repr.clone(),
                target: target.name.clone(),
                target_kind: "doctest".to_owned(),
                filter: None,
                exact_test: None,
                features: merged_features(context.base_features, &target.required_features),
                no_default_features: context.no_default_features,
                feature_gated,
                runner: VerifyRunner::Cargo,
                reason: format!(
                    "{}: doctests for `{}` are a separate scope; a nextest run never replaces \
                     the doctest gate{gate_note}",
                    context.reason_prefix, target.name
                ),
            });
        }
    }
    items
}

fn is_test_bearing_kind_target(target: &cargo_metadata::Target) -> bool {
    target.kind.iter().any(is_test_bearing_kind)
}

#[allow(clippy::too_many_arguments)]
fn build_test_plan(
    request: &VerifyRequest,
    snapshot: &WorkspaceSnapshot,
    record: Option<&ChangeRecord>,
    change: Option<&Arc<ChangeService>>,
    max_tests: u64,
) -> TestPlanBuild {
    let mut build = TestPlanBuild {
        max_tests,
        ..TestPlanBuild::default()
    };
    build
        .sources
        .push("cargo metadata test inventory".to_owned());
    build
        .sources
        .push("workspace package graph (reverse dependents)".to_owned());
    if !request.test_mappings.is_empty() {
        build.sources.push("explicit user mappings".to_owned());
    }
    if !request.semantic_references.is_empty() {
        build
            .sources
            .push("caller-provided semantic reference hints".to_owned());
    }
    if record.is_some_and(|record| !record.changed_files.is_empty()) {
        build.sources.push("change record changed files".to_owned());
    }

    let mut changed = resolve_changed_set(
        change,
        request.change_id.as_deref(),
        &request.changed_paths,
        &request.semantic_references,
        snapshot,
        &mut build.widened_because,
    );
    changed.sort();
    changed.dedup();

    let mut changed_packages = BTreeSet::new();
    for path in &changed {
        if let Some(global) = global_cargo_input(path) {
            build.widened_because.push(format!(
                "{global} changed ({}); a narrow test plan cannot be trusted",
                display_relative(path, snapshot)
            ));
            continue;
        }
        let Some(node) = owner_node(snapshot, path) else {
            build.widened_because.push(format!(
                "changed path {} has no owning workspace package",
                display_relative(path, snapshot)
            ));
            continue;
        };
        let Some(package) = snapshot
            .metadata
            .packages
            .iter()
            .find(|package| package.id.repr == node.package_id)
        else {
            build.widened_because.push(format!(
                "changed path {} belongs to package {} that is missing from metadata",
                display_relative(path, snapshot),
                node.name
            ));
            continue;
        };
        if package_has_proc_macro(package) {
            build.widened_because.push(format!(
                "procedural macro package {} changed; macro expansion affects consumers and \
                 cannot be covered by a narrow plan",
                node.name
            ));
            continue;
        }
        if lib_root_target(package, path) {
            build.widened_because.push(format!(
                "library root file {} changed; public API impact cannot be excluded",
                display_relative(path, snapshot)
            ));
            continue;
        }
        changed_packages.insert(node.package_id.clone());
    }
    if !snapshot.external_paths.is_empty() {
        build
            .widened_because
            .push("external path dependencies require workspace scope".to_owned());
    }

    let mut mapping_packages = BTreeSet::new();
    for mapping in &request.test_mappings {
        match mapping.package.as_deref() {
            Some(name) => {
                match snapshot
                    .metadata
                    .packages
                    .iter()
                    .find(|package| {
                        snapshot.metadata.workspace_members.contains(&package.id)
                            && package.name.as_str() == name
                    }) {
                    Some(package) => {
                        mapping_packages.insert(package.id.repr.clone());
                    }
                    None => build.skipped.push(SkippedCell {
                        id: format!("mapping-{}", sanitize_id(name)),
                        status: CellStatus::UnsupportedConfiguration,
                        reason: format!(
                            "explicit mapping names package `{name}` which is not a workspace member"
                        ),
                    }),
                }
            }
            None => {
                if let Some(path) = mapping.path.as_deref() {
                    let absolute = snapshot.canonical_worktree.join(path);
                    match owner_node(snapshot, &absolute) {
                        Some(node) => {
                            mapping_packages.insert(node.package_id.clone());
                        }
                        None => build.skipped.push(SkippedCell {
                            id: format!("mapping-{}", sanitize_id(path)),
                            status: CellStatus::UnsupportedConfiguration,
                            reason: format!(
                                "explicit mapping path `{path}` has no owning workspace package"
                            ),
                        }),
                    }
                }
            }
        }
    }

    let changed_only = changed_packages.clone();
    let dependents = snapshot
        .graph
        .reverse_dependents(changed_packages.iter().cloned())
        .difference(&changed_packages)
        .cloned()
        .collect::<BTreeSet<_>>();

    let no_changed_set = changed_packages.is_empty()
        && mapping_packages.is_empty()
        && request
            .test_mappings
            .iter()
            .all(|mapping| mapping.test_name.is_none());
    if no_changed_set {
        build.widened_because.push(
            "no changed set, mapping, or reference was provided; the plan conservatively \
             covers the workspace"
                .to_owned(),
        );
    }
    let widened = !build.widened_because.is_empty();
    if widened {
        build.widened_because.sort();
        build.widened_because.dedup();
    }

    let mut included = BTreeSet::new();
    included.extend(changed_packages.iter().cloned());
    included.extend(dependents.iter().cloned());
    included.extend(mapping_packages.iter().cloned());
    if widened {
        included.extend(
            snapshot
                .metadata
                .packages
                .iter()
                .filter(|package| snapshot.metadata.workspace_members.contains(&package.id))
                .map(|package| package.id.repr.clone()),
        );
    }

    let mut items: Vec<TestPlanItem> = Vec::new();
    let mut used_ids = BTreeSet::new();
    for package in &snapshot.metadata.packages {
        if !snapshot.metadata.workspace_members.contains(&package.id) {
            continue;
        }
        let package_id = package.id.repr.clone();
        if !included.contains(&package_id) {
            continue;
        }
        let (scope, rank_base, reason_prefix) = if mapping_packages.contains(&package_id)
            && !changed_only.contains(&package_id)
        {
            (
                "mapping",
                0_u64,
                format!("explicit user mapping selects package {}", package.name),
            )
        } else if changed_only.contains(&package_id) {
            (
                "changed",
                1_u64,
                format!(
                    "directly changed package {} (changed paths resolved through the package graph)",
                    package.name
                ),
            )
        } else if dependents.contains(&package_id) {
            (
                "consumer",
                4_u64,
                format!(
                    "reverse dependency consumer {} depends on a changed package",
                    package.name
                ),
            )
        } else {
            (
                "workspace",
                7_u64,
                format!(
                    "workspace-wide conservative widening includes package {}",
                    package.name
                ),
            )
        };
        let context = PackagePlanContext {
            base_features: &request.test_configuration.features,
            no_default_features: request.test_configuration.no_default_features,
            runner: VerifyRunner::Cargo,
            package_scope: scope,
            rank_base,
            reason_prefix: &reason_prefix,
        };
        for item in package_test_items(package, &context, &mut build.skipped) {
            let mut item = item;
            let id_source = format!(
                "{}--{}--{}--{}",
                item.package,
                item.target_kind,
                item.target,
                item.filter.as_deref().unwrap_or("suite")
            );
            let id = unique_id(&mut used_ids, &id_source);
            item.id = id.clone();
            items.push(item);
        }
    }

    let runner = match request.test_configuration.runner {
        TestRunner::Cargo => VerifyRunner::Cargo,
        TestRunner::Nextest => VerifyRunner::Nextest,
    };
    for (index, mapping) in request.test_mappings.iter().enumerate() {
        let Some(test_name) = mapping.test_name.as_deref() else {
            continue;
        };
        if !valid_test_name(test_name) {
            build.skipped.push(SkippedCell {
                id: format!("mapping-{}", index + 1),
                status: CellStatus::UnsupportedConfiguration,
                reason: "explicit mapping testName is not a bounded test name".to_owned(),
            });
            continue;
        }
        let package = mapping
            .package
            .clone()
            .unwrap_or_else(|| "workspace".to_owned());
        let target = mapping
            .target
            .clone()
            .unwrap_or_else(|| "mapped-test".to_owned());
        let doctest = target == "doctest" || mapping.target.as_deref() == Some("doctest");
        let mut item = TestPlanItem {
            id: String::new(),
            rank: 0,
            scope: "mapping".to_owned(),
            package: package.clone(),
            package_id: String::new(),
            target: target.clone(),
            target_kind: if doctest { "doctest" } else { "test" }.to_owned(),
            filter: if doctest {
                None
            } else {
                Some(test_name.to_owned())
            },
            exact_test: Some(test_name.to_owned()),
            features: request.test_configuration.features.clone(),
            no_default_features: request.test_configuration.no_default_features,
            feature_gated: false,
            runner: if doctest { VerifyRunner::Cargo } else { runner },
            reason: format!(
                "explicit user mapping requires exact test `{test_name}`{}",
                mapping
                    .package
                    .as_deref()
                    .map_or_else(String::new, |name| format!(" in package {name}"))
            ),
        };
        item.id = unique_id(&mut used_ids, &format!("mapping-{}-{test_name}", index + 1));
        items.push(item);
    }

    items.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.id.cmp(&right.id))
    });
    let full_requested = widened;
    if items.len() as u64 > max_tests {
        let excess = items.split_off(usize::try_from(max_tests).unwrap_or(usize::MAX));
        for item in excess {
            build.skipped.push(SkippedCell {
                id: item.id,
                status: CellStatus::SkippedBudget,
                reason: "verify.maxTests budget exhausted during test planning".to_owned(),
            });
        }
    }
    build.changed_packages = changed_packages.into_iter().collect();
    build.full = full_requested
        && build
            .skipped
            .iter()
            .all(|skipped| skipped.status != CellStatus::SkippedBudget);
    build.items = items;
    build
}

fn display_relative(path: &Path, snapshot: &WorkspaceSnapshot) -> String {
    path.strip_prefix(&snapshot.canonical_worktree).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

fn valid_test_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && !name.chars().any(char::is_control)
}

impl VerifyService {
    fn change_record(&self, request: &VerifyRequest) -> Option<ChangeRecord> {
        let id = request.change_id.as_deref()?;
        let change = self.change.as_ref()?;
        change.record(id).ok().flatten()
    }

    async fn execute_test(
        &self,
        request: VerifyRequest,
        progress: Option<ProgressCallback>,
        committed: CancellationToken,
    ) -> VerifyOutcome {
        let action = request.action;
        let plan_snapshot = match self
            .check
            .plan_snapshot(
                request.directory.clone(),
                request.client_roots.clone(),
                Some(committed.clone()),
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
        match action {
            VerifyAction::TestPlan => self.test_plan(&request, &plan_snapshot, &committed),
            VerifyAction::TestRun => {
                self.test_run(&request, &plan_snapshot, progress, &committed)
                    .await
            }
            VerifyAction::TestCandidate => {
                self.test_candidate(&request, &plan_snapshot, progress, &committed)
                    .await
            }
            VerifyAction::MatrixPlan | VerifyAction::MatrixRun => {
                unreachable!("execute_test is only reached for test actions")
            }
        }
    }

    fn test_plan(
        &self,
        request: &VerifyRequest,
        plan_snapshot: &super::check::PlanSnapshot,
        _committed: &CancellationToken,
    ) -> VerifyOutcome {
        let record = self.change_record(request);
        let max_tests = effective_max_tests(&request.budget, &self.config);
        let build = build_test_plan(
            request,
            &plan_snapshot.snapshot,
            record.as_ref(),
            self.change.as_ref(),
            max_tests,
        );
        let plan = build.plan_data();
        let mut outcome = test_outcome_header(&self.config, request, "PLANNED", String::new());
        outcome.skipped = plan.skipped.clone();
        outcome.missing_cell_ids = plan.skipped.iter().map(|row| row.id.clone()).collect();
        outcome.budget.planned = plan.items.len() as u64;
        outcome.complete = plan.full;
        outcome.reason = format!(
            "planned {} test scope(s), {} candidate(s) skipped; coverage: {}; sources: {}",
            plan.items.len(),
            plan.skipped.len(),
            if plan.full {
                "full workspace inventory"
            } else {
                "subset (development feedback; not the final gate)"
            },
            plan.sources.join(", ")
        );
        outcome.test_plan = Some(plan);
        outcome
    }

    async fn test_run(
        &self,
        request: &VerifyRequest,
        plan_snapshot: &super::check::PlanSnapshot,
        progress: Option<ProgressCallback>,
        committed: &CancellationToken,
    ) -> VerifyOutcome {
        let record = self.change_record(request);
        let max_tests = effective_max_tests(&request.budget, &self.config);
        let build = build_test_plan(
            request,
            &plan_snapshot.snapshot,
            record.as_ref(),
            self.change.as_ref(),
            max_tests,
        );
        let plan = build.plan_data();
        let mut outcome = test_outcome_header(&self.config, request, "INCONCLUSIVE", String::new());
        outcome.skipped = plan.skipped.clone();
        outcome.missing_cell_ids = plan.skipped.iter().map(|row| row.id.clone()).collect();
        outcome.budget.planned = plan.items.len() as u64;
        outcome.test_plan = Some(plan.clone());

        let started = Instant::now();
        let wall_deadline = started + Duration::from_millis(outcome.budget.max_wall_ms);
        let root = plan_snapshot.snapshot.workspace_root.clone();
        let total = build.items.len();
        let mut runs = Vec::with_capacity(total);
        let mut completed = 0_u64;
        let mut passed = 0_u64;
        let mut failed = false;
        let mut missing = plan
            .skipped
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        for (index, item) in build.items.iter().enumerate() {
            let planned = test_plan_item_data(item);
            if committed.is_cancelled() {
                runs.push(stopped_test_run(
                    planned,
                    "CANCELLED",
                    "run stopped after client cancellation before this scope started",
                ));
                missing.push(item.id.clone());
                continue;
            }
            if Instant::now() >= wall_deadline {
                runs.push(stopped_test_run(
                    planned,
                    "SKIPPED_BUDGET",
                    "verify.maxWallMs budget exhausted before this scope started",
                ));
                missing.push(item.id.clone());
                continue;
            }
            if let Some(callback) = progress.as_ref() {
                emit(
                    callback,
                    ProgressStage::Running,
                    &format!("test scope {}/{}: {}", index + 1, total, item.id),
                    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                );
            }
            let run = self.run_test_item(item, request, &root, committed).await;
            match run.status.as_str() {
                "PASS" => passed = passed.saturating_add(1),
                "FAIL" | "COMPILE_FAIL" => failed = true,
                _ => {}
            }
            if run.status != "PASS" {
                missing.push(item.id.clone());
            }
            completed = completed.saturating_add(1);
            runs.push(run);
        }
        outcome.budget.executed = completed;

        let doctest_items = runs
            .iter()
            .filter(|run| run.item.target_kind == "doctest")
            .collect::<Vec<_>>();
        let doctest_gate = if doctest_items.is_empty() {
            "NOT_REQUIRED".to_owned()
        } else {
            let statuses = doctest_items
                .iter()
                .map(|run| run.status.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let nextest_note = if request.test_configuration.runner == TestRunner::Nextest {
                "; the nextest runner does not replace the separate doctest gate"
            } else {
                ""
            };
            format!("REQUIRED: {statuses}{nextest_note}")
        };

        let all_pass = !runs.is_empty() && runs.iter().all(|run| run.status == "PASS");
        let status = if runs.is_empty() {
            "INCONCLUSIVE"
        } else if failed {
            "FAIL"
        } else if all_pass && plan.full {
            "FULL_REQUESTED_SUITE"
        } else if passed > 0 {
            "TESTED_SUBSET"
        } else {
            "INCONCLUSIVE"
        };
        outcome.status = status.to_owned();
        outcome.complete = status == "FULL_REQUESTED_SUITE";
        outcome.all_pass = outcome.complete;
        outcome.reason = match status {
            "FULL_REQUESTED_SUITE" => format!(
                "every requested workspace test scope completed with a pass ({passed} scope(s))"
            ),
            "TESTED_SUBSET" => format!(
                "{passed} of {total} test scope(s) passed; {} missing; a subset run is \
                 development feedback and never the final gate",
                missing.len()
            ),
            "FAIL" => format!(
                "at least one executed test scope failed; {passed} scope(s) passed and {} did not",
                (total as u64).saturating_sub(passed)
            ),
            _ => "no executable test evidence was produced; zero matches, ignored-only \
                  results, custom harnesses, and missing summaries are never a pass"
                .to_owned(),
        };
        if let Some(callback) = progress.as_ref() {
            emit(
                callback,
                ProgressStage::Completed,
                &outcome.reason,
                started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            );
        }
        outcome.test_run = Some(TestRunData {
            full: plan.full,
            requested: total as u64,
            completed,
            missing,
            suite: status.to_owned(),
            doctest_gate,
            items: runs,
        });
        outcome
    }

    async fn run_test_item(
        &self,
        item: &TestPlanItem,
        request: &VerifyRequest,
        root: &Path,
        committed: &CancellationToken,
    ) -> TestRunItemData {
        let doctest = item.target_kind == "doctest";
        let mut options = request.test_configuration.clone();
        options.features = item.features.clone();
        options.no_default_features = item.no_default_features;
        options.runner = if doctest {
            TestRunner::Cargo
        } else {
            item.runner.gate_runner()
        };
        options.test_filter = if doctest { None } else { item.filter.clone() };
        let target = if doctest {
            GateTargetId::Doc
        } else {
            GateTargetId::Test
        };
        let gate = GateRequest::new(root.to_path_buf(), target)
            .with_options(options)
            .with_detail(GateDetail::Compact)
            .with_client_roots(request.client_roots.clone())
            .with_root_epoch(request.root_epoch);
        let evidence = self.check.run(gate, None, Some(committed.clone())).await;
        let observation = observe_test_output(&evidence_text(&evidence));
        let (status, reason, exact_test_seen) =
            classify_test_evidence(item, &evidence, &observation);
        let tests_executed = evidence
            .steps
            .iter()
            .filter_map(|step| step.evidence.tests_executed)
            .max()
            .unwrap_or(0);
        TestRunItemData {
            item: test_plan_item_data(item),
            status,
            reason,
            exact_test_seen,
            tests_executed,
            passed: observation.passed,
            failed: observation.failed,
            ignored: observation.ignored,
            executed_names: observation
                .executed
                .iter()
                .take(MAX_EXECUTED_NAMES)
                .cloned()
                .collect(),
            duration_ms: evidence.response_ms,
            job_id: Some(evidence.job_id.clone()),
            gate_status: Some(evidence.status.as_str().to_owned()),
            input_hash: Some(evidence.input_hash.clone()),
            command_hash: Some(evidence.command_hash.clone()),
            environment_hash: Some(evidence.environment_hash.clone()),
            command: evidence
                .steps
                .first()
                .map(|step| step.command.clone())
                .filter(|command| !command.is_empty()),
            diagnostics: evidence
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
                .collect(),
        }
    }

    async fn test_candidate(
        &self,
        request: &VerifyRequest,
        plan_snapshot: &super::check::PlanSnapshot,
        _progress: Option<ProgressCallback>,
        committed: &CancellationToken,
    ) -> VerifyOutcome {
        let mut outcome = test_outcome_header(&self.config, request, "INCONCLUSIVE", String::new());
        let Some(contract) = request.behavior_contract.as_ref() else {
            outcome.status = "INVALID".to_owned();
            outcome.reason = "test_candidate requires behaviorContract".to_owned();
            return outcome;
        };
        let Some(change) = self.change.as_ref() else {
            outcome.reason = "test_candidate requires the change tool to be enabled".to_owned();
            return outcome;
        };
        let result =
            Box::pin(self.candidate_flow(request, &plan_snapshot.snapshot, contract, committed))
                .await;
        // The probe scratch is server-owned and always removed, including on
        // early failure paths; cleanup uses a fresh token so a cancelled
        // request still leaves no unbounded scratch behind.
        let probe_id = match &result {
            Ok(evaluation) => evaluation.data.probe_change_id.clone(),
            Err(failure) => failure.probe_change_id.clone(),
        };
        if let (Some(probe_id), Some(workspace)) = (probe_id, request.workspace.as_ref()) {
            let discard = change
                .execute(
                    change_discard_request(&probe_id),
                    workspace,
                    CancellationToken::new(),
                    None,
                )
                .await;
            if discard.status != "DISCARDED" {
                outcome.warnings.push(format!(
                    "probe scratch cleanup was incomplete: {}",
                    discard.data.reason
                ));
            }
        }
        match result {
            Ok(evaluation) => {
                let data = evaluation.data;
                outcome.status = data.contract_status.clone();
                outcome.complete = data.contract_status == "SATISFIED";
                outcome.all_pass = outcome.complete;
                outcome.warnings.extend(evaluation.warnings);
                outcome.reason = candidate_reason(&data);
                outcome.test_candidate = Some(data);
            }
            Err(failure) => {
                outcome.status = failure.status.to_owned();
                outcome.reason = failure.reason;
            }
        }
        outcome
    }

    async fn candidate_flow(
        &self,
        request: &VerifyRequest,
        snapshot: &WorkspaceSnapshot,
        contract: &BehaviorContract,
        committed: &CancellationToken,
    ) -> Result<CandidateEvaluation, CandidateFailure> {
        let change = self
            .change
            .as_ref()
            .ok_or_else(|| CandidateFailure::new("INCONCLUSIVE", "change tool disabled"))?;
        let workspace = request
            .workspace
            .as_ref()
            .ok_or_else(|| CandidateFailure::new("INCONCLUSIVE", "workspace root unavailable"))?;
        let change_id = request
            .change_id
            .clone()
            .ok_or_else(|| CandidateFailure::new("INVALID", "testCandidate requires changeId"))?;
        let record = change
            .record(&change_id)
            .map_err(|error| {
                CandidateFailure::new(
                    "INVALID",
                    format!("change record could not be read: {error}"),
                )
            })?
            .ok_or_else(|| {
                CandidateFailure::new(
                    "NOT_FOUND",
                    "the change id has no server-owned scratch record",
                )
            })?;
        if record.state != RecordState::Ready {
            return Err(CandidateFailure::new(
                "FAILED_INCONSISTENT",
                format!(
                    "the change is {} and has no completed candidate revision",
                    record.state.as_str()
                ),
            ));
        }
        if record.revision == 0 {
            return Err(CandidateFailure::new(
                "STALE",
                "the change has no staged revision; testCandidate needs the fix applied to a candidate",
            ));
        }
        if record.base_identity.is_empty() {
            return Err(CandidateFailure::new(
                "STALE",
                "the change record has no base identity to compare against",
            ));
        }
        if record.workspace_epoch != workspace.epoch() {
            return Err(CandidateFailure::new(
                "STALE",
                "the authorization root epoch changed since the change was captured",
            ));
        }
        if record.workspace_root != snapshot.workspace_root {
            return Err(CandidateFailure::new(
                "STALE",
                "the change belongs to a different workspace root",
            ));
        }
        let test_patch = request
            .test_patch
            .as_ref()
            .ok_or_else(|| CandidateFailure::new("INVALID", "testCandidate requires testPatch"))?;

        let repeats = effective_repeats(&request.budget, &self.config);
        let mut detections = analyze_test_patch(test_patch, contract);
        if !patch_provides_test(test_patch, contract) {
            detections.push(format!(
                "test-not-provided: contract test `{}` does not appear in the supplied test patch",
                contract.test_name
            ));
        }
        if !detections.is_empty() {
            return Ok(CandidateEvaluation {
                data: rejected_candidate_data(
                    &change_id, &record, contract, repeats, detections, None,
                ),
                warnings: Vec::new(),
            });
        }

        let create = change
            .execute(change_create_request(), workspace, committed.clone(), None)
            .await;
        if create.status != "CREATED" {
            return Err(CandidateFailure::new(
                create.status,
                format!("baseline probe capture failed: {}", create.data.reason),
            ));
        }
        let probe_id = create.data.change_id.clone().ok_or_else(|| {
            CandidateFailure::new("INCONCLUSIVE", "probe capture published no change id")
        })?;
        let probe_base = create.data.base_identity.clone().unwrap_or_default();
        if probe_base != record.base_identity {
            return Err(CandidateFailure {
                status: "STALE",
                reason: "the workspace content no longer matches the base identity recorded by \
                         the change; both snapshots would not share the same baseline"
                    .to_owned(),
                probe_change_id: Some(probe_id),
            });
        }

        let stage_test = change
            .execute(
                change_stage_request(
                    &probe_id,
                    create.data.revision,
                    &probe_base,
                    test_patch.patches.clone(),
                    test_patch.new_files.clone(),
                ),
                workspace,
                committed.clone(),
                None,
            )
            .await;
        if stage_test.status != "STAGED" {
            return Err(CandidateFailure {
                status: "INCONCLUSIVE",
                reason: format!(
                    "the regression test patch could not be staged: {}",
                    stage_test.data.reason
                ),
                probe_change_id: Some(probe_id),
            });
        }
        let baseline_revision = stage_test.data.revision;
        let probe_root = change.candidate_dir(&probe_id);
        let before = scan_test_markers(&probe_root);
        let baseline = self
            .run_candidate_side(change, &probe_id, request, contract, repeats, committed)
            .await;

        let stage_fix = change
            .execute(
                change_stage_request(
                    &probe_id,
                    baseline_revision,
                    &probe_base,
                    record
                        .patches
                        .iter()
                        .map(|patch| PatchInput {
                            file: patch.file.clone(),
                            old_string: patch.old_string.clone(),
                            new_string: patch.new_string.clone(),
                        })
                        .collect(),
                    record
                        .new_files
                        .iter()
                        .map(|file| NewFileInput {
                            file: file.file.clone(),
                            content: file.content.clone(),
                        })
                        .collect(),
                ),
                workspace,
                committed.clone(),
                None,
            )
            .await;
        if stage_fix.status != "STAGED" {
            return Err(CandidateFailure {
                status: "INCONCLUSIVE",
                reason: format!(
                    "the candidate fix patches could not be reconstructed on the probe snapshot: {}",
                    stage_fix.data.reason
                ),
                probe_change_id: Some(probe_id),
            });
        }
        let after = scan_test_markers(&probe_root);
        detections.extend(compare_test_markers(&before, &after));
        detections.truncate(TEST_DETECTION_LIMIT);
        if !detections.is_empty() {
            return Ok(CandidateEvaluation {
                data: rejected_candidate_data(
                    &change_id,
                    &record,
                    contract,
                    repeats,
                    detections,
                    Some(probe_id.clone()),
                ),
                warnings: Vec::new(),
            });
        }

        let candidate = self
            .run_candidate_side(change, &probe_id, request, contract, repeats, committed)
            .await;
        Ok(evaluate_candidate(
            &change_id,
            &record,
            contract,
            repeats,
            Some(probe_id),
            baseline,
            candidate,
        ))
    }

    async fn run_candidate_side(
        &self,
        change: &Arc<ChangeService>,
        probe_id: &str,
        request: &VerifyRequest,
        contract: &BehaviorContract,
        repeats: u64,
        committed: &CancellationToken,
    ) -> Vec<TestRepeatData> {
        let root = change.candidate_dir(probe_id);
        let dependency_roots = change
            .record(probe_id)
            .ok()
            .flatten()
            .map(|record| record.dependency_roots)
            .unwrap_or_default();
        let guard = match RootGuard::new([root.clone()], dependency_roots) {
            Ok(guard) => Arc::new(guard),
            Err(error) => {
                return vec![failed_repeat(
                    1,
                    "INCONCLUSIVE",
                    format!("probe snapshot could not be authorized: {error}"),
                )];
            }
        };
        let mut config = change.config().clone();
        config.gate.cache = GateCache::Isolated;
        config.gate.cache_dir = change.cache_dir(probe_id);
        let service = CheckService::new(config, guard);
        let mut runs = Vec::new();
        for run in 1..=repeats {
            if committed.is_cancelled() {
                runs.push(failed_repeat(
                    run,
                    "CANCELLED",
                    "candidate comparison was cancelled before this repeat started".to_owned(),
                ));
                break;
            }
            let mut options = request.test_configuration.clone();
            options.test_filter = Some(contract.test_name.clone());
            let gate = GateRequest::new(root.clone(), GateTargetId::Test)
                .with_options(options)
                .with_detail(GateDetail::Compact)
                .with_root_epoch(0);
            let evidence = service.run(gate, None, Some(committed.clone())).await;
            runs.push(candidate_repeat(run, contract, &evidence));
        }
        service.close().await;
        runs
    }
}

struct CandidateFailure {
    status: &'static str,
    reason: String,
    probe_change_id: Option<String>,
}

impl CandidateFailure {
    fn new(status: &'static str, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            probe_change_id: None,
        }
    }
}

struct CandidateEvaluation {
    data: TestCandidateData,
    warnings: Vec<String>,
}

fn change_create_request() -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Create,
        change_id: None,
        expected_revision: None,
        base_identity: None,
        patches: Vec::new(),
        new_files: Vec::new(),
        migration: None,
        target: GateTargetId::Check,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn change_stage_request(
    id: &str,
    revision: u64,
    base_identity: &str,
    patches: Vec<PatchInput>,
    new_files: Vec<NewFileInput>,
) -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Stage,
        change_id: Some(id.to_owned()),
        expected_revision: Some(revision),
        base_identity: Some(base_identity.to_owned()),
        patches,
        new_files,
        migration: None,
        target: GateTargetId::Check,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn change_discard_request(id: &str) -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Discard,
        change_id: Some(id.to_owned()),
        ..change_create_request()
    }
}

fn stopped_test_run(planned: TestPlanItemData, status: &str, reason: &str) -> TestRunItemData {
    TestRunItemData {
        item: planned,
        status: status.to_owned(),
        reason: reason.to_owned(),
        exact_test_seen: false,
        tests_executed: 0,
        passed: 0,
        failed: 0,
        ignored: 0,
        executed_names: Vec::new(),
        duration_ms: 0,
        job_id: None,
        gate_status: None,
        input_hash: None,
        command_hash: None,
        environment_hash: None,
        command: None,
        diagnostics: Vec::new(),
    }
}

fn evidence_text(evidence: &GateEvidence) -> String {
    let mut text = String::new();
    for step in &evidence.steps {
        text.push_str(&step.stdout);
        text.push('\n');
        text.push_str(&step.stderr);
        text.push('\n');
        text.push_str(&step.tail);
        text.push('\n');
    }
    text
}

fn evidence_is_compile_failure(evidence: &GateEvidence) -> bool {
    evidence
        .steps
        .iter()
        .any(|step| step.evidence.build_success == Some(false))
}

#[derive(Debug, Default, Clone)]
struct TestObservation {
    executed: BTreeSet<String>,
    ignored_names: BTreeSet<String>,
    passed: u64,
    failed: u64,
    ignored: u64,
    summary_seen: bool,
}

/// Parses libtest and nextest human output. A missing summary is never treated
/// as a pass, so custom harnesses cannot claim executed tests.
fn observe_test_output(text: &str) -> TestObservation {
    let mut observation = TestObservation::default();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("test ")
            && let Some((name, tail)) = rest.rsplit_once(" ... ")
        {
            let name = name.trim();
            if !name.is_empty() {
                match tail.trim() {
                    "ok" | "FAILED" => {
                        observation.executed.insert(name.to_owned());
                    }
                    "ignored" => {
                        observation.ignored_names.insert(name.to_owned());
                    }
                    _ => {}
                }
            }
        }
        if let Some(summary) = line.strip_prefix("test result: ") {
            observation.summary_seen = true;
            if let Some((_, summary)) = summary.split_once(". ") {
                for field in summary.split(';') {
                    let field = field.trim();
                    if let Some(value) = field.strip_suffix(" passed") {
                        observation.passed = observation
                            .passed
                            .saturating_add(value.parse::<u64>().unwrap_or(0));
                    } else if let Some(value) = field.strip_suffix(" failed") {
                        observation.failed = observation
                            .failed
                            .saturating_add(value.parse::<u64>().unwrap_or(0));
                    } else if let Some(value) = field.strip_suffix(" ignored") {
                        observation.ignored = observation
                            .ignored
                            .saturating_add(value.parse::<u64>().unwrap_or(0));
                    }
                }
            }
        }
        let trimmed = line.trim_start();
        for marker in ["PASS ", "FAIL "] {
            if let Some(rest) = trimmed.strip_prefix(marker)
                && let Some((_, name)) = rest.split_once("] ")
            {
                let name = name.trim();
                if !name.is_empty() {
                    observation.executed.insert(name.to_owned());
                }
            }
        }
        for needle in [" tests run: ", " test run: ", "test run: "] {
            if let Some((_, summary)) = line.split_once(needle) {
                observation.summary_seen = true;
                for field in summary.split(',') {
                    let tokens = field.split_whitespace().collect::<Vec<_>>();
                    if let (Some(value), Some(kind)) = (
                        tokens.first().and_then(|token| token.parse::<u64>().ok()),
                        tokens.get(1),
                    ) {
                        match *kind {
                            "passed" => {
                                observation.passed = observation.passed.saturating_add(value)
                            }
                            "failed" => {
                                observation.failed = observation.failed.saturating_add(value)
                            }
                            "ignored" | "skipped" => {
                                observation.ignored = observation.ignored.saturating_add(value);
                            }
                            _ => {}
                        }
                    }
                }
                break;
            }
        }
    }
    observation
}

/// Exact test-name evidence: libtest reports module paths (`tests::name`), so
/// the requested bare name must match a whole final segment; a plain substring
/// collision (for example `name_extra`) never matches.
fn test_name_matches(observed: &str, requested: &str) -> bool {
    observed == requested
        || observed
            .strip_suffix(requested)
            .is_some_and(|prefix| prefix.ends_with("::"))
}

fn classify_test_evidence(
    item: &TestPlanItem,
    evidence: &GateEvidence,
    observation: &TestObservation,
) -> (String, String, bool) {
    let exact_seen = item.exact_test.as_ref().is_none_or(|name| {
        observation
            .executed
            .iter()
            .any(|observed| test_name_matches(observed, name))
    });
    let exact_ignored = item.exact_test.as_ref().is_some_and(|name| {
        observation
            .ignored_names
            .iter()
            .any(|observed| test_name_matches(observed, name))
    });
    match evidence.status {
        GateStatus::Fail => {
            let compile = evidence_is_compile_failure(evidence);
            let reason = evidence
                .message
                .clone()
                .unwrap_or_else(|| evidence.status.as_str().to_owned());
            let status = if compile { "COMPILE_FAIL" } else { "FAIL" };
            (status.to_owned(), reason, exact_seen)
        }
        GateStatus::FastPass | GateStatus::FullPass => {
            if exact_ignored {
                return (
                    "IGNORED_ONLY".to_owned(),
                    "the requested test is marked #[ignore]; ignored-only results are never a pass"
                        .to_owned(),
                    false,
                );
            }
            if !observation.summary_seen {
                return (
                    "INCONCLUSIVE".to_owned(),
                    "no libtest or nextest summary was observed; a custom harness or missing \
                     result is never a pass"
                        .to_owned(),
                    exact_seen,
                );
            }
            if observation.passed == 0 && observation.failed == 0 {
                if observation.ignored > 0 {
                    return (
                        "IGNORED_ONLY".to_owned(),
                        "only ignored tests matched the filter; ignored-only results are never a pass"
                            .to_owned(),
                        false,
                    );
                }
                return (
                    "ZERO_MATCH".to_owned(),
                    "the filter matched no executed test; a zero-match run is never a pass"
                        .to_owned(),
                    false,
                );
            }
            if item.exact_test.is_some() && !exact_seen {
                return (
                    "INCONCLUSIVE".to_owned(),
                    "the filter executed tests but not the exact requested test name; a substring \
                     collision is not evidence"
                        .to_owned(),
                    false,
                );
            }
            (
                "PASS".to_owned(),
                format!(
                    "{} passed, {} failed, {} ignored",
                    observation.passed, observation.failed, observation.ignored
                ),
                exact_seen,
            )
        }
        GateStatus::Timeout => (
            "TIMEOUT".to_owned(),
            "the test run timed out and produced no complete evidence".to_owned(),
            exact_seen,
        ),
        GateStatus::Cancelled => (
            "CANCELLED".to_owned(),
            "the test run was cancelled".to_owned(),
            exact_seen,
        ),
        GateStatus::Unavailable => (
            "RUNNER_UNAVAILABLE".to_owned(),
            evidence
                .message
                .clone()
                .unwrap_or_else(|| "the test runner was unavailable".to_owned()),
            exact_seen,
        ),
        _ => {
            // The bounded Cargo gate already maps filtered zero-execution runs
            // to INCONCLUSIVE. Re-surface the more specific reason without ever
            // granting a pass.
            if exact_ignored
                || (observation.passed == 0
                    && observation.failed == 0
                    && observation.ignored > 0
                    && observation.summary_seen)
            {
                (
                    "IGNORED_ONLY".to_owned(),
                    "only ignored tests matched the filter; ignored-only results are never a pass"
                        .to_owned(),
                    false,
                )
            } else if observation.summary_seen && observation.passed == 0 && observation.failed == 0
            {
                (
                    "ZERO_MATCH".to_owned(),
                    "the filter matched no executed test; a zero-match run is never a pass"
                        .to_owned(),
                    false,
                )
            } else {
                (
                    "INCONCLUSIVE".to_owned(),
                    evidence.message.clone().unwrap_or_else(|| {
                        "the run did not produce authoritative test evidence".to_owned()
                    }),
                    exact_seen,
                )
            }
        }
    }
}

fn candidate_repeat(
    run: u64,
    contract: &BehaviorContract,
    evidence: &GateEvidence,
) -> TestRepeatData {
    let text = evidence_text(evidence);
    let observation = observe_test_output(&text);
    let exact_test_seen = observation
        .executed
        .iter()
        .any(|observed| test_name_matches(observed, &contract.test_name));
    let expected_failure_seen = text.contains(&contract.expected_failure);
    let compile = evidence_is_compile_failure(evidence);
    let status = match evidence.status {
        GateStatus::Fail if compile => "COMPILE_FAIL",
        GateStatus::Fail => "FAIL",
        GateStatus::FastPass | GateStatus::FullPass => {
            if observation.passed == 0 && observation.failed == 0 {
                if observation.ignored > 0 {
                    "IGNORED_ONLY"
                } else if observation.summary_seen {
                    "ZERO_MATCH"
                } else {
                    "INCONCLUSIVE"
                }
            } else if !exact_test_seen {
                "INCONCLUSIVE"
            } else {
                "PASS"
            }
        }
        GateStatus::Timeout => "TIMEOUT",
        GateStatus::Cancelled => "CANCELLED",
        GateStatus::Unavailable => "RUNNER_UNAVAILABLE",
        _ => "INCONCLUSIVE",
    };
    TestRepeatData {
        run,
        status: status.to_owned(),
        gate_status: evidence.status.as_str().to_owned(),
        exact_test_seen,
        expected_failure_seen,
        passed: observation.passed,
        failed: observation.failed,
        ignored: observation.ignored,
        tests_executed: evidence
            .steps
            .iter()
            .filter_map(|step| step.evidence.tests_executed)
            .max()
            .unwrap_or(0),
        duration_ms: evidence.response_ms,
        input_hash: Some(evidence.input_hash.clone()),
        command_hash: Some(evidence.command_hash.clone()),
        environment_hash: Some(evidence.environment_hash.clone()),
        reason: evidence
            .message
            .clone()
            .unwrap_or_else(|| evidence.status.as_str().to_owned()),
    }
}

fn failed_repeat(run: u64, status: &str, reason: String) -> TestRepeatData {
    TestRepeatData {
        run,
        status: status.to_owned(),
        gate_status: status.to_owned(),
        exact_test_seen: false,
        expected_failure_seen: false,
        passed: 0,
        failed: 0,
        ignored: 0,
        tests_executed: 0,
        duration_ms: 0,
        input_hash: None,
        command_hash: None,
        environment_hash: None,
        reason,
    }
}

fn side_data(repeats: Vec<TestRepeatData>) -> TestCandidateSideData {
    let pass_observed = repeats.iter().filter(|r| r.status == "PASS").count() as u64;
    let fail_observed = repeats
        .iter()
        .filter(|r| r.status == "FAIL" || r.status == "COMPILE_FAIL")
        .count() as u64;
    let other_observed = repeats.len() as u64 - pass_observed - fail_observed;
    let exact_test_seen = repeats.iter().any(|r| r.exact_test_seen);
    TestCandidateSideData {
        repeats,
        pass_observed,
        fail_observed,
        other_observed,
        exact_test_seen,
    }
}

fn evaluate_candidate(
    change_id: &str,
    record: &ChangeRecord,
    contract: &BehaviorContract,
    repeats: u64,
    probe_change_id: Option<String>,
    baseline: Vec<TestRepeatData>,
    candidate: Vec<TestRepeatData>,
) -> CandidateEvaluation {
    let baseline_pass_expected = baseline
        .iter()
        .filter(|r| r.status == "FAIL" && r.expected_failure_seen && r.exact_test_seen)
        .count() as u64;
    let baseline_other = repeats.saturating_sub(baseline_pass_expected);
    let candidate_pass = candidate.iter().filter(|r| r.status == "PASS").count() as u64;
    let candidate_other = repeats.saturating_sub(candidate_pass);
    let baseline_incompatible = baseline.iter().any(|r| r.status == "COMPILE_FAIL");
    let flaky = (baseline_pass_expected > 0 && baseline_other > 0)
        || (candidate_pass > 0 && candidate_other > 0);
    let contract_status = if baseline_incompatible {
        "BASELINE_INCOMPATIBLE"
    } else if flaky {
        "INCONCLUSIVE"
    } else if baseline_pass_expected == repeats && candidate_pass == repeats {
        "SATISFIED"
    } else {
        "VIOLATED"
    };
    let mut warnings = Vec::new();
    if flaky {
        warnings.push(format!(
            "flaky test: {} baseline and {} candidate repeat(s) passed across {repeats} \
             repeat(s); a retry after a failure does not erase the observed instability",
            candidate_pass, baseline_pass_expected
        ));
    }
    if baseline_incompatible {
        warnings.push(
            "the regression test does not compile against the baseline API; this is a separate \
             BASELINE_INCOMPATIBLE state, not evidence that the test catches the bug"
                .to_owned(),
        );
    }
    CandidateEvaluation {
        data: TestCandidateData {
            change_id: change_id.to_owned(),
            change_revision: record.revision,
            change_base_identity: record.base_identity.clone(),
            probe_change_id,
            reconstructed_candidate: true,
            test_name: contract.test_name.clone(),
            expected_failure: contract.expected_failure.clone(),
            repeats,
            baseline: side_data(baseline),
            candidate: side_data(candidate),
            contract_status: contract_status.to_owned(),
            compatible: !baseline_incompatible,
            flaky,
            detections: Vec::new(),
        },
        warnings,
    }
}

fn rejected_candidate_data(
    change_id: &str,
    record: &ChangeRecord,
    contract: &BehaviorContract,
    repeats: u64,
    detections: Vec<String>,
    probe_change_id: Option<String>,
) -> TestCandidateData {
    TestCandidateData {
        change_id: change_id.to_owned(),
        change_revision: record.revision,
        change_base_identity: record.base_identity.clone(),
        probe_change_id,
        reconstructed_candidate: true,
        test_name: contract.test_name.clone(),
        expected_failure: contract.expected_failure.clone(),
        repeats,
        baseline: side_data(Vec::new()),
        candidate: side_data(Vec::new()),
        contract_status: "REJECTED".to_owned(),
        compatible: false,
        flaky: false,
        detections,
    }
}

fn candidate_reason(data: &TestCandidateData) -> String {
    match data.contract_status.as_str() {
        "SATISFIED" => format!(
            "the regression test `{}` failed with the expected assertion on the baseline and \
             passed on the candidate across {} repeat(s); the candidate snapshot was reconstructed \
             from the recorded change patches on a server-owned baseline capture",
            data.test_name, data.repeats
        ),
        "VIOLATED" => format!(
            "the regression test `{}` did not show the required baseline failure and candidate \
             pass; baseline {} pass / {} fail / {} other, candidate {} pass / {} fail / {} other",
            data.test_name,
            data.baseline.pass_observed,
            data.baseline.fail_observed,
            data.baseline.other_observed,
            data.candidate.pass_observed,
            data.candidate.fail_observed,
            data.candidate.other_observed
        ),
        "BASELINE_INCOMPATIBLE" => {
            "the regression test does not compile against the baseline API; \
                                    this is not counted as catching the regression"
                .to_owned()
        }
        "REJECTED" => format!(
            "the regression test patch was rejected before comparison: {}",
            data.detections.join("; ")
        ),
        "INCONCLUSIVE" => format!(
            "observed outcomes were mixed across {} repeat(s) (baseline {} pass / {} fail, \
             candidate {} pass / {} fail); flakiness is reported and never erased by a retry",
            data.repeats,
            data.baseline.pass_observed,
            data.baseline.fail_observed,
            data.candidate.pass_observed,
            data.candidate.fail_observed
        ),
        other => format!("test candidate comparison finished with {other}"),
    }
}

fn count_token(text: &str, needle: &str) -> u64 {
    text.match_indices(needle).count() as u64
}

/// Bounded static analysis of the supplied regression-test patch. Deleting a
/// test, adding `#[ignore]`, weakening assertions, and narrowing scope are
/// rejected before any snapshot runs.
fn analyze_test_patch(patch: &TestPatchInput, contract: &BehaviorContract) -> Vec<String> {
    let mut detections = Vec::new();
    for entry in &patch.patches {
        let old_tests = count_token(&entry.old_string, "#[test]");
        let new_tests = count_token(&entry.new_string, "#[test]");
        if old_tests > 0 && new_tests < old_tests {
            detections.push(format!(
                "test-deletion: patch {} removes {} #[test] function(s)",
                entry.file,
                old_tests - new_tests
            ));
        }
        let old_ignored = count_token(&entry.old_string, "#[ignore");
        let new_ignored = count_token(&entry.new_string, "#[ignore");
        if new_ignored > old_ignored {
            detections.push(format!(
                "ignore-added: patch {} adds #[ignore] in place of execution",
                entry.file
            ));
        }
        let old_asserts = count_token(&entry.old_string, "assert");
        let new_asserts = count_token(&entry.new_string, "assert");
        if old_asserts > new_asserts {
            detections.push(format!(
                "assertion-weakened: patch {} removes {} assertion reference(s)",
                entry.file,
                old_asserts - new_asserts
            ));
        }
        let old_cfg = count_token(&entry.old_string, "#[cfg(");
        let new_cfg = count_token(&entry.new_string, "#[cfg(");
        if new_cfg > old_cfg {
            detections.push(format!(
                "scope-narrowed: patch {} adds a cfg gate that can skip execution",
                entry.file
            ));
        }
    }
    if !detections.is_empty() {
        detections.push(format!(
            "contract test `{}` would not prove the behavior contract",
            contract.test_name
        ));
    }
    detections.truncate(TEST_DETECTION_LIMIT);
    detections
}

fn declares_test_name(text: &str, name: &str) -> bool {
    for line in text.lines() {
        let Some(rest) = line.split("fn ").nth(1) else {
            continue;
        };
        let identifier = rest
            .trim_start()
            .split(|character: char| !(character.is_alphanumeric() || character == '_'))
            .next()
            .unwrap_or_default();
        if identifier == name {
            return true;
        }
    }
    false
}

fn patch_provides_test(patch: &TestPatchInput, contract: &BehaviorContract) -> bool {
    patch.patches.iter().any(|entry| {
        declares_test_name(&entry.new_string, &contract.test_name)
            || declares_test_name(&entry.old_string, &contract.test_name)
    }) || patch
        .new_files
        .iter()
        .any(|file| declares_test_name(&file.content, &contract.test_name))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TestMarkerSummary {
    test_functions: u64,
    ignored: u64,
    assertions: u64,
    files: BTreeSet<String>,
    truncated: bool,
}

fn scan_test_markers(root: &Path) -> TestMarkerSummary {
    let mut summary = TestMarkerSummary::default();
    let mut files = 0_usize;
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        if files >= TEST_SCAN_MAX_FILES {
            summary.truncated = true;
            break;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name();
            if name == "target" || name == ".git" {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                continue;
            }
            if files >= TEST_SCAN_MAX_FILES {
                summary.truncated = true;
                break;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > TEST_SCAN_MAX_FILE_BYTES {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            files += 1;
            let tests = count_token(&text, "#[test]");
            if tests > 0 {
                summary.test_functions = summary.test_functions.saturating_add(tests);
                if let Ok(relative) = path.strip_prefix(root) {
                    summary.files.insert(relative.display().to_string());
                }
            }
            summary.ignored = summary
                .ignored
                .saturating_add(count_token(&text, "#[ignore"));
            summary.assertions = summary
                .assertions
                .saturating_add(count_token(&text, "assert!"))
                .saturating_add(count_token(&text, "assert_eq!"))
                .saturating_add(count_token(&text, "assert_ne!"));
        }
    }
    summary
}

fn compare_test_markers(before: &TestMarkerSummary, after: &TestMarkerSummary) -> Vec<String> {
    let mut detections = Vec::new();
    if after.test_functions < before.test_functions {
        detections.push(format!(
            "candidate-deletes-tests: {} test function(s) disappeared while applying the candidate",
            before.test_functions - after.test_functions
        ));
    }
    if after.ignored > before.ignored {
        detections.push(format!(
            "candidate-adds-ignore: {} #[ignore] attribute(s) were added by the candidate",
            after.ignored - before.ignored
        ));
    }
    if after.assertions < before.assertions {
        detections.push(format!(
            "candidate-weakens-assertions: {} assertion(s) disappeared while applying the candidate",
            before.assertions - after.assertions
        ));
    }
    for file in before.files.difference(&after.files) {
        detections.push(format!("candidate-deletes-test-file: {file}"));
    }
    detections.truncate(TEST_DETECTION_LIMIT);
    detections
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
            max_tests: 4,
            repeats: 2,
        };
        let budget = VerifyBudget {
            max_cells: Some(16),
            max_wall_ms: Some(600_000),
            ..VerifyBudget::default()
        };
        assert_eq!(effective_max_cells(&budget, &config), 4);
        assert_eq!(effective_max_wall_ms(&budget, &config), 10_000);
        let narrow = VerifyBudget {
            max_cells: Some(2),
            max_wall_ms: Some(3_000),
            ..VerifyBudget::default()
        };
        assert_eq!(effective_max_cells(&narrow, &config), 2);
        assert_eq!(effective_max_wall_ms(&narrow, &config), 3_000);
        assert_eq!(effective_max_tests(&budget, &config), 4);
        assert_eq!(effective_repeats(&budget, &config), 2);
    }

    #[test]
    fn observe_test_output_reads_libtest_and_nextest_summaries() {
        let libtest = "running 2 tests\ntest tests::a ... ok\ntest tests::b ... FAILED\n\
                       test tests::c ... ignored\n\ntest result: FAILED. 1 passed; 1 failed; 1 \
                       ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let observation = observe_test_output(libtest);
        assert!(observation.summary_seen);
        assert_eq!(observation.passed, 1);
        assert_eq!(observation.failed, 1);
        assert_eq!(observation.ignored, 1);
        assert!(observation.executed.contains("tests::a"));
        assert!(observation.executed.contains("tests::b"));
        assert!(observation.ignored_names.contains("tests::c"));

        let nextest = "        PASS [   0.001s] pkg::a\n        FAIL [   0.002s] pkg::b\n\
                       Summary [   0.003s] 2 tests run: 1 passed, 1 failed, 0 skipped\n";
        let observation = observe_test_output(nextest);
        assert!(observation.summary_seen);
        assert_eq!(observation.passed, 1);
        assert_eq!(observation.failed, 1);
        assert!(observation.executed.contains("pkg::a"));
        assert!(observation.executed.contains("pkg::b"));

        let zero = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 \
                    measured; 0 filtered out\n";
        let observation = observe_test_output(zero);
        assert!(observation.summary_seen);
        assert_eq!(
            observation.passed + observation.failed + observation.ignored,
            0
        );
    }

    fn mapping_item(exact: Option<&str>) -> TestPlanItem {
        TestPlanItem {
            id: "item".to_owned(),
            rank: 0,
            scope: "unit".to_owned(),
            package: "fixture".to_owned(),
            package_id: "fixture 0.1.0".to_owned(),
            target: "fixture".to_owned(),
            target_kind: "lib".to_owned(),
            filter: None,
            exact_test: exact.map(str::to_owned),
            features: Vec::new(),
            no_default_features: false,
            feature_gated: false,
            runner: VerifyRunner::Cargo,
            reason: "test".to_owned(),
        }
    }

    fn test_step() -> crate::gate::GateStepResult {
        crate::gate::GateStepResult {
            evidence: crate::diagnostics::EvidenceStats::default(),
            diagnostics_omitted: 0,
            contexts: Vec::new(),
            target: GateTargetId::Test,
            command: "cargo test".to_owned(),
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
        }
    }

    fn evidence_with(
        status: GateStatus,
        stdout: &str,
        build_success: Option<bool>,
    ) -> GateEvidence {
        let mut evidence =
            GateEvidence::pending("job", &GateRequest::new("/tmp", GateTargetId::Test));
        evidence.status = status;
        evidence.input_hash = "input".to_owned();
        evidence.command_hash = "command".to_owned();
        evidence.environment_hash = "environment".to_owned();
        let mut step = test_step();
        step.evidence.build_success = build_success;
        step.evidence.tests_executed = Some(1);
        step.stdout = stdout.to_owned();
        evidence.steps.push(step);
        evidence
    }

    #[test]
    fn ignored_only_and_zero_match_are_never_pass() {
        let item = mapping_item(Some("tests::ignored"));
        let stdout = "test tests::ignored ... ignored\n\ntest result: ok. 0 passed; 0 failed; 1 \
                      ignored; 0 measured; 0 filtered out\n";
        let evidence = evidence_with(GateStatus::FastPass, stdout, Some(true));
        let observation = observe_test_output(&evidence_text(&evidence));
        let (status, reason, seen) = classify_test_evidence(&item, &evidence, &observation);
        assert_eq!(status, "IGNORED_ONLY", "{reason}");
        assert!(!seen);
        assert_ne!(status, "PASS");

        let item = mapping_item(Some("tests::missing"));
        let stdout = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 \
                      measured; 0 filtered out\n";
        let evidence = evidence_with(GateStatus::FastPass, stdout, Some(true));
        let observation = observe_test_output(&evidence_text(&evidence));
        let (status, reason, _) = classify_test_evidence(&item, &evidence, &observation);
        assert_eq!(status, "ZERO_MATCH", "{reason}");

        // A custom harness that exits zero without a summary is inconclusive.
        let evidence = evidence_with(GateStatus::FastPass, "custom harness output\n", None);
        let observation = observe_test_output(&evidence_text(&evidence));
        let (status, reason, _) = classify_test_evidence(&item, &evidence, &observation);
        assert_eq!(status, "INCONCLUSIVE", "{reason}");
    }

    #[test]
    fn substring_collision_is_not_exact_test_evidence() {
        let item = mapping_item(Some("tests::target"));
        let stdout = "test tests::target_extra ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 \
                      ignored; 0 measured; 0 filtered out\n";
        let evidence = evidence_with(GateStatus::FastPass, stdout, Some(true));
        let observation = observe_test_output(&evidence_text(&evidence));
        let (status, reason, seen) = classify_test_evidence(&item, &evidence, &observation);
        assert_eq!(status, "INCONCLUSIVE", "{reason}");
        assert!(!seen);

        let stdout = "test tests::target ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 \
                      ignored; 0 measured; 0 filtered out\n";
        let evidence = evidence_with(GateStatus::FastPass, stdout, Some(true));
        let observation = observe_test_output(&evidence_text(&evidence));
        let (status, _, seen) = classify_test_evidence(&item, &evidence, &observation);
        assert_eq!(status, "PASS");
        assert!(seen);
    }

    fn patch(file: &str, old: &str, new: &str) -> PatchInput {
        PatchInput {
            file: file.to_owned(),
            old_string: old.to_owned(),
            new_string: new.to_owned(),
        }
    }

    #[test]
    fn test_patch_analysis_catches_deletion_weakening_ignore_and_scope_narrowing() {
        let contract = BehaviorContract {
            test_name: "regression_catches_bug".to_owned(),
            package: None,
            target: None,
            expected_failure: "assertion `left == right` failed".to_owned(),
        };
        let clean = TestPatchInput {
            patches: vec![patch(
                "tests/regression.rs",
                "",
                "#[test]\nfn regression_catches_bug() { assert_eq!(2 + 2, 4); }\n",
            )],
            new_files: Vec::new(),
        };
        assert!(analyze_test_patch(&clean, &contract).is_empty());
        assert!(patch_provides_test(&clean, &contract));

        let deletion = TestPatchInput {
            patches: vec![patch(
                "tests/old.rs",
                "#[test]\nfn old_regression() { assert!(true); }\n",
                "",
            )],
            new_files: Vec::new(),
        };
        let detections = analyze_test_patch(&deletion, &contract);
        assert!(
            detections
                .iter()
                .any(|row| row.starts_with("test-deletion")),
            "{detections:#?}"
        );
        assert!(
            detections
                .iter()
                .any(|row| row.contains("would not prove the behavior contract")),
            "{detections:#?}"
        );

        let weakening = TestPatchInput {
            patches: vec![patch(
                "tests/regression.rs",
                "#[test]\nfn regression_catches_bug() { assert_eq!(value(), 4); }\n",
                "#[test]\nfn regression_catches_bug() { }\n",
            )],
            new_files: Vec::new(),
        };
        let detections = analyze_test_patch(&weakening, &contract);
        assert!(
            detections
                .iter()
                .any(|row| row.starts_with("assertion-weakened")),
            "{detections:#?}"
        );

        let ignored = TestPatchInput {
            patches: vec![patch(
                "tests/regression.rs",
                "#[test]\nfn regression_catches_bug() { assert_eq!(value(), 4); }\n",
                "#[test]\n#[ignore]\nfn regression_catches_bug() { assert_eq!(value(), 4); }\n",
            )],
            new_files: Vec::new(),
        };
        let detections = analyze_test_patch(&ignored, &contract);
        assert!(
            detections.iter().any(|row| row.starts_with("ignore-added")),
            "{detections:#?}"
        );

        let narrowed = TestPatchInput {
            patches: vec![patch(
                "tests/regression.rs",
                "#[test]\nfn regression_catches_bug() { assert_eq!(value(), 4); }\n",
                "#[cfg(feature = \"never\")]\n#[test]\nfn regression_catches_bug() { \
                 assert_eq!(value(), 4); }\n",
            )],
            new_files: Vec::new(),
        };
        let detections = analyze_test_patch(&narrowed, &contract);
        assert!(
            detections
                .iter()
                .any(|row| row.starts_with("scope-narrowed")),
            "{detections:#?}"
        );
        assert!(patch_provides_test(&narrowed, &contract));
    }

    #[test]
    fn marker_comparison_detects_candidate_test_regressions() {
        let before = TestMarkerSummary {
            test_functions: 3,
            ignored: 0,
            assertions: 5,
            files: ["tests/old.rs".to_owned()].into_iter().collect(),
            truncated: false,
        };
        let after = TestMarkerSummary {
            test_functions: 2,
            ignored: 1,
            assertions: 3,
            files: BTreeSet::new(),
            truncated: false,
        };
        let detections = compare_test_markers(&before, &after);
        assert!(
            detections
                .iter()
                .any(|row| row.starts_with("candidate-deletes-tests"))
        );
        assert!(
            detections
                .iter()
                .any(|row| row.starts_with("candidate-adds-ignore"))
        );
        assert!(
            detections
                .iter()
                .any(|row| row.starts_with("candidate-weakens-assertions"))
        );
        assert!(
            detections
                .iter()
                .any(|row| row.starts_with("candidate-deletes-test-file"))
        );
        assert!(compare_test_markers(&before, &before).is_empty());
    }
}
