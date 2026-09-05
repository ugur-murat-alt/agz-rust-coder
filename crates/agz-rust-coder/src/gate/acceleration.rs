//! Explicit optional tools. Compilation still belongs to the request's process tree.
use crate::{
    process::{ProcessRunOptions, ProcessSupervisor},
    workspace::AuthorizedRoot,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{
    ffi::OsStr,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio_util::sync::CancellationToken;

pub(crate) fn find_tool(name: &str, roots: &[Arc<AuthorizedRoot>]) -> Result<PathBuf, String> {
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(&executable))
        .filter_map(|candidate| std::fs::canonicalize(candidate).ok())
        .find(|candidate| candidate.is_file() && !roots.iter().any(|root| root.contains(candidate)))
        .ok_or_else(|| format!("optional tool {name} is unavailable on the trusted absolute PATH"))
}

pub(crate) async fn verify_tool(
    supervisor: &ProcessSupervisor,
    executable: &Path,
    args: &[&str],
    expected: &str,
    root: Arc<AuthorizedRoot>,
    environment: &BTreeMap<OsString, OsString>,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<(), String> {
    let output = supervisor
        .run_authorized(
            executable,
            args.iter().copied(),
            ProcessRunOptions::new(root.path())
                .with_environment(environment.clone())
                .with_timeout(Duration::from_secs(5))
                .with_deadline(deadline)
                .with_cancellation(cancel)
                .with_max_output_bytes(4096),
            root,
        )
        .await
        .map_err(|e| e.to_string())?;
    if output.exit_code != 0
        || output.cancelled
        || output.timed_out
        || !output.cleanup_complete
        || !output.drain_complete
        || !output
            .stdout
            .lines()
            .next()
            .is_some_and(|line| line == expected || line.starts_with(&format!("{expected} (")))
    {
        return Err(format!(
            "optional tool must report {expected}; version probe failed"
        ));
    }
    Ok(())
}

pub(crate) struct SccacheSession {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<
        Result<crate::process::ProcessRunResult, crate::process::ProcessError>,
    >,
    state: PathBuf,
}

impl Drop for SccacheSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl SccacheSession {
    #[cfg(unix)]
    pub(crate) async fn start(
        supervisor: ProcessSupervisor,
        root: Arc<AuthorizedRoot>,
        environment: &mut BTreeMap<OsString, OsString>,
        state_root: &Path,
        tool_roots: &[Arc<AuthorizedRoot>],
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<Self, String> {
        let wrapper = environment
            .get(OsStr::new("RUSTC_WRAPPER"))
            .filter(|value| !value.is_empty())
            .ok_or("sccache=true requires an explicitly configured absolute RUSTC_WRAPPER")?;
        let wrapper = PathBuf::from(wrapper);
        if !wrapper.is_absolute() {
            return Err("RUSTC_WRAPPER must be absolute for supervised sccache".into());
        }
        let wrapper = std::fs::canonicalize(wrapper).map_err(|e| e.to_string())?;
        if tool_roots.iter().any(|root| root.contains(&wrapper)) {
            return Err("sccache executable must not be workspace-controlled".into());
        }
        verify_tool(
            &supervisor,
            &wrapper,
            &["--version"],
            "sccache 0.17.0",
            root.clone(),
            environment,
            deadline,
            cancellation.clone(),
        )
        .await?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        super::cache::ensure_directory(state_root).map_err(|e| e.to_string())?;
        let state = state_root.join(format!(
            "sc-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&state)
            .map_err(|e| e.to_string())?;
        let socket = state.join("socket");
        if socket.as_os_str().as_encoded_bytes().len() > 100 {
            let _ = std::fs::remove_dir(&state);
            return Err("sccache socket path is too long; use a shorter gate.lease_dir".into());
        }
        let config = state.join("config");
        if let Err(error) = std::fs::write(&config, "[cache.disk]\nsize = 268435456\n") {
            let _ = std::fs::remove_dir_all(&state);
            return Err(error.to_string());
        }
        let cache_dir = state_root.join("sccache-cache");
        if let Err(error) = super::cache::ensure_directory(&cache_dir) {
            let _ = std::fs::remove_dir_all(&state);
            return Err(error.to_string());
        }
        // Never inherit a remote/distributed cache or a mode that moves rustc outside
        // the supervised client process. Leave the caller's incremental choice intact.
        environment.retain(|key, _| !key.to_string_lossy().starts_with("SCCACHE_"));
        environment.insert("RUSTC_WRAPPER".into(), wrapper.clone().into_os_string());
        environment.insert("SCCACHE_CONF".into(), config.clone().into_os_string());
        environment.insert(
            "SCCACHE_CACHED_CONF".into(),
            state.join("cached-config").into_os_string(),
        );
        environment.insert("SCCACHE_DIR".into(), cache_dir.into_os_string());
        environment.insert("SCCACHE_SERVER_UDS".into(), socket.clone().into_os_string());
        environment.insert("SCCACHE_NO_DAEMON".into(), "1".into());
        environment.insert("SCCACHE_CLIENT_SIDE".into(), "1".into());
        let mut server_env = environment.clone();
        server_env.remove(OsStr::new("SCCACHE_CLIENT_SIDE"));
        server_env.insert("SCCACHE_START_SERVER".into(), "1".into());
        let cancel = cancellation.child_token();
        let server_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            supervisor
                .run_authorized(
                    wrapper,
                    std::iter::empty::<OsString>(),
                    ProcessRunOptions::new(root.path())
                        .with_environment(server_env)
                        .with_deadline(deadline)
                        .with_cancellation(server_cancel)
                        .with_max_output_bytes(8192),
                    root,
                )
                .await
        });
        let session = Self {
            cancel,
            task,
            state,
        };
        let startup_end = (Instant::now() + Duration::from_secs(5)).min(deadline);
        loop {
            if socket.exists() && !session.task.is_finished() {
                return Ok(session);
            }
            if session.task.is_finished()
                || cancellation.is_cancelled()
                || Instant::now() >= startup_end
            {
                let detail = session
                    .close()
                    .await
                    .err()
                    .unwrap_or_else(|| "server was not ready".into());
                return Err(format!("supervised sccache startup failed: {detail}"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(not(unix))]
    pub(crate) async fn start(
        _supervisor: ProcessSupervisor,
        _root: Arc<AuthorizedRoot>,
        _environment: &mut BTreeMap<OsString, OsString>,
        _state_root: &Path,
        _tool_roots: &[Arc<AuthorizedRoot>],
        _deadline: Instant,
        _cancellation: CancellationToken,
    ) -> Result<Self, String> {
        Err("supervised sccache currently requires Unix local sockets; default Cargo remains available".into())
    }

    pub(crate) async fn close(mut self) -> Result<(), String> {
        self.cancel.cancel();
        let result = (&mut self.task)
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let clean = result.cleanup_complete && result.drain_complete;
        if clean {
            let _ = std::fs::remove_dir_all(&self.state);
        }
        if clean {
            Ok(())
        } else {
            Err("sccache cleanup was incomplete".into())
        }
    }
}
