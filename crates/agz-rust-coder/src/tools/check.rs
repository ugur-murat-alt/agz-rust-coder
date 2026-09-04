//! Authoritative Cargo validation service.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::workspace::identity::compute_input_identity_with_git_authority;
use crate::{
    config::{Config, GateScope as ConfigGateScope},
    diagnostics::{Diagnostic, machine_applicable_package, parse_cargo_output},
    gate::{
        CacheSelection, GateAuthority, GateBuildInfo, GateDiagnostic, GateEvidence, GateScheduler,
        GateScope, GateScopeStrategy, GateStatus, GateStepResult, ProgressCallback, ProgressEvent,
        ProgressStage, ScheduledJobContext, SchedulerError, SchedulerOptions, SuggestionPackage,
        SuggestionPatch, ValidationProfile, select_gate_cache, targets_for,
    },
    process::{ProcessRunOptions, ProcessSupervisor},
    workspace::{
        AuthorizedRoot, IdentityInput, IdentityLimits, InputIdentity, MetadataService, RootGuard,
        StdGitProbe, WorkspaceRoot, select_workspace,
    },
};

use crate::gate::{GateRequest, GateTargetId};

#[derive(Debug, Clone)]
pub struct CheckService {
    guard: Arc<RootGuard>,
    metadata: Arc<MetadataService>,
    scheduler: GateScheduler,
    supervisor: ProcessSupervisor,
    owns_supervisor: bool,
    cargo: PathBuf,
    config: Config,
    shutdown: CancellationToken,
}

impl CheckService {
    pub fn new(config: Config, guard: Arc<RootGuard>) -> Self {
        Self::with_supervisor(config, guard, ProcessSupervisor::without_journal(), true)
    }

    pub fn with_shared_supervisor(
        config: Config,
        guard: Arc<RootGuard>,
        supervisor: ProcessSupervisor,
    ) -> Self {
        Self::with_supervisor(config, guard, supervisor, false)
    }

    fn with_supervisor(
        config: Config,
        guard: Arc<RootGuard>,
        supervisor: ProcessSupervisor,
        owns_supervisor: bool,
    ) -> Self {
        let mut scheduler = SchedulerOptions::new(config.gate.lease_dir.clone());
        scheduler.debounce = Duration::from_millis(config.gate.debounce_ms);
        scheduler.host_concurrency = usize::try_from(config.gate.host_concurrency)
            .unwrap_or(usize::MAX)
            .max(1);
        scheduler.hard_timeout = Duration::from_millis(config.gate.hard_timeout_ms);
        scheduler.min_free_disk_mb = config.gate.min_free_disk_mb;
        scheduler.min_available_memory_mb = config.gate.min_available_memory_mb;
        Self {
            metadata: Arc::new(MetadataService::new(Arc::clone(&guard))),
            scheduler: GateScheduler::new(scheduler),
            supervisor,
            owns_supervisor,
            cargo: resolve_cargo(config.cargo.path.as_deref()),
            guard,
            config,
            shutdown: CancellationToken::new(),
        }
    }

    pub fn active_count(&self) -> usize {
        self.scheduler.active_count()
    }

