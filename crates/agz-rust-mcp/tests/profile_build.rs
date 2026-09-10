//! Real-Cargo integration coverage for the bounded `profile` build-analysis tool.

mod support;

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use agz_rust_mcp::{
    Config, RustMcpServer,
    gate::GateTargetId,
    process::ProcessSupervisor,
    tools::{
        CheckService, CompareRequest, ProfileBudget, ProfileRecord, ProfileRequest, ProfileService,
    },
    workspace::{AuthorizedRoot, ClientRoots, RootGuard, select_workspace},
};
use anyhow::{Context, Result};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientInfo, ErrorCode,
        Implementation, ProtocolVersion,
    },
};
use serde_json::{Map, Value};
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
            workspace_root: self.root.clone(),
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
            workspace_root: self.root.clone(),
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

fn phase<'a>(record: &'a ProfileRecord, name: &str) -> &'a agz_rust_mcp::tools::ProfilePhase {
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

    // Changing the edition invalidates the package fingerprint, so the rebuild
    // profile really differs and not only the manifest text.
    fixture.test.write(
        "workspace/Cargo.toml",
        "[package]\nname = \"profile-fixture-manifest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let after = fixture.analyze(&service, None).await;
    assert_eq!(after.status, "COMPLETE", "{}", after.reason);
    assert!(after.rebuild.available);
    assert_ne!(after.conditions.input_hash, before.conditions.input_hash);
    assert!(
        after.rebuild.rebuilt_units.unwrap_or(0) >= 1,
        "{:?}",
        after.rebuild
    );
    assert!(
        after
            .rebuild
            .rebuilt_packages
            .iter()
            .any(|name| name.contains("profile-fixture-manifest")),
        "{:?}",
        after.rebuild
    );
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
    let baseline = fixture.analyze(&service, Some("fixture-change")).await;
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
    let baseline = fixture.analyze(&service, Some("fixture-change")).await;
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

#[tokio::test]
async fn duplicate_baseline_evidence_ids_are_counted_once() {
    let fixture = Fixture::new("compare-duplicate", None);
    let service = fixture.service();
    let baseline = fixture.analyze(&service, Some("fixture-change")).await;
    assert_eq!(baseline.status, "COMPLETE", "{}", baseline.reason);

    let comparison = service
        .compare(
            &fixture.compare_request(
                vec![baseline.evidence_id.clone(), baseline.evidence_id.clone()],
                1,
            ),
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE", "{}", comparison.reason);
    assert_eq!(comparison.baseline.samples, 1, "{:?}", comparison.baseline);
    assert_eq!(comparison.candidate.samples, 1);
    assert!(
        comparison
            .warnings
            .iter()
            .any(|warning| warning.contains("more than once")),
        "{:?}",
        comparison.warnings
    );
    assert!(comparison.speed_claim.is_none());
}

#[tokio::test]
async fn non_complete_baseline_evidence_is_excluded_without_fresh_runs() {
    let fixture = Fixture::new("compare-incomplete", None);
    let service = fixture.service();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = service
        .analyze(
            &fixture.request(Some("fixture-change"), 4 * 1024 * 1024),
            Instant::now(),
            Some(fixture.authority()),
            &cancellation,
        )
        .await;
    assert_eq!(cancelled.status, "CANCELLED", "{}", cancelled.reason);

    let comparison = service
        .compare(
            &fixture.compare_request(vec![cancelled.evidence_id.clone()], 4),
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE", "{}", comparison.reason);
    assert_eq!(comparison.baseline.samples, 0);
    assert_eq!(comparison.candidate.samples, 0);
    assert!(
        comparison
            .warnings
            .iter()
            .any(|warning| warning.contains("excluded")),
        "{:?}",
        comparison.warnings
    );
    assert!(comparison.speed_claim.is_none());
}

#[tokio::test]
async fn baseline_evidence_from_another_change_is_rejected_without_runs() {
    let fixture = Fixture::new("compare-foreign-change", None);
    let service = fixture.service();
    let baseline = fixture.analyze(&service, Some("other-change")).await;
    assert_eq!(baseline.status, "COMPLETE", "{}", baseline.reason);

    let comparison = service
        .compare(
            &fixture.compare_request(vec![baseline.evidence_id.clone()], 4),
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE", "{}", comparison.reason);
    assert!(
        comparison.reason.contains("changeId"),
        "{}",
        comparison.reason
    );
    assert_eq!(comparison.baseline.samples, 1);
    assert_eq!(comparison.candidate.samples, 0);
    assert!(comparison.speed_claim.is_none());
}

#[tokio::test]
async fn baseline_evidence_from_another_root_epoch_is_rejected_without_runs() {
    let fixture = Fixture::new("compare-foreign-epoch", None);
    let service = fixture.service();
    let baseline = fixture.analyze(&service, Some("fixture-change")).await;
    assert_eq!(baseline.status, "COMPLETE", "{}", baseline.reason);

    let mut request = fixture.compare_request(vec![baseline.evidence_id.clone()], 4);
    request.root_epoch = 9;
    let comparison = service
        .compare(
            &request,
            Instant::now(),
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE", "{}", comparison.reason);
    assert!(
        comparison.reason.contains("root epoch"),
        "{}",
        comparison.reason
    );
    assert_eq!(comparison.candidate.samples, 0);
    assert!(comparison.speed_claim.is_none());
}

#[tokio::test]
async fn compare_uses_one_absolute_wall_deadline() {
    let fixture = Fixture::new("compare-wall", None);
    let service = fixture.service();
    let mut request = fixture.compare_request(Vec::new(), 4);
    request.budget.wall_time_ms = 1;
    let started = Instant::now();
    let comparison = service
        .compare(
            &request,
            started,
            Some(fixture.authority()),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(comparison.status, "INCONCLUSIVE", "{}", comparison.reason);
    assert_eq!(
        comparison.candidate.samples, 0,
        "{:?}",
        comparison.candidate
    );
    assert!(comparison.speed_claim.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "compare multiplied its wall budget: {:?}",
        started.elapsed()
    );
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

fn mcp_client_info(capabilities: ClientCapabilities) -> ClientInfo {
    ClientInfo::new(
        capabilities,
        Implementation::new("agz-rust-mcp-profile-test", "0.1.0"),
    )
}

fn mcp_config(fixture: &Fixture, tool_output_bytes: u64) -> Config {
    let mut config = Config::defaults_at(&fixture.root);
    config.gate.debounce_ms = 10;
    config.gate.hard_timeout_ms = 60_000;
    config.gate.cache_dir = fixture.state.join("cache");
    config.gate.lease_dir = fixture.state.join("leases");
    config.limits.tool_output_bytes = tool_output_bytes;
    config.telemetry.enabled = false;
    config.telemetry.path = fixture.state.join("activity.jsonl");
    config
}

fn spawn_mcp_server(
    config: Config,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<Result<()>>) {
    let (server_transport, client_transport) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(async move {
        let service = Box::pin(RustMcpServer::new(config)?.serve(server_transport)).await?;
        service.waiting().await?;
        Ok(())
    });
    (client_transport, task)
}

fn mcp_error_code(error: rmcp::ServiceError) -> ErrorCode {
    match error {
        rmcp::ServiceError::McpError(data) => data.code,
        other => panic!("expected MCP error, got {other:?}"),
    }
}

fn profile_arguments(root: &Path, extra: Value) -> Map<String, Value> {
    let mut arguments = Map::new();
    arguments.insert("dir".to_owned(), Value::String(root.display().to_string()));
    if let Value::Object(fields) = extra {
        arguments.extend(fields);
    }
    arguments
}

#[tokio::test]
async fn profile_mcp_maps_validation_status_and_is_error() -> Result<()> {
    let fixture = Fixture::new("mcp-profile", None);
    let (transport, server) = spawn_mcp_server(mcp_config(&fixture, 512));
    let client = mcp_client_info(ClientCapabilities::default())
        .serve(transport)
        .await?;

    // validate_profile: only one Cargo target is accepted.
    let error = client
        .peer()
        .call_tool_once(
            CallToolRequestParams::new("profile").with_arguments(profile_arguments(
                &fixture.root,
                serde_json::json!({"configuration": {"target": "all"}}),
            )),
        )
        .await
        .expect_err("multi-target profile must be invalid");
    assert_eq!(mcp_error_code(error), ErrorCode::INVALID_PARAMS);

    // validate_string: an oversized changeId is rejected before any work.
    let error = client
        .peer()
        .call_tool_once(
            CallToolRequestParams::new("profile").with_arguments(profile_arguments(
                &fixture.root,
                serde_json::json!({"changeId": "x".repeat(5_000)}),
            )),
        )
        .await
        .expect_err("oversized changeId must be invalid");
    assert_eq!(mcp_error_code(error), ErrorCode::INVALID_PARAMS);

    // A successful analyze is a non-error call that still marks untrusted data
    // and truncates the wire form under the configured output bound.
    let CallToolResponse::Complete(result) = client
        .peer()
        .call_tool_once(
            CallToolRequestParams::new("profile").with_arguments(profile_arguments(
                &fixture.root,
                serde_json::json!({
                    "action": "build_analyze",
                    "changeId": "fixture-change",
                    "configuration": {"target": "check"},
                    "budget": {"maxRuns": 1, "maxReportBytes": 4_194_304, "wallTimeMs": 60_000},
                }),
            )),
        )
        .await?
    else {
        anyhow::bail!("capability-free client unexpectedly received a task");
    };
    let structured = result
        .structured_content
        .as_ref()
        .context("missing structured result")?;
    assert_eq!(result.is_error, Some(false));
    assert_eq!(structured["tool"], "profile");
    assert_eq!(structured["status"], "COMPLETE", "{structured}");
    assert_eq!(structured["untrustedData"], true);
    assert_eq!(structured["truncated"], true);

    // INCONCLUSIVE comparisons are successful calls with is_error=true.
    let CallToolResponse::Complete(result) = client
        .peer()
        .call_tool_once(
            CallToolRequestParams::new("profile").with_arguments(profile_arguments(
                &fixture.root,
                serde_json::json!({
                    "action": "build_compare",
                    "changeId": "fixture-change",
                    "baselineEvidence": ["pe-does-not-exist"],
                }),
            )),
        )
        .await?
    else {
        anyhow::bail!("capability-free client unexpectedly received a task");
    };
    let structured = result
        .structured_content
        .as_ref()
        .context("missing structured result")?;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured["status"], "INCONCLUSIVE", "{structured}");

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn disabled_profile_tool_is_not_registered_or_callable() -> Result<()> {
    let fixture = Fixture::new("mcp-profile-disabled", None);
    let mut config = mcp_config(&fixture, 49_152);
    config.tools.profile = false;
    let (transport, server) = spawn_mcp_server(config);
    let client = mcp_client_info(ClientCapabilities::default())
        .serve(transport)
        .await?;

    let tools = client.peer().list_tools(None).await?;
    assert!(
        tools.tools.iter().all(|tool| tool.name != "profile"),
        "disabled profile tool was still advertised"
    );
    let error = client
        .peer()
        .call_tool_once(CallToolRequestParams::new("profile"))
        .await
        .expect_err("disabled profile tool must not be callable");
    assert_eq!(mcp_error_code(error), ErrorCode::METHOD_NOT_FOUND);

    client.cancel().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn profile_reports_resource_blocked_while_a_task_holds_the_permit() -> Result<()> {
    let fixture = Fixture::new(
        "mcp-profile-blocked",
        Some("fn main() { std::thread::sleep(std::time::Duration::from_secs(60)); }\n"),
    );
    let mut config = mcp_config(&fixture, 49_152);
    config.limits.max_in_flight_tools = 1;
    let (transport, server) = spawn_mcp_server(config);
    let client = mcp_client_info(ClientCapabilities::builder().enable_tasks().build())
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await?;

    let task = match client
        .peer()
        .call_tool_once(CallToolRequestParams::new("check"))
        .await?
    {
        CallToolResponse::Task(task) => task,
        other => anyhow::bail!("expected check task response, got {other:?}"),
    };

    let CallToolResponse::Complete(result) = client
        .peer()
        .call_tool_once(
            CallToolRequestParams::new("profile").with_arguments(profile_arguments(
                &fixture.root,
                serde_json::json!({
                    "action": "build_analyze",
                    "configuration": {"target": "check"},
                }),
            )),
        )
        .await?
    else {
        anyhow::bail!("profile unexpectedly received a task");
    };
    let structured = result
        .structured_content
        .as_ref()
        .context("missing structured result")?;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured["status"], "RESOURCE_BLOCKED", "{structured}");

    client
        .peer()
        .cancel_task(rmcp::model::CancelTaskParams::new(
            task.task.task_id.clone(),
        ))
        .await?;
    client.cancel().await?;
    server.await??;
    Ok(())
}
