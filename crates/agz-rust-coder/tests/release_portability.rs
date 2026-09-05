use agz_rust_coder::gate::lease::{LeaseError, acquire_lease_with_timeout};
use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[tokio::test]
async fn a_live_lease_is_not_reclaimed_and_release_allows_reacquisition() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = fs::canonicalize(std::env::temp_dir())
        .expect("canonical temp")
        .join(format!(
            "agz-lease-regression-{}-{stamp}",
            std::process::id()
        ));
    fs::create_dir(&root).expect("create lease directory");
    let mut first = acquire_lease_with_timeout(&root, "same-key", Duration::from_secs(2), None)
        .await
        .expect("first lease");
    let second =
        acquire_lease_with_timeout(&root, "same-key", Duration::from_millis(200), None).await;
    assert!(
        matches!(second, Err(LeaseError::TimedOut { .. })),
        "live lease was stolen: {second:?}"
    );
    first.release();
    let mut third = acquire_lease_with_timeout(&root, "same-key", Duration::from_secs(2), None)
        .await
        .expect("lease after release");
    third.release();
    fs::remove_dir_all(root).expect("remove lease fixture");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_job_can_be_reaped_after_leader_polling() {
    use agz_rust_coder::process::{ProcessRunOptions, ProcessSupervisor};
    let supervisor = ProcessSupervisor::without_journal();
    let executable = std::env::var_os("ComSpec").expect("Windows command processor");
    let cwd = fs::canonicalize(std::env::temp_dir()).expect("canonical cwd");
    for _ in 0..20 {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            supervisor.run(
                std::path::PathBuf::from(&executable),
                ["/D", "/C", "exit 0"],
                ProcessRunOptions::new(&cwd)
                    .with_environment(std::env::vars_os())
                    .with_timeout(Duration::from_secs(2)),
            ),
        )
        .await
        .expect("bounded completed job")
        .expect("launch completed job");
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert!(
            result.cleanup_complete && result.drain_complete,
            "{result:?}"
        );
        assert_eq!(supervisor.active_count(), 0);
    }
    assert_eq!(supervisor.close().await.remaining, 0);
}