    pub async fn run(
        &self,
        request: GateRequest,
        progress: Option<ProgressCallback>,
        cancellation: Option<CancellationToken>,
    ) -> GateEvidence {
        let accepted_at = Instant::now();
        emit_progress(
            progress.as_ref(),
            accepted_at,
            ProgressStage::Accepted,
            "accepted",
            false,
        );
        let admission_root = match self.admission_root(&request) {
            Ok(root) => root,
            Err((status, message)) => {
                return terminal_evidence(&request, status, message, accepted_at);
            }
        };
        if let Err(error) = self.scheduler.assert_admitted(&admission_root) {
            return terminal_evidence(
                &request,
                GateStatus::ResourceBlocked,
                error.to_string(),
                accepted_at,
            );
        }

        emit_progress(
            progress.as_ref(),
            accepted_at,
            ProgressStage::Preflight,
            "preflight",
            false,
        );
        let deadline = accepted_at + Duration::from_millis(self.config.gate.hard_timeout_ms);
        let requested_cancellation = cancellation.unwrap_or_default();
        let cancellation = CancellationToken::new();
        if requested_cancellation.is_cancelled() || self.shutdown.is_cancelled() {
            cancellation.cancel();
        }
        let cancellation_forwarder = forward_cancellation(
            requested_cancellation,
            self.shutdown.clone(),
            cancellation.clone(),
        );
        let heartbeat =
            spawn_preflight_heartbeat(progress.clone(), cancellation.clone(), accepted_at);
        let service = self.clone();
        let prepare_request = request.clone();
        let metadata_control = crate::workspace::metadata::MetadataControl::new(
            deadline,
            cancellation.clone(),
            self.supervisor.clone(),
            tokio::runtime::Handle::current(),
        );
        let preparing = tokio::task::spawn_blocking(move || {
            service.prepare(&prepare_request, &metadata_control)
        });
        // The controlled preflight owns any metadata child through the supervisor.
        // Awaiting it here prevents detaching the blocking worker while still bounding
        // cancellation and deadline handling at every metadata checkpoint.
        let prepared = match preparing.await {
            Ok(result) => result,
            Err(error) => Err((
                GateStatus::Unavailable,
                format!("gate preflight failed: {error}"),
            )),
        };
        if let Some(heartbeat) = heartbeat {
            heartbeat.abort();
        }
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err((status, message)) => {
                cancellation_forwarder.abort();
                return terminal_evidence(&request, status, message, accepted_at);
            }
        };
        if cancellation.is_cancelled() {
            cancellation_forwarder.abort();
            return terminal_evidence(
                &request,
                GateStatus::Cancelled,
                "gate request was cancelled during preflight".to_owned(),
                accepted_at,
            );
        }
        if Instant::now() >= deadline {
            cancellation_forwarder.abort();
            return terminal_evidence(
                &request,
                GateStatus::Timeout,
                "gate deadline elapsed during preflight".to_owned(),
                accepted_at,
            );
        }
        if !prepared.identity.complete {
            cancellation_forwarder.abort();
            return terminal_evidence(
                &request,
                GateStatus::Inconclusive,
                format!(
                    "input identity is incomplete: {:?}; no Cargo process was started",
                    prepared.identity.incomplete_reason
                ),
                accepted_at,
            );
        }

