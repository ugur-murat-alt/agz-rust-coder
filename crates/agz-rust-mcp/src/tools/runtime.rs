//! Bounded runtime baseline/candidate comparison for `profile(action=runtime_compare)`.
//!
//! The service measures only typed, operator-authorized adapters on verified
//! server-owned snapshots of one change candidate and its reconstructed
//! baseline. Correctness gates run before any measurement; a fast-but-wrong
//! candidate is rejected by the gate, not by a heuristic. Measurement uses
//! balanced repetitions, warmup runs, raw samples, and a threshold that must
//! be declared in the request before any process starts. Insufficient or noisy
//! samples produce `INCONCLUSIVE`; a single sample can never produce a claim.
//! Model interpretation and measured findings are separate output fields, and
//! no source file is ever written or automatically applied.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    change::{ChangeService, SnapshotError},
    config::{Config, GateCache, RuntimeAdapterConfig, RuntimeWorkloadConfig},
    gate::{GateDetail, GateRequest, GateStatus, ValidationOptions},
    process::{ProcessError, ProcessRunOptions, ProcessRunResult, ProcessSupervisor},
    workspace::RootGuard,
};

use super::{
    CheckService, ProfileBudget,
    profile::{BudgetSnapshot, ConditionValue, hardware_class, summarize_toolchain},
};

/// Structured runtime-comparison schema version emitted by this tool.
pub const RUNTIME_COMPARE_FORMAT_VERSION: u8 = 1;
/// Bounded per-process output retained by the runtime runner.
pub const RUNTIME_PROCESS_OUTPUT_BYTES: usize = 64 * 1024;
/// Maximum hypothesis bytes retained in the result.
pub const MAX_RUNTIME_HYPOTHESIS_BYTES: usize = 4_096;

const RUNTIME_SCRATCH_DIR: &str = "profile-runtime";
const TOOLCHAIN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TOOLCHAIN_BYTES: usize = 16 * 1024;
const NOISE_RATIO: f64 = 0.5;
static RUNTIME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A typed runtime comparison request after protocol validation.
#[derive(Debug, Clone)]
pub struct RuntimeCompareRequest {
    pub change_id: String,
    /// Always 0: only the captured revision can be reconstructed byte-exactly.
    pub baseline_revision: u64,
    pub candidate_revision: u64,
    pub adapter: String,
    pub workload: String,
    pub hypothesis: String,
    /// Acceptance threshold in percent, declared before any measurement.
    pub threshold_percent: f64,
    /// Measured samples per side; `None` uses the configured minimum.
    pub samples: Option<u64>,
    /// Discarded warmup runs per side.
    pub warmup: Option<u64>,
    pub gate_target: crate::gate::GateTargetId,
    pub gate_options: ValidationOptions,
    pub budget: ProfileBudget,
    pub root_epoch: u64,
    pub workspace_root: PathBuf,
}

/// Source/harness identity every runtime result binds to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeBinding {
    pub change_id: String,
    pub baseline_revision: u64,
    pub candidate_revision: u64,
    pub base_identity: String,
    pub patch_hash: String,
    pub manifest_hash: String,
    pub workspace_root: String,
    pub workspace_epoch: u64,
    pub root_epoch: u64,
    pub baseline_source_digest: String,
    pub candidate_source_digest: String,
    /// True only when the two sides have different verified source identities.
    pub source_changed: bool,
    pub changed_files: Vec<String>,
    /// SHA-256 over the exact adapter command and argv.
    pub adapter_digest: String,
}

/// Fixed machine conditions recorded for the experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConditions {
    pub toolchain: ConditionValue,
    pub hardware: ConditionValue,
}

/// Bounded correctness-gate evidence for one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeGateResult {
    pub side: String,
    pub status: String,
    pub passed: bool,
    pub cancelled: bool,
    pub timed_out: bool,
    /// Total gate wall time, including compilation; never workload time.
    pub duration_ms: u64,
    pub commands: Vec<String>,
    pub exit_codes: Vec<i32>,
    pub evidence_id: String,
}

/// Both sides' correctness gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeGates {
    pub baseline: RuntimeGateResult,
    pub candidate: RuntimeGateResult,
}

/// One raw adapter process sample. `spawnError` is set when the process could
/// not be started at all; a failed sample is never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSample {
    /// `baseline` or `candidate`.
    pub side: String,
    /// `warmup` or `measured`.
    pub phase: String,
    /// Global execution index; sample order is visible.
    pub sequence: u64,
    /// `baseline-candidate` or `candidate-baseline`: which side ran first.
    pub order: String,
    pub wall_ms: u64,
    pub exit_code: i32,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output_truncated: bool,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_error: Option<String>,
}

/// Distribution summary for one side. All values come from raw samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSeries {
    pub samples: u64,
    pub median_ms: Option<u64>,
    pub min_ms: Option<u64>,
    pub max_ms: Option<u64>,
    /// Median absolute deviation; `None` when fewer than two samples exist.
    pub mad_ms: Option<u64>,
}

/// A metric the MVP does not measure for real.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnavailableMetric {
    pub name: String,
    pub reason: String,
}

