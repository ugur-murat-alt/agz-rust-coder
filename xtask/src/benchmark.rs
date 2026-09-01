#![allow(dead_code)]

use std::{
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::{child_process, mcp};

const DEFAULT_REPETITIONS: u32 = 1;
const MAX_REPETITIONS: u32 = 20;
const SMOKE_SCORER_VERSION: &str = "stage7-typed-oracle-v6";
const FIXTURE_IDS: [&str; 2] = ["clean", "broken"];
const LEGACY_MANIFEST: &str = include_str!("../../benchmark/legacy/historical-manifest.json");
const LEGACY_MANIFEST_SHA256: &str =
    include_str!("../../benchmark/legacy/historical-manifest.sha256");
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The typed result used by both benchmark arms.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TypedOutcome {
    pub status: String,
    pub is_error: bool,
    pub structured_text_equivalent: bool,
    #[serde(skip, default)]
    pub reason: String,
}

/// Whether the Rust MCP result matches the Cargo oracle's typed outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutcomeComparison {
    Equivalent,
    Mismatch,
}

/// Counters for side effects that are forbidden in provider-free smoke mode.
///
/// Cargo oracle children are local authority probes and are reaped before this
/// snapshot is made. `process` therefore means a process left alive by the
/// benchmark, not the number of completed local Cargo invocations.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BenchmarkCounters {
    pub process: u32,
    pub network: u32,
    pub model: u32,
    pub provider: u32,
    pub public: u32,
    pub paid: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrialEvidence {
    pub repetition: u32,
    pub arm: String,
    pub outcome: TypedOutcome,
    pub source_hash_before: String,
    pub source_hash_after: String,
    pub source_unchanged: bool,
    pub comparison: Option<OutcomeComparison>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FixtureEvidence {
    pub id: String,
    pub fixture_hash: String,
    pub expected_status: String,
    pub trials: Vec<TrialEvidence>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LegacyValidation {
    pub manifest_id: String,
    pub checksum: String,
    pub schema_version: u64,
    pub schema_valid: bool,
    pub checksum_valid: bool,
    pub executed: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BenchmarkEvidence {
    pub schema_version: u32,
    pub command: String,
    pub status: String,
    pub passed: bool,
    pub mode: String,
    pub arms: Vec<String>,
    pub repetitions: u32,
    pub warnings: Vec<String>,
    pub fixtures: Vec<FixtureEvidence>,
    pub legacy: LegacyValidation,
    pub counters: BenchmarkCounters,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub network: bool,
    pub public: bool,
    pub paid: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize)]
pub struct FrozenRunManifest {
    pub schema_version: u64,
    pub frozen: bool,
    pub run_id: String,
    pub source_commit: String,
    pub source_checksum: String,
    pub provider: String,
    pub model: String,
    pub variant: Option<String>,
    pub arms: Vec<String>,
    pub fixtures: Vec<FrozenFixture>,
    pub repetitions: u32,
    pub order_seed: u64,
    pub scorer_version: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Serialize)]
pub struct FrozenFixture {
    pub id: String,
    pub checksum: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LivePreflight {
    pub schema_version: u32,
    pub ready: bool,
    pub provider: String,
    pub model: String,
    pub scorer_version: String,
    pub model_requests: u32,
    pub provider_requests: u32,
    pub paid_requests: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveTrialRequest {
    pub schema_version: u32,
    pub run_id: String,
    pub trial_id: String,
    pub arm: String,
    pub fixture: String,
    pub repetition: u32,
    pub order_index: u32,
    pub provider: String,
    pub model: String,
    pub variant: Option<String>,
    pub scorer_version: String,
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LiveTrialResult {
    pub schema_version: u32,
    pub status: String,
    pub passed: bool,
    pub scorer_version: String,
    pub duration_ms: u64,
    pub model_requests: u32,
    pub provider_requests: u32,
    pub paid_requests: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LiveTrialEvidence {
    pub trial_id: String,
    pub arm: String,
    pub fixture: String,
    pub repetition: u32,
    pub order_index: u32,
    pub source_hash_before: String,
    pub source_hash_after: String,
    pub source_unchanged: bool,
    pub result: LiveTrialResult,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LiveBenchmarkEvidence {
    pub schema_version: u32,
    pub run_id: String,
    pub status: String,
    pub source_commit: String,
    pub source_checksum: String,
    pub provider: String,
    pub model: String,
    pub variant: Option<String>,
    pub scorer_version: String,
    pub order_seed: u64,
    pub trials: Vec<LiveTrialEvidence>,
    pub counters: BenchmarkCounters,
    pub paid: bool,
}

pub type LiveFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait LiveTrialAdapter: Send + Sync {
    fn preflight<'a>(
        &'a self,
        root: &'a Path,
        manifest: &'a FrozenRunManifest,
    ) -> LiveFuture<'a, LivePreflight>;

    fn run_trial<'a>(&'a self, request: &'a LiveTrialRequest) -> LiveFuture<'a, LiveTrialResult>;
}

struct CommandLiveAdapter {
    command: PathBuf,
}

impl CommandLiveAdapter {
    fn from_env() -> Result<Self> {
        let command = std::env::var_os("AGZ_RUST_CODER_LIVE_ADAPTER")
            .map(PathBuf::from)
            .context("live mode requires AGZ_RUST_CODER_LIVE_ADAPTER")?;
        let metadata = fs::symlink_metadata(&command).context("inspect live adapter")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("live adapter must be a regular executable file");
        }
        Ok(Self { command })
    }

    async fn invoke<T: Serialize, O: for<'de> Deserialize<'de>>(
        &self,
        mode: &str,
        current_dir: &Path,
        input: &T,
    ) -> Result<O> {
        let input = serde_json::to_vec(input).context("serialize live adapter input")?;
        let mut command = Command::new(&self.command);
        command.arg(mode).current_dir(current_dir);
        let output = child_process::output(
            command,
            Some(input),
            Duration::from_secs(30 * 60),
            8 * 1024 * 1024,
        )
        .await
        .context("run live benchmark adapter")?;
        if !output.status.success() {
            bail!(
                "live adapter {mode} failed: {}",
                bounded_text(&String::from_utf8_lossy(&output.stderr))
            );
        }
        serde_json::from_slice(&output.stdout).context("parse live adapter output")
    }
}

impl LiveTrialAdapter for CommandLiveAdapter {
    fn preflight<'a>(
        &'a self,
        root: &'a Path,
        manifest: &'a FrozenRunManifest,
    ) -> LiveFuture<'a, LivePreflight> {
        Box::pin(self.invoke("preflight", root, manifest))
    }

    fn run_trial<'a>(&'a self, request: &'a LiveTrialRequest) -> LiveFuture<'a, LiveTrialResult> {
        Box::pin(self.invoke("trial", &request.workspace, request))
    }
}

/// The result of the live preflight. No live work is started by this value.
#[allow(dead_code)]
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum LiveRunDecision {
    Smoke,
    Approved(FrozenRunManifest),
}

impl LiveRunDecision {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Approved(_))
    }
}