        let key = format!(
            "{}:{}:{}:{}:{}",
            request.target.as_str(),
            request.timings,
            detail_name(request.detail),
            prepared.identity.hash,
            request.root_epoch
        );
        let identity_key = prepared.identity.hash.clone();
        let root = prepared.snapshot.workspace_root.clone();
        let supervisor = self.supervisor.clone();
        let config = self.config.clone();
        let cargo = self.cargo.clone();
        let work_request = request.clone();
        let job =
            match self
                .scheduler
                .submit_at(root, key, identity_key, accepted_at, move |context| {
                    Box::pin(execute_prepared(
                        context,
                        work_request,
                        prepared,
                        cargo,
                        config,
                        supervisor,
                    ))
                }) {
                Ok(job) => job,
                Err(error) => {
                    cancellation_forwarder.abort();
                    return terminal_evidence(
                        &request,
                        scheduler_status(&error),
                        error.to_string(),
                        accepted_at,
                    );
                }
            };
        let _progress_registration =
            progress.and_then(|progress| job.progress().add_callback(progress));
        let subscription = job.subscribe(Some(cancellation));
        let evidence = match subscription.wait().await {
            Ok(evidence) => evidence,
            Err(error) => terminal_evidence(
                &request,
                scheduler_status(&error),
                error.to_string(),
                accepted_at,
            ),
        };
        cancellation_forwarder.abort();
        evidence
    }

    pub async fn close(&self) {
        self.shutdown.cancel();
        self.scheduler.close().await;
        if self.owns_supervisor {
            let _ = self.supervisor.close().await;
        }
    }

    fn prepare(
        &self,
        request: &GateRequest,
        control: &crate::workspace::metadata::MetadataControl,
    ) -> Result<PreparedCheck, (GateStatus, String)> {
        control.checkpoint().map_err(preflight_control_error)?;
        let roots = self
            .guard
            .snapshot(request.client_roots.clone())
            .map_err(|error| {
                (
                    GateStatus::Inconclusive,
                    format!("workspace root snapshot failed: {error}"),
                )
            })?;
        let selection =
            select_workspace(&roots, request.directory.as_deref()).map_err(|error| {
                (
                    GateStatus::Inconclusive,
                    format!("workspace selection failed: {error}"),
                )
            })?;
        let workspace_root = selection.requested_root();
        let load = self
            .metadata
            .acquire_controlled(&selection, self.cargo.clone(), control)
            .map_err(preflight_metadata_error)?;
        control.checkpoint().map_err(preflight_control_error)?;
        let snapshot = Arc::clone(&load.snapshot);
        let workspace_authority = if snapshot.workspace_root == selection.package_authority().path()
        {
            selection.package_authority().clone()
        } else {
            selection
                .worktree_authority()
                .authorize_dir(&snapshot.workspace_root)
                .map_err(|error| {
                    (
                        GateStatus::Inconclusive,
                        format!("workspace execution authorization failed: {error}"),
                    )
                })?
        };
        let git_authority = selection.worktree_authority().clone();
        let cache =
            select_gate_cache(&snapshot, &self.config.gate, request.mode()).map_err(|error| {
                (
                    GateStatus::Inconclusive,
                    format!("gate cache selection failed: {error}"),
                )
            })?;
        let initial_scope_args = if request.target == GateTargetId::Check {
            vec![OsString::from("--workspace")]
        } else {
            Vec::new()
        };
        let mut targets = targets_for(
            &snapshot,
            request.target,
            &snapshot.manifest_path,
            request.timings,
            &initial_scope_args,
        );
        if targets.is_empty() {
            return Err((
                GateStatus::Inconclusive,
                "no applicable Cargo target was selected".to_owned(),
            ));
        }
        let mut command = targets
            .iter()
            .flat_map(|target| target.args.iter().cloned())
            .collect::<Vec<_>>();
        let external_roots = authorize_external_roots(&self.guard, &snapshot.external_paths)
            .map_err(|message| {
                (
                    GateStatus::Inconclusive,
                    format!("external dependency authorization failed: {message}"),
                )
            })?;
        let mut identity = identity_for(
            &workspace_root,
            &git_authority,
            &snapshot.manifest_path,
            &self.cargo,
            &command,
            &cache,
            &external_roots,
            identity_limits(&self.config),
        )
        .map_err(|error| {
            (
                GateStatus::Inconclusive,
                format!("input identity failed: {error}"),
            )
        })?;
        control.checkpoint().map_err(preflight_control_error)?;
        let mut scope = check_scope(
            &snapshot,
            request.target,
            self.config.gate.scope,
            identity.changed_paths.clone(),
        );
        if request.target == GateTargetId::Check && scope.args != initial_scope_args {
            targets = targets_for(
                &snapshot,
                request.target,
                &snapshot.manifest_path,
                request.timings,
                &scope.args,
            );
            command = targets
                .iter()
                .flat_map(|target| target.args.iter().cloned())
                .collect();
            identity = identity_for(
                &workspace_root,
                &git_authority,
                &snapshot.manifest_path,
                &self.cargo,
                &command,
                &cache,
                &external_roots,
                identity_limits(&self.config),
            )
            .map_err(|error| {
                (
                    GateStatus::Inconclusive,
                    format!("input identity failed: {error}"),
                )
            })?;
            control.checkpoint().map_err(preflight_control_error)?;
            scope.evidence.changed_paths = identity.changed_paths.clone();
        }
        control.checkpoint().map_err(preflight_control_error)?;
        Ok(PreparedCheck {
            workspace_root,
            workspace_authority,
            git_authority,
            snapshot,
            cache,
            targets,
            command,
            external_roots,
            identity,
            scope: scope.evidence,
            metadata_cache: format!("{:?}", load.cache).to_ascii_lowercase(),
        })
    }

    fn admission_root(&self, request: &GateRequest) -> Result<PathBuf, (GateStatus, String)> {
        let roots = self
            .guard
            .snapshot(request.client_roots.clone())
            .map_err(|error| (GateStatus::Inconclusive, error.to_string()))?;
        select_workspace(&roots, request.directory.as_deref())
            .map(|selection| selection.requested_root().path().to_owned())
            .map_err(|error| (GateStatus::Inconclusive, error.to_string()))
    }
}

