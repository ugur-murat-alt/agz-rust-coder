//! Request-scoped Git execution using the same process ownership as Cargo.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use super::{AuthorizedRoot, GitOutput, GitProbe, IdentityError, StdGitProbe};
use crate::process::{ProcessError, ProcessRunOptions, ProcessSupervisor};

#[derive(Debug)]
pub(crate) struct ControlledGitProbe {
    executable: PathBuf,
    deadline: Instant,
    cancellation: CancellationToken,
    supervisor: ProcessSupervisor,
    runtime: Handle,
}

impl ControlledGitProbe {
    pub(crate) fn fixed(
        deadline: Instant,
        cancellation: CancellationToken,
        supervisor: ProcessSupervisor,
        runtime: Handle,
    ) -> Self {
        let git = StdGitProbe::fixed();
        let executable = if git.executable().is_absolute() {
            git.executable().to_owned()
        } else {
            // Windows has no fixed /usr/bin/git. Resolve PATH before handing
            // the executable to the supervisor, which requires an exact path.
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|directory| directory.join(format!("git{}", std::env::consts::EXE_SUFFIX)))
                .find(|path| path.is_file())
                .unwrap_or_else(|| git.executable().to_owned())
        };
        Self {
            executable,
            deadline,
            cancellation,
            supervisor,
            runtime,
        }
    }

    fn collect(
        &self,
        cwd: &Path,
        args: &[OsString],
        max_output_bytes: usize,
        authority: Option<Arc<AuthorizedRoot>>,
    ) -> Result<GitOutput, IdentityError> {
        self.checkpoint()?;
        let environment: BTreeMap<OsString, OsString> = [
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        ]
        .into_iter()
        .collect();
        let options = ProcessRunOptions::new(cwd)
            .with_deadline(self.deadline)
            .with_cancellation(self.cancellation.clone())
            .with_environment(environment)
            .with_max_output_bytes(max_output_bytes)
            .with_raw_stdout();
        // All callers run on a blocking worker. This keeps the runtime available
        // for cancellation, pipe draining, and process-tree cleanup.
        let output = self
            .runtime
            .block_on(async {
                if let Some(authority) = authority {
                    self.supervisor
                        .run_authorized(&self.executable, args, options, authority)
                        .await
                } else {
                    self.supervisor.run(&self.executable, args, options).await
                }
            })
            .map_err(identity_process_error)?;
        self.checkpoint()?;
        if output.cancelled {
            return Err(IdentityError::Cancelled);
        }
        if output.timed_out {
            return Err(IdentityError::TimedOut);
        }
        if !output.drain_complete || !output.cleanup_complete {
            return Err(IdentityError::Git(
                "git process cleanup was incomplete".to_owned(),
            ));
        }
        Ok(GitOutput {
            status: Some(output.exit_code),
            // Sanitized text strips NUL separators used by Git -z. Only the
            // bounded raw prefix is authoritative for identity parsing.
            stdout: output
                .raw_stdout
                .ok_or_else(|| IdentityError::Git("git raw stdout was not captured".to_owned()))?,
            truncated: output.output_truncated,
        })
    }
}

fn identity_process_error(error: ProcessError) -> IdentityError {
    match error {
        ProcessError::Cancelled => IdentityError::Cancelled,
        ProcessError::TimedOut => IdentityError::TimedOut,
        error => IdentityError::Git(error.to_string()),
    }
}

impl GitProbe for ControlledGitProbe {
    fn checkpoint(&self) -> Result<(), IdentityError> {
        if self.cancellation.is_cancelled() {
            return Err(IdentityError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(IdentityError::TimedOut);
        }
        Ok(())
    }

    fn run(
        &self,
        cwd: &Path,
        args: &[OsString],
        max_output_bytes: usize,
    ) -> Result<GitOutput, IdentityError> {
        self.collect(cwd, args, max_output_bytes, None)
    }

    fn run_authorized(
        &self,
        cwd: &Path,
        args: &[OsString],
        max_output_bytes: usize,
        authority: Arc<AuthorizedRoot>,
    ) -> Result<GitOutput, IdentityError> {
        self.collect(cwd, args, max_output_bytes, Some(authority))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn pre_spawn_lifecycle_errors_keep_their_typed_identity_status() {
        assert!(matches!(
            identity_process_error(ProcessError::Cancelled),
            IdentityError::Cancelled
        ));
        assert!(matches!(
            identity_process_error(ProcessError::TimedOut),
            IdentityError::TimedOut
        ));
        assert!(matches!(
            identity_process_error(ProcessError::Closing),
            IdentityError::Git(_)
        ));
    }

    fn shell_probe(
        deadline: Instant,
        cancellation: CancellationToken,
        supervisor: ProcessSupervisor,
    ) -> ControlledGitProbe {
        ControlledGitProbe {
            executable: PathBuf::from("/bin/sh"),
            deadline,
            cancellation,
            supervisor,
            runtime: Handle::current(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raw_git_output_preserves_nul_delimited_paths() {
        let supervisor = ProcessSupervisor::without_journal();
        let probe = shell_probe(
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
            supervisor.clone(),
        );
        let output = tokio::task::spawn_blocking(move || {
            probe.run(
                Path::new("/tmp"),
                &[
                    OsString::from("-c"),
                    OsString::from("printf 'a.rs\\000b.rs\\000'"),
                ],
                1_024,
            )
        })
        .await
        .expect("blocking Git worker")
        .expect("Git output");
        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout, b"a.rs\0b.rs\0");
        assert!(!output.truncated);
        assert_eq!(supervisor.active_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hanging_git_and_descendant_are_bounded_by_the_request_deadline() {
        let supervisor = ProcessSupervisor::without_journal();
        let started = Instant::now();
        let probe = shell_probe(
            started + Duration::from_millis(200),
            CancellationToken::new(),
            supervisor.clone(),
        );
        let result = tokio::task::spawn_blocking(move || {
            probe.run(
                Path::new("/tmp"),
                &[OsString::from("-c"), OsString::from("/bin/sleep 30 & wait")],
                1_024,
            )
        })
        .await
        .expect("blocking Git worker");
        assert!(matches!(result, Err(IdentityError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(supervisor.active_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_git_is_reaped_before_the_worker_returns() {
        let supervisor = ProcessSupervisor::without_journal();
        let cancellation = CancellationToken::new();
        let probe = shell_probe(
            Instant::now() + Duration::from_secs(30),
            cancellation.clone(),
            supervisor.clone(),
        );
        let worker = tokio::task::spawn_blocking(move || {
            probe.run(
                Path::new("/tmp"),
                &[OsString::from("-c"), OsString::from("/bin/sleep 30 & wait")],
                1_024,
            )
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while supervisor.active_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Git process registered");
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("cancelled Git worker terminated")
            .expect("blocking Git worker");
        assert!(matches!(result, Err(IdentityError::Cancelled)));
        assert_eq!(supervisor.active_count(), 0);
    }
}
