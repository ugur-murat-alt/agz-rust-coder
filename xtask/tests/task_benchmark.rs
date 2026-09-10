#[path = "../src/evidence.rs"]
mod evidence;

#[path = "../src/benchmark_tasks.rs"]
mod benchmark_tasks;

use std::{collections::BTreeMap, path::PathBuf};

use benchmark_tasks::{BenchmarkArm, evaluate_provider_free_replay};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask parent")
        .to_path_buf()
}

#[tokio::test]
async fn frozen_corpus_replay_is_paired_balanced_and_retains_negative_outcomes() {
    let evidence = evaluate_provider_free_replay(&repository_root())
        .await
        .expect("provider-free task benchmark replay");

    assert!(evidence.passed);
    assert!(evidence.order_balanced);
    assert_eq!(evidence.total_trials, 72);
    assert_eq!(evidence.retained_failed_trials, 4);
    assert_eq!(evidence.retained_timeout_trials, 1);
    assert_eq!(evidence.retained_cancelled_trials, 1);
    assert!(!evidence.gate.default_on_eligible);
    assert!(evidence.gate.quality_pass);
    assert!(evidence.gate.host_turn_pass);
    assert!(evidence.gate.cargo_call_pass);
    assert!(evidence.gate.wall_time_pass);

    let summaries = evidence
        .summaries
        .iter()
        .map(|summary| (summary.arm, summary))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(summaries[&BenchmarkArm::ShellFiles].successes, 22);
    assert_eq!(summaries[&BenchmarkArm::Mcp020].successes, 23);
    assert_eq!(summaries[&BenchmarkArm::ChangeEngine].successes, 23);
    assert!(summaries.values().all(|summary| {
        summary.success_rate_wilson_95_low_percent <= summary.success_rate_percent
            && summary.success_rate_percent <= summary.success_rate_wilson_95_high_percent
    }));
}

#[tokio::test]
async fn public_adapter_request_contains_no_oracle_or_hidden_test_material() {
    let evidence = evaluate_provider_free_replay(&repository_root())
        .await
        .expect("provider-free task benchmark replay");

    for trial in &evidence.trials {
        let request = serde_json::to_string(&trial.request).expect("serialize public request");
        let request = request.to_ascii_lowercase();
        assert!(!request.contains("oracle"));
        assert!(!request.contains("hidden"));
        assert!(!request.contains("expected_solution"));
    }
}

#[tokio::test]
async fn independent_oracle_overrides_agent_claims_and_mutation_shortcuts() {
    let evidence = evaluate_provider_free_replay(&repository_root())
        .await
        .expect("provider-free task benchmark replay");

    let hidden_failure = evidence
        .trials
        .iter()
        .find(|trial| {
            trial.request.task_id == "feature-only"
                && trial.request.repetition == 2
                && trial.request.arm == BenchmarkArm::Mcp020
        })
        .expect("hidden-test negative control");
    assert!(hidden_failure.adapter.claimed_pass);
    assert!(!hidden_failure.oracle.hidden_tests_passed);
    assert!(!hidden_failure.success);

    let mutation_failure = evidence
        .trials
        .iter()
        .find(|trial| {
            trial.request.task_id == "regression-test"
                && trial.request.repetition == 2
                && trial.request.arm == BenchmarkArm::ChangeEngine
        })
        .expect("mutation-guard negative control");
    assert!(mutation_failure.adapter.claimed_pass);
    assert!(!mutation_failure.oracle.mutation_guard_intact);
    assert!(!mutation_failure.success);
}

#[tokio::test]
async fn unavailable_usage_and_cost_are_unknown_instead_of_zero() {
    let evidence = evaluate_provider_free_replay(&repository_root())
        .await
        .expect("provider-free task benchmark replay");

    for trial in &evidence.trials {
        assert_eq!(trial.adapter.tokens.input.as_str(), Some("unknown"));
        assert_eq!(trial.adapter.tokens.output.as_str(), Some("unknown"));
        assert_eq!(trial.adapter.tokens.cache.as_str(), Some("unknown"));
        assert_eq!(trial.adapter.tokens.schema.as_str(), Some("unknown"));
        assert_eq!(trial.adapter.cost_usd.as_str(), Some("unknown"));
    }
}

#[tokio::test]
async fn every_arm_occupies_every_order_position_for_each_task() {
    let evidence = evaluate_provider_free_replay(&repository_root())
        .await
        .expect("provider-free task benchmark replay");
    let mut positions: BTreeMap<(String, BenchmarkArm), Vec<u32>> = BTreeMap::new();
    for trial in &evidence.trials {
        positions
            .entry((trial.request.task_id.clone(), trial.request.arm))
            .or_default()
            .push(trial.order_index);
    }

    for values in positions.values_mut() {
        values.sort_unstable();
        assert_eq!(values, &[0, 1, 2]);
    }
}