/// Measured workload findings; compile/prepare time is kept out of these series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeMeasurements {
    pub metric: String,
    pub required_samples: u64,
    pub baseline: RuntimeSeries,
    pub candidate: RuntimeSeries,
    pub raw_samples: Vec<RuntimeSample>,
    pub uncertainty_ms: Option<u64>,
    pub relative_spread: Option<f64>,
    pub noisy: bool,
    pub unavailable_metrics: Vec<UnavailableMetric>,
}

/// The declared experiment contract, captured before measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeExperiment {
    pub adapter: String,
    pub workload: String,
    /// Exact argv as executed, including the executable.
    pub command: Vec<String>,
    pub prepare_command: Vec<String>,
    pub threshold_percent: f64,
    pub threshold_declared_before_measurement: bool,
    pub samples_per_side: u64,
    pub warmup_per_side: u64,
    pub order_policy: String,
    pub compile_time_separated: bool,
    /// Per-side prepare/compile wall time, excluded from workload series.
    pub prepare_ms: BTreeMap<String, u64>,
    pub budget: BudgetSnapshot,
}

/// Model-supplied hypothesis and the tool's separated interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInterpretation {
    pub hypothesis: String,
    /// Qualitative statements kept strictly separate from measured findings.
    pub statements: Vec<String>,
    pub caveats: Vec<String>,
}

/// One complete runtime comparison result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeComparison {
    pub format_version: u8,
    /// COMPARABLE, INCONCLUSIVE, INCOMPARABLE, REJECTED, CANCELLED, TIMEOUT,
    /// BUDGET_EXHAUSTED, RESOURCE_BLOCKED, or UNAVAILABLE.
    pub status: String,
    /// Only on a clean comparison: CANDIDATE_FASTER, BASELINE_FASTER, or
    /// NO_MATERIAL_DIFFERENCE.
    pub verdict: Option<String>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<RuntimeBinding>,
    pub conditions: RuntimeConditions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correctness_gate: Option<RuntimeGates>,
    pub experiment: RuntimeExperiment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurements: Option<RuntimeMeasurements>,
    /// Facts derived only from gates, raw samples, and hashes.
    pub measured_findings: Vec<String>,
    pub interpretation: RuntimeInterpretation,
    pub candidate_exportable: bool,
    pub warnings: Vec<String>,
}

/// Bounded runtime-comparison service.
#[derive(Debug)]
pub struct RuntimeCompareService {
    config: Config,
    supervisor: ProcessSupervisor,
    cargo: PathBuf,
}

impl RuntimeCompareService {
    pub fn new(config: Config, supervisor: ProcessSupervisor) -> Self {
        let cargo = super::check::resolve_cargo(config.cargo.path.as_deref());
        Self {
            config,
            supervisor,
            cargo,
        }
    }

    /// Effective budget: caller values are clamped by the server configuration.
    pub fn effective_budget(&self, requested: &ProfileBudget) -> ProfileBudget {
        ProfileBudget {
            max_runs: requested.max_runs.min(self.config.profile.max_runs).max(1),
            max_report_bytes: requested
                .max_report_bytes
                .min(self.config.profile.max_report_bytes)
                .max(1_024),
            wall_time_ms: requested
                .wall_time_ms
                .min(self.config.gate.hard_timeout_ms)
                .max(1),
        }
    }

    fn adapter(&self, name: &str) -> Option<&RuntimeAdapterConfig> {
        self.config
            .profile
            .runtime_adapters
            .iter()
            .find(|adapter| adapter.name == name)
    }