#[derive(Debug)]
struct PreparedCheck {
    workspace_root: WorkspaceRoot,
    workspace_authority: Arc<AuthorizedRoot>,
    git_authority: Arc<AuthorizedRoot>,
    snapshot: Arc<crate::workspace::WorkspaceSnapshot>,
    cache: CacheSelection,
    targets: Vec<crate::gate::GateTarget>,
    command: Vec<OsString>,
    external_roots: Vec<Arc<AuthorizedRoot>>,
    identity: InputIdentity,
    scope: GateScope,
    metadata_cache: String,
}

struct CheckScope {
    evidence: GateScope,
    args: Vec<OsString>,
}

fn check_scope(
    snapshot: &crate::workspace::WorkspaceSnapshot,
    target: GateTargetId,
    configured: ConfigGateScope,
    changed_paths: Vec<PathBuf>,
) -> CheckScope {
    if target != GateTargetId::Check {
        return CheckScope {
            evidence: GateScope::workspace(
                changed_paths,
                "gate.scope applies only to target=check",
            ),
            args: Vec::new(),
        };
    }
    if configured == ConfigGateScope::Workspace {
        return CheckScope {
            evidence: GateScope::workspace(changed_paths, "configured workspace scope"),
            args: vec![OsString::from("--workspace")],
        };
    }

    let affected = affected_packages(snapshot, &changed_paths);
    match (configured, affected) {
        (ConfigGateScope::Shadow, Ok((packages, package_ids))) => CheckScope {
            evidence: GateScope {
                strategy: GateScopeStrategy::Shadow,
                packages,
                package_ids,
                changed_paths,
                widened_because: Vec::new(),
            },
            args: vec![OsString::from("--workspace")],
        },
        (ConfigGateScope::Shadow, Err(reason)) => CheckScope {
            evidence: GateScope {
                strategy: GateScopeStrategy::Shadow,
                packages: Vec::new(),
                package_ids: Vec::new(),
                changed_paths,
                widened_because: vec![reason],
            },
            args: vec![OsString::from("--workspace")],
        },
        (ConfigGateScope::Affected, Ok((packages, package_ids))) => {
            let args = packages
                .iter()
                .flat_map(|package| [OsString::from("-p"), OsString::from(package)])
                .collect();
            CheckScope {
                evidence: GateScope {
                    strategy: GateScopeStrategy::Affected,
                    packages,
                    package_ids,
                    changed_paths,
                    widened_because: Vec::new(),
                },
                args,
            }
        }
        (ConfigGateScope::Affected, Err(reason)) => CheckScope {
            evidence: GateScope::workspace(changed_paths, reason),
            args: vec![OsString::from("--workspace")],
        },
        (ConfigGateScope::Workspace, _) => unreachable!("workspace scope returned above"),
    }
}

fn affected_packages(
    snapshot: &crate::workspace::WorkspaceSnapshot,
    changed_paths: &[PathBuf],
) -> Result<(Vec<String>, Vec<String>), String> {
    if changed_paths.is_empty() {
        return Err("changed set is empty; widened to workspace".to_owned());
    }
    if changed_paths.iter().any(|path| is_global_cargo_input(path)) {
        return Err("global Cargo input changed; widened to workspace".to_owned());
    }
    let absolute_paths = changed_paths
        .iter()
        .map(|path| snapshot.canonical_worktree.join(path))
        .collect::<Vec<_>>();
    let owned = absolute_paths.iter().all(|path| {
        snapshot
            .graph
            .nodes()
            .values()
            .filter(|node| node.workspace_member)
            .any(|node| path == &node.root || path.starts_with(&node.root))
    });
    if !owned {
        return Err("changed path has no workspace package owner; widened to workspace".to_owned());
    }
    let affected_ids = snapshot
        .graph
        .affected_by_paths(absolute_paths.iter().map(PathBuf::as_path));
    let mut packages = affected_ids
        .iter()
        .filter_map(|package_id| snapshot.graph.node(package_id))
        .filter(|node| node.workspace_member)
        .map(|node| (node.name.clone(), node.package_id.clone()))
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| left.1.cmp(&right.1));
    packages.dedup();
    if packages.is_empty() {
        return Err(
            "changed set resolved to no workspace package; widened to workspace".to_owned(),
        );
    }
    Ok(packages.into_iter().unzip())
}