/// A boxed async seam keeps benchmark orchestration independent of the Phase 3
/// checker implementation while still allowing a real RMCP call by default.
pub type CheckFuture<'a> = Pin<Box<dyn Future<Output = Result<TypedOutcome>> + Send + 'a>>;

pub trait RustMcpCheckAdapter: Send + Sync {
    fn check<'a>(&'a self, directory: &'a Path) -> CheckFuture<'a>;
}

impl<F> RustMcpCheckAdapter for F
where
    F: for<'a> Fn(&'a Path) -> CheckFuture<'a> + Send + Sync,
{
    fn check<'a>(&'a self, directory: &'a Path) -> CheckFuture<'a> {
        self(directory)
    }
}

struct ChildProcessRustMcp {
    root: PathBuf,
}

impl RustMcpCheckAdapter for ChildProcessRustMcp {
    fn check<'a>(&'a self, directory: &'a Path) -> CheckFuture<'a> {
        Box::pin(async move {
            let observation = mcp::check(&self.root, directory).await?;
            Ok(TypedOutcome {
                status: observation.status,
                is_error: observation.is_error,
                structured_text_equivalent: observation.structured_text_equivalent,
                reason: observation.reason,
            })
        })
    }
}

#[allow(dead_code)]
struct OwnedAdapter<F>(F);

impl<F, Fut> RustMcpCheckAdapter for OwnedAdapter<F>
where
    F: Fn(PathBuf) -> Fut + Send + Sync,
    Fut: Future<Output = Result<TypedOutcome>> + Send + 'static,
{
    fn check<'a>(&'a self, directory: &'a Path) -> CheckFuture<'a> {
        Box::pin((self.0)(directory.to_path_buf()))
    }
}

/// Run the default provider-free smoke from the xtask repository root.
#[allow(dead_code)]
pub async fn run_benchmark_smoke() -> Result<BenchmarkEvidence> {
    let root = repository_root()?;
    run_benchmark_smoke_at(&root).await
}

/// Run the default provider-free smoke against an explicit repository root.
#[allow(dead_code)]
pub async fn run_benchmark_smoke_at(root: &Path) -> Result<BenchmarkEvidence> {
    let adapter = ChildProcessRustMcp {
        root: root.to_path_buf(),
    };
    run_benchmark_smoke_with_adapter(root, DEFAULT_REPETITIONS, &adapter).await
}

/// Run smoke with an owned-path closure. This is convenient for tests and for
/// the temporary Phase 3 adapter while preserving the real default path above.
#[allow(dead_code)]
pub async fn run_benchmark_smoke_with<F, Fut>(root: &Path, adapter: F) -> Result<BenchmarkEvidence>
where
    F: Fn(PathBuf) -> Fut + Send + Sync,
    Fut: Future<Output = Result<TypedOutcome>> + Send + 'static,
{
    let adapter = OwnedAdapter(adapter);
    run_benchmark_smoke_with_repetitions(root, DEFAULT_REPETITIONS, &adapter).await
}

/// Run smoke with a caller-owned RMCP/check adapter and a fixed repetition
/// count. The adapter is called only after guard-free smoke setup succeeds.
#[allow(clippy::too_many_lines)]
pub async fn run_benchmark_smoke_with_repetitions<A>(
    root: &Path,
    repetitions: u32,
    adapter: &A,
) -> Result<BenchmarkEvidence>
where
    A: RustMcpCheckAdapter,
{
    validate_repetitions(repetitions)?;
    let root = canonical_directory(root).context("benchmark repository root")?;
    let legacy = validate_legacy_manifest()?;
    let temporary = TemporaryDirectory::new("stage7-smoke")?;
    let mut fixtures = Vec::with_capacity(FIXTURE_IDS.len());

    for fixture_id in FIXTURE_IDS {
        let source = fixture_source(&root, fixture_id)?;
        validate_fixture(&source, fixture_id)?;
        let fixture_hash = source_hash(&source)?;
        let expected_status = expected_status(fixture_id);
        let mut trials = Vec::with_capacity(repetitions as usize * 2);

        for repetition in 1..=repetitions {
            let off_workspace = temporary
                .path
                .join(format!("{fixture_id}-off-{repetition}"));
            copy_fixture(&source, &off_workspace)?;
            let before = source_hash(&off_workspace)?;
            if before != fixture_hash {
                bail!("fixture copy changed before the off oracle");
            }
            let oracle = cargo_oracle(&off_workspace).await?;
            let after = source_hash(&off_workspace)?;
            let unchanged = before == after;
            if !unchanged {
                bail!("off oracle modified fixture source");
            }
            if oracle.status != expected_status {
                bail!("off oracle returned an unexpected typed outcome");
            }
            let expected_outcome = oracle.clone();
            trials.push(TrialEvidence {
                repetition,
                arm: "off".to_owned(),
                outcome: oracle,
                source_hash_before: before,
                source_hash_after: after,
                source_unchanged: unchanged,
                comparison: None,
            });

            let rust_workspace = temporary
                .path
                .join(format!("{fixture_id}-rust-mcp-{repetition}"));
            copy_fixture(&source, &rust_workspace)?;
            let before = source_hash(&rust_workspace)?;
            if before != fixture_hash {
                bail!("fixture copy changed before the Rust MCP arm");
            }
            let rust_outcome = adapter.check(&rust_workspace).await?;
            let after = source_hash(&rust_workspace)?;
            let unchanged = before == after;
            if !unchanged {
                bail!("Rust MCP arm modified fixture source");
            }
            let comparison = compare_typed_outcomes(&expected_outcome, &rust_outcome);
            if comparison == OutcomeComparison::Mismatch {
                bail!(
                    "Rust MCP arm for fixture {fixture_id} expected {expected_status} but returned status={} is_error={}: {}",
                    rust_outcome.status,
                    rust_outcome.is_error,
                    rust_outcome.reason
                );
            }
            trials.push(TrialEvidence {
                repetition,
                arm: "rust_mcp".to_owned(),
                outcome: rust_outcome,
                source_hash_before: before,
                source_hash_after: after,
                source_unchanged: unchanged,
                comparison: Some(comparison),
            });
        }

        fixtures.push(FixtureEvidence {
            id: fixture_id.to_owned(),
            fixture_hash,
            expected_status: expected_status.to_owned(),
            trials,
        });
    }

    let mut warnings = Vec::new();
    if repetitions < 3 {
        warnings.push(
            "fewer-than-three-repetitions: observed smoke results are underpowered".to_owned(),
        );
    }
    Ok(BenchmarkEvidence {
        schema_version: 1,
        command: "benchmark-smoke".to_owned(),
        status: "pass".to_owned(),
        passed: true,
        mode: "provider-free".to_owned(),
        arms: vec!["off".to_owned(), "rust_mcp".to_owned()],
        repetitions,
        warnings,
        fixtures,
        legacy,
        counters: BenchmarkCounters::default(),
        provider: None,
        model: None,
        network: false,
        public: false,
        paid: false,
    })
}