    /// Run one bounded runtime comparison against server-owned snapshots of
    /// `request.change_id`.
    #[allow(clippy::too_many_lines)]
    pub async fn compare(
        &self,
        request: &RuntimeCompareRequest,
        change: &ChangeService,
        started_at: Instant,
        cancellation: &CancellationToken,
    ) -> RuntimeComparison {
        let budget = self.effective_budget(&request.budget);
        let deadline = started_at + Duration::from_millis(budget.wall_time_ms);
        let conditions = RuntimeConditions {
            toolchain: ConditionValue::unavailable("not probed"),
            hardware: hardware_class(),
        };
        let Some(adapter) = self.adapter(&request.adapter) else {
            return RuntimeComparison::terminal(
                request,
                self.experiment(request, None, &budget, Vec::new()),
                conditions,
                "INCONCLUSIVE",
                format!(
                    "no operator-authorized runtime adapter named {:?} is configured; runtime \
                     adapters are declared in [profile.runtime]",
                    request.adapter
                ),
            );
        };
        let Some(workload) = adapter
            .workloads
            .iter()
            .find(|workload| workload.name == request.workload)
        else {
            return RuntimeComparison::terminal(
                request,
                self.experiment(request, Some(adapter), &budget, Vec::new()),
                conditions,
                "INCONCLUSIVE",
                format!(
                    "adapter {:?} has no authorized workload named {:?}",
                    adapter.name, request.workload
                ),
            );
        };
        let argv = adapter
            .args
            .iter()
            .chain(workload.args.iter())
            .cloned()
            .collect::<Vec<_>>();
        let experiment = self.experiment(request, Some(adapter), &budget, argv.clone());
        if cancellation.is_cancelled() {
            return RuntimeComparison::terminal(
                request,
                experiment,
                conditions,
                "CANCELLED",
                "the runtime comparison was cancelled before it started".to_owned(),
            );
        }
        if Instant::now() >= deadline {
            return RuntimeComparison::terminal(
                request,
                experiment,
                conditions,
                "BUDGET_EXHAUSTED",
                "the wall-time budget was exhausted before snapshot materialization".to_owned(),
            );
        }
        let scratch = self
            .config
            .gate
            .lease_dir
            .join(RUNTIME_SCRATCH_DIR)
            .join(unique_scratch_name());
        let mut comparison = self
            .execute(
                request,
                adapter,
                workload,
                &argv,
                budget,
                deadline,
                &scratch,
                change,
                cancellation,
            )
            .await;
        match fs::remove_dir_all(&scratch) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => comparison.warnings.push(format!(
                "runtime scratch {} could not be fully removed: {error}",
                scratch.display()
            )),
        }
        comparison
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute(
        &self,
        request: &RuntimeCompareRequest,
        adapter: &RuntimeAdapterConfig,
        workload: &RuntimeWorkloadConfig,
        argv: &[String],
        budget: ProfileBudget,
        deadline: Instant,
        scratch: &Path,
        change: &ChangeService,
        cancellation: &CancellationToken,
    ) -> RuntimeComparison {
        let mut warnings = Vec::new();
        let conditions = RuntimeConditions {
            toolchain: ConditionValue::unavailable("not probed"),
            hardware: hardware_class(),
        };
        let experiment = self.experiment(request, Some(adapter), &budget, argv.to_vec());
        let pair = match change
            .materialize_runtime_snapshots(
                &request.change_id,
                request.baseline_revision,
                request.candidate_revision,
                scratch,
                cancellation,
            )
            .await
        {
            Ok(pair) => pair,
            Err(error) => {
                let (status, reason) = snapshot_failure(&error);
                return RuntimeComparison::terminal(
                    request, experiment, conditions, status, reason,
                );
            }
        };
        if pair.workspace_root != request.workspace_root
            || pair.workspace_epoch != request.root_epoch
        {
            return RuntimeComparison::terminal(
                request,
                experiment,
                conditions,
                "INCOMPARABLE",
                format!(
                    "the change belongs to workspace {} epoch {} instead of {} epoch {}",
                    pair.workspace_root.display(),
                    pair.workspace_epoch,
                    request.workspace_root.display(),
                    request.root_epoch
                ),
            );
        }
        let binding = RuntimeBinding {
            change_id: pair.change_id.clone(),
            baseline_revision: pair.baseline_revision,
            candidate_revision: pair.candidate_revision,
            base_identity: pair.base_identity.clone(),
            patch_hash: pair.patch_hash.clone(),
            manifest_hash: pair.manifest_hash.clone(),
            workspace_root: pair.workspace_root.display().to_string(),
            workspace_epoch: pair.workspace_epoch,
            root_epoch: request.root_epoch,
            baseline_source_digest: pair.baseline_source_digest.clone(),
            candidate_source_digest: pair.candidate_source_digest.clone(),
            source_changed: pair.baseline_source_digest != pair.candidate_source_digest,
            changed_files: pair.changed_files.clone(),
            adapter_digest: adapter_digest(adapter, workload, argv),
        };
        if !pair.excluded.is_empty() {
            warnings.push(format!(
                "capture-excluded directories were not measured: {}",
                pair.excluded.join(", ")
            ));
        }
        if !binding.source_changed {
            return RuntimeComparison::terminal(
                request,
                experiment,
                conditions,
                "INCOMPARABLE",
                "baseline and candidate source digests are identical; there is no staged change \
                 to compare"
                    .to_owned(),
            )
            .with_binding(binding)
            .with_warnings(warnings.clone());
        }
        let toolchain = self.probe_toolchain(deadline, cancellation.clone()).await;
        let conditions = RuntimeConditions {
            toolchain,
            hardware: hardware_class(),
        };
        if !conditions.toolchain.available {
            let reason = format!(
                "the toolchain condition could not be bound: {}",
                conditions.toolchain.reason
            );
            return RuntimeComparison::terminal(
                request,
                experiment,
                conditions,
                "INCONCLUSIVE",
                reason,
            )
            .with_binding(binding)
            .with_warnings(warnings.clone());
        }

        // Correctness gates run first on both verified snapshots.
        let baseline_gate = self
            .run_gate(
                &pair.baseline_root,
                &pair.dependency_roots,
                request,
                &scratch.join("gate-baseline"),
                deadline,
                cancellation,
            )
            .await;
        let candidate_gate = self
            .run_gate(
                &pair.candidate_root,
                &pair.dependency_roots,
                request,
                &scratch.join("gate-candidate"),
                deadline,
                cancellation,
            )
            .await;
        let gates = RuntimeGates {
            baseline: baseline_gate,
            candidate: candidate_gate,
        };
        let mut comparison = RuntimeComparison::terminal(
            request,
            experiment,
            conditions,
            "INCONCLUSIVE",
            "comparison did not run".to_owned(),
        )
        .with_binding(binding)
        .with_gates(gates.clone())
        .with_warnings(warnings);
        if cancellation.is_cancelled() {
            comparison.status = "CANCELLED".to_owned();
            comparison.reason = "the runtime comparison was cancelled".to_owned();
            return comparison;
        }
        if !gates.baseline.passed {
            comparison.reason = format!(
                "the baseline correctness gate finished as {}; a broken baseline cannot be a \
                 reference",
                gates.baseline.status
            );
            comparison.measured_findings = vec![format!(
                "baseline correctness gate status {}; candidate gate status {}",
                gates.baseline.status, gates.candidate.status
            )];
            return comparison;
        }
        if !gates.candidate.passed {
            comparison.status = "REJECTED".to_owned();
            comparison.reason = format!(
                "the candidate correctness gate finished as {}; a fast-but-wrong or work-skipping \
                 candidate is rejected before measurement",
                gates.candidate.status
            );
            comparison.measured_findings = vec![
                format!("baseline correctness gate status {}", gates.baseline.status),
                format!(
                    "candidate correctness gate status {}; no workload sample was taken",
                    gates.candidate.status
                ),
            ];
            comparison.interpretation.statements.push(
                "The candidate is not a valid optimization for this workload because it fails the \
                 declared correctness gate."
                    .to_owned(),
            );
            return comparison;
        }

        // Prepare/compile runs are measured separately and excluded from the
        // workload series.
        let mut prepare_ms = BTreeMap::new();
        if !adapter.prepare_args.is_empty() {
            for (side, root) in [
                ("baseline", pair.baseline_root.clone()),
                ("candidate", pair.candidate_root.clone()),
            ] {
                if cancellation.is_cancelled() {
                    comparison.status = "CANCELLED".to_owned();
                    comparison.reason = "the runtime comparison was cancelled".to_owned();
                    return comparison;
                }
                if Instant::now() >= deadline {
                    comparison.status = "BUDGET_EXHAUSTED".to_owned();
                    comparison.reason =
                        "the wall-time budget was exhausted before the prepare command".to_owned();
                    return comparison;
                }
                let cache = scratch.join(format!("gate-{side}"));
                match self
                    .run_adapter(
                        &adapter.command,
                        &adapter.prepare_args,
                        &root,
                        &cache,
                        deadline,
                        cancellation,
                        false,
                    )
                    .await
                {
                    Ok(result)
                        if result.exit_code == 0 && !result.timed_out && !result.cancelled =>
                    {
                        prepare_ms.insert(side.to_owned(), result.duration_ms);
                    }
                    Ok(result) => {
                        comparison.status = "INCONCLUSIVE".to_owned();
                        comparison.reason = format!(
                            "the {side} prepare command exited {} (timed_out={} cancelled={}); \
                             workload measurement is not possible",
                            result.exit_code, result.timed_out, result.cancelled
                        );
                        return comparison;
                    }
                    Err(error) => {
                        comparison.status = "UNAVAILABLE".to_owned();
                        comparison.reason = format!("the {side} prepare command failed: {error}");
                        return comparison;
                    }
                }
            }
        }
        comparison.experiment.prepare_ms = prepare_ms;

        let samples_per_side = request
            .samples
            .unwrap_or(self.config.profile.runtime_min_samples)
            .clamp(
                self.config.profile.runtime_min_samples,
                self.config.profile.runtime_max_samples,
            );
        let warmup = request
            .warmup
            .unwrap_or(0)
            .min(self.config.profile.runtime_max_warmup);
        let mut raw_samples = Vec::new();
        let mut sequence = 0_u64;
        let mut budget_exhausted = false;
        'pairs: for index in 0..warmup.saturating_add(samples_per_side) {
            let phase = if index < warmup { "warmup" } else { "measured" };
            let baseline_first = index % 2 == 0;
            let order = if baseline_first {
                "baseline-candidate"
            } else {
                "candidate-baseline"
            };
            let sides: [(&str, &PathBuf); 2] = if baseline_first {
                [
                    ("baseline", &pair.baseline_root),
                    ("candidate", &pair.candidate_root),
                ]
            } else {
                [
                    ("candidate", &pair.candidate_root),
                    ("baseline", &pair.baseline_root),
                ]
            };
            for (side, root) in sides {
                if cancellation.is_cancelled() {
                    comparison.status = "CANCELLED".to_owned();
                    comparison.reason =
                        "the runtime comparison was cancelled during measurement".to_owned();
                    comparison.measurements =
                        Some(partial_measurements(samples_per_side, &raw_samples));
                    return comparison;
                }
                if Instant::now() >= deadline {
                    budget_exhausted = true;
                    comparison.warnings.push(format!(
                        "wall-time budget was exhausted before all {phase} samples completed"
                    ));
                    break 'pairs;
                }
                sequence = sequence.saturating_add(1);
                let cache = scratch.join(format!("gate-{side}"));
                let sample = match self
                    .run_adapter(
                        &adapter.command,
                        argv,
                        root,
                        &cache,
                        deadline,
                        cancellation,
                        true,
                    )
                    .await
                {
                    Ok(result) => RuntimeSample {
                        side: side.to_owned(),
                        phase: phase.to_owned(),
                        sequence,
                        order: order.to_owned(),
                        wall_ms: result.duration_ms,
                        exit_code: result.exit_code,
                        timed_out: result.timed_out,
                        cancelled: result.cancelled,
                        output_truncated: result.output_truncated,
                        stdout_sha256: sha256_hex(result.stdout.as_bytes()),
                        stderr_sha256: sha256_hex(result.stderr.as_bytes()),
                        spawn_error: None,
                    },
                    Err(error) => {
                        comparison
                            .warnings
                            .push(format!("{side} {phase} sample could not start: {error}"));
                        RuntimeSample {
                            side: side.to_owned(),
                            phase: phase.to_owned(),
                            sequence,
                            order: order.to_owned(),
                            wall_ms: 0,
                            exit_code: -1,
                            timed_out: false,
                            cancelled: false,
                            output_truncated: false,
                            stdout_sha256: sha256_hex(&[]),
                            stderr_sha256: sha256_hex(&[]),
                            spawn_error: Some(error.to_string()),
                        }
                    }
                };
                raw_samples.push(sample);
            }
        }
        if cancellation.is_cancelled() {
            comparison.status = "CANCELLED".to_owned();
            comparison.reason =
                "the runtime comparison was cancelled during measurement".to_owned();
            comparison.measurements = Some(partial_measurements(samples_per_side, &raw_samples));
            return comparison;
        }
        let base_values = measured_values(&raw_samples, "baseline");
        let cand_values = measured_values(&raw_samples, "candidate");
        let series_baseline = series(&base_values);
        let series_candidate = series(&cand_values);
        let relative_spread = max_relative_spread(&series_baseline, &series_candidate);
        let noisy = relative_spread.is_some_and(|spread| spread > NOISE_RATIO);
        let uncertainty_ms = series_baseline
            .mad_ms
            .zip(series_candidate.mad_ms)
            .map(|(base, cand)| base.max(cand));
        let measurements = RuntimeMeasurements {
            metric: "wall_duration_ms".to_owned(),
            required_samples: samples_per_side,
            baseline: series_baseline.clone(),
            candidate: series_candidate.clone(),
            raw_samples: raw_samples.clone(),
            uncertainty_ms,
            relative_spread,
            noisy,
            unavailable_metrics: unavailable_metrics(),
        };
        comparison.measurements = Some(measurements);
        comparison.measured_findings = findings(
            &series_baseline,
            &series_candidate,
            samples_per_side,
            request.threshold_percent,
            &gates,
            &comparison.experiment.prepare_ms,
        );
        if budget_exhausted {
            comparison.status = "INCONCLUSIVE".to_owned();
            comparison.reason = format!(
                "the wall-time budget was exhausted before the required {samples_per_side} \
                 samples per side"
            );
            return comparison;
        }
        if (base_values.len() as u64) < samples_per_side
            || (cand_values.len() as u64) < samples_per_side
        {
            comparison.status = "INCONCLUSIVE".to_owned();
            comparison.reason = format!(
                "insufficient clean samples (baseline {}, candidate {}; {samples_per_side} \
                 required per side)",
                base_values.len(),
                cand_values.len()
            );
            return comparison;
        }
        if samples_per_side < 2 {
            comparison.status = "INCONCLUSIVE".to_owned();
            comparison.reason =
                "at least two samples per side are required; a single run can never show a win"
                    .to_owned();
            return comparison;
        }
        if noisy {
            comparison.status = "INCONCLUSIVE".to_owned();
            comparison.reason = format!(
                "samples are too noisy to compare (relative spread above {:.0}%)",
                NOISE_RATIO * 100.0
            );
            return comparison;
        }
        let (Some(base_median), Some(cand_median)) =
            (series_baseline.median_ms, series_candidate.median_ms)
        else {
            comparison.status = "INCONCLUSIVE".to_owned();
            comparison.reason = "a median sample could not be computed on both sides".to_owned();
            return comparison;
        };
        if base_median == 0 {
            comparison.status = "INCONCLUSIVE".to_owned();
            comparison.reason = "baseline median is zero; no ratio is measurable".to_owned();
            return comparison;
        }
        let (verdict, reason) = classify_runtime_verdict(
            base_median,
            cand_median,
            request.threshold_percent,
            samples_per_side,
        );
        comparison.verdict = Some(verdict.to_owned());
        comparison.reason = reason;
        comparison.status = "COMPARABLE".to_owned();
        comparison.candidate_exportable = true;
        comparison.measured_findings.push(format!(
            "verdict {verdict}; candidate export/apply is not performed by this tool"
        ));
        comparison.interpretation.statements.extend([
            "Measured findings above are limited to this workload, harness, input, and machine."
                .to_owned(),
            "The declared hypothesis is neither generally confirmed nor refuted by one workload."
                .to_owned(),
            "Apply, if ever desired, requires the separate change-apply capability; this tool \
             never writes workspace source."
                .to_owned(),
        ]);
        if verdict == "BASELINE_FASTER" {
            comparison.candidate_exportable = false;
            comparison.warnings.push(
                "the candidate regressed on the measured workload; exporting it is not recommended"
                    .to_owned(),
            );
        }
        comparison
    }

    fn experiment(
        &self,
        request: &RuntimeCompareRequest,
        adapter: Option<&RuntimeAdapterConfig>,
        budget: &ProfileBudget,
        argv: Vec<String>,
    ) -> RuntimeExperiment {
        let samples_per_side = request
            .samples
            .unwrap_or(self.config.profile.runtime_min_samples)
            .clamp(
                self.config.profile.runtime_min_samples,
                self.config.profile.runtime_max_samples,
            );
        let warmup = request
            .warmup
            .unwrap_or(0)
            .min(self.config.profile.runtime_max_warmup);
        let mut command = Vec::new();
        let mut prepare_command = Vec::new();
        if let Some(adapter) = adapter {
            command.push(adapter.command.display().to_string());
            command.extend(argv);
            prepare_command.push(adapter.command.display().to_string());
            prepare_command.extend(adapter.prepare_args.iter().cloned());
        }
        RuntimeExperiment {
            adapter: request.adapter.clone(),
            workload: request.workload.clone(),
            command,
            prepare_command,
            threshold_percent: request.threshold_percent,
            threshold_declared_before_measurement: request.threshold_percent > 0.0,
            samples_per_side,
            warmup_per_side: warmup,
            order_policy: "alternating baseline-candidate / candidate-baseline pairs".to_owned(),
            compile_time_separated: true,
            prepare_ms: BTreeMap::new(),
            budget: BudgetSnapshot {
                max_runs: budget.max_runs,
                max_report_bytes: budget.max_report_bytes,
                wall_time_ms: budget.wall_time_ms,
            },
        }
    }

    async fn run_gate(
        &self,
        root: &Path,
        dependency_roots: &[PathBuf],
        request: &RuntimeCompareRequest,
        cache_dir: &Path,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> RuntimeGateResult {
        let side = if root.ends_with("baseline") {
            "baseline"
        } else {
            "candidate"
        };
        let failed = |status: &str, reason: &str| RuntimeGateResult {
            side: side.to_owned(),
            status: status.to_owned(),
            passed: false,
            cancelled: status == "CANCELLED",
            timed_out: status == "TIMEOUT",
            duration_ms: 0,
            commands: Vec::new(),
            exit_codes: Vec::new(),
            evidence_id: reason.to_owned(),
        };
        if cancellation.is_cancelled() {
            return failed("CANCELLED", "gate was not started");
        }
        if Instant::now() >= deadline {
            return failed("BUDGET_EXHAUSTED", "gate was not started");
        }
        let guard = match RootGuard::new([root.to_owned()], dependency_roots.to_vec()) {
            Ok(guard) => Arc::new(guard),
            Err(reason) => return failed("UNAVAILABLE", &format!("root guard failed: {reason}")),
        };
        let mut config = self.config.clone();
        config.gate.cache = GateCache::Isolated;
        config.gate.cache_dir = cache_dir.to_owned();
        let service = CheckService::new(config, Arc::clone(&guard));
        let gate_request = GateRequest::new(root.to_owned(), request.gate_target)
            .with_options(request.gate_options.clone())
            .with_detail(GateDetail::Compact)
            .with_root_epoch(0);
        let evidence = service
            .run(gate_request, None, Some(cancellation.clone()))
            .await;
        service.close().await;
        RuntimeGateResult {
            side: side.to_owned(),
            status: evidence.status.as_str().to_owned(),
            passed: matches!(evidence.status, GateStatus::FastPass | GateStatus::FullPass),
            cancelled: evidence.status == GateStatus::Cancelled,
            timed_out: evidence.status == GateStatus::Timeout,
            duration_ms: evidence.response_ms,
            commands: evidence
                .steps
                .iter()
                .map(|step| step.command.clone())
                .collect(),
            exit_codes: evidence.steps.iter().map(|step| step.exit_code).collect(),
            evidence_id: evidence.job_id,
        }
    }

    async fn probe_toolchain(
        &self,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> ConditionValue {
        let options = ProcessRunOptions::new(self.config.gate.lease_dir.clone())
            .with_timeout(TOOLCHAIN_TIMEOUT)
            .with_deadline(deadline)
            .with_cancellation(cancellation)
            .with_max_output_bytes(MAX_TOOLCHAIN_BYTES);
        match self
            .supervisor
            .run(self.cargo.clone(), ["-Vv"], options)
            .await
        {
            Ok(output)
                if output.exit_code == 0
                    && output.drain_complete
                    && output.cleanup_complete
                    && !output.cancelled
                    && !output.timed_out =>
            {
                ConditionValue {
                    available: true,
                    summary: summarize_toolchain(&output.stdout),
                    reason: String::new(),
                }
            }
            Ok(output) => ConditionValue::unavailable(format!(
                "cargo -Vv exited {} (cancelled={} timed_out={} drain={} cleanup={})",
                output.exit_code,
                output.cancelled,
                output.timed_out,
                output.drain_complete,
                output.cleanup_complete
            )),
            Err(error) => ConditionValue::unavailable(format!("cargo -Vv failed: {error}")),
        }
    }

    /// Run one adapter process with the side's isolated Cargo target directory
    /// so an adapter that happens to invoke Cargo cannot write build artifacts
    /// into the measured snapshot.
    #[allow(clippy::too_many_arguments)]
    async fn run_adapter(
        &self,
        command: &Path,
        argv: &[String],
        cwd: &Path,
        cache_dir: &Path,
        deadline: Instant,
        cancellation: &CancellationToken,
        retain_output: bool,
    ) -> Result<ProcessRunResult, ProcessError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = Duration::from_millis(self.config.profile.runtime_run_timeout_ms)
            .min(remaining)
            .max(Duration::from_millis(1));
        let options = ProcessRunOptions::new(cwd.to_owned())
            .with_timeout(timeout)
            .with_deadline(deadline)
            .with_cancellation(cancellation.clone())
            .with_env("CARGO_TARGET_DIR", cache_dir.to_owned())
            .with_max_output_bytes(if retain_output {
                RUNTIME_PROCESS_OUTPUT_BYTES
            } else {
                16 * 1024
            });
        self.supervisor
            .run(command.to_owned(), argv.to_vec(), options)
            .await
    }
}