fn is_global_cargo_input(path: &Path) -> bool {
    path == Path::new("Cargo.toml")
        || path == Path::new("Cargo.lock")
        || path == Path::new("rust-toolchain")
        || path == Path::new("rust-toolchain.toml")
        || path.starts_with(".cargo")
}

async fn execute_prepared(
    context: ScheduledJobContext,
    request: GateRequest,
    prepared: PreparedCheck,
    cargo: PathBuf,
    config: Config,
    supervisor: ProcessSupervisor,
) -> Result<GateEvidence, SchedulerError> {
    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    let mut status = GateStatus::FastPass;
    let target_count = prepared.targets.len().max(1) as f64;

    for (index, target) in prepared.targets.iter().enumerate() {
        if context.cancellation.is_cancelled() {
            status = GateStatus::Cancelled;
            break;
        }
        context.progress.emit(
            ProgressStage::Running,
            Some(target.id),
            index as f64 / target_count,
            Some(1.0),
            target.label,
            false,
        );
        let options = ProcessRunOptions::new(&prepared.snapshot.workspace_root)
            .with_timeout(target.timeout)
            .with_deadline(context.deadline)
            .with_cancellation(context.cancellation.clone())
            .with_environment(prepared.cache.environment.clone())
            .with_max_output_bytes(
                usize::try_from(config.limits.process_output_bytes).unwrap_or(usize::MAX),
            );
        let result = supervisor
            .run_authorized(
                cargo.clone(),
                target.args.clone(),
                options,
                prepared.workspace_authority.clone(),
            )
            .await
            .map_err(|error| SchedulerError::Internal(error.to_string()))?;
        warnings.extend(result.warnings.iter().cloned());
        let parsed = parse_cargo_output(&format!("{}\n{}", result.stdout, result.stderr));
        let suggestion_package = convert_suggestion_package(machine_applicable_package(
            &prepared.snapshot.workspace_root,
            &parsed.diagnostics,
        ));
        let step = GateStepResult {
            target: target.id,
            command: result.command,
            exit_code: result.exit_code,
            signal: result.signal,
            timed_out: result.timed_out,
            cancelled: result.cancelled,
            duration_ms: result.duration_ms,
            first_diagnostic_ms: result.first_diagnostic_ms,
            diagnostics: parsed
                .diagnostics
                .iter()
                .take(diagnostic_limit(request.detail))
                .map(convert_diagnostic)
                .collect(),
            suggestion_package,
            tail: bounded_tail(&result.output, 8_000),
            stdout: bounded_tail(&result.stdout, 16_000),
            stderr: bounded_tail(&result.stderr, 16_000),
            output_truncated: result.output_truncated,
            drain_complete: result.drain_complete,
            cleanup_complete: result.cleanup_complete,
            build: parsed
                .build
                .map(|build| crate::gate::types::CargoBuildTelemetry {
                    total_units: build.total_units as u64,
                    fresh_units: build.fresh_units as u64,
                    rebuilt_units: build.rebuilt_units as u64,
                    build_scripts: build.build_scripts as u64,
                    linked_units: build.linked_units as u64,
                    partial: parsed.truncated,
                }),
        };
        status = if step.cancelled {
            GateStatus::Cancelled
        } else if step.timed_out {
            GateStatus::Timeout
        } else if !step.drain_complete || !step.cleanup_complete {
            GateStatus::Unavailable
        } else if step.exit_code != 0 {
            GateStatus::Fail
        } else {
            status
        };
        steps.push(step);
        if !matches!(status, GateStatus::FastPass) {
            break;
        }
    }

    let post_identity = identity_for(
        &prepared.workspace_root,
        &prepared.git_authority,
        &prepared.snapshot.manifest_path,
        &cargo,
        &prepared.command,
        &prepared.cache,
        &prepared.external_roots,
        identity_limits(&config),
    )
    .map_err(SchedulerError::Internal)?;
    if status == GateStatus::FastPass
        && (!post_identity.complete || post_identity.hash != prepared.identity.hash)
    {
        status = GateStatus::Stale;
        for step in &mut steps {
            step.suggestion_package = None;
        }
        warnings.push(
            "repository inputs changed while validation was running; no pass was published"
                .to_owned(),
        );
    } else if status == GateStatus::FastPass && request.target == GateTargetId::All {
        status = GateStatus::FullPass;
    }

    let finished_at = Instant::now();
    context.progress.emit(
        if matches!(status, GateStatus::FastPass | GateStatus::FullPass) {
            ProgressStage::Completed
        } else {
            ProgressStage::Failed
        },
        None,
        1.0,
        Some(1.0),
        status.as_str(),
        false,
    );
    Ok(GateEvidence {
        version: 1,
        job_id: context.id,
        status,
        authority: status.authority(),
        mode: request.mode(),
        generation: context.generation,
        requested_at: timestamp_now(),
        started_at: Some(timestamp_now()),
        finished_at: Some(timestamp_now()),
        response_ms: finished_at
            .duration_since(context.timing.requested_at)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        queue_ms: context.timing.queue_ms,
        first_diagnostic_ms: steps
            .iter()
            .filter_map(|step| step.first_diagnostic_ms)
            .min(),
        requested_dir: request
            .directory
            .clone()
            .unwrap_or_else(|| prepared.snapshot.requested_dir.clone()),
        workspace_root: Some(prepared.snapshot.workspace_root.clone()),
        manifest_path: Some(prepared.snapshot.manifest_path.clone()),
        input_hash: prepared.identity.hash.clone(),
        command_hash: prepared.identity.command_hash.clone(),
        environment_hash: prepared.identity.environment_hash.clone(),
        cache_mode: prepared.cache.mode.as_str().to_owned(),
        scope: prepared.scope,
        steps,
        build: Some(GateBuildInfo {
            metadata_cache: prepared.metadata_cache,
            target_directory: prepared.cache.target_directory.clone(),
        }),
        profile: Some(ValidationProfile {
            id: profile_id(
                &prepared.identity,
                request.target,
                prepared.cache.mode.as_str(),
            ),
            command_hash: prepared.identity.command_hash,
            cache_mode: prepared.cache.mode.as_str().to_owned(),
            target: request.target.as_str().to_owned(),
        }),
        source: request.source,
        message: Some(format!("{} target(s) completed", prepared.targets.len())),
        warnings,
    })
}