/// Adapter-oriented spelling kept explicit for callers wiring the Phase 3
/// checker. The repetition count is part of the frozen comparison input.
pub async fn run_benchmark_smoke_with_adapter<A>(
    root: &Path,
    repetitions: u32,
    adapter: &A,
) -> Result<BenchmarkEvidence>
where
    A: RustMcpCheckAdapter,
{
    run_benchmark_smoke_with_repetitions(root, repetitions, adapter).await
}

/// The xtask entry point. Live requests are guarded before fixture or process
/// work; valid live approval currently stops at the Phase 3 execution boundary.
pub async fn run(root: &Path, args: Vec<String>) -> Result<()> {
    let decision = guard_live_run(&args)?;
    if let LiveRunDecision::Approved(manifest) = decision {
        let adapter = CommandLiveAdapter::from_env()?;
        let evidence = run_live_benchmark_with_adapter(root, &manifest, &adapter).await?;
        let run_manifest = live_run_manifest(&manifest);
        let results =
            serde_json::to_value(&evidence).context("serialize live benchmark results")?;
        let report = live_report(&evidence);
        let published = publish_evidence(root, &run_manifest, &results, &report)?;
        remove_live_checkpoints(root, &manifest.run_id)?;
        println!(
            "benchmark-smoke: {} (approved live evidence published at {})",
            evidence.status,
            published.display()
        );
        return Ok(());
    }
    let repetitions = parse_repetitions(&args)?;
    let adapter = ChildProcessRustMcp {
        root: root.to_path_buf(),
    };
    let evidence = run_benchmark_smoke_with_adapter(root, repetitions, &adapter).await?;
    let run_manifest = smoke_run_manifest(root, &evidence)?;
    let results = serde_json::to_value(&evidence).context("serialize benchmark results")?;
    let report = smoke_report(&evidence);
    let published = publish_evidence(root, &run_manifest, &results, &report)?;
    println!(
        "benchmark-smoke: {} (provider-free evidence published)",
        evidence.status
    );
    let _ = published;
    Ok(())
}

pub async fn run_live_benchmark_with_adapter<A>(
    root: &Path,
    manifest: &FrozenRunManifest,
    adapter: &A,
) -> Result<LiveBenchmarkEvidence>
where
    A: LiveTrialAdapter,
{
    let root = canonical_directory(root).context("live benchmark repository root")?;
    validate_live_source(&root, manifest)?;
    let preflight = adapter.preflight(&root, manifest).await?;
    if preflight.schema_version != 1
        || !preflight.ready
        || preflight.provider != manifest.provider
        || preflight.model != manifest.model
        || preflight.scorer_version != manifest.scorer_version
        || preflight.model_requests != 0
        || preflight.provider_requests != 0
        || preflight.paid_requests != 0
    {
        bail!("live adapter preflight did not match the frozen zero-request contract");
    }

    let temporary = TemporaryDirectory::new("stage7-live")?;
    let schedule = live_schedule(manifest);
    let mut trials = Vec::with_capacity(schedule.len());
    let mut counters = BenchmarkCounters::default();
    for (order_index, (arm, fixture, repetition)) in schedule.into_iter().enumerate() {
        let source = fixture_source(&root, &fixture)?;
        let workspace = temporary.path.join(format!(
            "trial-{order_index:04}-{arm}-{fixture}-{repetition}"
        ));
        copy_fixture(&source, &workspace)?;
        let source_hash_before = source_hash(&workspace)?;
        let trial_id = format!("{order_index:04}-{arm}-{fixture}-{repetition}");
        let request = LiveTrialRequest {
            schema_version: 1,
            run_id: manifest.run_id.clone(),
            trial_id: trial_id.clone(),
            arm: arm.clone(),
            fixture: fixture.clone(),
            repetition,
            order_index: u32::try_from(order_index).unwrap_or(u32::MAX),
            provider: manifest.provider.clone(),
            model: manifest.model.clone(),
            variant: manifest.variant.clone(),
            scorer_version: manifest.scorer_version.clone(),
            workspace,
        };
        let result = adapter.run_trial(&request).await?;
        validate_live_trial_result(&result, &manifest.scorer_version)?;
        counters.model = counters.model.saturating_add(result.model_requests);
        counters.provider = counters.provider.saturating_add(result.provider_requests);
        counters.paid = counters.paid.saturating_add(result.paid_requests);
        counters.network = counters.network.saturating_add(result.provider_requests);
        let source_hash_after = source_hash(&request.workspace)?;
        let source_unchanged = source_hash_before == source_hash_after;
        if !source_unchanged {
            bail!("live adapter mutated the frozen fixture source");
        }
        trials.push(LiveTrialEvidence {
            trial_id,
            arm,
            fixture,
            repetition,
            order_index: request.order_index,
            source_hash_before,
            source_hash_after,
            source_unchanged,
            result,
        });
        write_live_checkpoint(&root, manifest, &trials, &counters)?;
    }
    let status = if trials.iter().all(|trial| trial.result.passed) {
        "PASS"
    } else {
        "FAIL"
    };
    Ok(LiveBenchmarkEvidence {
        schema_version: 1,
        run_id: manifest.run_id.clone(),
        status: status.to_owned(),
        source_commit: manifest.source_commit.clone(),
        source_checksum: manifest.source_checksum.clone(),
        provider: manifest.provider.clone(),
        model: manifest.model.clone(),
        variant: manifest.variant.clone(),
        scorer_version: manifest.scorer_version.clone(),
        order_seed: manifest.order_seed,
        trials,
        paid: counters.paid > 0,
        counters,
    })
}