impl RuntimeComparison {
    fn terminal(
        request: &RuntimeCompareRequest,
        experiment: RuntimeExperiment,
        conditions: RuntimeConditions,
        status: &str,
        reason: String,
    ) -> Self {
        Self {
            format_version: RUNTIME_COMPARE_FORMAT_VERSION,
            status: status.to_owned(),
            verdict: None,
            reason,
            binding: None,
            conditions,
            correctness_gate: None,
            experiment,
            measurements: None,
            measured_findings: Vec::new(),
            interpretation: RuntimeInterpretation {
                hypothesis: request.hypothesis.clone(),
                statements: Vec::new(),
                caveats: vec![
                    "Runtime comparisons cover only the designated workload, harness, input, \
                     feature set, toolchain, and machine."
                        .to_owned(),
                    "The tool never applies or exports source changes; apply requires the \
                     separate change-apply capability."
                        .to_owned(),
                ],
            },
            candidate_exportable: false,
            warnings: Vec::new(),
        }
    }

    fn with_binding(mut self, binding: RuntimeBinding) -> Self {
        self.binding = Some(binding);
        self
    }

    fn with_gates(mut self, gates: RuntimeGates) -> Self {
        self.correctness_gate = Some(gates);
        self
    }

    fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }
}

fn snapshot_failure(error: &SnapshotError) -> (&'static str, String) {
    match error {
        SnapshotError::NotFound => (
            "INCOMPARABLE",
            "the change id has no server-owned scratch".to_owned(),
        ),
        SnapshotError::Invalid(reason) => ("INCOMPARABLE", reason.clone()),
        SnapshotError::Stale(reason) => ("INCOMPARABLE", reason.clone()),
        SnapshotError::Incomparable(reason) => ("INCOMPARABLE", reason.clone()),
        SnapshotError::Cancelled => ("CANCELLED", error.reason().to_owned()),
        SnapshotError::Unavailable(reason) => ("UNAVAILABLE", reason.clone()),
    }
}

