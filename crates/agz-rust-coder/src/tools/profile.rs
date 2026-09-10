//! Bounded build-profile analysis over the authoritative check service.
//!
//! The service never invents timing or cache causality. It reports Cargo's
//! observed artifact/fresh telemetry, separates measured phases, stores the
//! bounded `--timings` HTML artifact as opaque evidence, and marks every
//! explanation as observed, reasoned hypothesis, or unknown.
//!
//! The stable Cargo `--timings` HTML report is treated as a version-bound,
//! fail-closed artifact: extraction of its embedded unit data is attempted only
//! with an exact shape, and any deviation produces a typed `unavailable`
//! result instead of guessed numbers.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    gate::{
        GateDetail, GateEvidence, GateRequest, GateSource, GateStatus, GateTargetId,
        ValidationOptions,
    },
    process::{ProcessRunOptions, ProcessSupervisor},
    workspace::{AuthorizedRoot, ClientRoots},
};

use super::CheckService;

/// Structured evidence schema version emitted by this tool.
pub const PROFILE_FORMAT_VERSION: u8 = 1;
/// Maximum number of retained in-memory evidence records.
pub const MAX_RETAINED_EVIDENCE: usize = 64;
/// Maximum number of units accepted from a timing report before failing closed.
pub const MAX_TIMING_UNITS: usize = 4_096;
/// Binding for the extracted embedded unit-data shape.
pub const TIMING_UNIT_FORMAT: &str = "cargo-timings-html/unit-data-v1";

const MAX_CRITICAL_PATH_DEPTH: usize = 64;
const MAX_EXPENSIVE_UNITS: usize = 10;
const MAX_PATH_REFS: usize = 32;
const MAX_TOOLCHAIN_BYTES: usize = 16 * 1024;
const TOOLCHAIN_TIMEOUT: Duration = Duration::from_secs(10);
const NOISE_RATIO: f64 = 0.5;
const MATERIALITY_RATIO: f64 = 0.05;

/// Effective run/report/wall budget for one profile invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileBudget {
    pub max_runs: u64,
    pub max_report_bytes: u64,
    pub wall_time_ms: u64,
}

/// One measured phase. `observed=false` means the value was not measured and
/// `ms` is `None`; zero is never reported for an unmeasured phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePhase {
    pub name: String,
    pub ms: Option<u64>,
    pub observed: bool,
    pub source: String,
}

/// Cargo-observed rebuild telemetry. When `available=false`, numeric fields are
/// absent and must not be read as zero cache hits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRebuildReport {
    pub available: bool,
    pub reason: String,
    pub total_units: Option<u64>,
    pub fresh_units: Option<u64>,
    pub rebuilt_units: Option<u64>,
    pub build_scripts: Option<u64>,
    pub linked_units: Option<u64>,
    pub partial: bool,
    pub rebuilt_packages: Vec<String>,
    pub build_script_packages: Vec<String>,
    pub packages_truncated: bool,
}