pub fn validate_live_trial_result(result: &LiveTrialResult, scorer_version: &str) -> Result<()> {
    if result.schema_version != 1
        || result.scorer_version != scorer_version
        || !matches!(result.status.as_str(), "PASS" | "FAIL")
        || result.passed != (result.status == "PASS")
    {
        bail!("live adapter returned an invalid typed trial result");
    }
    Ok(())
}

fn validate_live_source(root: &Path, manifest: &FrozenRunManifest) -> Result<()> {
    let (commit, _) = git_source_identity(root)?;
    if commit != manifest.source_commit {
        bail!("live source commit does not match the frozen manifest");
    }
    let mut fixtures = Vec::with_capacity(manifest.fixtures.len());
    for fixture in &manifest.fixtures {
        let source = fixture_source(root, &fixture.id)?;
        validate_fixture(&source, &fixture.id)?;
        let checksum = source_hash(&source)?;
        if checksum != fixture.checksum {
            bail!("live fixture checksum does not match the frozen manifest");
        }
        fixtures.push((fixture.id.as_str(), fixture.checksum.as_str()));
    }
    if source_checksum_for_fixtures(root, &fixtures)? != manifest.source_checksum {
        bail!("live source checksum does not match the frozen manifest");
    }
    Ok(())
}

fn live_schedule(manifest: &FrozenRunManifest) -> Vec<(String, String, u32)> {
    let mut schedule = Vec::new();
    for repetition in 1..=manifest.repetitions {
        for fixture in &manifest.fixtures {
            for arm in &manifest.arms {
                schedule.push((arm.clone(), fixture.id.clone(), repetition));
            }
        }
    }
    schedule.sort_by_key(|(arm, fixture, repetition)| {
        sha256_bytes(format!("{}\0{arm}\0{fixture}\0{repetition}", manifest.order_seed).as_bytes())
    });
    schedule
}

fn live_run_manifest(manifest: &FrozenRunManifest) -> Value {
    serde_json::json!({
        "schema_version": manifest.schema_version,
        "run_id": manifest.run_id,
        "frozen": true,
        "mode": "live",
        "source_commit": manifest.source_commit,
        "source_checksum": manifest.source_checksum,
        "provider": manifest.provider,
        "model": manifest.model,
        "variant": manifest.variant,
        "arms": manifest.arms,
        "fixtures": manifest.fixtures,
        "repetitions": manifest.repetitions,
        "order_seed": manifest.order_seed,
        "scorer_version": manifest.scorer_version,
    })
}

fn live_report(evidence: &LiveBenchmarkEvidence) -> String {
    format!(
        "# Stage 7 live benchmark\n\nStatus: {}\nTrials: {}\nPaid requests: {}\nSource and fixture hashes remained frozen.\n",
        evidence.status,
        evidence.trials.len(),
        evidence.counters.paid
    )
}

/// Reject a live request unless both a frozen manifest and a separate explicit
/// approval flag are present. This function performs no child/network work.
pub fn guard_live_run<I, S>(args: I) -> Result<LiveRunDecision>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    let mut live = false;
    let mut approved = false;
    let mut manifest_path = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--live" => live = true,
            "--approve" | "--approve-live" | "--approval" | "--approved" => approved = true,
            "--manifest" => {
                index += 1;
                let value = args
                    .get(index)
                    .context("--manifest requires a frozen manifest path")?;
                if value.is_empty() {
                    bail!("--manifest requires a frozen manifest path");
                }
                if manifest_path.replace(PathBuf::from(value)).is_some() {
                    bail!("live run manifest was supplied more than once");
                }
            }
            value if value.starts_with("--manifest=") => {
                let value = value.trim_start_matches("--manifest=");
                if value.is_empty() {
                    bail!("--manifest requires a frozen manifest path");
                }
                if manifest_path.replace(PathBuf::from(value)).is_some() {
                    bail!("live run manifest was supplied more than once");
                }
            }
            "--repetitions" => {
                index += 1;
                args.get(index)
                    .context("--repetitions requires an integer")?
                    .parse::<u32>()
                    .context("--repetitions requires an integer")?;
            }
            value if value.starts_with("--repetitions=") => {
                value
                    .trim_start_matches("--repetitions=")
                    .parse::<u32>()
                    .context("--repetitions requires an integer")?;
            }
            other => bail!("unknown benchmark option: {other}"),
        }
        index += 1;
    }

    if !live {
        if approved || manifest_path.is_some() {
            bail!("manifest and approval are valid only with --live");
        }
        return Ok(LiveRunDecision::Smoke);
    }
    if !approved {
        bail!("--live requires separate explicit approval (--approve-live)");
    }
    let manifest_path = manifest_path.context("--live requires --manifest <frozen-run.json>")?;
    let manifest = read_frozen_manifest(&manifest_path)?;
    Ok(LiveRunDecision::Approved(manifest))
}

/// Validate the immutable historical legacy manifest embedded in the binary.
/// It is evidence only and is never executed.
pub fn validate_legacy_manifest() -> Result<LegacyValidation> {
    let expected = LEGACY_MANIFEST_SHA256
        .split_whitespace()
        .next()
        .context("historical manifest checksum file is empty")?;
    validate_legacy_manifest_bytes(LEGACY_MANIFEST.as_bytes(), expected)
}

