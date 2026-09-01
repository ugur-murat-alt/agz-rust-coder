#[path = "../src/mcp.rs"]
mod mcp;

#[path = "../src/benchmark.rs"]
mod benchmark;

#[path = "../src/child_process.rs"]
mod child_process;

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use benchmark::{
    BenchmarkCounters, FrozenFixture, FrozenRunManifest, LiveFuture, LivePreflight,
    LiveTrialAdapter, LiveTrialRequest, LiveTrialResult, OutcomeComparison, TypedOutcome,
    classify_cargo_oracle, compare_typed_outcomes, guard_live_run, publish_evidence,
    relevant_source_checksum, remove_live_checkpoints, run_benchmark_smoke_with,
    run_live_benchmark_with_adapter, smoke_run_id, smoke_run_manifest, validate_legacy_manifest,
    validate_live_trial_result,
};
use serde_json::json;

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask parent")
        .to_path_buf()
}

#[test]
fn live_checkpoint_cleanup_distinguishes_missing_and_unsafe_roots() {
    let root = std::env::temp_dir().join(format!(
        "stage7-checkpoint-cleanup-test-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);

    remove_live_checkpoints(&root, "missing").expect("missing checkpoint root is clean");

    let results = root.join("benchmark/results");
    let stage7 = results.join("stage7");
    fs::create_dir_all(&results).expect("create benchmark results parent");
    fs::write(&stage7, b"not a directory").expect("write unsafe checkpoint root");
    assert!(remove_live_checkpoints(&root, "file").is_err());
    fs::remove_file(&stage7).expect("remove unsafe checkpoint file");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("missing-target"), &stage7)
            .expect("create dangling checkpoint symlink");
        assert!(remove_live_checkpoints(&root, "symlink").is_err());
        fs::remove_file(&stage7).expect("remove checkpoint symlink");
    }

    fs::remove_dir_all(root).expect("remove checkpoint cleanup test root");
}

