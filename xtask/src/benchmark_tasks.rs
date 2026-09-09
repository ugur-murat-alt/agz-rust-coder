#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::{Component, Path},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::evidence;

const FIXTURES_PATH: &str = "benchmark/task-corpus/fixtures.json";
const MANIFEST_PATH: &str = "benchmark/task-corpus/manifest.json";
const ORACLE_PATH: &str = "benchmark/task-corpus/oracle.json";
const REPLAY_PATH: &str = "benchmark/task-corpus/provider-free-replay.json";
const ARM_LABELS: [&str; 3] = ["shell_files", "mcp_0_2_0", "change_engine"];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum BenchmarkArm {
    #[serde(rename = "shell_files")]
    ShellFiles,
    #[serde(rename = "mcp_0_2_0")]
    Mcp020,
    #[serde(rename = "change_engine")]
    ChangeEngine,
}

impl BenchmarkArm {
    fn all() -> [Self; 3] {
        [Self::ShellFiles, Self::Mcp020, Self::ChangeEngine]
    }

    fn label(self) -> &'static str {
        match self {
            Self::ShellFiles => "shell_files",
            Self::Mcp020 => "mcp_0_2_0",
            Self::ChangeEngine => "change_engine",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TrialBudget {
    pub wall_time_ms: u64,
    pub max_host_turns: u32,
    pub max_cargo_calls: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskTrialRequest {
    pub task_id: String,
    pub class: String,
    pub task_summary: String,
    pub repetition: u32,
    pub arm: BenchmarkArm,
    pub budget: TrialBudget,
}

#[derive(Clone, Debug)]
pub struct TaskWorkspaceSnapshot {
    pub fixture_hash: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TokenMetrics {
    pub input: Value,
    pub output: Value,
    pub cache: Value,
    pub schema: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AdapterObservation {
    pub task_id: String,
    pub repetition: u32,
    pub arm: BenchmarkArm,
    pub status: String,
    pub claimed_pass: bool,
    pub wall_time_ms: u32,
    pub cpu_time_ms: u32,
    pub snapshot_prep_ms: u32,
    pub cargo_calls: u32,
    pub recompile_count: u32,
    pub host_turns: u32,
    pub cache_state: String,
    pub tokens: TokenMetrics,
    pub cost_usd: Value,
    pub reported_validation: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OracleObservation {
    pub task_id: String,
    pub repetition: u32,
    pub arm: BenchmarkArm,
    pub status: String,
    pub hidden_tests_passed: bool,
    pub mutation_guard_intact: bool,
    pub checks_passed: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AdapterTrialOutput {
    pub observation: AdapterObservation,
    pub candidate: TaskWorkspaceSnapshot,
}

/// A/B/C executors receive public task data and a frozen workspace snapshot.
/// Oracle rules, hidden tests, expected solutions and success decisions are
/// deliberately absent from this contract.
pub trait TaskBenchmarkAdapter: Send + Sync {
    fn arm(&self) -> BenchmarkArm;
    fn run_trial(
        &self,
        request: &TaskTrialRequest,
        workspace: &TaskWorkspaceSnapshot,
    ) -> Result<AdapterTrialOutput>;
}

/// Success is owned by an independent oracle. Live implementations can inject
/// hidden checks around the returned candidate without exposing them to agents.
pub trait TaskBenchmarkOracle: Send + Sync {
    fn evaluate(
        &self,
        request: &TaskTrialRequest,
        candidate: &TaskWorkspaceSnapshot,
        adapter: &AdapterObservation,
    ) -> Result<OracleObservation>;
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskTrialEvidence {
    pub request: TaskTrialRequest,
    pub order_index: u32,
    pub adapter: AdapterObservation,
    pub oracle: OracleObservation,
    pub success: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ArmSummary {
    pub arm: BenchmarkArm,
    pub total: u32,
    pub successes: u32,
    pub failures: u32,
    pub timeouts: u32,
    pub cancelled: u32,
    pub success_rate_percent: f64,
    pub success_rate_wilson_95_low_percent: f64,
    pub success_rate_wilson_95_high_percent: f64,
    pub average_wall_time_ms: f64,
    pub wall_time_stddev_ms: f64,
    pub average_cpu_time_ms: f64,
    pub average_snapshot_prep_ms: f64,
    pub average_cargo_calls: f64,
    pub average_recompile_count: f64,
    pub average_host_turns: f64,
    pub cold_trials: u32,
    pub warm_trials: u32,
    pub unknown_input_tokens: u32,
    pub unknown_output_tokens: u32,
    pub unknown_cache_tokens: u32,
    pub unknown_schema_tokens: u32,
    pub unknown_cost: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct GateEvidence {
    pub quality_pass: bool,
    pub host_turn_pass: bool,
    pub cargo_call_pass: bool,
    pub wall_time_pass: bool,
    pub non_inferiority_margin_percentage_points: f64,
    pub host_turn_reduction_target_percent: f64,
    pub cargo_call_reduction_target_percent: f64,
    pub wall_time_regression_limit_percent: f64,
    pub default_on_eligible: bool,
    pub default_on_reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskBenchmarkEvidence {
    pub schema_version: u32,
    pub corpus_id: String,
    pub status: String,
    pub passed: bool,
    pub mode: String,
    pub fixture_set_hash: String,
    pub replay_sha256: String,
    pub repetitions: u32,
    pub order_seeds: Vec<u64>,
    pub order_balanced: bool,
    pub total_trials: u32,
    pub retained_failed_trials: u32,
    pub retained_timeout_trials: u32,
    pub retained_cancelled_trials: u32,
    pub summaries: Vec<ArmSummary>,
    pub gate: GateEvidence,
    pub trials: Vec<TaskTrialEvidence>,
}

#[derive(Clone, Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    corpus_id: String,
    frozen: bool,
    frozen_before_change_engine: bool,
    frozen_source_commit: String,
    baseline_mcp_release: String,
    harness_version: String,
    repetitions: u32,
    arms: Vec<String>,
    budget: TrialBudget,
    quality_gate: QualityGate,
    performance_targets: PerformanceTargets,
    ablations: Vec<String>,
    fixture_set_hash: String,
    tasks: Vec<TaskSpec>,
    order_seeds: Vec<u64>,
    fixture_catalog_sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
struct QualityGate {
    baseline_arm: String,
    candidate_arm: String,
    non_inferiority_margin_percentage_points: f64,
}

#[derive(Clone, Debug, Deserialize)]
struct PerformanceTargets {
    host_turn_reduction_percent: f64,
    cargo_call_reduction_percent: f64,
    wall_time_regression_limit_percent: f64,
}

#[derive(Clone, Debug, Deserialize)]
struct TaskSpec {
    id: String,
    class: String,
    task_summary: String,
    fixture_hash: String,
    oracle_id: String,
    fixture_id: String,
}

#[derive(Clone, Debug, Deserialize)]
struct FixtureCatalog {
    schema_version: u32,
    corpus_id: String,
    fixture_set_hash: String,
    fixtures: Vec<FrozenFixture>,
}

#[derive(Clone, Debug, Deserialize)]
struct FrozenFixture {
    id: String,
    files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
struct OracleCatalog {
    schema_version: u32,
    corpus_id: String,
    fixture_set_hash: String,
    forbidden_success_shortcuts: Vec<String>,
    oracles: Vec<OracleSpec>,
}

#[derive(Clone, Debug, Deserialize)]
struct OracleSpec {
    id: String,
    task_id: String,
    required_checks: Vec<String>,
    hidden_test_ids: Vec<String>,
    requires_mutation_guard: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Replay {
    schema_version: u32,
    corpus_id: String,
    recording_kind: String,
    provenance: ReplayProvenance,
    usage: ReplayUsage,
    session_columns: Vec<String>,
    sessions: Vec<ReplaySession>,
    adapter_columns: Vec<String>,
    adapter_rows: Vec<AdapterRow>,
    oracle_columns: Vec<String>,
    oracle_rows: Vec<OracleRow>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplayProvenance {
    provider: String,
    model: String,
    harness_version: String,
    mcp_sha: String,
    manifest_sha256: String,
    oracle_sha256: String,
    fixture_set_hash: String,
    toolchain: String,
    os: String,
    hardware: String,
    cache_state: String,
    settings_hash: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplayUsage {
    input_tokens: Value,
    output_tokens: Value,
    cache_tokens: Value,
    schema_tokens: Value,
    cost_usd: Value,
}

type ReplaySession = (String, u32, u64, String, Vec<BenchmarkArm>);
type AdapterRow = (
    String,
    u32,
    BenchmarkArm,
    String,
    bool,
    u32,
    u32,
    u32,
    u32,
    u32,
    u32,
    String,
    Vec<String>,
);
type OracleRow = (String, u32, BenchmarkArm, String, bool, bool, Vec<String>);
type TrialKey = (String, u32, BenchmarkArm);

struct ReplayAdapter<'a> {
    arm: BenchmarkArm,
    rows: &'a BTreeMap<TrialKey, AdapterObservation>,
}

impl TaskBenchmarkAdapter for ReplayAdapter<'_> {
    fn arm(&self) -> BenchmarkArm {
        self.arm
    }

    fn run_trial(
        &self,
        request: &TaskTrialRequest,
        workspace: &TaskWorkspaceSnapshot,
    ) -> Result<AdapterTrialOutput> {
        let key = (request.task_id.clone(), request.repetition, request.arm);
        let observation = self
            .rows
            .get(&key)
            .cloned()
            .with_context(|| format!("missing adapter replay row for {}", request.task_id))?;
        Ok(AdapterTrialOutput {
            observation,
            candidate: workspace.clone(),
        })
    }
}

struct ReplayOracle<'a> {
    rows: &'a BTreeMap<TrialKey, OracleObservation>,
}

impl TaskBenchmarkOracle for ReplayOracle<'_> {
    fn evaluate(
        &self,
        request: &TaskTrialRequest,
        _candidate: &TaskWorkspaceSnapshot,
        _adapter: &AdapterObservation,
    ) -> Result<OracleObservation> {
        self.rows
            .get(&(request.task_id.clone(), request.repetition, request.arm))
            .cloned()
            .with_context(|| format!("missing oracle replay row for {}", request.task_id))
    }
}

pub async fn run(root: &Path) -> Result<()> {
    let benchmark = evaluate_provider_free_replay(root).await?;
    let run = json!({
        "schema_version": 1,
        "run_id": format!("task-benchmark-{}", &benchmark.replay_sha256[..16]),
        "mode": "provider-free-replay",
        "corpus_id": benchmark.corpus_id.clone(),
        "source_commit": command_text(root, "git", &["rev-parse", "HEAD"] )?,
        "source_dirty": !command_text(root, "git", &["status", "--porcelain"] )?.is_empty(),
        "fixture_set_hash": benchmark.fixture_set_hash.clone(),
        "replay_sha256": benchmark.replay_sha256.clone(),
        "provider": null,
        "model": null,
        "toolchain": command_text(root, "rustc", &["--version", "--verbose"] )?,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        "cache_state": "paired-cold-warm-replay",
        "network": false,
        "paid": false
    });
    let results = serde_json::to_value(&benchmark).context("serialize task benchmark results")?;
    let report = report(&benchmark)?;
    let output = evidence::publish("task-benchmark-smoke", &run, &results, &report)?;
    if !benchmark.passed {
        bail!("task-benchmark-smoke failed; evidence published at {}", output.display());
    }
    println!("task-benchmark-smoke: PASS ({})", output.display());
    Ok(())
}

pub async fn evaluate_provider_free_replay(root: &Path) -> Result<TaskBenchmarkEvidence> {
    let root = fs::canonicalize(root).context("canonicalize benchmark root")?;
    let (fixtures, fixture_bytes) = read_json::<FixtureCatalog>(&root, FIXTURES_PATH)?;
    let (manifest, manifest_bytes) = read_json::<Manifest>(&root, MANIFEST_PATH)?;
    let (oracles, oracle_bytes) = read_json::<OracleCatalog>(&root, ORACLE_PATH)?;
    let (replay, replay_bytes) = read_json::<Replay>(&root, REPLAY_PATH)?;
    let manifest_hash = sha256(&manifest_bytes);
    let oracle_hash = sha256(&oracle_bytes);
    let replay_hash = sha256(&replay_bytes);

    let fixture_map = validate_fixture_contract(&manifest, &fixtures, &sha256(&fixture_bytes))?;
    let oracle_specs = validate_oracle_contract(&manifest, &oracles)?;
    validate_replay_contract(&manifest, &replay, &manifest_hash, &oracle_hash)?;

    let adapter_rows = adapter_map(&replay)?;
    let oracle_rows = oracle_map(&replay)?;
    let expected = expected_keys(&manifest);
    if adapter_rows.keys().cloned().collect::<BTreeSet<_>>() != expected
        || oracle_rows.keys().cloned().collect::<BTreeSet<_>>() != expected
    {
        bail!("replay rows do not exactly cover the frozen task matrix");
    }
    let order = balanced_order(&manifest, &replay.sessions)?;

    let shell = ReplayAdapter { arm: BenchmarkArm::ShellFiles, rows: &adapter_rows };
    let mcp = ReplayAdapter { arm: BenchmarkArm::Mcp020, rows: &adapter_rows };
    let change = ReplayAdapter { arm: BenchmarkArm::ChangeEngine, rows: &adapter_rows };
    let oracle = ReplayOracle { rows: &oracle_rows };

    let mut trials = Vec::with_capacity(expected.len());
    for task in &manifest.tasks {
        let fixture = fixture_map
            .get(&task.fixture_id)
            .with_context(|| format!("missing fixture {}", task.fixture_id))?;
        let workspace = TaskWorkspaceSnapshot {
            fixture_hash: task.fixture_hash.clone(),
            files: fixture.files.clone(),
        };
        for repetition in 1..=manifest.repetitions {
            for arm in BenchmarkArm::all() {
                let request = TaskTrialRequest {
                    task_id: task.id.clone(),
                    class: task.class.clone(),
                    task_summary: task.task_summary.clone(),
                    repetition,
                    arm,
                    budget: manifest.budget.clone(),
                };
                let adapter: &dyn TaskBenchmarkAdapter = match arm {
                    BenchmarkArm::ShellFiles => &shell,
                    BenchmarkArm::Mcp020 => &mcp,
                    BenchmarkArm::ChangeEngine => &change,
                };
                if adapter.arm() != arm {
                    bail!("A/B/C adapter identity mismatch");
                }
                let output = adapter.run_trial(&request, &workspace)?;
                let oracle_observation =
                    oracle.evaluate(&request, &output.candidate, &output.observation)?;
                let spec = oracle_specs.get(&task.id).context("validated oracle disappeared")?;
                let success = oracle_success(spec, &oracle_observation);
                let order_index = *order
                    .get(&(task.id.clone(), repetition, arm))
                    .context("balanced order index missing")?;
                trials.push(TaskTrialEvidence {
                    request,
                    order_index,
                    adapter: output.observation,
                    oracle: oracle_observation,
                    success,
                });
            }
        }
    }

    let summaries = BenchmarkArm::all()
        .into_iter()
        .map(|arm| summarize(arm, &trials))
        .collect::<Result<Vec<_>>>()?;
    let gate = gate(&manifest, &summaries)?;
    let failed = count_u32(trials.iter().filter(|trial| !trial.success).count())?;
    let timeouts = count_u32(
        trials.iter().filter(|trial| trial.adapter.status == "timeout" || trial.oracle.status == "timeout").count(),
    )?;
    let cancelled = count_u32(
        trials.iter().filter(|trial| trial.adapter.status == "cancelled" || trial.oracle.status == "cancelled").count(),
    )?;
    if failed == 0 || timeouts == 0 || cancelled == 0 {
        bail!("replay must retain failure, timeout and cancellation controls");
    }

    let passed = gate.quality_pass && gate.host_turn_pass && gate.cargo_call_pass && gate.wall_time_pass;
    Ok(TaskBenchmarkEvidence {
        schema_version: 1,
        corpus_id: manifest.corpus_id,
        status: if passed { "PASS" } else { "FAIL" }.to_owned(),
        passed,
        mode: "provider-free-replay".to_owned(),
        fixture_set_hash: manifest.fixture_set_hash,
        replay_sha256: replay_hash,
        repetitions: manifest.repetitions,
        order_seeds: manifest.order_seeds,
        order_balanced: true,
        total_trials: count_u32(trials.len())?,
        retained_failed_trials: failed,
        retained_timeout_trials: timeouts,
        retained_cancelled_trials: cancelled,
        summaries,
        gate,
        trials,
    })
}

fn validate_fixture_contract<'a>(
    manifest: &Manifest,
    catalog: &'a FixtureCatalog,
    catalog_hash: &str,
) -> Result<BTreeMap<String, &'a FrozenFixture>> {
    if manifest.schema_version != 1
        || !manifest.frozen
        || !manifest.frozen_before_change_engine
        || manifest.corpus_id != "rust-agent-tasks-v1"
        || manifest.harness_version != "task-benchmark-v1"
        || manifest.baseline_mcp_release != "0.2.0"
        || manifest.fixture_catalog_sha256 != catalog_hash
        || catalog.schema_version != 1
        || catalog.corpus_id != manifest.corpus_id
        || catalog.fixture_set_hash != manifest.fixture_set_hash
    {
        bail!("frozen corpus identity or fixture binding drift");
    }
    if manifest.repetitions < 3 || manifest.repetitions % 3 != 0 {
        bail!("benchmark needs paired repetitions in multiples of three");
    }
    if manifest.order_seeds.iter().copied().collect::<BTreeSet<_>>().len() < 3 {
        bail!("benchmark needs at least three distinct order seeds");
    }
    if manifest.arms.iter().map(String::as_str).collect::<Vec<_>>() != ARM_LABELS.to_vec() {
        bail!("benchmark must contain exactly A/B/C arms");
    }
    if manifest.tasks.len() != 8 || manifest.ablations.len() < 3 {
        bail!("frozen v1 task/ablation coverage drift");
    }

    let mut map = BTreeMap::new();
    let mut hashes = Vec::new();
    for fixture in &catalog.fixtures {
        if fixture.files.is_empty() || map.insert(fixture.id.clone(), fixture).is_some() {
            bail!("invalid or duplicate frozen fixture {}", fixture.id);
        }
        for path in fixture.files.keys() {
            validate_relative_path(path)?;
        }
        hashes.push((fixture.id.clone(), fixture_hash(&fixture.files)));
    }
    for task in &manifest.tasks {
        let fixture = map
            .get(&task.fixture_id)
            .with_context(|| format!("missing frozen fixture {}", task.fixture_id))?;
        if fixture_hash(&fixture.files) != task.fixture_hash {
            bail!("fixture hash drift for {}", task.id);
        }
    }
    if map.len() != manifest.tasks.len() {
        bail!("fixture catalog contains entries outside the frozen task list");
    }
    hashes.sort();
    let mut hasher = Sha256::new();
    for (fixture_id, hash) in hashes {
        hasher.update(fixture_id.as_bytes());
        hasher.update([0]);
        hasher.update(hash.as_bytes());
        hasher.update([0]);
    }
    if format!("{:x}", hasher.finalize()) != manifest.fixture_set_hash {
        bail!("fixture set hash drift");
    }
    if manifest.quality_gate.baseline_arm != "mcp_0_2_0"
        || manifest.quality_gate.candidate_arm != "change_engine"
        || manifest.quality_gate.non_inferiority_margin_percentage_points < 0.0
        || !(0.0..=100.0).contains(&manifest.performance_targets.host_turn_reduction_percent)
        || !(0.0..=100.0).contains(&manifest.performance_targets.cargo_call_reduction_percent)
        || manifest.performance_targets.wall_time_regression_limit_percent < 0.0
    {
        bail!("predeclared benchmark gates are invalid");
    }
    Ok(map)
}

fn validate_oracle_contract<'a>(
    manifest: &Manifest,
    catalog: &'a OracleCatalog,
) -> Result<BTreeMap<String, &'a OracleSpec>> {
    if catalog.schema_version != 1
        || catalog.corpus_id != manifest.corpus_id
        || catalog.fixture_set_hash != manifest.fixture_set_hash
    {
        bail!("oracle catalog is not bound to the frozen corpus");
    }
    for shortcut in ["delete_tests", "weaken_assertions", "disable_required_lints", "skip_required_verification"] {
        if !catalog.forbidden_success_shortcuts.iter().any(|value| value == shortcut) {
            bail!("oracle shortcut guard missing: {shortcut}");
        }
    }
    let mut map = BTreeMap::new();
    for spec in &catalog.oracles {
        if spec.required_checks.is_empty()
            || spec.hidden_test_ids.is_empty()
            || !spec.requires_mutation_guard
            || map.insert(spec.task_id.clone(), spec).is_some()
        {
            bail!("invalid independent oracle {}", spec.id);
        }
    }
    for task in &manifest.tasks {
        if map.get(&task.id).map(|spec| spec.id.as_str()) != Some(task.oracle_id.as_str()) {
            bail!("oracle identity drift for {}", task.id);
        }
    }
    if map.len() != manifest.tasks.len() {
        bail!("oracle catalog contains entries outside the frozen tasks");
    }
    Ok(map)
}

fn validate_replay_contract(
    manifest: &Manifest,
    replay: &Replay,
    manifest_hash: &str,
    oracle_hash: &str,
) -> Result<()> {
    if replay.schema_version != 1
        || replay.corpus_id != manifest.corpus_id
        || replay.recording_kind != "provider_free_harness_replay"
        || replay.provenance.provider != "provider-free-replay"
        || replay.provenance.model != "deterministic-transcript"
        || replay.provenance.harness_version != manifest.harness_version
        || replay.provenance.mcp_sha != manifest.frozen_source_commit
        || replay.provenance.manifest_sha256 != manifest_hash
        || replay.provenance.oracle_sha256 != oracle_hash
        || replay.provenance.fixture_set_hash != manifest.fixture_set_hash
        || replay.provenance.settings_hash != manifest_hash
        || replay.provenance.toolchain.is_empty()
        || replay.provenance.os.is_empty()
        || replay.provenance.hardware.is_empty()
        || replay.provenance.cache_state.is_empty()
    {
        bail!("provider-free replay provenance drift");
    }
    let session_columns = ["task_id", "repetition", "order_seed", "cache_state", "arm_order"];
    let adapter_columns = [
        "task_id", "repetition", "arm", "status", "claimed_pass", "wall_time_ms",
        "cpu_time_ms", "snapshot_prep_ms", "cargo_calls", "recompile_count", "host_turns",
        "cache_state", "reported_validation",
    ];
    let oracle_columns = [
        "task_id", "repetition", "arm", "status", "hidden_tests_passed",
        "mutation_guard_intact", "checks_passed",
    ];
    if replay.session_columns.iter().map(String::as_str).collect::<Vec<_>>() != session_columns.to_vec()
        || replay.adapter_columns.iter().map(String::as_str).collect::<Vec<_>>() != adapter_columns.to_vec()
        || replay.oracle_columns.iter().map(String::as_str).collect::<Vec<_>>() != oracle_columns.to_vec()
    {
        bail!("provider-free replay column schema drift");
    }
    for (name, value) in [
        ("input_tokens", &replay.usage.input_tokens),
        ("output_tokens", &replay.usage.output_tokens),
        ("cache_tokens", &replay.usage.cache_tokens),
        ("schema_tokens", &replay.usage.schema_tokens),
        ("cost_usd", &replay.usage.cost_usd),
    ] {
        if value.as_str() != Some("unknown") {
            bail!("unavailable {name} must be recorded as unknown");
        }
    }
    Ok(())
}

fn adapter_map(replay: &Replay) -> Result<BTreeMap<TrialKey, AdapterObservation>> {
    let mut map = BTreeMap::new();
    for row in &replay.adapter_rows {
        let (task_id, repetition, arm, status, claimed_pass, wall, cpu, snapshot, cargo,
            recompile, turns, cache_state, validation) = row;
        if !matches!(status.as_str(), "completed" | "failed" | "timeout" | "cancelled")
            || !matches!(cache_state.as_str(), "cold" | "warm")
        {
            bail!("invalid adapter replay state");
        }
        let observation = AdapterObservation {
            task_id: task_id.clone(), repetition: *repetition, arm: *arm, status: status.clone(),
            claimed_pass: *claimed_pass, wall_time_ms: *wall, cpu_time_ms: *cpu,
            snapshot_prep_ms: *snapshot, cargo_calls: *cargo, recompile_count: *recompile,
            host_turns: *turns, cache_state: cache_state.clone(),
            tokens: TokenMetrics {
                input: replay.usage.input_tokens.clone(), output: replay.usage.output_tokens.clone(),
                cache: replay.usage.cache_tokens.clone(), schema: replay.usage.schema_tokens.clone(),
            },
            cost_usd: replay.usage.cost_usd.clone(), reported_validation: validation.clone(),
        };
        let key = (task_id.clone(), *repetition, *arm);
        if map.insert(key, observation).is_some() {
            bail!("duplicate adapter replay row");
        }
    }
    Ok(map)
}

fn oracle_map(replay: &Replay) -> Result<BTreeMap<TrialKey, OracleObservation>> {
    let mut map = BTreeMap::new();
    for row in &replay.oracle_rows {
        let (task_id, repetition, arm, status, hidden, mutation, checks) = row;
        if !matches!(status.as_str(), "pass" | "fail" | "timeout" | "cancelled" | "infrastructure_error") {
            bail!("invalid oracle replay state");
        }
        let observation = OracleObservation {
            task_id: task_id.clone(), repetition: *repetition, arm: *arm, status: status.clone(),
            hidden_tests_passed: *hidden, mutation_guard_intact: *mutation, checks_passed: checks.clone(),
        };
        if map.insert((task_id.clone(), *repetition, *arm), observation).is_some() {
            bail!("duplicate oracle replay row");
        }
    }
    Ok(map)
}

fn balanced_order(manifest: &Manifest, sessions: &[ReplaySession]) -> Result<BTreeMap<TrialKey, u32>> {
    let repetition_count = usize::try_from(manifest.repetitions).context("repetition count does not fit usize")?;
    if sessions.len() != manifest.tasks.len() * repetition_count {
        bail!("replay session count drift");
    }
    let task_ids = manifest.tasks.iter().map(|task| task.id.as_str()).collect::<BTreeSet<_>>();
    let expected_per_position = manifest.repetitions / 3;
    let mut seen = BTreeSet::new();
    let mut positions: BTreeMap<(String, BenchmarkArm, u32), u32> = BTreeMap::new();
    let mut order = BTreeMap::new();

    for (task_id, repetition, seed, cache_state, arms) in sessions {
        if !task_ids.contains(task_id.as_str())
            || *repetition == 0
            || *repetition > manifest.repetitions
            || !matches!(cache_state.as_str(), "cold" | "warm")
        {
            bail!("invalid replay session identity");
        }
        let index = usize::try_from(*repetition).context("repetition does not fit usize")? - 1;
        if *seed != manifest.order_seeds[index % manifest.order_seeds.len()]
            || !seen.insert((task_id.clone(), *repetition))
        {
            bail!("replay order seed or session identity drift");
        }
        if arms.len() != 3
            || arms.iter().copied().collect::<BTreeSet<_>>() != BenchmarkArm::all().into_iter().collect()
        {
            bail!("each replay session must contain one A/B/C permutation");
        }
        for (position, arm) in arms.iter().copied().enumerate() {
            let position = u32::try_from(position).context("order position overflow")?;
            *positions.entry((task_id.clone(), arm, position)).or_default() += 1;
            order.insert((task_id.clone(), *repetition, arm), position);
        }
    }
    for task in &manifest.tasks {
        for arm in BenchmarkArm::all() {
            for position in 0..3 {
                if positions.get(&(task.id.clone(), arm, position)).copied().unwrap_or_default()
                    != expected_per_position
                {
                    bail!("A/B/C replay is not position-balanced for {}", task.id);
                }
            }
        }
    }
    Ok(order)
}

fn expected_keys(manifest: &Manifest) -> BTreeSet<TrialKey> {
    let mut keys = BTreeSet::new();
    for task in &manifest.tasks {
        for repetition in 1..=manifest.repetitions {
            for arm in BenchmarkArm::all() {
                keys.insert((task.id.clone(), repetition, arm));
            }
        }
    }
    keys
}

fn oracle_success(spec: &OracleSpec, observation: &OracleObservation) -> bool {
    observation.status == "pass"
        && observation.hidden_tests_passed
        && (!spec.requires_mutation_guard || observation.mutation_guard_intact)
        && spec.required_checks.iter().all(|required| {
            observation.checks_passed.iter().any(|actual| actual == required)
        })
}

fn summarize(arm: BenchmarkArm, trials: &[TaskTrialEvidence]) -> Result<ArmSummary> {
    let rows = trials.iter().filter(|trial| trial.request.arm == arm).collect::<Vec<_>>();
    if rows.is_empty() {
        bail!("empty benchmark arm");
    }
    let total = count_u32(rows.len())?;
    let successes = count_u32(rows.iter().filter(|trial| trial.success).count())?;
    let average = |project: fn(&TaskTrialEvidence) -> f64| {
        rows.iter().map(|trial| project(trial)).sum::<f64>() / f64::from(total)
    };
    let average_wall = average(|trial| f64::from(trial.adapter.wall_time_ms));
    let variance = rows.iter().map(|trial| {
        let delta = f64::from(trial.adapter.wall_time_ms) - average_wall;
        delta * delta
    }).sum::<f64>() / f64::from(total);
    let (wilson_low, wilson_high) = wilson_95(successes, total);

    Ok(ArmSummary {
        arm,
        total,
        successes,
        failures: total - successes,
        timeouts: count_u32(rows.iter().filter(|trial| trial.adapter.status == "timeout" || trial.oracle.status == "timeout").count())?,
        cancelled: count_u32(rows.iter().filter(|trial| trial.adapter.status == "cancelled" || trial.oracle.status == "cancelled").count())?,
        success_rate_percent: f64::from(successes) * 100.0 / f64::from(total),
        success_rate_wilson_95_low_percent: wilson_low,
        success_rate_wilson_95_high_percent: wilson_high,
        average_wall_time_ms: average_wall,
        wall_time_stddev_ms: variance.sqrt(),
        average_cpu_time_ms: average(|trial| f64::from(trial.adapter.cpu_time_ms)),
        average_snapshot_prep_ms: average(|trial| f64::from(trial.adapter.snapshot_prep_ms)),
        average_cargo_calls: average(|trial| f64::from(trial.adapter.cargo_calls)),
        average_recompile_count: average(|trial| f64::from(trial.adapter.recompile_count)),
        average_host_turns: average(|trial| f64::from(trial.adapter.host_turns)),
        cold_trials: count_u32(rows.iter().filter(|trial| trial.adapter.cache_state == "cold").count())?,
        warm_trials: count_u32(rows.iter().filter(|trial| trial.adapter.cache_state == "warm").count())?,
        unknown_input_tokens: unknown_count(&rows, |trial| &trial.adapter.tokens.input)?,
        unknown_output_tokens: unknown_count(&rows, |trial| &trial.adapter.tokens.output)?,
        unknown_cache_tokens: unknown_count(&rows, |trial| &trial.adapter.tokens.cache)?,
        unknown_schema_tokens: unknown_count(&rows, |trial| &trial.adapter.tokens.schema)?,
        unknown_cost: unknown_count(&rows, |trial| &trial.adapter.cost_usd)?,
    })
}

fn unknown_count(
    rows: &[&TaskTrialEvidence],
    project: fn(&TaskTrialEvidence) -> &Value,
) -> Result<u32> {
    count_u32(rows.iter().filter(|trial| project(trial).as_str() == Some("unknown")).count())
}

fn wilson_95(successes: u32, total: u32) -> (f64, f64) {
    let n = f64::from(total);
    let p = f64::from(successes) / n;
    let z = 1.959_963_984_540_054_f64;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let half = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / denominator;
    ((center - half).max(0.0) * 100.0, (center + half).min(1.0) * 100.0)
}

fn gate(manifest: &Manifest, summaries: &[ArmSummary]) -> Result<GateEvidence> {
    let baseline = summaries.iter().find(|summary| summary.arm == BenchmarkArm::Mcp020).context("missing B summary")?;
    let candidate = summaries.iter().find(|summary| summary.arm == BenchmarkArm::ChangeEngine).context("missing C summary")?;
    let margin = manifest.quality_gate.non_inferiority_margin_percentage_points;
    let host_target = manifest.performance_targets.host_turn_reduction_percent;
    let cargo_target = manifest.performance_targets.cargo_call_reduction_percent;
    let wall_limit = manifest.performance_targets.wall_time_regression_limit_percent;

    Ok(GateEvidence {
        quality_pass: candidate.success_rate_percent + margin >= baseline.success_rate_percent,
        host_turn_pass: candidate.average_host_turns <= baseline.average_host_turns * (1.0 - host_target / 100.0),
        cargo_call_pass: candidate.average_cargo_calls <= baseline.average_cargo_calls * (1.0 - cargo_target / 100.0),
        wall_time_pass: candidate.average_wall_time_ms <= baseline.average_wall_time_ms * (1.0 + wall_limit / 100.0),
        non_inferiority_margin_percentage_points: margin,
        host_turn_reduction_target_percent: host_target,
        cargo_call_reduction_target_percent: cargo_target,
        wall_time_regression_limit_percent: wall_limit,
        default_on_eligible: false,
        default_on_reason: "provider-free transcript replay validates the harness only; comparable opt-in live runs are required".to_owned(),
    })
}

fn report(benchmark: &TaskBenchmarkEvidence) -> Result<String> {
    let mut report = String::new();
    writeln!(&mut report, "# Rust task benchmark provider-free replay")?;
    writeln!(&mut report)?;
    writeln!(&mut report, "{}: frozen A/B/C corpus replayed without provider, model, network or paid requests.", benchmark.status)?;
    writeln!(&mut report, "Replay timings and turn counts are fixtures for harness validation, not measured Change Engine performance.")?;
    writeln!(&mut report)?;
    writeln!(&mut report, "| arm | success | Wilson 95% | wall ms ± sd | cargo | host turns |")?;
    writeln!(&mut report, "| --- | ---: | ---: | ---: | ---: | ---: |")?;
    for summary in &benchmark.summaries {
        writeln!(&mut report, "| {} | {}/{} | {:.1}–{:.1}% | {:.1} ± {:.1} | {:.2} | {:.2} |",
            summary.arm.label(), summary.successes, summary.total,
            summary.success_rate_wilson_95_low_percent, summary.success_rate_wilson_95_high_percent,
            summary.average_wall_time_ms, summary.wall_time_stddev_ms,
            summary.average_cargo_calls, summary.average_host_turns)?;
    }
    writeln!(&mut report)?;
    writeln!(&mut report, "Retained failures={}, timeouts={}, cancellations={}; unavailable token and cost values remain `unknown`.",
        benchmark.retained_failed_trials, benchmark.retained_timeout_trials, benchmark.retained_cancelled_trials)?;
    writeln!(&mut report, "Gates: quality={}, host-turn={}, cargo-call={}, wall-time={}. Default-on=false.",
        benchmark.gate.quality_pass, benchmark.gate.host_turn_pass,
        benchmark.gate.cargo_call_pass, benchmark.gate.wall_time_pass)?;
    Ok(report)
}

fn read_json<T: DeserializeOwned>(root: &Path, relative: &str) -> Result<(T, Vec<u8>)> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).with_context(|| format!("inspect benchmark input {relative}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("benchmark input must be a regular file: {relative}");
    }
    let bytes = fs::read(&path).with_context(|| format!("read {relative}"))?;
    let value = serde_json::from_slice(&bytes).with_context(|| format!("parse {relative}"))?;
    Ok((value, bytes))
}

fn fixture_hash(files: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (path, content) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(content.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty() || path.components().any(|component| !matches!(component, Component::Normal(_))) {
        bail!("unsafe benchmark relative path: {value}");
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn count_u32(value: usize) -> Result<u32> {
    u32::try_from(value).context("benchmark count does not fit u32")
}

fn command_text(root: &Path, program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("run provenance command {program}"))?;
    if !output.status.success() {
        bail!("provenance command failed: {program}");
    }
    String::from_utf8(output.stdout)
        .context("provenance command returned non-UTF-8")
        .map(|value| value.trim().to_owned())
}