/// Validate a legacy manifest and its expected SHA-256 without starting its
/// historical runtime. This is public for focused integrity tests.
#[allow(dead_code)]
pub fn validate_legacy_manifest_file(
    path: &Path,
    expected_checksum: &str,
) -> Result<LegacyValidation> {
    let metadata = fs::symlink_metadata(path).context("read historical manifest metadata")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("historical manifest must be a regular file");
    }
    let bytes = fs::read(path).context("read historical manifest")?;
    validate_legacy_manifest_bytes(&bytes, expected_checksum)
}

/// Compare the complete typed status contract. Unavailable results are not
/// parity evidence once the Rust MCP checker is connected.
pub fn compare_typed_outcomes(expected: &TypedOutcome, actual: &TypedOutcome) -> OutcomeComparison {
    let same_status = matches!(
        expected.status.as_str(),
        "FAST_PASS" | "FULL_PASS" | "PASS" | "FAIL"
    ) && actual.status == expected.status;
    if same_status
        && actual.is_error == expected.is_error
        && actual.structured_text_equivalent == expected.structured_text_equivalent
    {
        OutcomeComparison::Equivalent
    } else {
        OutcomeComparison::Mismatch
    }
}

/// Atomically publish a validated evidence bundle below `benchmark/results`.
/// A failed writer removes only its own temporary directory.
pub fn publish_evidence(
    root: &Path,
    run: &Value,
    results: &Value,
    report: &str,
) -> Result<PathBuf> {
    let results_root = root.join("benchmark").join("results").join("stage7");
    ensure_directory_no_symlink(&results_root)?;
    let lock_path = results_root.join(".publish.lock");
    reject_symlink_or_nonfile(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .context("open benchmark evidence lock")?;
    FileExt::lock(&lock).context("lock benchmark evidence publication")?;
    let result = publish_evidence_locked(&results_root, root, run, results, report);
    let _ = FileExt::unlock(&lock);
    result
}

fn write_live_checkpoint(
    root: &Path,
    manifest: &FrozenRunManifest,
    trials: &[LiveTrialEvidence],
    counters: &BenchmarkCounters,
) -> Result<()> {
    let results_root = root.join("benchmark").join("results").join("stage7");
    ensure_directory_no_symlink(&results_root)?;
    let lock_path = results_root.join(".publish.lock");
    reject_symlink_or_nonfile(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .context("open live checkpoint lock")?;
    FileExt::lock(&lock).context("lock live checkpoint publication")?;
    let result = write_live_checkpoint_locked(&results_root, manifest, trials, counters);
    let _ = FileExt::unlock(&lock);
    result
}

fn write_live_checkpoint_locked(
    results_root: &Path,
    manifest: &FrozenRunManifest,
    trials: &[LiveTrialEvidence],
    counters: &BenchmarkCounters,
) -> Result<()> {
    let final_path = results_root.join(format!(
        ".live-{}-checkpoint-{:04}",
        manifest.run_id,
        trials.len()
    ));
    if final_path.exists() {
        bail!("live checkpoint already exists");
    }
    let temporary = unique_child_directory(results_root, "live-checkpoint")?;
    let result = (|| {
        write_new_synced(
            &temporary.join("run.json"),
            &json_bytes(&live_run_manifest(manifest))?,
        )?;
        write_new_synced(
            &temporary.join("results.json"),
            &json_bytes(&serde_json::json!({
                "schema_version": 1,
                "completed_trials": trials,
                "counters": counters,
            }))?,
        )?;
        write_new_synced(
            &temporary.join("report.md"),
            format!(
                "# Live benchmark checkpoint\n\nCompleted trials: {}\n",
                trials.len()
            )
            .as_bytes(),
        )?;
        sync_directory(&temporary);
        fs::rename(&temporary, &final_path).context("publish live checkpoint")?;
        sync_directory(results_root);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

pub(crate) fn remove_live_checkpoints(root: &Path, run_id: &str) -> Result<()> {
    let results_root = root.join("benchmark").join("results").join("stage7");
    let prefix = format!(".live-{run_id}-checkpoint-");
    let metadata = match fs::symlink_metadata(&results_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect live checkpoint directory"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("live checkpoint root is unsafe");
    }
    let entries = fs::read_dir(&results_root).context("read live checkpoint directory")?;
    for entry in entries {
        let entry = entry.context("read live checkpoint entry")?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let metadata = fs::symlink_metadata(entry.path()).context("inspect live checkpoint")?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("live checkpoint path is unsafe");
            }
            fs::remove_dir_all(entry.path()).context("remove completed live checkpoint")?;
        }
    }
    sync_directory(&results_root);
    Ok(())
}

fn publish_evidence_locked(
    results_root: &Path,
    root: &Path,
    run: &Value,
    results: &Value,
    report: &str,
) -> Result<PathBuf> {
    let run_id = run
        .get("run_id")
        .and_then(Value::as_str)
        .context("run evidence must contain run_id")?;
    validate_artifact_component(run_id).context("unsafe benchmark run_id")?;
    let run_bytes = json_bytes(run)?;
    let result_bytes = json_bytes(results)?;
    let report_bytes = report.as_bytes();
    let payload = [run_bytes.as_slice(), result_bytes.as_slice(), report_bytes].concat();
    let root_text = root.to_string_lossy();
    let text = String::from_utf8_lossy(&payload).to_ascii_lowercase();
    for forbidden in [
        root_text.to_ascii_lowercase(),
        "prompt".to_owned(),
        "session".to_owned(),
        "credential".to_owned(),
        "process_id".to_owned(),
        "pid".to_owned(),
    ] {
        if !forbidden.is_empty() && text.contains(&forbidden) {
            bail!("benchmark evidence contains forbidden private text");
        }
    }

    let final_path = results_root.join(format!("benchmark-smoke-{run_id}"));
    match fs::symlink_metadata(&final_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("existing benchmark evidence is unsafe");
            }
            validate_existing_bundle(&final_path, &run_bytes, &result_bytes, report_bytes)?;
            return Ok(final_path);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect benchmark evidence destination"),
    }

    let temporary = unique_child_directory(results_root, "benchmark-smoke")?;
    let result = (|| {
        write_new_synced(&temporary.join("run.json"), &run_bytes)?;
        write_new_synced(&temporary.join("results.json"), &result_bytes)?;
        write_new_synced(&temporary.join("report.md"), report_bytes)?;
        sync_directory(&temporary);
        match fs::rename(&temporary, &final_path) {
            Ok(()) => {
                sync_directory(results_root);
                Ok(final_path)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("benchmark evidence was published concurrently")
            }
            Err(error) => Err(error).with_context(|| "atomically publish benchmark evidence"),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

async fn cargo_oracle(directory: &Path) -> Result<TypedOutcome> {
    let cargo_home = directory.join(".cargo-home");
    ensure_directory_no_symlink(&cargo_home)?;
    let output = Command::new("cargo")
        .current_dir(directory)
        .args(["check", "--locked", "--offline", "--message-format=json"])
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .await
        .context("run locked offline Cargo oracle")?;
    classify_cargo_oracle(output.status.success(), &output.stdout, &output.stderr)
}

pub fn classify_cargo_oracle(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<TypedOutcome> {
    let reason = String::from_utf8_lossy(stderr).into_owned();
    if success {
        return Ok(TypedOutcome {
            status: "FAST_PASS".to_owned(),
            is_error: false,
            structured_text_equivalent: true,
            reason,
        });
    }
    let compiler_error = stdout.split(|byte| *byte == b'\n').any(|line| {
        serde_json::from_slice::<Value>(line)
            .ok()
            .is_some_and(|value| {
                value.get("reason").and_then(Value::as_str) == Some("compiler-message")
                    && value.pointer("/message/level").and_then(Value::as_str) == Some("error")
            })
    });
    if !compiler_error {
        bail!(
            "Cargo oracle failed without a compiler diagnostic: {}",
            bounded_text(&reason)
        );
    }
    Ok(TypedOutcome {
        status: "FAIL".to_owned(),
        is_error: false,
        structured_text_equivalent: true,
        reason,
    })
}

fn validate_repetitions(repetitions: u32) -> Result<()> {
    if !(1..=MAX_REPETITIONS).contains(&repetitions) {
        bail!("benchmark repetitions must be between 1 and 20");
    }
    Ok(())
}

fn parse_repetitions(args: &[String]) -> Result<u32> {
    let mut repetitions = DEFAULT_REPETITIONS;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repetitions" => {
                index += 1;
                repetitions = args
                    .get(index)
                    .context("--repetitions requires an integer")?
                    .parse()
                    .context("--repetitions requires an integer")?;
            }
            value if value.starts_with("--repetitions=") => {
                repetitions = value
                    .trim_start_matches("--repetitions=")
                    .parse()
                    .context("--repetitions requires an integer")?;
            }
            _ => {}
        }
        index += 1;
    }
    validate_repetitions(repetitions)?;
    Ok(repetitions)
}

pub(crate) fn smoke_run_manifest(root: &Path, evidence: &BenchmarkEvidence) -> Result<Value> {
    let fixture_values: Vec<Value> = evidence
        .fixtures
        .iter()
        .map(|fixture| {
            let mut value = Map::new();
            value.insert("id".to_owned(), Value::String(fixture.id.clone()));
            value.insert(
                "checksum".to_owned(),
                Value::String(fixture.fixture_hash.clone()),
            );
            Value::Object(value)
        })
        .collect();
    let source_checksum = relevant_source_checksum(root, evidence)?;
    let (source_commit, source_dirty) = git_source_identity(root)?;
    let scorer_version = SMOKE_SCORER_VERSION;
    let run_id = smoke_run_id(
        &source_commit,
        source_dirty,
        &source_checksum,
        evidence.repetitions,
        &evidence.arms,
        scorer_version,
    );
    Ok(serde_json::json!({
        "schema_version": 1,
        "run_id": run_id,
        "frozen": false,
        "mode": "provider-free",
        "source_commit": source_commit,
        "source_dirty": source_dirty,
        "source_checksum": source_checksum,
        "provider": null,
        "model": null,
        "variant": null,
        "arms": evidence.arms,
        "fixtures": fixture_values,
        "repetitions": evidence.repetitions,
        "order_seed": 0,
        "scorer_version": scorer_version,
        "estimated_cost": 0,
        "estimated_duration": 0,
        "non_inferiority_margin": 0,
        "cache_strata": "isolated",
        "legacy": {
            "checksum": evidence.legacy.checksum,
            "executed": false,
        },
    }))
}

pub(crate) fn smoke_run_id(
    source_commit: &str,
    source_dirty: bool,
    source_checksum: &str,
    repetitions: u32,
    arms: &[String],
    scorer_version: &str,
) -> String {
    let run_checksum = sha256_bytes(
        format!(
            "source_commit={source_commit}\nsource_dirty={source_dirty}\nsource_checksum={source_checksum}\nrepetitions={repetitions}\nscorer={scorer_version}\narms={}\n",
            arms.join(",")
        )
        .as_bytes(),
    );
    format!("stage7-smoke-v6-{run_checksum}")
}

pub(crate) fn relevant_source_checksum(
    root: &Path,
    evidence: &BenchmarkEvidence,
) -> Result<String> {
    let fixtures = evidence
        .fixtures
        .iter()
        .map(|fixture| (fixture.id.as_str(), fixture.fixture_hash.as_str()))
        .collect::<Vec<_>>();
    source_checksum_for_fixtures(root, &fixtures)
}

fn source_checksum_for_fixtures(root: &Path, fixtures: &[(&str, &str)]) -> Result<String> {
    let mut digest = Sha256::new();
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "crates/agz-rust-coder/src",
        "xtask/src",
        "xtask/tests",
    ] {
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect benchmark source input {relative}"))?;
        if metadata.file_type().is_symlink() {
            bail!("benchmark source input is a symlink: {relative}");
        }
        if metadata.is_file() {
            digest.update(relative.as_bytes());
            digest.update([0]);
            digest.update(fs::read(&path).context("read benchmark source input")?);
            digest.update([0]);
        } else if metadata.is_dir() {
            let mut files = Vec::new();
            collect_source_files(&path, &path, &mut files)?;
            files.sort_by(|left, right| left.0.cmp(&right.0));
            for (nested, bytes) in files {
                digest.update(relative.as_bytes());
                digest.update(b"/");
                digest.update(nested.as_bytes());
                digest.update([0]);
                digest.update(bytes);
                digest.update([0]);
            }
        } else {
            bail!("benchmark source input is not regular: {relative}");
        }
    }
    for (id, checksum) in fixtures {
        digest.update(id.as_bytes());
        digest.update([0]);
        digest.update(checksum.as_bytes());
        digest.update([0]);
    }
    Ok(hex_digest(digest.finalize()))
}

fn git_source_identity(root: &Path) -> Result<(String, bool)> {
    let commit = std::process::Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("read benchmark source commit")?;
    if !commit.status.success() {
        bail!("could not read benchmark source commit");
    }
    let commit = String::from_utf8(commit.stdout)
        .context("benchmark source commit is not UTF-8")?
        .trim()
        .to_owned();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("benchmark source commit is not a full Git SHA");
    }
    let status = std::process::Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .output()
        .context("read benchmark source status")?;
    if !status.status.success() {
        bail!("could not read benchmark source status");
    }
    Ok((commit, !status.stdout.is_empty()))
}

fn bounded_text(value: &str) -> String {
    value.chars().take(2_000).collect()
}

fn smoke_report(evidence: &BenchmarkEvidence) -> String {
    let repetition_note = if evidence.repetitions < 3 {
        "Fewer than three repetitions were run; this is not statistical evidence."
    } else {
        "The repetition count meets the smoke warning threshold."
    };
    let phase_note = "The Rust MCP typed result matched the Cargo oracle.";
    format!(
        "# Stage 7 benchmark smoke\n\nStatus: {}\nMode: provider-free\nArms: off, rust_mcp\n{}\n{}\nLegacy historical evidence was checksum/schema validated and not executed.\n",
        evidence.status, repetition_note, phase_note
    )
}

fn validate_legacy_manifest_bytes(
    bytes: &[u8],
    expected_checksum: &str,
) -> Result<LegacyValidation> {
    if !is_sha256(expected_checksum) {
        bail!("historical manifest expected checksum is not SHA-256");
    }
    let checksum = sha256_bytes(bytes);
    if checksum != expected_checksum {
        bail!("historical manifest checksum mismatch");
    }
    let value: Value = serde_json::from_slice(bytes).context("parse historical manifest")?;
    let schema_version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .context("historical manifest schema_version")?;
    let manifest_id = value
        .get("run_id")
        .and_then(Value::as_str)
        .context("historical manifest run_id")?;
    let arms = value
        .get("arms")
        .and_then(Value::as_array)
        .context("historical manifest arms")?;
    let executed = value
        .get("executed")
        .and_then(Value::as_bool)
        .context("historical manifest executed flag")?;
    let schema_valid = schema_version == 1
        && manifest_id == "stage6-legacy-historical"
        && arms.iter().any(|arm| arm.as_str() == Some("legacy"))
        && !executed;
    if !schema_valid {
        bail!("historical legacy manifest schema mismatch");
    }
    Ok(LegacyValidation {
        manifest_id: manifest_id.to_owned(),
        checksum,
        schema_version,
        schema_valid,
        checksum_valid: true,
        executed,
    })
}

fn read_frozen_manifest(path: &Path) -> Result<FrozenRunManifest> {
    let metadata = fs::symlink_metadata(path).context("read frozen manifest metadata")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("frozen run manifest must be a regular file");
    }
    let bytes = fs::read(path).context("read frozen run manifest")?;
    let value: Value = serde_json::from_slice(&bytes).context("parse frozen run manifest")?;
    validate_manifest_privacy(&value)?;
    let schema_version = required_u64(&value, "schema_version")?;
    let frozen = value
        .get("frozen")
        .and_then(Value::as_bool)
        .context("frozen run manifest must contain frozen=true")?;
    if schema_version != 1 || !frozen {
        bail!("live run manifest is not a frozen schema version 1 manifest");
    }
    let run_id = required_string(&value, "run_id")?;
    validate_artifact_component(&run_id).context("frozen run_id")?;
    let source_commit = required_string(&value, "source_commit")?;
    if source_commit.len() != 40 || !source_commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("frozen source_commit is not a full Git SHA");
    }
    let source_checksum = required_string(&value, "source_checksum")?;
    if !is_sha256(&source_checksum) {
        bail!("frozen source_checksum is not SHA-256");
    }
    let provider = required_string(&value, "provider")?;
    let model = required_string(&value, "model")?;
    let variant = match value.get("variant") {
        Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .context("frozen variant must be a string or null")?
                .to_owned(),
        ),
        None => bail!("frozen run manifest must contain variant"),
    };
    let arms = required_string_array(&value, "arms")?;
    if arms.is_empty()
        || arms
            .iter()
            .any(|arm| !matches!(arm.as_str(), "off" | "legacy" | "rust_mcp"))
        || arms
            .iter()
            .enumerate()
            .any(|(index, arm)| arms[..index].contains(arm))
    {
        bail!("frozen run manifest has an invalid arm list");
    }
    let fixtures = value
        .get("fixtures")
        .and_then(Value::as_array)
        .context("frozen run manifest fixtures")?
        .iter()
        .map(|fixture| {
            let id = required_string(fixture, "id")?;
            validate_artifact_component(&id).context("frozen fixture id")?;
            let checksum = required_string(fixture, "checksum")?;
            if !is_sha256(&checksum) {
                bail!("frozen fixture checksum is not SHA-256");
            }
            Ok(FrozenFixture { id, checksum })
        })
        .collect::<Result<Vec<_>>>()?;
    if fixtures.is_empty() {
        bail!("frozen run manifest must list at least one fixture");
    }
    let repetitions = required_u64(&value, "repetitions")?;
    let repetitions = u32::try_from(repetitions).context("frozen repetitions out of range")?;
    validate_repetitions(repetitions)?;
    let order_seed = required_u64(&value, "order_seed")?;
    let scorer_version = required_string(&value, "scorer_version")?;
    for field in ["estimated_cost", "non_inferiority_margin", "cache_strata"] {
        if value.get(field).is_none_or(Value::is_null) {
            bail!("frozen run manifest is missing {field}");
        }
    }
    if value
        .get("estimated_duration")
        .or_else(|| value.get("estimated_duration_ms"))
        .is_none_or(Value::is_null)
    {
        bail!("frozen run manifest is missing estimated_duration");
    }
    Ok(FrozenRunManifest {
        schema_version,
        frozen,
        run_id,
        source_commit,
        source_checksum,
        provider,
        model,
        variant,
        arms,
        fixtures,
        repetitions,
        order_seed,
        scorer_version,
    })
}

