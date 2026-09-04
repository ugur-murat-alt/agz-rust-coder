#[cfg(target_os = "linux")]
mod linux_process_supervisor {
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        process::Stdio,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use agz_rust_coder::process::{
        JournalRecord, ProcessError, ProcessGroupIdentity, ProcessJournal, ProcessRunOptions,
        ProcessSupervisor, RecoveryDisposition,
    };
    use agz_rust_coder::workspace::RootGuard;
    use tokio_util::sync::CancellationToken;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/process")
            .join(name)
    }

    fn options(cwd: &Path, timeout: Duration) -> ProcessRunOptions {
        ProcessRunOptions {
            cwd: cwd.to_owned(),
            env: BTreeMap::new(),
            timeout,
            deadline: None,
            cancel: None,
            max_output_bytes: 1024,
            kill_grace: Duration::from_millis(50),
            cleanup_timeout: Duration::from_secs(2),
            diagnostic_callback: None,
        }
    }

    fn unique_directory(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).expect("create test directory");
        path
    }

    fn process_start_time(pid: u32) -> u64 {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("read process stat");
        let end_of_name = stat.rfind(')').expect("process name in stat");
        stat[end_of_name + 1..]
            .split_whitespace()
            .nth(19)
            .expect("process start time in stat")
            .parse()
            .expect("numeric process start time")
    }

    fn process_is_live(pid: u32) -> bool {
        let path = format!("/proc/{pid}/stat");
        let Ok(stat) = fs::read_to_string(path) else {
            return false;
        };
        let Some(end_of_name) = stat.rfind(')') else {
            return false;
        };
        stat[end_of_name + 1..]
            .split_whitespace()
            .next()
            .is_some_and(|state| state != "Z")
    }

    async fn wait_until_gone(pid: u32) {
        for _ in 0..100 {
            if !process_is_live(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!process_is_live(pid), "process {pid} is still live");
    }

    fn args(parts: impl IntoIterator<Item = PathBuf>) -> Vec<OsString> {
        parts.into_iter().map(PathBuf::into_os_string).collect()
    }

    #[tokio::test]
    async fn drains_both_streams_and_reports_a_clean_first_diagnostic() {
        let root = unique_directory("mcp-process-drain");
        let runner = ProcessSupervisor::without_journal();
        let callback_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_seen_by_runner = std::sync::Arc::clone(&callback_seen);
        let mut run_options = options(&root, Duration::from_secs(5));
        run_options.max_output_bytes = 512;
        run_options.diagnostic_callback = Some(std::sync::Arc::new(move |line| {
            let matched = line == "FINAL-STDERR";
            if matched {
                callback_seen_by_runner.store(true, std::sync::atomic::Ordering::Release);
            }
            matched
        }));
        let result = runner
            .run("/bin/sh", args([fixture("flood-output.sh")]), run_options)
            .await
            .expect("run output fixture");

        assert_eq!(result.exit_code, 0);
        assert!(result.drain_complete);
        assert!(result.cleanup_complete);
        assert!(result.output_truncated);
        assert!(result.stderr.contains("FINAL-STDERR"));
        assert!(!result.output.contains('\u{1b}'));
        assert!(result.first_diagnostic_ms.is_some());
        assert!(callback_seen.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(runner.active_count(), 0);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[tokio::test]
    async fn timeout_kills_the_complete_process_group() {
        let root = unique_directory("mcp-process-timeout");
        let pid_file = root.join("descendant.pid");
        let runner = ProcessSupervisor::without_journal();
        let result = runner
            .run(
                "/bin/sh",
                args([fixture("spawn-descendant.sh"), pid_file.clone()]),
                options(&root, Duration::from_millis(150)),
            )
            .await
            .expect("run descendant fixture");

        assert!(result.timed_out);
        assert_eq!(result.exit_code, 124);
        let child_pid = fs::read_to_string(pid_file)
            .expect("descendant PID file")
            .trim()
            .parse()
            .expect("numeric descendant PID");
        wait_until_gone(child_pid).await;
        assert_eq!(runner.active_count(), 0);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[tokio::test]
    async fn timeout_waits_for_force_kill_after_the_leader_exits() {
        let root = unique_directory("mcp-process-early-leader-exit");
        let pid_file = root.join("descendant.pid");
        let runner = ProcessSupervisor::without_journal();
        let result = runner
            .run(
                "/bin/sh",
                args([
                    fixture("leader-exits-descendant-ignores-term.sh"),
                    pid_file.clone(),
                ]),
                options(&root, Duration::from_millis(100))
                    .with_kill_grace(Duration::from_millis(100)),
            )
            .await
            .expect("run TERM-resistant descendant fixture");

        assert!(result.timed_out);
        assert!(result.cleanup_complete);
        assert!(result.drain_complete);
        let child_pid = fs::read_to_string(pid_file)
            .expect("descendant PID file")
            .trim()
            .parse()
            .expect("numeric descendant PID");
        wait_until_gone(child_pid).await;
        assert_eq!(runner.active_count(), 0);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[tokio::test]
    async fn cooperative_cancel_is_distinct_and_cleans_descendants() {
        let root = unique_directory("mcp-process-cancel");
        let pid_file = root.join("descendant.pid");
        let token = CancellationToken::new();
        let runner = ProcessSupervisor::without_journal();
        let task_runner = runner.clone();
        let task_token = token.clone();
        let task_root = root.clone();
        let task_pid_file = pid_file.clone();
        let pending = tokio::spawn(async move {
            task_runner
                .run(
                    "/bin/sh",
                    args([fixture("spawn-descendant.sh"), task_pid_file]),
                    ProcessRunOptions::new(task_root)
                        .with_timeout(Duration::from_secs(60))
                        .with_cancellation(task_token)
                        .with_kill_grace(Duration::from_millis(50)),
                )
                .await
        });
        for _ in 0..100 {
            if fs::metadata(root.join("descendant.pid")).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        token.cancel();
        let result = pending
            .await
            .expect("join process task")
            .expect("run cancellation fixture");

        assert!(result.cancelled);
        assert!(!result.timed_out);
        let child_pid = fs::read_to_string(root.join("descendant.pid"))
            .expect("descendant PID file")
            .trim()
            .parse()
            .expect("numeric descendant PID");
        wait_until_gone(child_pid).await;
        assert_eq!(runner.active_count(), 0);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[tokio::test]
    async fn authorized_root_replacement_fails_before_the_target_starts() {
        let root = unique_directory("mcp-process-root-binding");
        let marker = root.join("replacement-ran");
        let authority = RootGuard::new([root.clone()], std::iter::empty())
            .expect("authorize root")
            .configured_roots()[0]
            .clone();
        let original = root.with_extension("original");
        fs::rename(&root, &original).expect("rename authorized root");
        fs::create_dir(&root).expect("create replacement root");
        let result = ProcessSupervisor::without_journal()
            .run_authorized(
                "/bin/sh",
                [
                    OsString::from("-c"),
                    OsString::from("touch replacement-ran"),
                ],
                options(&root, Duration::from_secs(2)),
                authority,
            )
            .await
            .expect("guard process starts");
        assert_ne!(result.exit_code, 0);
        assert!(!marker.exists(), "replacement target must not run");
        fs::remove_dir_all(&root).expect("remove replacement root");
        fs::remove_dir_all(&original).expect("remove original root");
    }

    #[tokio::test]
    async fn authorized_launch_rejects_a_guard_binary_inside_its_root() {
        let guard = PathBuf::from(env!("CARGO_BIN_EXE_agz-rust-coder"));
        let root = guard
            .parent()
            .expect("guard binary has a parent")
            .to_owned();
        let authority = RootGuard::new([root.clone()], std::iter::empty())
            .expect("authorize guard parent")
            .configured_roots()[0]
            .clone();

        let result = ProcessSupervisor::without_journal()
            .run_authorized(
                "/bin/sh",
                [OsString::from("-c"), OsString::from("exit 0")],
                options(&root, Duration::from_secs(2)),
                authority,
            )
            .await;

        assert!(matches!(result, Err(ProcessError::RootBinding(_))));
    }

    #[tokio::test]
    async fn root_contained_executable_uses_the_verified_directory_after_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_directory("mcp-process-executable-binding");
        let executable = root.join("analyzer");
        let original_marker = root.with_extension("original-marker");
        let replacement_marker = root.with_extension("replacement-marker");
        let ready = root.with_extension("ready");
        let continue_file = root.with_extension("continue");
        fs::write(&executable, "#!/bin/sh\nprintf original > \"$1\"\n")
            .expect("write original executable");
        let mut permissions = fs::metadata(&executable)
            .expect("original executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).expect("make original executable runnable");
        let authority = RootGuard::new([root.clone()], std::iter::empty())
            .expect("authorize root")
            .configured_roots()[0]
            .clone();
        let mut run_options = options(&root, Duration::from_secs(5));
        run_options.env.insert(
            OsString::from("AGZ_RUST_CODER_GUARD_TEST_READY"),
            ready.as_os_str().to_owned(),
        );
        run_options.env.insert(
            OsString::from("AGZ_RUST_CODER_GUARD_TEST_CONTINUE"),
            continue_file.as_os_str().to_owned(),
        );
        let runner = Arc::new(ProcessSupervisor::without_journal());
        let running = {
            let runner = Arc::clone(&runner);
            let executable = executable.clone();
            let original_marker = original_marker.clone();
            let replacement_marker = replacement_marker.clone();
            tokio::spawn(async move {
                runner
                    .run_authorized(
                        executable,
                        [
                            original_marker.into_os_string(),
                            replacement_marker.into_os_string(),
                        ],
                        run_options,
                        authority,
                    )
                    .await
            })
        };
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(ready.exists(), "guard did not complete its identity check");

        let original = root.with_extension("original");
        fs::rename(&root, &original).expect("move verified root");
        fs::create_dir(&root).expect("create replacement root");
        let replacement = root.join("analyzer");
        fs::write(&replacement, "#!/bin/sh\nprintf replacement > \"$2\"\n")
            .expect("write replacement executable");
        let mut permissions = fs::metadata(&replacement)
            .expect("replacement executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&replacement, permissions)
            .expect("make replacement executable runnable");
        fs::write(&continue_file, b"continue").expect("release guard");

        let result = running
            .await
            .expect("join guarded process")
            .expect("run guarded process");
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            fs::read_to_string(&original_marker).expect("original marker"),
            "original"
        );
        assert!(
            !replacement_marker.exists(),
            "replacement executable must not run"
        );
        assert_eq!(runner.active_count(), 0);
        fs::remove_dir_all(&root).expect("remove replacement root");
        fs::remove_dir_all(&original).expect("remove original root");
        let _ = fs::remove_file(original_marker);
        let _ = fs::remove_file(replacement_marker);
        let _ = fs::remove_file(ready);
        let _ = fs::remove_file(continue_file);
    }

    #[tokio::test]
    async fn recovery_does_not_kill_a_reused_pid_identity() {
        let root = unique_directory("mcp-process-journal");
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn identity fixture");
        let child_pid = child.id();
        let start_time = process_start_time(child_pid);
        let journal = ProcessJournal::new(&root).expect("create process journal");
        let mut record = JournalRecord::new(
            "identity-mismatch",
            child_pid,
            Some(start_time.saturating_add(1)),
            "/bin/sleep",
            ProcessGroupIdentity::Unix { pgid: child_pid },
            "not-the-live-command-hash",
        );
        record.owner_pid = u32::MAX;
        record.owner_start_time = Some(0);
        let path = root.join("jobs/identity-mismatch.json");
        fs::write(
            path,
            serde_json::to_vec(&record).expect("serialize test journal"),
        )
        .expect("write test journal");

        let report = journal.recover_orphans();
        assert!(report.entries.iter().any(|entry| {
            entry.token == "identity-mismatch"
                && entry.disposition == RecoveryDisposition::IdentityMismatch
        }));
        assert!(process_is_live(child_pid));
        child.kill().expect("kill identity fixture");
        child.wait().expect("wait identity fixture");
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[tokio::test]
    async fn recovery_accepts_guard_handoff_after_root_replacement() {
        let root = unique_directory("mcp-process-journal-root");
        let journal_root = unique_directory("mcp-process-journal-handoff");
        let journal = ProcessJournal::new(&journal_root).expect("create process journal");
        let runner = Arc::new(ProcessSupervisor::with_journal(journal));
        let authority = RootGuard::new([root.clone()], std::iter::empty())
            .expect("authorize root")
            .configured_roots()[0]
            .clone();
        let root_argument = root.clone();
        let running = {
            let runner = Arc::clone(&runner);
            let run_root = root.clone();
            tokio::spawn(async move {
                runner
                    .run_authorized(
                        "/bin/sh",
                        [
                            OsString::from("-c"),
                            OsString::from("test -d \"$1\" || exit 1; while :; do sleep 1; done"),
                            OsString::from("agz-root-guard-test"),
                            root_argument.into_os_string(),
                        ],
                        options(&run_root, Duration::from_secs(30)),
                        authority,
                    )
                    .await
            })
        };
        let jobs = journal_root.join("jobs");
        let mut record_path = None;
        for _ in 0..200 {
            record_path = fs::read_dir(&jobs).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.extension().and_then(|extension| extension.to_str()) == Some("json")
                    })
            });
            if record_path.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let record_path = record_path.expect("guarded child journal record");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&record_path).expect("read child journal record"))
                .expect("deserialize child journal record");
        let token = record["token"].as_str().expect("journal token").to_owned();
        // Simulate the owner having crashed; recovery must only target the
        // child group after preserving PID/start-time/group verification.
        record["owner_pid"] = serde_json::json!(u32::MAX);
        record["owner_start_time"] = serde_json::json!(0);
        fs::write(
            &record_path,
            serde_json::to_vec(&record).expect("serialize orphan journal record"),
        )
        .expect("write orphan journal record");

        let original = root.with_extension("original");
        fs::rename(&root, &original).expect("move authorized root");
        fs::create_dir(&root).expect("create replacement root");

        let recovery = ProcessJournal::new(&journal_root)
            .expect("open recovery journal")
            .recover_orphans();
        assert!(recovery.entries.iter().any(|entry| {
            entry.disposition == RecoveryDisposition::Killed && entry.token == token
        }));
        let result = running
            .await
            .expect("join guarded child")
            .expect("guarded child result");
        assert!(result.cleanup_complete);
        assert_eq!(runner.active_count(), 0);
        fs::remove_dir_all(&root).expect("remove replacement root");
        fs::remove_dir_all(&original).expect("remove original root");
        fs::remove_dir_all(&journal_root).expect("remove journal root");
    }

    #[tokio::test]
    async fn incomplete_cleanup_retains_journal_and_registration_for_recovery() {
        let root = unique_directory("mcp-process-incomplete-cleanup");
        let journal = ProcessJournal::new(&root).expect("create process journal");
        let runner = ProcessSupervisor::with_journal(journal)
            .with_shutdown_timeout(Duration::from_millis(20));
        let mut run_options = options(&root, Duration::from_millis(50));
        run_options.kill_grace = Duration::from_millis(20);
        run_options.cleanup_timeout = Duration::ZERO;
        let result = runner
            .run(
                "/bin/sh",
                args([
                    PathBuf::from("-c"),
                    PathBuf::from("trap '' TERM; while :; do sleep 1; done"),
                ]),
                run_options,
            )
            .await
            .expect("run resistant child fixture");

        assert!(result.timed_out);
        assert!(!result.cleanup_complete);
        assert_eq!(runner.active_count(), 1);
        assert!(
            root.join("jobs")
                .join(format!("{}.json", result.token))
                .is_file()
        );

        wait_until_gone(result.child_pid).await;
        let report = runner.close().await;
        assert_eq!(report.requested, 1);
        assert_eq!(report.completed, 0);
        assert_eq!(report.remaining, 1);
        assert!(
            root.join("jobs")
                .join(format!("{}.json", result.token))
                .is_file()
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }
}

#[cfg(unix)]
mod unix_process_journal {
    use std::{
        fs,
        os::unix::fs::symlink,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use agz_rust_coder::process::{JournalError, ProcessJournal};

    fn unique_directory(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).expect("create test directory");
        path
    }

    fn assert_rejected_at_symlink(result: Result<ProcessJournal, JournalError>, link: &Path) {
        match result {
            Err(JournalError::NotDirectory(path)) => assert_eq!(path, link),
            other => panic!("expected symlink ancestor rejection, got {other:?}"),
        }
    }

    #[test]
    fn rejects_an_existing_symlink_ancestor() {
        let root = unique_directory("mcp-process-journal-existing-link");
        let target = unique_directory("mcp-process-journal-existing-target");
        let target_root = target.join("journal");
        fs::create_dir(&target_root).expect("create existing target root");
        fs::create_dir(target_root.join("jobs")).expect("create existing target jobs");
        let link = root.join("link");
        symlink(&target_root, &link).expect("create symlink ancestor");

        assert_rejected_at_symlink(ProcessJournal::new(link.join("journal")), &link);

        fs::remove_file(link).expect("remove symlink");
        fs::remove_dir_all(root).expect("remove test root");
        fs::remove_dir_all(target).expect("remove symlink target");
    }

    #[test]
    fn rejects_a_symlink_ancestor_before_creating_missing_components() {
        let root = unique_directory("mcp-process-journal-missing-link");
        let target = unique_directory("mcp-process-journal-missing-target");
        let link = root.join("link");
        symlink(&target, &link).expect("create symlink ancestor");
        let requested = link.join("missing-parent").join("journal");

        assert_rejected_at_symlink(ProcessJournal::new(requested), &link);
        assert!(!target.join("missing-parent").exists());

        fs::remove_file(link).expect("remove symlink");
        fs::remove_dir_all(root).expect("remove test root");
        fs::remove_dir_all(target).expect("remove symlink target");
    }

    #[test]
    fn creates_missing_components_beneath_a_verified_parent() {
        let root = unique_directory("mcp-process-journal-missing-components");
        let requested = root.join("one").join("two");

        ProcessJournal::new(&requested).expect("create missing journal components");
        assert!(requested.join("jobs").is_dir());

        fs::remove_dir_all(root).expect("remove test directory");
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn process_module_keeps_portable_cfg_boundaries() {
    let _ = agz_rust_coder::process::ProcessSupervisor::without_journal();
    let _ = agz_rust_coder::process::ProcessGroupIdentity::Unmanaged;
}
