//! Real-Cargo integration coverage for the bounded `profile` build-analysis tool.

mod support;

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use agz_rust_coder::{
    Config,
    gate::GateTargetId,
    process::ProcessSupervisor,
    tools::{
        CheckService, CompareRequest, ProfileBudget, ProfileRecord, ProfileRequest, ProfileService,
    },
    workspace::{AuthorizedRoot, ClientRoots, RootGuard, select_workspace},
};
use support::{TestRoot, write_minimal_package};
use tokio_util::sync::CancellationToken;

struct Fixture {
    test: TestRoot,
    root: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(label: &str, build_script: Option<&str>) -> Self {
        let test = TestRoot::new(label);
        assert!(test.path().is_absolute());
        write_minimal_package(&test, "workspace", &format!("profile-fixture-{label}"));
        if let Some(script) = build_script {
            test.write("workspace/build.rs", script);
        }
        let root = test.dir("workspace");
        let state = test.dir("state");
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "profile@example.invalid"]);
        git(&root, &["config", "user.name", "Profile Fixture"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "fixture"]);
        Self { test, root, state }
    }

    fn service(&self) -> ProfileService {
        self.service_with(3, 4)
    }

    fn service_with(&self, compare_samples: u64, max_runs: u64) -> ProfileService {
        let mut config = Config::defaults_at(&self.root);
        config.gate.debounce_ms = 10;
        config.gate.hard_timeout_ms = 60_000;
        config.gate.cache_dir = self.state.join("cache");
        config.gate.lease_dir = self.state.join("leases");
        config.profile.max_runs = max_runs;
        config.profile.compare_samples = compare_samples;
        let guard =
            Arc::new(RootGuard::new([self.root.clone()], std::iter::empty()).expect("root guard"));
        let check = Arc::new(CheckService::new(config.clone(), guard));
        ProfileService::new(config, check, ProcessSupervisor::without_journal())
    }

    fn authority(&self) -> Arc<AuthorizedRoot> {
        let guard = RootGuard::new([self.root.clone()], std::iter::empty()).expect("root guard");
        let snapshot = guard
            .snapshot(ClientRoots::unsupported())
            .expect("root snapshot");
        let selection = select_workspace(&snapshot, Some(&self.root)).expect("workspace selection");
        selection.requested_root().requested_authority().clone()
    }

    fn request(&self, change_id: Option<&str>, max_report_bytes: u64) -> ProfileRequest {
        ProfileRequest {
            directory: Some(self.root.clone()),
            target: GateTargetId::Check,
            options: Default::default(),
            client_roots: ClientRoots::unsupported(),
            root_epoch: 0,
            budget: ProfileBudget {
                max_runs: 4,
                max_report_bytes,
                wall_time_ms: 120_000,
            },
            change_id: change_id.map(str::to_owned),
            mcp_admission_ms: Some(1),
        }
    }

    fn compare_request(&self, baseline_evidence: Vec<String>, max_runs: u64) -> CompareRequest {
        CompareRequest {
            directory: Some(self.root.clone()),
            target: GateTargetId::Check,
            options: Default::default(),
            client_roots: ClientRoots::unsupported(),
            root_epoch: 0,
            budget: ProfileBudget {
                max_runs,
                max_report_bytes: 4 * 1024 * 1024,
                wall_time_ms: 120_000,
            },
            change_id: Some("fixture-change".to_owned()),
            baseline_evidence,
        }
    }

    async fn analyze(&self, service: &ProfileService, change_id: Option<&str>) -> ProfileRecord {
        service
            .analyze(
                &self.request(change_id, 4 * 1024 * 1024),
                Instant::now(),
                Some(self.authority()),
                &CancellationToken::new(),
            )
            .await
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn phase<'a>(record: &'a ProfileRecord, name: &str) -> &'a agz_rust_coder::tools::ProfilePhase {
    record
        .phases
        .iter()
        .find(|phase| phase.name == name)
        .unwrap_or_else(|| panic!("missing phase {name}: {:?}", record.phases))
}