fn validate_manifest_privacy(value: &Value) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let lower = key.to_ascii_lowercase();
                if [
                    "prompt",
                    "path",
                    "session",
                    "credential",
                    "pid",
                    "process_id",
                ]
                .iter()
                .any(|forbidden| lower.contains(forbidden))
                {
                    bail!("frozen run manifest contains private field");
                }
                validate_manifest_privacy(child)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_manifest_privacy(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("frozen run manifest field {field} must be a non-empty string"))
}

fn required_u64(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("frozen run manifest field {field} must be an integer"))
}

fn required_string_array(value: &Value, field: &str) -> Result<Vec<String>> {
    value
        .get(field)
        .and_then(Value::as_array)
        .with_context(|| format!("frozen run manifest field {field} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .with_context(|| format!("frozen run manifest field {field} contains a non-string"))
        })
        .collect()
}

#[allow(dead_code)]
fn repository_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .context("xtask must live directly below the repository root")
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).context("read benchmark directory metadata")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("benchmark root must be a regular directory");
    }
    fs::canonicalize(path).context("canonicalize benchmark directory")
}

fn fixture_source(root: &Path, fixture_id: &str) -> Result<PathBuf> {
    let source = root
        .join("tests")
        .join("fixtures")
        .join("stage7")
        .join("benchmark")
        .join(fixture_id);
    let source = canonical_directory(&source)?;
    if !source.starts_with(root) {
        bail!("benchmark fixture escapes the repository root");
    }
    Ok(source)
}