fn identity_for(
    root: &WorkspaceRoot,
    git_authority: &Arc<AuthorizedRoot>,
    manifest: &Path,
    cargo: &Path,
    command: &[OsString],
    cache: &CacheSelection,
    external_roots: &[Arc<AuthorizedRoot>],
    limits: IdentityLimits,
) -> Result<InputIdentity, String> {
    let git = StdGitProbe::default();
    compute_input_identity_with_git_authority(
        &IdentityInput::new(root, manifest, cargo, command, &cache.environment, &git)
            .with_git_cwd(git_authority.path())
            .with_external_roots(external_roots)
            .with_target_directory(&cache.target_directory)
            .with_limits(limits),
        git_authority.clone(),
    )
    .map_err(|error| error.to_string())
}

fn authorize_external_roots(
    guard: &RootGuard,
    roots: &[PathBuf],
) -> Result<Vec<Arc<AuthorizedRoot>>, String> {
    roots
        .iter()
        .map(|root| {
            guard
                .authorize_dependency(root)
                .map_err(|error| format!("{}: {error}", root.display()))
        })
        .collect()
}

fn identity_limits(config: &Config) -> IdentityLimits {
    IdentityLimits {
        max_files: usize::try_from(config.limits.identity_files).unwrap_or(usize::MAX),
        max_file_bytes: config.limits.identity_file_bytes,
        max_total_bytes: config.limits.identity_total_bytes,
        max_external_files: usize::try_from(config.limits.external_files).unwrap_or(usize::MAX),
        max_external_bytes: config.limits.external_bytes,
        max_git_output_bytes: usize::try_from(config.limits.git_output_bytes).unwrap_or(usize::MAX),
    }
}