fn unique_scratch_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!(
        "pr-{}-{}-{nanos}",
        std::process::id(),
        RUNTIME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn adapter_digest(
    adapter: &RuntimeAdapterConfig,
    workload: &RuntimeWorkloadConfig,
    argv: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-mcp-runtime-adapter-v1\0");
    hasher.update(adapter.name.as_bytes());
    hasher.update([0]);
    hasher.update(adapter.command.to_string_lossy().as_bytes());
    hasher.update([0]);
    for argument in argv {
        hasher.update(argument.as_bytes());
        hasher.update([0]);
    }
    hasher.update(workload.name.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn measured_values(samples: &[RuntimeSample], side: &str) -> Vec<u64> {
    samples
        .iter()
        .filter(|sample| sample.phase == "measured" && sample.side == side)
        .filter(|sample| {
            sample.spawn_error.is_none()
                && sample.exit_code == 0
                && !sample.timed_out
                && !sample.cancelled
        })
        .map(|sample| sample.wall_ms)
        .collect()
}

fn partial_measurements(required: u64, raw: &[RuntimeSample]) -> RuntimeMeasurements {
    let baseline = series(&measured_values(raw, "baseline"));
    let candidate = series(&measured_values(raw, "candidate"));
    let relative_spread = max_relative_spread(&baseline, &candidate);
    RuntimeMeasurements {
        metric: "wall_duration_ms".to_owned(),
        required_samples: required,
        baseline: baseline.clone(),
        candidate: candidate.clone(),
        raw_samples: raw.to_vec(),
        uncertainty_ms: baseline
            .mad_ms
            .zip(candidate.mad_ms)
            .map(|(base, cand)| base.max(cand)),
        relative_spread,
        noisy: relative_spread.is_some_and(|spread| spread > NOISE_RATIO),
        unavailable_metrics: unavailable_metrics(),
    }
}

fn series(values: &[u64]) -> RuntimeSeries {
    RuntimeSeries {
        samples: values.len() as u64,
        median_ms: median(values),
        min_ms: values.iter().copied().min(),
        max_ms: values.iter().copied().max(),
        mad_ms: mad(values),
    }
}

fn median(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
}

fn mad(values: &[u64]) -> Option<u64> {
    let median_value = median(values)?;
    if values.len() < 2 {
        return None;
    }
    let deviations = values
        .iter()
        .map(|value| value.abs_diff(median_value))
        .collect::<Vec<_>>();
    median(&deviations)
}

fn max_relative_spread(baseline: &RuntimeSeries, candidate: &RuntimeSeries) -> Option<f64> {
    [baseline, candidate]
        .into_iter()
        .filter(|series| series.samples >= 2)
        .filter_map(|series| {
            let median = series.median_ms?;
            if median == 0 {
                return None;
            }
            let min = series.min_ms?;
            let max = series.max_ms?;
            Some((max.saturating_sub(min) as f64) / (median as f64))
        })
        .reduce(f64::max)
}

fn unavailable_metrics() -> Vec<UnavailableMetric> {
    [
        "allocations",
        "peakRssBytes",
        "hardwareCounters",
        "throughput",
    ]
    .into_iter()
    .map(|name| UnavailableMetric {
        name: name.to_owned(),
        reason: "no configured adapter measures this metric for real; unknown metrics are never \
                 reported as zero"
            .to_owned(),
    })
    .collect()
}

/// Classify median durations against the predeclared threshold.
///
/// Extracted so the exact boundary behavior (faster at `>=`, slower at `<=`)
/// is unit-tested with crafted measurements instead of depending on wall-clock
/// noise from a real fixture run.
fn classify_runtime_verdict(
    baseline_median_ms: u64,
    candidate_median_ms: u64,
    threshold_percent: f64,
    samples_per_side: u64,
) -> (&'static str, String) {
    let delta = candidate_median_ms as f64 - baseline_median_ms as f64;
    let improvement_percent = -delta / baseline_median_ms as f64 * 100.0;
    if improvement_percent >= threshold_percent {
        (
            "CANDIDATE_FASTER",
            format!(
                "candidate median improved {improvement_percent:+.1}% against the declared \
                 {threshold_percent:.1}% threshold across {samples_per_side} samples per side"
            ),
        )
    } else if improvement_percent <= -threshold_percent {
        (
            "BASELINE_FASTER",
            format!(
                "candidate median regressed {improvement_percent:+.1}% against the declared \
                 {threshold_percent:.1}% threshold across {samples_per_side} samples per side"
            ),
        )
    } else {
        (
            "NO_MATERIAL_DIFFERENCE",
            format!(
                "candidate median changed {improvement_percent:+.1}%, within the declared \
                 {threshold_percent:.1}% threshold"
            ),
        )
    }
}

fn findings(
    baseline: &RuntimeSeries,
    candidate: &RuntimeSeries,
    required: u64,
    threshold_percent: f64,
    gates: &RuntimeGates,
    prepare_ms: &BTreeMap<String, u64>,
) -> Vec<String> {
    let mut findings = vec![
        format!(
            "baseline correctness gate {} and candidate correctness gate {}",
            gates.baseline.status, gates.candidate.status
        ),
        format!(
            "baseline median {} ms across {} samples (min {}, max {}, mad {}); candidate median \
             {} ms across {} samples (min {}, max {}, mad {})",
            display(baseline.median_ms),
            baseline.samples,
            display(baseline.min_ms),
            display(baseline.max_ms),
            display(baseline.mad_ms),
            display(candidate.median_ms),
            candidate.samples,
            display(candidate.min_ms),
            display(candidate.max_ms),
            display(candidate.mad_ms),
        ),
        format!(
            "acceptance threshold {threshold_percent:.1}% was declared before measurement; \
             {required} samples per side were required"
        ),
    ];
    for (side, value) in prepare_ms {
        findings.push(format!(
            "{side} prepare/compile time {value} ms was excluded from workload samples"
        ));
    }
    if baseline.samples > 0 && candidate.samples > 0 {
        findings.push(
            "no allocation, peak-RSS, hardware-counter, or throughput metric is measured by the \
             configured adapter; unknown metrics stay unavailable"
                .to_owned(),
        );
    }
    findings
}

fn display(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_reports_median_bounds_and_spread() {
        let values = [10, 12, 11, 30];
        let summary = series(&values);
        assert_eq!(summary.samples, 4);
        assert_eq!(summary.median_ms, Some(12));
        assert_eq!(summary.min_ms, Some(10));
        assert_eq!(summary.max_ms, Some(30));
        assert_eq!(summary.mad_ms, Some(2));
        let spread = max_relative_spread(&summary, &summary).expect("spread");
        assert!(spread > NOISE_RATIO);
    }

    #[test]
    fn a_single_sample_cannot_produce_a_series_claim() {
        let one = series(&[7]);
        assert_eq!(one.samples, 1);
        assert_eq!(one.mad_ms, None);
        assert!(max_relative_spread(&one, &one).is_none());
    }

    #[test]
    fn verdict_classification_pins_threshold_boundaries() {
        // Equal medians can never move the verdict.
        let (verdict, _) = classify_runtime_verdict(2_000, 2_000, 25.0, 5);
        assert_eq!(verdict, "NO_MATERIAL_DIFFERENCE");

        // Exactly at the declared threshold in either direction is material.
        assert_eq!(
            classify_runtime_verdict(2_000, 1_500, 25.0, 5).0,
            "CANDIDATE_FASTER"
        );
        assert_eq!(
            classify_runtime_verdict(2_000, 2_500, 25.0, 5).0,
            "BASELINE_FASTER"
        );

        // Just inside the threshold stays neutral.
        assert_eq!(
            classify_runtime_verdict(2_000, 1_510, 25.0, 5).0,
            "NO_MATERIAL_DIFFERENCE"
        );
        assert_eq!(
            classify_runtime_verdict(2_000, 2_490, 25.0, 5).0,
            "NO_MATERIAL_DIFFERENCE"
        );

        // The boundary follows the declared threshold, not a fixed percentage.
        assert_eq!(
            classify_runtime_verdict(2_000, 1_810, 10.0, 5).0,
            "NO_MATERIAL_DIFFERENCE"
        );
        assert_eq!(
            classify_runtime_verdict(2_000, 1_800, 10.0, 5).0,
            "CANDIDATE_FASTER"
        );
    }
}