fn validate_fixture(source: &Path, fixture_id: &str) -> Result<()> {
    let manifest = source.join("Cargo.toml");
    let lockfile = source.join("Cargo.lock");
    let source_file = source.join("src").join("lib.rs");
    for path in [&manifest, &lockfile, &source_file] {
        let metadata = fs::symlink_metadata(path).with_context(|| "read fixture file metadata")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("fixture is missing a regular locked source file");
        }
    }
    let manifest_text = fs::read_to_string(&manifest).context("read fixture Cargo.toml")?;
    if !manifest_text.contains("[package]") || manifest_text.contains("[dependencies]") {
        bail!("fixture must be a dependency-free package");
    }
    let lock_text = fs::read_to_string(&lockfile).context("read fixture Cargo.lock")?;
    if !lock_text.contains("version = 4")
        || !lock_text.contains(&format!("name = \"stage7-{fixture_id}-fixture\""))
    {
        bail!("fixture does not have the expected locked package");
    }
    Ok(())
}

fn expected_status(fixture_id: &str) -> &'static str {
    if fixture_id == "clean" {
        "FAST_PASS"
    } else {
        "FAIL"
    }
}

fn copy_fixture(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        bail!("benchmark destination already exists");
    }
    fs::create_dir(destination).context("create isolated fixture directory")?;
    copy_tree(source, destination)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)
        .context("read fixture directory")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("read fixture directory entry")?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).context("read fixture entry metadata")?;
        if metadata.file_type().is_symlink() {
            bail!("fixture symlinks are not allowed");
        }
        if metadata.is_dir() {
            fs::create_dir(&destination_path).context("create isolated fixture subdirectory")?;
            copy_tree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            let bytes = fs::read(&source_path).context("read fixture source")?;
            write_new_synced(&destination_path, &bytes)?;
        } else {
            bail!("fixture contains a non-regular entry");
        }
    }
    Ok(())
}

