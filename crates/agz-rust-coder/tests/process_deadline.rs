#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_later_request_deadline_does_not_disable_the_process_timeout() {
    use std::time::{Duration, Instant};

    use agz_rust_coder::process::{ProcessRunOptions, ProcessSupervisor};

    let supervisor = ProcessSupervisor::without_journal();
    let options = ProcessRunOptions::new("/tmp")
        .with_timeout(Duration::from_millis(100))
        .with_deadline(Instant::now() + Duration::from_secs(30));
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        supervisor.run("/bin/sh", ["-c", "/bin/sleep 30 & wait"], options),
    )
    .await
    .expect("the shorter process timeout must win")
    .expect("supervised command");
    assert!(result.timed_out);
    assert!(result.cleanup_complete);
    assert!(result.drain_complete);
    assert_eq!(supervisor.active_count(), 0);
}