fn emit_progress(
    callback: Option<&ProgressCallback>,
    accepted_at: Instant,
    stage: ProgressStage,
    message: &str,
    heartbeat: bool,
) {
    if let Some(callback) = callback {
        callback(ProgressEvent {
            stage,
            target: None,
            progress: 0.0,
            total: None,
            message: message.to_owned(),
            heartbeat,
            elapsed_ms: accepted_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        });
    }
}

fn spawn_preflight_heartbeat(
    callback: Option<ProgressCallback>,
    cancellation: CancellationToken,
    accepted_at: Instant,
) -> Option<tokio::task::JoinHandle<()>> {
    callback.map(|callback| {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(8)) => {
                        emit_progress(
                            Some(&callback),
                            accepted_at,
                            ProgressStage::Heartbeat,
                            "preflight heartbeat",
                            true,
                        );
                    }
                    () = cancellation.cancelled() => return,
                }
            }
        })
    })
}

fn forward_cancellation(
    request: CancellationToken,
    shutdown: CancellationToken,
    combined: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            () = request.cancelled() => combined.cancel(),
            () = shutdown.cancelled() => combined.cancel(),
        }
    })
}

fn preflight_control_error(error: crate::workspace::MetadataError) -> (GateStatus, String) {
    preflight_metadata_error(error)
}

fn preflight_metadata_error(error: crate::workspace::MetadataError) -> (GateStatus, String) {
    let status = match &error {
        crate::workspace::MetadataError::Cancelled => GateStatus::Cancelled,
        crate::workspace::MetadataError::TimedOut => GateStatus::Timeout,
        _ => GateStatus::Inconclusive,
    };
    (status, format!("Cargo metadata preflight failed: {error}"))
}

fn terminal_evidence(
    request: &GateRequest,
    status: GateStatus,
    message: String,
    started: Instant,
) -> GateEvidence {
    let mut evidence = GateEvidence::pending("gate-preflight", request);
    evidence.status = status;
    evidence.authority = GateAuthority::None;
    evidence.requested_at = timestamp_now();
    evidence.finished_at = Some(timestamp_now());
    evidence.response_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    evidence.message = Some(message);
    evidence
}