fn source_hash(directory: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_source_files(directory, directory, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    for (relative, bytes) in files {
        digest.update(relative.as_bytes());
        digest.update([0]);
        digest.update(bytes);
        digest.update([0]);
    }
    Ok(hex_digest(digest.finalize()))
}

fn collect_source_files(
    root: &Path,
    current: &Path,
    output: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    let mut entries = fs::read_dir(current)
        .context("read source snapshot directory")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("read source snapshot entry")?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name == ".cargo-home" || name == ".git" {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).context("read source snapshot metadata")?;
        if metadata.file_type().is_symlink() {
            bail!("source snapshot contains a symlink");
        }
        if metadata.is_dir() {
            collect_source_files(root, &path, output)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .context("make source snapshot relative")?
                .to_string_lossy()
                .replace('\\', "/");
            output.push((
                relative,
                fs::read(&path).context("read source snapshot file")?,
            ));
        } else {
            bail!("source snapshot contains a non-regular entry");
        }
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing a hexadecimal digest to a string");
    }
    output
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_artifact_component(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("artifact component contains an unsafe character");
    }
    Ok(())
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(label: &str) -> Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = base.join(format!("agz-rust-coder-{label}-{timestamp}-{nonce}"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("create safe benchmark temp directory"),
            }
        }
        bail!("could not allocate a unique benchmark temp directory")
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn ensure_directory_no_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("benchmark directory must not be a symlink or non-directory")
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("benchmark directory has no parent")?;
            ensure_directory_no_symlink(parent)?;
            fs::create_dir(path).context("create benchmark directory")?;
            Ok(())
        }
        Err(error) => Err(error).context("read benchmark directory metadata"),
    }
}

fn reject_symlink_or_nonfile(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("benchmark evidence lock must be a regular file")
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("read benchmark evidence lock metadata"),
    }
}

fn unique_child_directory(parent: &Path, label: &str) -> Result<PathBuf> {
    for _ in 0..16 {
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = parent.join(format!(".{label}-{timestamp}-{nonce}.tmp"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create benchmark publication temp entry"),
        }
    }
    bail!("could not allocate a unique publication temp entry")
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| "create benchmark evidence file")?;
    file.write_all(bytes)
        .context("write benchmark evidence file")?;
    file.sync_all().context("sync benchmark evidence file")?;
    Ok(())
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec_pretty(value).context("serialize benchmark evidence JSON")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn validate_existing_bundle(path: &Path, run: &[u8], results: &[u8], report: &[u8]) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("read existing benchmark evidence")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("existing benchmark evidence is unsafe");
    }
    for (name, expected) in [
        ("run.json", run),
        ("results.json", results),
        ("report.md", report),
    ] {
        let file = path.join(name);
        let metadata = fs::symlink_metadata(&file).context("read existing benchmark artifact")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || fs::read(&file)? != expected
        {
            bail!("existing benchmark evidence does not match the requested bundle");
        }
    }
    Ok(())
}