/// Observed / reasoned-hypothesis / unknown classification for one statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileExplanation {
    /// `observed`, `reasonedHypothesis`, or `unknown`.
    pub class: String,
    pub claim: String,
    pub evidence: Vec<String>,
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CriticalPathUnit {
    pub name: String,
    pub version: String,
    pub target: String,
    pub duration_secs: f64,
    pub start_secs: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CriticalPath {
    pub available: bool,
    pub reason: String,
    pub method: String,
    pub path_duration_secs: Option<f64>,
    pub units: Vec<CriticalPathUnit>,
    pub caveat: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimingsReport {
    pub available: bool,
    pub reason: String,
    /// Server-owned path relative to `root`; never caller-controlled.
    pub relative_path: Option<String>,
    pub root: Option<String>,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    pub units_extracted: Option<u64>,
    /// `parsed` or `unavailable`; never a guessed value.
    pub extraction: String,
    pub format: String,
    pub expensive_units: Vec<CriticalPathUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConditionValue {
    pub available: bool,
    pub summary: String,
    pub reason: String,
}

impl ConditionValue {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            summary: "unavailable".to_owned(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConditionSnapshot {
    pub toolchain: ConditionValue,
    pub hardware: ConditionValue,
    /// `warm`, `cold`, `mixed`, or `unknown`; derived from Cargo fresh counts.
    pub cache_state: String,
    pub input_hash: String,
    pub command_hash: String,
    pub environment_hash: String,
    pub change_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationSnapshot {
    pub target: String,
    pub options: ValidationOptions,
    pub cargo: String,
    pub cache_mode: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetSnapshot {
    pub max_runs: u64,
    pub max_report_bytes: u64,
    pub wall_time_ms: u64,
}

/// One complete build-analysis evidence record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRecord {
    pub format_version: u8,
    pub evidence_id: String,
    pub action: String,
    /// COMPLETE, UNAVAILABLE, CANCELLED, STALE, TIMEOUT, RESOURCE_BLOCKED,
    /// BUDGET_EXHAUSTED, INCONCLUSIVE, or SUPERSEDED.
    pub status: String,
    pub gate_status: String,
    pub reason: String,
    pub change_id: Option<String>,
    pub configuration: ConfigurationSnapshot,
    pub conditions: ConditionSnapshot,
    pub phases: Vec<ProfilePhase>,
    pub rebuild: ProfileRebuildReport,
    pub explanations: Vec<ProfileExplanation>,
    pub critical_path: CriticalPath,
    pub timings_report: TimingsReport,
    pub wall_ms: u64,
    pub budget: BudgetSnapshot,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComparisonSide {
    pub samples: u64,
    pub evidence_ids: Vec<String>,
    pub wall_median_ms: Option<u64>,
    pub wall_min_ms: Option<u64>,
    pub wall_max_ms: Option<u64>,
    pub cargo_median_ms: Option<u64>,
    pub cargo_min_ms: Option<u64>,
    pub cargo_max_ms: Option<u64>,
    pub toolchain: ConditionValue,
    pub hardware: ConditionValue,
    pub cache_state: String,
    pub input_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeBinding {
    pub change_id: Option<String>,
    pub baseline_input_hash: String,
    pub candidate_input_hash: String,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PhaseDelta {
    pub name: String,
    pub baseline_ms: Option<u64>,
    pub candidate_ms: Option<u64>,
    pub delta_ms: Option<i64>,
    pub delta_percent: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileComparison {
    pub format_version: u8,
    /// COMPARABLE or INCONCLUSIVE.
    pub status: String,
    pub reason: String,
    pub configuration: Option<ConfigurationSnapshot>,
    pub conditions_match: bool,
    pub condition_differences: Vec<String>,
    pub change_binding: ChangeBinding,
    pub required_samples: u64,
    pub baseline: ComparisonSide,
    pub candidate: ComparisonSide,
    pub phase_deltas: Vec<PhaseDelta>,
    /// Absent unless both sides have enough clean samples and a material change.
    pub speed_claim: Option<String>,
    pub warnings: Vec<String>,
}

/// One extracted `UNIT_DATA` row. Unknown fields are ignored; missing fields
/// fail closed through `serde` defaults only for the documented v1 shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimingUnit {
    #[serde(default)]
    pub i: usize,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub start: f64,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub unblocked_units: Vec<usize>,
}

/// A typed build-analysis request after protocol validation.
#[derive(Debug, Clone)]
pub struct ProfileRequest {
    pub directory: Option<PathBuf>,
    pub target: GateTargetId,
    pub options: ValidationOptions,
    pub client_roots: ClientRoots,
    pub root_epoch: u64,
    pub budget: ProfileBudget,
    pub change_id: Option<String>,
    /// Protocol-level admission duration measured by the handler, if any.
    pub mcp_admission_ms: Option<u64>,
}

/// A typed baseline/candidate comparison request.
#[derive(Debug, Clone)]
pub struct CompareRequest {
    pub directory: Option<PathBuf>,
    pub target: GateTargetId,
    pub options: ValidationOptions,
    pub client_roots: ClientRoots,
    pub root_epoch: u64,
    pub budget: ProfileBudget,
    pub change_id: Option<String>,
    pub baseline_evidence: Vec<String>,
}

/// Bounded build-profile service built on the shared check service.
#[derive(Debug)]
pub struct ProfileService {
    check: Arc<CheckService>,
    config: Config,
    cargo: PathBuf,
    supervisor: ProcessSupervisor,
    evidence: Arc<Mutex<VecDeque<ProfileRecord>>>,
    toolchain: Arc<Mutex<Option<ConditionValue>>>,
}

impl ProfileService {
    pub fn new(config: Config, check: Arc<CheckService>, supervisor: ProcessSupervisor) -> Self {
        let cargo = super::check::resolve_cargo(config.cargo.path.as_deref());
        Self {
            check,
            config,
            cargo,
            supervisor,
            evidence: Arc::new(Mutex::new(VecDeque::new())),
            toolchain: Arc::new(Mutex::new(None)),
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

    pub fn lookup(&self, evidence_id: &str) -> Option<ProfileRecord> {
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|record| record.evidence_id == evidence_id)
            .cloned()
    }

    /// Run one bounded analysis sample and retain its evidence record.
    pub async fn analyze(
        &self,
        request: &ProfileRequest,
        started_at: Instant,
        authority: Option<Arc<AuthorizedRoot>>,
        cancellation: &CancellationToken,
    ) -> ProfileRecord {
        let budget = self.effective_budget(&request.budget);
        let deadline = started_at + Duration::from_millis(budget.wall_time_ms);
        let toolchain = match authority.as_ref() {
            Some(authority) => {
                self.toolchain(authority.clone(), deadline, cancellation.clone())
                    .await
            }
            None => ConditionValue::unavailable(
                "no authorized root was available for a bounded toolchain probe",
            ),
        };
        let hardware = hardware_class();
        let run_cancel = cancellation.child_token();
        let timer = spawn_budget_timer(run_cancel.clone(), deadline);
        let gate_request = GateRequest {
            options: request.options.clone(),
            directory: request.directory.clone(),
            target: request.target,
            timings: true,
            detail: GateDetail::Standard,
            client_roots: request.client_roots.clone(),
            root_epoch: request.root_epoch,
            source: GateSource::Explicit,
        };
        let evidence = self
            .check
            .run(gate_request, None, Some(run_cancel.clone()))
            .await;
        timer.abort();
        let budget_exhausted = Instant::now() >= deadline
            && matches!(evidence.status, GateStatus::Cancelled | GateStatus::Timeout);
        let record = self.assemble_record(
            request,
            &budget,
            started_at,
            toolchain,
            hardware,
            &evidence,
            budget_exhausted,
        );
        self.retain(record.clone());
        record
    }

    /// Compare retained or freshly-run baseline evidence against fresh candidate
    /// samples. A single run per side can never produce a speed claim.
    pub async fn compare(
        &self,
        request: &CompareRequest,
        started_at: Instant,
        authority: Option<Arc<AuthorizedRoot>>,
        cancellation: &CancellationToken,
    ) -> ProfileComparison {
        let budget = self.effective_budget(&request.budget);
        let deadline = started_at + Duration::from_millis(budget.wall_time_ms);
        let required = self.config.profile.compare_samples.max(1);
        let mut warnings = Vec::new();
        let mut missing = Vec::new();
        let mut baseline = Vec::new();
        for id in &request.baseline_evidence {
            match self.lookup(id) {
                Some(record) => baseline.push(record),
                None => missing.push(id.clone()),
            }
        }
        if !missing.is_empty() {
            return self.inconclusive_comparison(
                request,
                &budget,
                required,
                format!("unknown baseline evidence id(s): {}", missing.join(", ")),
            );
        }
        for record in &baseline {
            if record.status == "COMPLETE" {
                continue;
            }
            warnings.push(format!(
                "retained baseline sample {} finished as {}: {}",
                record.evidence_id, record.status, record.reason
            ));
        }

        let mut runs_used = 0_u64;
        if baseline.is_empty() {
            if budget.max_runs < 1 {
                return self.inconclusive_comparison(
                    request,
                    &budget,
                    required,
                    "budget.maxRuns does not allow a fresh baseline run".to_owned(),
                );
            }
            let record = self
                .analyze(
                    &analyze_request(request, budget),
                    started_at,
                    authority.clone(),
                    cancellation,
                )
                .await;
            let status = record.status.clone();
            let reason = record.reason.clone();
            runs_used += 1;
            baseline.push(record);
            if status != "COMPLETE" {
                warnings.push(format!("fresh baseline run finished as {status}: {reason}"));
            }
        }

        let remaining = budget.max_runs.saturating_sub(runs_used);
        let candidate_runs = required.min(remaining);
        if candidate_runs == 0 {
            return self.inconclusive_comparison(
                request,
                &budget,
                required,
                "budget.maxRuns is exhausted before any candidate sample".to_owned(),
            );
        }
        let mut candidate = Vec::new();
        for _ in 0..candidate_runs {
            if Instant::now() >= deadline {
                warnings.push(
                    "wall-time budget was exhausted before all candidate samples completed"
                        .to_owned(),
                );
                break;
            }
            let record = self
                .analyze(
                    &analyze_request(request, budget),
                    Instant::now(),
                    authority.clone(),
                    cancellation,
                )
                .await;
            if record.status != "COMPLETE" {
                warnings.push(format!(
                    "candidate sample finished as {}: {}",
                    record.status, record.reason
                ));
            }
            candidate.push(record);
        }
        if candidate.is_empty() {
            return self.inconclusive_comparison(
                request,
                &budget,
                required,
                "no candidate sample could be completed within the budget".to_owned(),
            );
        }

        self.build_comparison(request, required, &baseline, &candidate, warnings)
    }

    #[allow(clippy::too_many_lines)]
    fn build_comparison(
        &self,
        request: &CompareRequest,
        required: u64,
        baseline: &[ProfileRecord],
        candidate: &[ProfileRecord],
        mut warnings: Vec<String>,
    ) -> ProfileComparison {
        let configuration = baseline
            .first()
            .or_else(|| candidate.first())
            .map(|record| record.configuration.clone());
        let mut differences = Vec::new();
        differences.extend(condition_differences(baseline, candidate));
        if let Some(configuration) = &configuration
            && candidate
                .iter()
                .chain(baseline.iter())
                .any(|record| &record.configuration != configuration)
        {
            differences.push("configuration differs between samples".to_owned());
        }
        let baseline_side = side(baseline);
        let candidate_side = side(candidate);
        let change_binding = ChangeBinding {
            change_id: request.change_id.clone(),
            baseline_input_hash: baseline_side.input_hash.clone(),
            candidate_input_hash: candidate_side.input_hash.clone(),
            changed: baseline_side.input_hash != candidate_side.input_hash,
        };
        let phase_deltas = phase_deltas(baseline, candidate);
        let conditions_match = differences.is_empty();
        let compare_result = compare_measurements(
            &baseline_side,
            &candidate_side,
            required,
            change_binding.changed,
            conditions_match,
        );
        let (status, reason, speed_claim) = match compare_result {
            Ok((claim, reason)) => ("COMPARABLE".to_owned(), reason, claim),
            Err(reason) => ("INCONCLUSIVE".to_owned(), reason, None),
        };
        if !conditions_match {
            warnings.push(
                "conditions differed between sides; no speed claim is valid from these samples"
                    .to_owned(),
            );
        }
        ProfileComparison {
            format_version: PROFILE_FORMAT_VERSION,
            status,
            reason,
            configuration,
            conditions_match,
            condition_differences: differences,
            change_binding,
            required_samples: required,
            baseline: baseline_side,
            candidate: candidate_side,
            phase_deltas,
            speed_claim,
            warnings,
        }
    }

    fn inconclusive_comparison(
        &self,
        request: &CompareRequest,
        budget: &ProfileBudget,
        required: u64,
        reason: String,
    ) -> ProfileComparison {
        ProfileComparison {
            format_version: PROFILE_FORMAT_VERSION,
            status: "INCONCLUSIVE".to_owned(),
            reason,
            configuration: None,
            conditions_match: false,
            condition_differences: Vec::new(),
            change_binding: ChangeBinding {
                change_id: request.change_id.clone(),
                baseline_input_hash: String::new(),
                candidate_input_hash: String::new(),
                changed: false,
            },
            required_samples: required,
            baseline: empty_side(),
            candidate: empty_side(),
            phase_deltas: Vec::new(),
            speed_claim: None,
            warnings: vec![format!(
                "no comparison was possible under budget maxRuns={} maxReportBytes={} wallTimeMs={}",
                budget.max_runs, budget.max_report_bytes, budget.wall_time_ms
            )],
        }
    }

    fn retain(&self, record: ProfileRecord) {
        let mut evidence = self
            .evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        evidence.push_back(record);
        while evidence.len() > MAX_RETAINED_EVIDENCE {
            evidence.pop_front();
        }
    }

    async fn toolchain(
        &self,
        authority: Arc<AuthorizedRoot>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> ConditionValue {
        let cached = {
            self.toolchain
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        if let Some(cached) = cached {
            return cached;
        }
        let value = self
            .probe_toolchain(authority, deadline, cancellation)
            .await;
        {
            let mut slot = self
                .toolchain
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(value.clone());
            }
        }
        value
    }

    async fn probe_toolchain(
        &self,
        authority: Arc<AuthorizedRoot>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> ConditionValue {
        let result = self
            .supervisor
            .run_authorized(
                self.cargo.clone(),
                ["-Vv"],
                ProcessRunOptions::new(authority.path())
                    .with_timeout(TOOLCHAIN_TIMEOUT)
                    .with_deadline(deadline)
                    .with_cancellation(cancellation)
                    .with_max_output_bytes(MAX_TOOLCHAIN_BYTES),
                authority,
            )
            .await;
        match result {
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

    #[allow(clippy::too_many_lines)]
    fn assemble_record(
        &self,
        request: &ProfileRequest,
        budget: &ProfileBudget,
        started_at: Instant,
        toolchain: ConditionValue,
        hardware: ConditionValue,
        evidence: &GateEvidence,
        budget_exhausted: bool,
    ) -> ProfileRecord {
        let status = if budget_exhausted {
            "BUDGET_EXHAUSTED".to_owned()
        } else {
            profile_status(evidence.status)
        };
        let changed_paths = evidence
            .scope
            .changed_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let rebuild = rebuild_report(evidence);
        let id = evidence_id(evidence);
        let (timings_report, timing_units) = self.timings_report(evidence, budget, &id);
        let critical_path = timing_units.as_deref().map_or_else(
            || CriticalPath {
                available: false,
                reason: timings_report.reason.clone(),
                method: "longest-weighted-path over extracted dependency edges".to_owned(),
                path_duration_secs: None,
                units: Vec::new(),
                caveat: critical_path_caveat(),
            },
            |units| match build_critical_path(units) {
                Ok((duration, units)) => CriticalPath {
                    available: true,
                    reason: String::new(),
                    method: "longest-weighted-path over extracted dependency edges".to_owned(),
                    path_duration_secs: Some(duration),
                    units,
                    caveat: critical_path_caveat(),
                },
                Err(reason) => CriticalPath {
                    available: false,
                    reason,
                    method: "longest-weighted-path over extracted dependency edges".to_owned(),
                    path_duration_secs: None,
                    units: Vec::new(),
                    caveat: critical_path_caveat(),
                },
            },
        );
        let explanations = explain(
            &rebuild,
            &changed_paths,
            timing_units.as_deref(),
            evidence.status,
        );
        let cache_state = cache_state(&rebuild);
        ProfileRecord {
            format_version: PROFILE_FORMAT_VERSION,
            evidence_id: id,
            action: "buildAnalyze".to_owned(),
            status,
            gate_status: evidence.status.as_str().to_owned(),
            reason: evidence
                .message
                .clone()
                .unwrap_or_else(|| evidence.status.as_str().to_owned()),
            change_id: request.change_id.clone(),
            configuration: ConfigurationSnapshot {
                target: request.target.as_str().to_owned(),
                options: request.options.clone(),
                cargo: self.cargo.display().to_string(),
                cache_mode: evidence.cache_mode.clone(),
                detail: "standard".to_owned(),
            },
            conditions: ConditionSnapshot {
                toolchain,
                hardware,
                cache_state,
                input_hash: evidence.input_hash.clone(),
                command_hash: evidence.command_hash.clone(),
                environment_hash: evidence.environment_hash.clone(),
                change_id: request.change_id.clone(),
            },
            phases: phases(evidence, request.mcp_admission_ms),
            rebuild,
            explanations,
            critical_path,
            timings_report,
            wall_ms: elapsed_ms(started_at),
            budget: BudgetSnapshot {
                max_runs: budget.max_runs,
                max_report_bytes: budget.max_report_bytes,
                wall_time_ms: budget.wall_time_ms,
            },
            warnings: evidence.warnings.clone(),
        }
    }

    fn timings_report(
        &self,
        evidence: &GateEvidence,
        budget: &ProfileBudget,
        id: &str,
    ) -> (TimingsReport, Option<Vec<TimingUnit>>) {
        let unavailable = |reason: String| TimingsReport {
            available: false,
            reason,
            relative_path: None,
            root: None,
            bytes: None,
            sha256: None,
            units_extracted: None,
            extraction: "unavailable".to_owned(),
            format: TIMING_UNIT_FORMAT.to_owned(),
            expensive_units: Vec::new(),
        };
        let Some(target_directory) = evidence
            .build
            .as_ref()
            .map(|build| build.target_directory.clone())
        else {
            return (
                unavailable("Cargo build evidence did not include a target directory".to_owned()),
                None,
            );
        };
        let path = target_directory
            .join("cargo-timings")
            .join("cargo-timing.html");
        let Ok(metadata) = fs::metadata(&path) else {
            return (
                unavailable(format!(
                    "the stable timing artifact {} was not produced",
                    path.display()
                )),
                None,
            );
        };
        let bytes = metadata.len();
        if bytes > budget.max_report_bytes {
            return (
                unavailable(format!(
                    "timing artifact is {bytes} bytes and exceeds budget.maxReportBytes={}",
                    budget.max_report_bytes
                )),
                None,
            );
        }
        let mut text = String::new();
        let read = fs::File::open(&path).and_then(|file| {
            file.take(budget.max_report_bytes.saturating_add(1))
                .read_to_string(&mut text)
        });
        if let Err(error) = read {
            return (
                unavailable(format!(
                    "timing artifact {} could not be read: {error}",
                    path.display()
                )),
                None,
            );
        }
        if text.len() as u64 > budget.max_report_bytes {
            return (
                unavailable(format!(
                    "timing artifact exceeded budget.maxReportBytes={} while reading",
                    budget.max_report_bytes
                )),
                None,
            );
        }
        let relative_path = self.persist_report(id, text.as_bytes());
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let (extraction, units, reason) = match extract_timing_units(&text, MAX_TIMING_UNITS) {
            Ok(units) => (
                "parsed".to_owned(),
                Some(units),
                "embedded UNIT_DATA parsed for the recorded Cargo output".to_owned(),
            ),
            Err(reason) => ("unavailable".to_owned(), None, reason),
        };
        let stored = relative_path.is_some();
        let available = extraction == "parsed" && stored;
        let reason = if extraction == "parsed" && !stored {
            "timing artifact was read within budget but could not be stored as evidence".to_owned()
        } else {
            reason
        };
        let expensive_units = units.as_deref().map_or_else(Vec::new, expensive_units);
        let report = TimingsReport {
            available,
            reason,
            relative_path,
            root: Some(self.config.gate.lease_dir.display().to_string()),
            bytes: Some(bytes),
            sha256: Some(digest),
            units_extracted: units.as_ref().map(|units| units.len() as u64),
            extraction,
            format: TIMING_UNIT_FORMAT.to_owned(),
            expensive_units,
        };
        (report, units)
    }

    fn persist_report(&self, id: &str, bytes: &[u8]) -> Option<String> {
        use std::io::Write as _;
        let directory = self.config.gate.lease_dir.join("profile-evidence");
        fs::create_dir_all(&directory).ok()?;
        let path = directory.join(format!("{id}.html"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()?;
        file.write_all(bytes).ok()?;
        file.flush().ok()?;
        Some(format!("profile-evidence/{id}.html"))
    }
}

fn analyze_request(request: &CompareRequest, budget: ProfileBudget) -> ProfileRequest {
    ProfileRequest {
        directory: request.directory.clone(),
        target: request.target,
        options: request.options.clone(),
        client_roots: request.client_roots.clone(),
        root_epoch: request.root_epoch,
        budget,
        change_id: request.change_id.clone(),
        mcp_admission_ms: None,
    }
}

/// Extract the embedded `UNIT_DATA` array from a Cargo `--timings` HTML report.
///
/// This is deliberately version-bound: only the exact `const UNIT_DATA = [`
/// anchor and a balanced JSON array are accepted. Any deviation is a typed
/// error, never a partially guessed extraction.
pub fn extract_timing_units(html: &str, max_units: usize) -> Result<Vec<TimingUnit>, String> {
    const MARKER: &str = "const UNIT_DATA = [";
    let Some(start) = html.find(MARKER) else {
        return Err(format!(
            "report does not contain the {TIMING_UNIT_FORMAT} anchor; extraction is unavailable"
        ));
    };
    let candidate = &html[start + MARKER.len() - 1..];
    let end = balanced_array_end(candidate).ok_or_else(|| {
        "report UNIT_DATA array is malformed or truncated; extraction is fail-closed".to_owned()
    })?;
    let units: Vec<TimingUnit> = serde_json::from_str(&candidate[..end]).map_err(|error| {
        format!("report UNIT_DATA does not match the version-bound shape: {error}")
    })?;
    if units.len() > max_units {
        return Err(format!(
            "report UNIT_DATA had {} units and exceeds the {max_units}-unit bound",
            units.len()
        ));
    }
    Ok(units)
}

fn balanced_array_end(input: &str) -> Option<usize> {
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '[' | '{' => depth = depth.saturating_add(1),
            ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + character.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// Longest weighted dependency path over extracted `unblocked_units` edges.
///
/// The returned duration is a parallel-schedule lower bound along one dependency
/// chain, never a wall-clock measurement.
pub fn build_critical_path(units: &[TimingUnit]) -> Result<(f64, Vec<CriticalPathUnit>), String> {
    if units.is_empty() {
        return Err("timing report contained no compilable units".to_owned());
    }
    let mut index_of = BTreeMap::new();
    for (position, unit) in units.iter().enumerate() {
        index_of.insert(unit.i, position);
    }
    let mut adjacency = vec![BTreeSet::new(); units.len()];
    let mut indegree = vec![0_usize; units.len()];
    for (position, unit) in units.iter().enumerate() {
        for unblocked in &unit.unblocked_units {
            if let Some(target) = index_of.get(unblocked)
                && adjacency[position].insert(*target)
            {
                indegree[*target] = indegree[*target].saturating_add(1);
            }
        }
    }
    let mut queue = VecDeque::new();
    let mut best = units
        .iter()
        .map(|unit| unit.duration.max(0.0))
        .collect::<Vec<_>>();
    let mut previous = vec![None; units.len()];
    for (position, degree) in indegree.iter().enumerate() {
        if *degree == 0 {
            queue.push_back(position);
        }
    }
    let mut processed = 0_usize;
    while let Some(node) = queue.pop_front() {
        processed += 1;
        let nexts = adjacency[node].iter().copied().collect::<Vec<_>>();
        for next in nexts {
            let candidate = best[node] + units[next].duration.max(0.0);
            if candidate > best[next] {
                best[next] = candidate;
                previous[next] = Some(node);
            }
            indegree[next] = indegree[next].saturating_sub(1);
            if indegree[next] == 0 {
                queue.push_back(next);
            }
        }
    }
    if processed != units.len() {
        return Err(
            "timing unit graph contains a cycle; critical path is not measurable".to_owned(),
        );
    }
    let Some(end) = (0..units.len()).max_by(|left, right| {
        best[*left]
            .partial_cmp(&best[*right])
            .unwrap_or(std::cmp::Ordering::Equal)
    }) else {
        return Err("timing unit graph is empty".to_owned());
    };
    let mut chain = Vec::new();
    let mut cursor = Some(end);
    while let Some(node) = cursor {
        chain.push(node);
        if chain.len() >= MAX_CRITICAL_PATH_DEPTH {
            break;
        }
        cursor = previous[node];
    }
    chain.reverse();
    let path = chain
        .into_iter()
        .map(|position| {
            let unit = &units[position];
            CriticalPathUnit {
                name: unit.name.clone(),
                version: unit.version.clone(),
                target: unit.target.clone(),
                duration_secs: unit.duration.max(0.0),
                start_secs: unit.start,
            }
        })
        .collect();
    Ok((best[end], path))
}

fn expensive_units(units: &[TimingUnit]) -> Vec<CriticalPathUnit> {
    let mut ranked = units.to_vec();
    ranked.sort_by(|left, right| {
        right
            .duration
            .partial_cmp(&left.duration)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.name.cmp(&right.name))
    });
    ranked
        .into_iter()
        .take(MAX_EXPENSIVE_UNITS)
        .map(|unit| CriticalPathUnit {
            name: unit.name,
            version: unit.version,
            target: unit.target,
            duration_secs: unit.duration.max(0.0),
            start_secs: unit.start,
        })
        .collect()
}

fn critical_path_caveat() -> String {
    "Extracted unit graph only: path duration is a lower bound for one dependency chain, not \
     wall-clock time, and parallel unit durations are never summed as elapsed time."
        .to_owned()
}

fn explain(
    rebuild: &ProfileRebuildReport,
    changed_paths: &[String],
    timing_units: Option<&[TimingUnit]>,
    gate_status: GateStatus,
) -> Vec<ProfileExplanation> {
    let mut explanations = Vec::new();
    if !rebuild.available {
        explanations.push(ProfileExplanation {
            class: "observed".to_owned(),
            claim: format!(
                "Cargo build telemetry is unavailable ({}); no cache-hit or rebuild counts are reported",
                rebuild.reason
            ),
            evidence: vec!["cargo-json-message-stream".to_owned()],
            refs: Vec::new(),
        });
        return explanations;
    }
    let total = rebuild.total_units.unwrap_or(0);
    let rebuilt = rebuild.rebuilt_units.unwrap_or(0);
    let fresh = rebuild.fresh_units.unwrap_or(0);
    explanations.push(ProfileExplanation {
        class: "observed".to_owned(),
        claim: format!(
            "Cargo observed {rebuilt} rebuilt of {total} compilation unit(s); {fresh} were fresh{}",
            if rebuild.partial {
                "; the artifact stream was partial and these counts are a lower bound"
            } else {
                ""
            }
        ),
        evidence: vec!["cargo-json:compiler-artifact".to_owned()],
        refs: rebuild.rebuilt_packages.clone(),
    });
    if !rebuild.build_script_packages.is_empty() {
        explanations.push(ProfileExplanation {
            class: "observed".to_owned(),
            claim: format!(
                "Cargo executed build scripts for: {}",
                rebuild.build_script_packages.join(", ")
            ),
            evidence: vec!["cargo-json:build-script-executed".to_owned()],
            refs: rebuild.build_script_packages.clone(),
        });
    }
    let changed_sources = changed_paths
        .iter()
        .filter(|path| is_source_path(path))
        .take(MAX_PATH_REFS)
        .cloned()
        .collect::<Vec<_>>();
    let changed_build_scripts = changed_paths
        .iter()
        .filter(|path| path.ends_with("build.rs"))
        .take(MAX_PATH_REFS)
        .cloned()
        .collect::<Vec<_>>();
    let changed_manifests = changed_paths
        .iter()
        .filter(|path| is_manifest_path(path))
        .take(MAX_PATH_REFS)
        .cloned()
        .collect::<Vec<_>>();
    let changed_non_source = changed_paths
        .iter()
        .filter(|path| {
            !is_source_path(path) && !is_manifest_path(path) && !is_resolution_path(path)
        })
        .take(MAX_PATH_REFS)
        .cloned()
        .collect::<Vec<_>>();
    let changed_resolution = changed_paths
        .iter()
        .filter(|path| is_resolution_path(path))
        .take(MAX_PATH_REFS)
        .cloned()
        .collect::<Vec<_>>();
    if rebuilt > 0 && !changed_sources.is_empty() {
        explanations.push(ProfileExplanation {
            class: "reasonedHypothesis".to_owned(),
            claim: "Source edits under the workspace are a plausible cause for the rebuilt units; \
                    Cargo's internal dirty reasons were not captured, so per-package causality is \
                    not asserted"
                .to_owned(),
            evidence: vec![
                "input-identity:changed-paths".to_owned(),
                "cargo-json:compiler-artifact".to_owned(),
            ],
            refs: changed_sources,
        });
    }
    if rebuild.build_scripts.unwrap_or(0) > 0 && !changed_build_scripts.is_empty() {
        explanations.push(ProfileExplanation {
            class: "reasonedHypothesis".to_owned(),
            claim: "A build-script input changed and build-script executions were observed, which \
                    is consistent with build-script reruns; the fingerprint decision itself was \
                    not captured"
                .to_owned(),
            evidence: vec![
                "input-identity:changed-paths".to_owned(),
                "cargo-json:build-script-executed".to_owned(),
            ],
            refs: changed_build_scripts,
        });
    }
    if !changed_manifests.is_empty() {
        explanations.push(ProfileExplanation {
            class: "reasonedHypothesis".to_owned(),
            claim: "Manifest or feature-selection inputs changed; a wide rebuild is plausible \
                    because feature resolution can invalidate many units, but the impact is not \
                    isolated by the package graph alone"
                .to_owned(),
            evidence: vec![
                "input-identity:changed-paths".to_owned(),
                "cargo-json:compiler-artifact".to_owned(),
            ],
            refs: changed_manifests,
        });
    }
    if rebuild.build_scripts.unwrap_or(0) > 0 && !changed_non_source.is_empty() {
        explanations.push(ProfileExplanation {
            class: "reasonedHypothesis".to_owned(),
            claim: "A non-source build input changed and build-script executions were observed; \
                    this is consistent with a build-script input change, but the declared \
                    rerun-if-changed set was not inspected"
                .to_owned(),
            evidence: vec![
                "input-identity:changed-paths".to_owned(),
                "cargo-json:build-script-executed".to_owned(),
            ],
            refs: changed_non_source,
        });
    }
    if !changed_resolution.is_empty() {
        explanations.push(ProfileExplanation {
            class: "reasonedHypothesis".to_owned(),
            claim: "Lockfile or toolchain inputs changed; dependency-resolution rebuilds are \
                    plausible, and dependency versions were not compared"
                .to_owned(),
            evidence: vec!["input-identity:changed-paths".to_owned()],
            refs: changed_resolution,
        });
    }
    if let Some(units) = timing_units {
        let mut versions: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for unit in units {
            versions
                .entry(unit.name.as_str())
                .or_default()
                .insert(unit.version.as_str());
        }
        let repeated = versions
            .iter()
            .filter(|(_, versions)| versions.len() > 1)
            .map(|(name, versions)| {
                format!(
                    "{name}: {}",
                    versions.iter().copied().collect::<Vec<_>>().join(", ")
                )
            })
            .take(MAX_PATH_REFS)
            .collect::<Vec<_>>();
        if !repeated.is_empty() {
            explanations.push(ProfileExplanation {
                class: "unknown".to_owned(),
                claim: "Multiple versions of the same dependency were observed in the timing \
                        report; the rebuild cost attributable to each version was not measured"
                    .to_owned(),
                evidence: vec!["cargo-timings-html:unit-data".to_owned()],
                refs: repeated,
            });
        }
    }
    if total > 0 && rebuilt.saturating_mul(2) > total {
        explanations.push(ProfileExplanation {
            class: "reasonedHypothesis".to_owned(),
            claim: "More than half of the observed units were rebuilt; this is consistent with a \
                    dependency-visible configuration change, not proof of one"
                .to_owned(),
            evidence: vec!["cargo-json:compiler-artifact".to_owned()],
            refs: Vec::new(),
        });
    }
    if rebuilt > 0 && changed_paths.is_empty() {
        explanations.push(ProfileExplanation {
            class: "unknown".to_owned(),
            claim: "Units were rebuilt while the input identity reported no changed paths; the \
                    cause was not observed"
                .to_owned(),
            evidence: vec![
                "input-identity:changed-paths".to_owned(),
                "cargo-json:compiler-artifact".to_owned(),
            ],
            refs: rebuild.rebuilt_packages.clone(),
        });
    }
    if !matches!(
        gate_status,
        GateStatus::FastPass | GateStatus::FullPass | GateStatus::Fail
    ) {
        explanations.push(ProfileExplanation {
            class: "unknown".to_owned(),
            claim: format!(
                "The run ended as {}; no speed conclusion can be drawn from a non-terminal result",
                gate_status.as_str()
            ),
            evidence: vec!["gate-evidence:status".to_owned()],
            refs: Vec::new(),
        });
    }
    explanations
}

fn is_source_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
        && !path.ends_with("build.rs")
}

fn is_manifest_path(path: &str) -> bool {
    path.ends_with("Cargo.toml") || path.contains(".cargo/")
}

fn is_resolution_path(path: &str) -> bool {
    path.ends_with("Cargo.lock")
        || path.ends_with("rust-toolchain")
        || path.ends_with("rust-toolchain.toml")
}

fn rebuild_report(evidence: &GateEvidence) -> ProfileRebuildReport {
    let unavailable = |reason: String| ProfileRebuildReport {
        available: false,
        reason,
        total_units: None,
        fresh_units: None,
        rebuilt_units: None,
        build_scripts: None,
        linked_units: None,
        partial: false,
        rebuilt_packages: Vec::new(),
        build_script_packages: Vec::new(),
        packages_truncated: false,
    };
    let Some(step) = evidence
        .steps
        .iter()
        .rev()
        .find(|step| step.build.is_some())
    else {
        return unavailable(
            "Cargo produced no compiler-artifact telemetry for this configuration".to_owned(),
        );
    };
    let Some(build) = step.build.as_ref() else {
        return unavailable("Cargo build telemetry was missing".to_owned());
    };
    ProfileRebuildReport {
        available: true,
        reason: if build.partial {
            "artifact stream was partial; counts are a lower bound".to_owned()
        } else {
            String::new()
        },
        total_units: Some(build.total_units),
        fresh_units: Some(build.fresh_units),
        rebuilt_units: Some(build.rebuilt_units),
        build_scripts: Some(build.build_scripts),
        linked_units: Some(build.linked_units),
        partial: build.partial,
        rebuilt_packages: build.rebuilt_packages.clone(),
        build_script_packages: build.build_script_packages.clone(),
        packages_truncated: build.packages_truncated,
    }
}

fn cache_state(rebuild: &ProfileRebuildReport) -> String {
    if !rebuild.available || rebuild.partial {
        return "unknown".to_owned();
    }
    let total = rebuild.total_units.unwrap_or(0);
    let rebuilt = rebuild.rebuilt_units.unwrap_or(0);
    let fresh = rebuild.fresh_units.unwrap_or(0);
    if total == 0 {
        "unknown".to_owned()
    } else if rebuilt == 0 {
        "warm".to_owned()
    } else if fresh == 0 {
        "cold".to_owned()
    } else {
        "mixed".to_owned()
    }
}

fn phases(evidence: &GateEvidence, mcp_admission_ms: Option<u64>) -> Vec<ProfilePhase> {
    let mut phases = Vec::new();
    let terminal = evidence.steps.is_empty();
    let step_sum = evidence
        .steps
        .iter()
        .map(|step| step.duration_ms)
        .sum::<u64>();
    let gate_observed = !terminal;
    phases.push(ProfilePhase {
        name: "mcpAdmission".to_owned(),
        ms: mcp_admission_ms,
        observed: mcp_admission_ms.is_some(),
        source: "protocol-handler:entry-to-domain-service (permit, root resolution, workspace)"
            .to_owned(),
    });
    for (name, ms, source) in [
        (
            "gateAdmission",
            Some(evidence.admission_ms),
            "gate-evidence:validation-and-scheduler-admission",
        ),
        (
            "gateQueue",
            Some(evidence.queue_ms),
            "gate-evidence:scheduler-queue",
        ),
        (
            "preflight",
            Some(evidence.preflight_ms),
            "gate-evidence:metadata-identity-scope",
        ),
    ] {
        phases.push(ProfilePhase {
            name: name.to_owned(),
            ms: if gate_observed { ms } else { None },
            observed: gate_observed,
            source: source.to_owned(),
        });
    }
    for step in &evidence.steps {
        phases.push(ProfilePhase {
            name: format!("cargo:{}", step.target.as_str()),
            ms: Some(step.duration_ms),
            observed: true,
            source: "supervisor:process-wall-time (parallel children may overlap)".to_owned(),
        });
    }
    if !terminal {
        let measured = evidence
            .admission_ms
            .saturating_add(evidence.preflight_ms)
            .saturating_add(evidence.queue_ms)
            .saturating_add(step_sum);
        phases.push(ProfilePhase {
            name: "finalization".to_owned(),
            ms: Some(evidence.response_ms.saturating_sub(measured)),
            observed: true,
            source: "gate-evidence:residual (post-validation identity checks and cleanup)"
                .to_owned(),
        });
    }
    phases
}

fn profile_status(status: GateStatus) -> String {
    match status {
        GateStatus::FastPass | GateStatus::FullPass | GateStatus::Fail => "COMPLETE".to_owned(),
        other => other.as_str().to_owned(),
    }
}

fn evidence_id(evidence: &GateEvidence) -> String {
    let mut hasher = Sha256::new();
    hasher.update(evidence.job_id.as_bytes());
    hasher.update(evidence.response_ms.to_le_bytes());
    hasher.update(evidence.input_hash.as_bytes());
    hasher.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    let digest = format!("{:x}", hasher.finalize());
    format!("pe-{}", &digest[..16])
}

fn spawn_budget_timer(cancel: CancellationToken, deadline: Instant) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);
        tokio::select! {
            () = &mut sleep => cancel.cancel(),
            () = cancel.cancelled() => {}
        }
    })
}

fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn summarize_toolchain(stdout: &str) -> String {
    let mut summary = Vec::new();
    for line in stdout.lines().take(8) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("cargo ")
            || line.starts_with("release:")
            || line.starts_with("host:")
            || line.starts_with("rustc ")
        {
            summary.push(line.chars().take(160).collect::<String>());
        }
    }
    if summary.is_empty() {
        "unrecognized cargo -Vv output".to_owned()
    } else {
        summary.join(" ")
    }
}

fn hardware_class() -> ConditionValue {
    let parallelism = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(0);
    let model = fs::read("/proc/cpuinfo")
        .ok()
        .and_then(|bytes| {
            let text = String::from_utf8(bytes.into_iter().take(64 * 1024).collect()).ok()?;
            text.lines()
                .find_map(|line| line.strip_prefix("model name"))
                .map(|value| value.trim_start_matches(':').trim().to_owned())
        })
        .map(|model| model.chars().take(96).collect::<String>());
    ConditionValue {
        available: true,
        summary: match model {
            Some(model) => format!(
                "{}/{} cpus={parallelism} cpu={model}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            None => format!(
                "{}/{} cpus={parallelism}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        },
        reason: "process-visible hardware class only; exact machine identity is not claimed"
            .to_owned(),
    }
}

fn condition_differences(baseline: &[ProfileRecord], candidate: &[ProfileRecord]) -> Vec<String> {
    let mut differences = Vec::new();
    for (label, side) in [("baseline", baseline), ("candidate", candidate)] {
        let conditions = side
            .iter()
            .map(|record| &record.conditions)
            .collect::<Vec<_>>();
        for (field, value) in [
            ("toolchain", conditions.first().map(|c| c.toolchain.clone())),
            ("hardware", conditions.first().map(|c| c.hardware.clone())),
        ] {
            if let Some(value) = value {
                if !value.available {
                    differences.push(format!("{label} {field} is unavailable: {}", value.reason));
                } else if conditions.iter().any(|condition| match field {
                    "toolchain" => condition.toolchain.summary != value.summary,
                    _ => condition.hardware.summary != value.summary,
                }) {
                    differences.push(format!("{label} {field} differs between samples"));
                }
            }
        }
        if conditions
            .iter()
            .any(|condition| condition.cache_state != side[0].conditions.cache_state)
        {
            differences.push(format!(
                "{label} mixes cache states; warm and cold experiments are not comparable"
            ));
        }
    }
    if let (Some(base), Some(cand)) = (baseline.first(), candidate.first()) {
        if base.conditions.toolchain.available
            && cand.conditions.toolchain.available
            && base.conditions.toolchain.summary != cand.conditions.toolchain.summary
        {
            differences.push("toolchain differs between baseline and candidate".to_owned());
        }
        if base.conditions.hardware.summary != cand.conditions.hardware.summary {
            differences.push("hardware class differs between baseline and candidate".to_owned());
        }
        if base.conditions.cache_state != cand.conditions.cache_state {
            differences.push(format!(
                "cache state differs between baseline ({}) and candidate ({})",
                base.conditions.cache_state, cand.conditions.cache_state
            ));
        }
    }
    let mut seen = BTreeSet::new();
    differences.retain(|difference| seen.insert(difference.clone()));
    differences
}

fn side(records: &[ProfileRecord]) -> ComparisonSide {
    let wall = records
        .iter()
        .map(|record| record.wall_ms)
        .collect::<Vec<_>>();
    let cargo = records
        .iter()
        .map(cargo_phase_ms)
        .collect::<Vec<Option<u64>>>();
    let cargo = cargo.into_iter().flatten().collect::<Vec<_>>();
    let first = records.first();
    ComparisonSide {
        samples: records.len() as u64,
        evidence_ids: records
            .iter()
            .map(|record| record.evidence_id.clone())
            .collect(),
        wall_median_ms: median(&wall),
        wall_min_ms: wall.iter().copied().min(),
        wall_max_ms: wall.iter().copied().max(),
        cargo_median_ms: median(&cargo),
        cargo_min_ms: cargo.iter().copied().min(),
        cargo_max_ms: cargo.iter().copied().max(),
        toolchain: first
            .map(|record| record.conditions.toolchain.clone())
            .unwrap_or_else(|| ConditionValue::unavailable("no samples")),
        hardware: first
            .map(|record| record.conditions.hardware.clone())
            .unwrap_or_else(|| ConditionValue::unavailable("no samples")),
        cache_state: first
            .map(|record| record.conditions.cache_state.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
        input_hash: first
            .map(|record| record.conditions.input_hash.clone())
            .unwrap_or_default(),
    }
}

fn empty_side() -> ComparisonSide {
    ComparisonSide {
        samples: 0,
        evidence_ids: Vec::new(),
        wall_median_ms: None,
        wall_min_ms: None,
        wall_max_ms: None,
        cargo_median_ms: None,
        cargo_min_ms: None,
        cargo_max_ms: None,
        toolchain: ConditionValue::unavailable("no samples"),
        hardware: ConditionValue::unavailable("no samples"),
        cache_state: "unknown".to_owned(),
        input_hash: String::new(),
    }
}

fn phase_deltas(baseline: &[ProfileRecord], candidate: &[ProfileRecord]) -> Vec<PhaseDelta> {
    let mut names = BTreeSet::new();
    for record in baseline.iter().chain(candidate.iter()) {
        for phase in &record.phases {
            names.insert(phase.name.clone());
        }
    }
    names
        .into_iter()
        .map(|name| {
            let base = phase_median(baseline, &name);
            let cand = phase_median(candidate, &name);
            let delta = base.zip(cand).map(|(base, cand)| {
                i64::try_from(cand).unwrap_or(i64::MAX) - i64::try_from(base).unwrap_or(i64::MAX)
            });
            let percent = base.zip(cand).and_then(|(base, cand)| {
                (base > 0).then(|| (cand as f64 - base as f64) / base as f64 * 100.0)
            });
            PhaseDelta {
                name,
                baseline_ms: base,
                candidate_ms: cand,
                delta_ms: delta,
                delta_percent: percent,
            }
        })
        .collect()
}

fn phase_median(records: &[ProfileRecord], name: &str) -> Option<u64> {
    let values = records
        .iter()
        .flat_map(|record| record.phases.iter())
        .filter(|phase| phase.name == name)
        .filter_map(|phase| phase.ms)
        .collect::<Vec<_>>();
    median(&values)
}

fn cargo_phase_ms(record: &ProfileRecord) -> Option<u64> {
    let mut found = false;
    let mut total = 0_u64;
    for phase in &record.phases {
        if phase.name.starts_with("cargo:") {
            if let Some(ms) = phase.ms {
                found = true;
                total = total.saturating_add(ms);
            }
        }
    }
    found.then_some(total)
}

fn median(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
}

#[allow(clippy::type_complexity)]
fn compare_measurements(
    baseline: &ComparisonSide,
    candidate: &ComparisonSide,
    required: u64,
    changed: bool,
    conditions_match: bool,
) -> Result<(Option<String>, String), String> {
    if required < 2 {
        return Err(
            "compareSamples must be at least 2; a single run per side can never yield a speed claim"
                .to_owned(),
        );
    }
    if baseline.samples < required || candidate.samples < required {
        return Err(format!(
            "insufficient samples (baseline {}, candidate {}; {required} required per side)",
            baseline.samples, candidate.samples
        ));
    }
    if !conditions_match {
        return Err("conditions differ between baseline and candidate".to_owned());
    }
    if !changed {
        return Err(
            "baseline and candidate input identities are identical; no source change was recorded"
                .to_owned(),
        );
    }
    let dimensions = [
        (
            "cargo",
            baseline.cargo_median_ms,
            baseline.cargo_min_ms,
            baseline.cargo_max_ms,
            candidate.cargo_median_ms,
            candidate.cargo_min_ms,
            candidate.cargo_max_ms,
        ),
        (
            "wall",
            baseline.wall_median_ms,
            baseline.wall_min_ms,
            baseline.wall_max_ms,
            candidate.wall_median_ms,
            candidate.wall_min_ms,
            candidate.wall_max_ms,
        ),
    ];
    for (label, base, base_min, base_max, cand, cand_min, cand_max) in dimensions {
        if noisy(&base, &base_min, &base_max) || noisy(&cand, &cand_min, &cand_max) {
            return Err(format!(
                "{label} samples are too noisy to compare (relative spread above {:.0}%)",
                NOISE_RATIO * 100.0
            ));
        }
    }
    let (Some(base), Some(cand)) = (baseline.cargo_median_ms, candidate.cargo_median_ms) else {
        return Err("Cargo phase timing was not observed on both sides".to_owned());
    };
    if base == 0 {
        return Err("baseline Cargo phase median is zero; no ratio is measurable".to_owned());
    }
    let ratio = (cand as f64 - base as f64) / base as f64;
    let claim = if ratio <= -MATERIALITY_RATIO {
        Some("candidate-faster".to_owned())
    } else if ratio >= MATERIALITY_RATIO {
        Some("baseline-faster".to_owned())
    } else {
        None
    };
    let reason = claim.as_ref().map_or_else(
        || {
            format!(
                "median Cargo phase changed {:+.1}%, within the {:.0}% materiality threshold",
                ratio * 100.0,
                MATERIALITY_RATIO * 100.0
            )
        },
        |claim| {
            format!(
                "median Cargo phase changed {:+.1}% across {required} samples per side; claim={claim}",
                ratio * 100.0
            )
        },
    );
    Ok((claim, reason))
}

fn noisy(median: &Option<u64>, min: &Option<u64>, max: &Option<u64>) -> bool {
    let (Some(median), Some(min), Some(max)) = (median, min, max) else {
        return false;
    };
    *median > 0 && (max.saturating_sub(*min) as f64) / (*median as f64) > NOISE_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_HTML: &str = r#"<html><script>
constructor = 1;
const UNIT_DATA = [
  {"i": 0, "name": "a", "version": "0.1.0", "target": " (lib)", "start": 0.0, "duration": 1.0, "unblocked_units": [1]},
  {"i": 1, "name": "b", "version": "0.2.0", "target": " (lib)", "start": 1.0, "duration": 2.0, "unblocked_units": []}
];
</script></html>"#;

    #[test]
    fn extraction_parses_the_version_bound_shape() {
        let units = extract_timing_units(VALID_HTML, MAX_TIMING_UNITS).expect("valid shape");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].unblocked_units, [1]);
    }

    #[test]
    fn extraction_fails_closed_for_missing_or_malformed_data() {
        assert!(extract_timing_units("<html>no data</html>", MAX_TIMING_UNITS).is_err());
        assert!(extract_timing_units("const UNIT_DATA = [{\"i\": 0", MAX_TIMING_UNITS).is_err());
        assert!(extract_timing_units("const UNIT_DATA = [not json]", MAX_TIMING_UNITS).is_err());
        assert!(extract_timing_units("const UNIT_DATA = [", MAX_TIMING_UNITS).is_err());
    }

    #[test]
    fn extraction_respects_the_unit_bound() {
        assert!(extract_timing_units(VALID_HTML, 1).is_err());
    }

    #[test]
    fn critical_path_uses_dependency_edges_and_detects_cycles() {
        let units = extract_timing_units(VALID_HTML, MAX_TIMING_UNITS).expect("valid shape");
        let (duration, path) = build_critical_path(&units).expect("acyclic");
        assert!((duration - 3.0).abs() < f64::EPSILON);
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].name, "a");
        let cyclic = vec![
            TimingUnit {
                i: 0,
                name: "a".to_owned(),
                version: "1".to_owned(),
                target: " (lib)".to_owned(),
                start: 0.0,
                duration: 1.0,
                unblocked_units: vec![1],
            },
            TimingUnit {
                i: 1,
                name: "b".to_owned(),
                version: "1".to_owned(),
                target: " (lib)".to_owned(),
                start: 0.0,
                duration: 1.0,
                unblocked_units: vec![0],
            },
        ];
        assert!(build_critical_path(&cyclic).is_err());
    }
}