fn convert_diagnostic(diagnostic: &Diagnostic) -> GateDiagnostic {
    GateDiagnostic {
        code: diagnostic.code.clone(),
        level: diagnostic.level.as_str().to_owned(),
        file: diagnostic.file.clone(),
        line: diagnostic.line.map(|line| line as u64),
        message: diagnostic.message.clone(),
        rendered: diagnostic.rendered.clone(),
        spans: diagnostic.spans.iter().map(convert_span).collect(),
        children: diagnostic.children.iter().map(convert_child).collect(),
        suggestions: diagnostic
            .suggestions
            .iter()
            .map(|suggestion| crate::gate::CompilerSuggestion {
                message: suggestion.message.clone(),
                applicability: convert_applicability(suggestion.applicability),
                edits: suggestion
                    .edits
                    .iter()
                    .map(|edit| crate::gate::SuggestionEdit {
                        file: edit.file.clone(),
                        line_start: edit.line_start as u64,
                        line_end: edit.line_end as u64,
                        column_start: edit.column_start as u64,
                        column_end: edit.column_end as u64,
                        replacement: edit.replacement.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn convert_span(span: &crate::diagnostics::DiagnosticSpan) -> crate::gate::DiagnosticSpan {
    crate::gate::DiagnosticSpan {
        file: span.file.clone(),
        byte_start: span.byte_start,
        byte_end: span.byte_end,
        line_start: span.line_start as u64,
        line_end: span.line_end as u64,
        column_start: span.column_start as u64,
        column_end: span.column_end as u64,
        is_primary: span.is_primary,
        label: span.label.clone(),
        suggested_replacement: span.suggested_replacement.clone(),
        suggestion_applicability: span.suggestion_applicability.map(convert_applicability),
    }
}

fn convert_child(child: &crate::diagnostics::DiagnosticChild) -> crate::gate::DiagnosticChild {
    crate::gate::DiagnosticChild {
        level: child.level.clone(),
        message: child.message.clone(),
        rendered: child.rendered.clone(),
        spans: child.spans.iter().map(convert_span).collect(),
        children: child.children.iter().map(convert_child).collect(),
    }
}

const fn convert_applicability(
    applicability: crate::diagnostics::SuggestionApplicability,
) -> crate::gate::SuggestionApplicability {
    match applicability {
        crate::diagnostics::SuggestionApplicability::MachineApplicable => {
            crate::gate::SuggestionApplicability::MachineApplicable
        }
        crate::diagnostics::SuggestionApplicability::MaybeIncorrect => {
            crate::gate::SuggestionApplicability::MaybeIncorrect
        }
        crate::diagnostics::SuggestionApplicability::HasPlaceholders => {
            crate::gate::SuggestionApplicability::HasPlaceholders
        }
        crate::diagnostics::SuggestionApplicability::Unspecified => {
            crate::gate::SuggestionApplicability::Unspecified
        }
    }
}

fn convert_suggestion_package(
    package: crate::diagnostics::WriteFreePackage,
) -> Option<SuggestionPackage> {
    if package.patches.is_empty() && package.skipped.is_empty() {
        return None;
    }
    Some(SuggestionPackage {
        patches: package
            .patches
            .into_iter()
            .map(|patch| SuggestionPatch {
                file: patch.file,
                old_string: patch.old_string,
                new_string: patch.new_string,
            })
            .collect(),
        skipped: package
            .skipped
            .into_iter()
            .map(|skipped| skipped.reason)
            .collect(),
    })
}

fn scheduler_status(error: &SchedulerError) -> GateStatus {
    match error {
        SchedulerError::Cancelled => GateStatus::Cancelled,
        SchedulerError::Superseded => GateStatus::Superseded,
        SchedulerError::TimedOut => GateStatus::Timeout,
        SchedulerError::ResourceBlocked(_) => GateStatus::ResourceBlocked,
        SchedulerError::Closing | SchedulerError::Lease(_) | SchedulerError::Internal(_) => {
            GateStatus::Unavailable
        }
    }
}

pub(crate) fn resolve_cargo(configured: Option<&Path>) -> PathBuf {
    if let Some(path) = configured {
        return path.to_owned();
    }
    if let Some(path) = option_env!("CARGO").map(PathBuf::from)
        && path.is_file()
    {
        return path;
    }
    if let Some(path) = rustup_toolchain_cargo()
        && path.is_file()
    {
        return path;
    }
    let mut candidates = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(executable_name("cargo")))
        .collect::<Vec<_>>();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".cargo")
                .join("bin")
                .join(executable_name("cargo")),
        );
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(executable_name("cargo")))
}

fn rustup_toolchain_cargo() -> Option<PathBuf> {
    let rustup_home = std::env::var_os("RUSTUP_HOME").map_or_else(
        || std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")),
        |home| Some(PathBuf::from(home)),
    )?;
    let toolchain = std::env::var_os("RUSTUP_TOOLCHAIN")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let settings = std::fs::read_to_string(rustup_home.join("settings.toml")).ok()?;
            settings
                .parse::<toml::Table>()
                .ok()?
                .get("default_toolchain")?
                .as_str()
                .map(OsString::from)
        })?;
    Some(
        rustup_home
            .join("toolchains")
            .join(toolchain)
            .join("bin")
            .join(executable_name("cargo")),
    )
}

#[cfg(windows)]
fn executable_name(name: &str) -> String {
    format!("{name}.exe")
}

#[cfg(not(windows))]
fn executable_name(name: &str) -> String {
    name.to_owned()
}

fn profile_id(identity: &InputIdentity, target: GateTargetId, cache: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identity.hash.as_bytes());
    hasher.update(target.as_str().as_bytes());
    hasher.update(cache.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn timestamp_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn bounded_tail(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_owned();
    }
    value
        .chars()
        .skip(count.saturating_sub(max_chars))
        .collect()
}

const fn diagnostic_limit(detail: crate::gate::GateDetail) -> usize {
    match detail {
        crate::gate::GateDetail::Compact => 5,
        crate::gate::GateDetail::Standard => 12,
        crate::gate::GateDetail::Full => 24,
    }
}

const fn detail_name(detail: crate::gate::GateDetail) -> &'static str {
    match detail {
        crate::gate::GateDetail::Compact => "compact",
        crate::gate::GateDetail::Standard => "standard",
        crate::gate::GateDetail::Full => "full",
    }
}
