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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pre_cancelled_or_expired_commands_never_start() {
    use agz_rust_coder::process::{ProcessError, ProcessRunOptions, ProcessSupervisor};
    use std::time::{Duration, Instant};
    let supervisor = ProcessSupervisor::without_journal();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let cancelled = supervisor
        .run(
            "/bin/sh",
            ["-c", "exit 99"],
            ProcessRunOptions::new("/tmp").with_cancellation(token),
        )
        .await;
    assert!(matches!(cancelled, Err(ProcessError::Cancelled)));
    let expired = supervisor
        .run(
            "/bin/sh",
            ["-c", "exit 99"],
            ProcessRunOptions::new("/tmp")
                .with_deadline(Instant::now().checked_sub(Duration::from_secs(1)).unwrap()),
        )
        .await;
    assert!(matches!(expired, Err(ProcessError::TimedOut)));
    let overflow = supervisor
        .run(
            "/bin/sh",
            ["-c", "exit 99"],
            ProcessRunOptions::new("/tmp").with_timeout(Duration::MAX),
        )
        .await;
    assert!(matches!(
        overflow,
        Err(ProcessError::InvalidSpecification(_))
    ));
    assert_eq!(supervisor.active_count(), 0);
}