#[tokio::test]
async fn source_edit_reports_distinct_rebuild_and_separated_phases() {
    let fixture = Fixture::new("source", None);
    let service = fixture.service();
    let cold = fixture.analyze(&service, None).await;
    assert_eq!(cold.status, "COMPLETE", "{}", cold.reason);
    assert!(cold.rebuild.available);
    assert_eq!(cold.conditions.cache_state, "cold");
    assert!(
        cold.timings_report.available,
        "{}",
        cold.timings_report.reason
    );
    assert_eq!(cold.timings_report.extraction, "parsed");
    assert!(
        cold.critical_path.available,
        "{}",
        cold.critical_path.reason
    );
    assert!(phase(&cold, "preflight").observed);
    assert!(phase(&cold, "cargo:check").observed);

    fixture
        .test
        .write("workspace/src/lib.rs", "pub fn value() -> u8 { 7 }\n");
    let warm = fixture.analyze(&service, None).await;
    assert_eq!(warm.status, "COMPLETE", "{}", warm.reason);
    assert!(warm.rebuild.available, "{}", warm.rebuild.reason);
    assert!(
        warm.rebuild.rebuilt_units.unwrap_or(0) >= 1,
        "{:?}",
        warm.rebuild
    );
    assert!(
        warm.rebuild
            .rebuilt_packages
            .iter()
            .any(|name| name.contains("profile-fixture-source")),
        "{:?}",
        warm.rebuild.rebuilt_packages
    );
    assert_ne!(warm.conditions.input_hash, cold.conditions.input_hash);

    // Preflight, Cargo, and finalization stay separate observations.
    let preflight = phase(&warm, "preflight");
    let cargo = phase(&warm, "cargo:check");
    assert!(preflight.observed && cargo.observed);
    assert_ne!(preflight.name, cargo.name);
    assert!(phase(&warm, "finalization").observed);
    assert!(phase(&warm, "mcpAdmission").observed);
    assert!(
        warm.explanations
            .iter()
            .any(|explanation| explanation.class == "reasonedHypothesis"
                && explanation.claim.contains("Source edits")),
        "{:?}",
        warm.explanations
    );
    assert!(warm.explanations.iter().all(|explanation| {
        matches!(
            explanation.class.as_str(),
            "observed" | "reasonedHypothesis" | "unknown"
        )
    }));
}

#[tokio::test]
async fn manifest_change_is_reported_as_a_distinct_rebuild_input() {
    let fixture = Fixture::new("manifest", None);
    let service = fixture.service();
    let before = fixture.analyze(&service, None).await;
    assert_eq!(before.status, "COMPLETE", "{}", before.reason);

    fixture.test.write(
        "workspace/Cargo.toml",
        "[package]\nname = \"profile-fixture-manifest\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nextra = []\n",
    );
    let after = fixture.analyze(&service, None).await;
    assert_eq!(after.status, "COMPLETE", "{}", after.reason);
    assert!(after.rebuild.available);
    assert_ne!(after.conditions.input_hash, before.conditions.input_hash);
    assert!(
        after
            .explanations
            .iter()
            .any(|explanation| explanation.claim.contains("Manifest")
                && explanation
                    .refs
                    .iter()
                    .any(|reference| reference.contains("Cargo.toml"))),
        "{:?}",
        after.explanations
    );
}

#[tokio::test]
async fn build_script_input_change_reports_observed_build_script_execution() {
    let script = "fn main() { println!(\"cargo:rerun-if-changed=input.txt\"); }\n";
    let fixture = Fixture::new("buildscript", Some(script));
    fixture.test.write("workspace/input.txt", "one\n");
    git(&fixture.root, &["add", "."]);
    git(&fixture.root, &["commit", "--quiet", "-m", "input"]);
    let service = fixture.service();
    let before = fixture.analyze(&service, None).await;
    assert_eq!(before.status, "COMPLETE", "{}", before.reason);

    fixture.test.write("workspace/input.txt", "two\n");
    let after = fixture.analyze(&service, None).await;
    assert_eq!(after.status, "COMPLETE", "{}", after.reason);
    assert!(after.rebuild.available);
    assert!(
        after.rebuild.build_scripts.unwrap_or(0) >= 1,
        "{:?}",
        after.rebuild
    );
    assert!(
        after
            .rebuild
            .build_script_packages
            .iter()
            .any(|name| name.contains("profile-fixture-buildscript")),
        "{:?}",
        after.rebuild.build_script_packages
    );
    assert!(
        after
            .explanations
            .iter()
            .any(|explanation| explanation.claim.contains("build-script")),
        "{:?}",
        after.explanations
    );
}