#[test]
fn live_guard_rejects_before_side_effects() {
    let root = std::env::temp_dir().join(format!("stage7-guard-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).expect("guard test directory");
    let sentinel = root.join("sentinel");
    fs::write(&sentinel, b"keep").expect("sentinel");

    let result = guard_live_run(["--live"]);

    assert!(result.is_err());
    assert_eq!(fs::read(&sentinel).expect("sentinel remains"), b"keep");
    assert!(!root.join("benchmark").exists());
    fs::remove_dir_all(root).expect("remove guard test directory");
}

#[test]
fn live_guard_requires_manifest_and_separate_approval() {
    assert!(guard_live_run(["--live", "--approve-live"]).is_err());
    assert!(guard_live_run(["--live", "--manifest", "missing.json"]).is_err());
}

#[test]
fn live_guard_accepts_only_a_complete_frozen_manifest() {
    let root = std::env::temp_dir().join(format!("stage7-manifest-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).expect("manifest test directory");
    let manifest = root.join("frozen.json");
    fs::write(
        &manifest,
        r#"{
          "schema_version": 1,
          "frozen": true,
          "run_id": "stage7-live-test",
          "source_commit": "0123456789abcdef0123456789abcdef01234567",
          "source_checksum": "0000000000000000000000000000000000000000000000000000000000000000",
          "provider": "provider-under-test",
          "model": "model-under-test",
          "variant": null,
          "arms": ["off", "rust_mcp"],
          "fixtures": [{"id": "clean", "checksum": "1111111111111111111111111111111111111111111111111111111111111111"}],
          "repetitions": 3,
          "order_seed": 7,
          "scorer_version": "stage7-test",
          "estimated_cost": 0,
          "estimated_duration": 1,
          "non_inferiority_margin": 0,
          "cache_strata": "isolated"
        }"#,
    )
    .expect("write frozen manifest");

    let decision = guard_live_run([
        "--live".to_owned(),
        "--manifest".to_owned(),
        manifest.to_string_lossy().into_owned(),
        "--approve-live".to_owned(),
    ])
    .expect("complete frozen manifest");

    assert!(decision.is_live());
    fs::remove_dir_all(root).expect("remove manifest test directory");
}

#[tokio::test]
async fn clean_and_broken_oracles_are_typed_and_sources_stay_unchanged() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_adapter = Arc::clone(&calls);
    let evidence = run_benchmark_smoke_with(repository_root().as_path(), move |directory| {
        calls_for_adapter.fetch_add(1, Ordering::SeqCst);
        let clean = fs::read_to_string(directory.join("Cargo.toml"))
            .expect("fixture manifest")
            .contains("stage7-clean");
        let status = if clean { "FAST_PASS" } else { "FAIL" };
        async move {
            Ok(TypedOutcome {
                status: status.to_owned(),
                is_error: false,
                structured_text_equivalent: true,
                reason: "mock oracle".to_owned(),
            })
        }
    })
    .await
    .expect("benchmark smoke");

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(evidence.fixtures.len(), 2);
    assert_eq!(evidence.fixtures[0].expected_status, "FAST_PASS");
    assert_eq!(evidence.fixtures[1].expected_status, "FAIL");
    assert!(
        evidence
            .fixtures
            .iter()
            .flat_map(|fixture| &fixture.trials)
            .all(|trial| trial.source_unchanged)
    );
    assert!(
        evidence
            .fixtures
            .iter()
            .flat_map(|fixture| &fixture.trials)
            .all(|trial| trial.source_hash_before == trial.source_hash_after)
    );
    assert_eq!(evidence.counters, BenchmarkCounters::default());
    assert!(
        evidence
            .warnings
            .iter()
            .any(|warning| warning.contains("three-repetitions"))
    );

    let one_run_id = smoke_run_manifest(&repository_root(), &evidence)
        .expect("one-repetition manifest")["run_id"]
        .as_str()
        .expect("one-repetition run id")
        .to_owned();
    let mut repeated = evidence.clone();
    repeated.repetitions = 2;
    let two_run =
        smoke_run_manifest(&repository_root(), &repeated).expect("two-repetition manifest");
    let two_run_id = two_run["run_id"].as_str().expect("two-repetition run id");
    assert_ne!(one_run_id, two_run_id);
}

#[test]
fn cargo_oracle_requires_a_real_compiler_error_for_nonzero_exit() {
    let compiler_message = br#"{"reason":"compiler-message","message":{"level":"error"}}"#;
    let failure = classify_cargo_oracle(false, compiler_message, b"could not compile")
        .expect("compiler failure");
    assert_eq!(failure.status, "FAIL");
    assert!(!failure.is_error);

    let infrastructure = classify_cargo_oracle(
        false,
        br#"{"reason":"build-finished","success":false}"#,
        b"toolchain unavailable",
    );
    assert!(infrastructure.is_err());
}

#[test]
fn smoke_run_identity_binds_commit_and_dirty_state() {
    let arms = vec!["off".to_owned(), "rust_mcp".to_owned()];
    let clean = smoke_run_id(
        "0123456789abcdef0123456789abcdef01234567",
        false,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        1,
        &arms,
        "stage7-typed-oracle-v6",
    );
    let different_commit = smoke_run_id(
        "fedcba9876543210fedcba9876543210fedcba98",
        false,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        1,
        &arms,
        "stage7-typed-oracle-v6",
    );
    let dirty = smoke_run_id(
        "0123456789abcdef0123456789abcdef01234567",
        true,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        1,
        &arms,
        "stage7-typed-oracle-v6",
    );

    assert!(clean.starts_with("stage7-smoke-v6-"));
    assert_ne!(clean, different_commit);
    assert_ne!(clean, dirty);
}

#[tokio::test]
async fn source_checksum_changes_when_harness_source_changes() {
    let evidence = run_benchmark_smoke_with(repository_root().as_path(), |directory| {
        let clean = fs::read_to_string(directory.join("Cargo.toml"))
            .expect("fixture manifest")
            .contains("stage7-clean");
        async move {
            Ok(TypedOutcome {
                status: if clean { "FAST_PASS" } else { "FAIL" }.to_owned(),
                is_error: false,
                structured_text_equivalent: true,
                reason: String::new(),
            })
        }
    })
    .await
    .expect("benchmark evidence");
    let source = repository_root();
    let first = relevant_source_checksum(&source, &evidence).expect("first source checksum");

    let temp = std::env::temp_dir().join(format!("stage7-source-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(temp.join("crates/agz-rust-coder/src")).expect("crate source dir");
    fs::create_dir_all(temp.join("xtask/src")).expect("xtask source dir");
    fs::create_dir_all(temp.join("xtask/tests")).expect("xtask tests dir");
    fs::copy(source.join("Cargo.toml"), temp.join("Cargo.toml")).expect("copy manifest");
    fs::copy(source.join("Cargo.lock"), temp.join("Cargo.lock")).expect("copy lockfile");
    fs::write(temp.join("crates/agz-rust-coder/src/lib.rs"), "one\n").expect("crate source");
    fs::write(temp.join("xtask/src/main.rs"), "one\n").expect("xtask source");
    fs::write(temp.join("xtask/tests/test.rs"), "one\n").expect("xtask test");
    let second = relevant_source_checksum(&temp, &evidence).expect("second source checksum");
    fs::write(temp.join("xtask/src/main.rs"), "two\n").expect("mutate xtask source");
    let third = relevant_source_checksum(&temp, &evidence).expect("third source checksum");

    assert_ne!(first, second);
    assert_ne!(second, third);
    fs::remove_dir_all(temp).expect("remove source checksum fixture");
}

#[test]
fn unavailable_and_wrong_typed_outcomes_are_not_equal() {
    let unavailable = TypedOutcome {
        status: "UNAVAILABLE".to_owned(),
        is_error: true,
        structured_text_equivalent: true,
        reason: "unavailable".to_owned(),
    };
    let wrong = TypedOutcome {
        status: "FAIL".to_owned(),
        is_error: false,
        structured_text_equivalent: true,
        reason: "wrong".to_owned(),
    };
    assert_eq!(
        compare_typed_outcomes(
            &TypedOutcome {
                status: "FAST_PASS".to_owned(),
                is_error: false,
                structured_text_equivalent: true,
                reason: String::new(),
            },
            &unavailable,
        ),
        OutcomeComparison::Mismatch
    );
    assert_eq!(
        compare_typed_outcomes(
            &TypedOutcome {
                status: "FAST_PASS".to_owned(),
                is_error: false,
                structured_text_equivalent: true,
                reason: String::new(),
            },
            &wrong,
        ),
        OutcomeComparison::Mismatch
    );
}

#[test]
fn compiler_failure_requires_non_error_typed_parity() {
    let expected = TypedOutcome {
        status: "FAIL".to_owned(),
        is_error: false,
        structured_text_equivalent: true,
        reason: String::new(),
    };
    let mut actual = expected.clone();
    assert_eq!(
        compare_typed_outcomes(&expected, &actual),
        OutcomeComparison::Equivalent
    );
    actual.is_error = true;
    assert_eq!(
        compare_typed_outcomes(&expected, &actual),
        OutcomeComparison::Mismatch
    );
}

#[test]
fn validation_authority_status_must_match_exactly() {
    let expected = TypedOutcome {
        status: "FAST_PASS".to_owned(),
        is_error: false,
        structured_text_equivalent: true,
        reason: String::new(),
    };
    let actual = TypedOutcome {
        status: "FULL_PASS".to_owned(),
        ..expected.clone()
    };
    assert_eq!(
        compare_typed_outcomes(&expected, &actual),
        OutcomeComparison::Mismatch
    );
}

#[test]
fn live_trial_status_and_passed_flag_must_agree() {
    for (status, passed) in [("FAIL", true), ("PASS", false)] {
        let result = LiveTrialResult {
            schema_version: 1,
            status: status.to_owned(),
            passed,
            scorer_version: "stage7-test".to_owned(),
            duration_ms: 1,
            model_requests: 1,
            provider_requests: 1,
            paid_requests: 1,
        };
        assert!(validate_live_trial_result(&result, "stage7-test").is_err());
    }
}

#[test]
fn legacy_manifest_is_validated_without_execution() {
    let evidence = validate_legacy_manifest().expect("legacy manifest");
    assert!(evidence.schema_valid);
    assert!(evidence.checksum_valid);
    assert!(!evidence.executed);
}

#[test]
fn evidence_publication_is_atomic_and_private() {
    let root = std::env::temp_dir().join(format!("stage7-evidence-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).expect("evidence test directory");
    let run = json!({"schema_version": 1, "run_id": "test-bundle"});
    let results = json!({"status": "pass", "provider": null, "model": null, "paid": 0});
    let report = "# Evidence\n\nNo provider or model was used.\n";
    let published = publish_evidence(&root, &run, &results, report).expect("publish evidence");
    assert!(published.join("run.json").is_file());
    assert!(published.join("results.json").is_file());
    assert!(published.join("report.md").is_file());
    let entries: Vec<_> = fs::read_dir(published.parent().expect("results parent"))
        .expect("read result directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(
        entries
            .iter()
            .all(|entry| !entry.to_string_lossy().ends_with(".tmp"))
    );
    let second = publish_evidence(&root, &run, &results, report).expect("idempotent publish");
    assert_eq!(published, second);
    fs::remove_dir_all(root).expect("remove evidence test directory");
}

struct FakeLiveAdapter {
    calls: AtomicUsize,
}

impl LiveTrialAdapter for FakeLiveAdapter {
    fn preflight<'a>(
        &'a self,
        _root: &'a std::path::Path,
        manifest: &'a FrozenRunManifest,
    ) -> LiveFuture<'a, LivePreflight> {
        Box::pin(async move {
            Ok(LivePreflight {
                schema_version: 1,
                ready: true,
                provider: manifest.provider.clone(),
                model: manifest.model.clone(),
                scorer_version: manifest.scorer_version.clone(),
                model_requests: 0,
                provider_requests: 0,
                paid_requests: 0,
            })
        })
    }

    fn run_trial<'a>(&'a self, request: &'a LiveTrialRequest) -> LiveFuture<'a, LiveTrialResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(LiveTrialResult {
                schema_version: 1,
                status: "PASS".to_owned(),
                passed: true,
                scorer_version: request.scorer_version.clone(),
                duration_ms: 1,
                model_requests: 1,
                provider_requests: 1,
                paid_requests: 1,
            })
        })
    }
}

#[tokio::test]
async fn approved_live_runner_preflights_orders_checkpoints_and_preserves_sources() {
    let root = repository_root();
    let evidence = run_benchmark_smoke_with(root.as_path(), |directory| {
        let clean = fs::read_to_string(directory.join("Cargo.toml"))
            .expect("fixture manifest")
            .contains("stage7-clean");
        async move {
            Ok(TypedOutcome {
                status: if clean { "FAST_PASS" } else { "FAIL" }.to_owned(),
                is_error: false,
                structured_text_equivalent: true,
                reason: String::new(),
            })
        }
    })
    .await
    .expect("benchmark evidence");
    let source_commit = String::from_utf8(
        std::process::Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git source commit")
            .stdout,
    )
    .expect("UTF-8 source commit")
    .trim()
    .to_owned();
    let manifest = FrozenRunManifest {
        schema_version: 1,
        frozen: true,
        run_id: format!("stage7-live-adapter-test-{}", std::process::id()),
        source_commit,
        source_checksum: relevant_source_checksum(&root, &evidence).expect("live source checksum"),
        provider: "fake-provider".to_owned(),
        model: "fake-model".to_owned(),
        variant: None,
        arms: vec!["off".to_owned(), "rust_mcp".to_owned()],
        fixtures: evidence
            .fixtures
            .iter()
            .map(|fixture| FrozenFixture {
                id: fixture.id.clone(),
                checksum: fixture.fixture_hash.clone(),
            })
            .collect(),
        repetitions: 1,
        order_seed: 17,
        scorer_version: "stage7-test-scorer".to_owned(),
    };
    remove_live_checkpoints(&root, &manifest.run_id).expect("clear stale live checkpoints");
    let adapter = FakeLiveAdapter {
        calls: AtomicUsize::new(0),
    };
    let result = run_live_benchmark_with_adapter(&root, &manifest, &adapter)
        .await
        .expect("live benchmark with fake adapter");

    assert_eq!(result.status, "PASS");
    assert_eq!(result.trials.len(), 4);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 4);
    assert!(result.trials.iter().all(|trial| trial.source_unchanged));
    assert_eq!(result.counters.model, 4);
    assert_eq!(result.counters.provider, 4);
    assert_eq!(result.counters.paid, 4);
    remove_live_checkpoints(&root, &manifest.run_id).expect("remove live checkpoints");
}