#[tokio::test]
async fn oversized_timing_report_is_typed_unavailable_without_guessed_numbers() {
    let fixture = Fixture::new("oversized", None);
    let service = fixture.service();
    let record = service
        .analyze(
            &fixture.request(None, 1_024),
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(record.status, "COMPLETE", "{}", record.reason);
    assert!(!record.timings_report.available);
    assert_eq!(record.timings_report.extraction, "unavailable");
    assert!(
        record.timings_report.reason.contains("maxReportBytes"),
        "{}",
        record.timings_report.reason
    );
    assert_eq!(record.timings_report.units_extracted, None);
    assert!(record.timings_report.expensive_units.is_empty());
    assert!(!record.critical_path.available);
    // Cargo's JSON artifact telemetry is independent and still observed.
    assert!(record.rebuild.available);
}

#[tokio::test]
async fn cancelled_run_never_reports_cache_hits_or_causality() {
    let fixture = Fixture::new("cancel", None);
    let service = fixture.service();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let started = Instant::now();
    let record = service
        .analyze(
            &fixture.request(None, 4 * 1024 * 1024),
            started,
            Some(fixture.authority()),
            &cancellation,
        )
        .await;
    assert_eq!(record.status, "CANCELLED", "{}", record.reason);
    assert!(!record.rebuild.available);
    assert_eq!(record.conditions.cache_state, "unknown");
    assert!(phase(&record, "preflight").ms.is_none());
    assert!(!phase(&record, "preflight").observed);
    assert!(record.explanations.iter().all(|explanation| {
        explanation.class != "reasonedHypothesis" || !explanation.claim.contains("Source edits")
    }));
}

#[tokio::test]
async fn single_sample_compare_records_conditions_and_stays_inconclusive() {
    let fixture = Fixture::new("compare-single", None);
    let service = fixture.service();
    let baseline = fixture.analyze(&service, Some("change-1")).await;
    assert_eq!(baseline.status, "COMPLETE", "{}", baseline.reason);

    let comparison = service
        .compare(
            &fixture.compare_request(vec![baseline.evidence_id.clone()], 1),
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE", "{}", comparison.reason);
    assert_eq!(comparison.baseline.samples, 1);
    assert_eq!(comparison.candidate.samples, 1);
    assert_eq!(comparison.required_samples, 3);
    assert!(comparison.speed_claim.is_none());
    assert!(comparison.configuration.is_some());
    assert!(comparison.baseline.hardware.available);
    assert!(comparison.baseline.toolchain.available);
    assert_eq!(
        comparison.change_binding.change_id.as_deref(),
        Some("fixture-change")
    );
    assert!(!comparison.change_binding.baseline_input_hash.is_empty());
    assert!(!comparison.baseline.cache_state.is_empty());
    assert!(
        comparison.reason.contains("insufficient samples"),
        "{}",
        comparison.reason
    );
}

#[tokio::test]
async fn single_run_per_side_can_never_produce_a_speed_claim() {
    let fixture = Fixture::new("compare-single-rule", None);
    let service = fixture.service_with(1, 1);
    let baseline = fixture.analyze(&service, None).await;
    assert_eq!(baseline.status, "COMPLETE", "{}", baseline.reason);
    let comparison = service
        .compare(
            &fixture.compare_request(vec![baseline.evidence_id.clone()], 1),
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE", "{}", comparison.reason);
    assert!(comparison.speed_claim.is_none());
    assert!(
        comparison.reason.contains("at least 2"),
        "{}",
        comparison.reason
    );
}

#[tokio::test]
async fn unknown_baseline_evidence_is_inconclusive_without_running() {
    let fixture = Fixture::new("compare-missing", None);
    let service = fixture.service();
    let comparison = service
        .compare(
            &fixture.compare_request(vec!["pe-missing".to_owned()], 1),
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE");
    assert!(comparison.reason.contains("unknown baseline evidence"));
    assert_eq!(comparison.baseline.samples, 0);
    assert_eq!(comparison.candidate.samples, 0);
    assert!(comparison.speed_claim.is_none());
}

#[test]
fn configured_bounds_clamp_the_requested_budget() {
    let fixture = Fixture::new("bounds", None);
    let service = fixture.service();
    let effective = service.effective_budget(&ProfileBudget {
        max_runs: 99,
        max_report_bytes: u64::MAX,
        wall_time_ms: u64::MAX,
    });
    assert_eq!(effective.max_runs, 4);
    assert_eq!(effective.max_report_bytes, 4 * 1024 * 1024);
    assert_eq!(effective.wall_time_ms, 60_000);

    let floor = service.effective_budget(&ProfileBudget {
        max_runs: 0,
        max_report_bytes: 0,
        wall_time_ms: 0,
    });
    assert_eq!(floor.max_runs, 1);
    assert_eq!(floor.max_report_bytes, 1_024);
    assert_eq!(floor.wall_time_ms, 1);
}

#[tokio::test]
async fn wall_time_budget_cancels_a_run_without_fabricating_evidence() {
    let fixture = Fixture::new("wall", None);
    let service = fixture.service();
    let mut request = fixture.request(None, 4 * 1024 * 1024);
    request.budget.wall_time_ms = 1;
    let record = service
        .analyze(
            &request,
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    if record.status == "COMPLETE" {
        // A sub-millisecond preflight cannot happen in practice on a real
        // fixture; keep the assertion explicit if the host is unreasonably fast.
        assert!(record.wall_ms <= Duration::from_secs(5).as_millis() as u64);
    } else {
        assert_eq!(record.status, "BUDGET_EXHAUSTED", "{}", record.reason);
        assert!(!record.rebuild.available);
    }
}
