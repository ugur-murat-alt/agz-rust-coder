use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures::future::BoxFuture;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{
    lease::{LeaseError, acquire_host_slot, acquire_lease},
    types::{GateEvidence, ProgressCallback, ProgressEvent, ProgressStage},
};

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);
const DEFAULT_HARD_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(8);
#[cfg(target_os = "linux")]
const RESOURCE_MEMORY_PATH: &str = "/proc/meminfo";

#[derive(Debug, Clone)]
pub struct SchedulerOptions {
    pub lease_dir: PathBuf,
    pub debounce: Duration,
    pub host_concurrency: usize,
    pub hard_timeout: Duration,
    pub min_free_disk_mb: u64,
    pub min_available_memory_mb: u64,
    pub heartbeat: Duration,
}

impl SchedulerOptions {
    pub fn new(lease_dir: impl Into<PathBuf>) -> Self {
        Self {
            lease_dir: lease_dir.into(),
            debounce: DEFAULT_DEBOUNCE,
            host_concurrency: 1,
            hard_timeout: DEFAULT_HARD_TIMEOUT,
            min_free_disk_mb: 0,
            min_available_memory_mb: 0,
            heartbeat: DEFAULT_HEARTBEAT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    Closing,
    Cancelled,
    Superseded,
    TimedOut,
    ResourceBlocked(String),
    Lease(String),
    Internal(String),
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closing => formatter.write_str("gate scheduler is closing"),
            Self::Cancelled => formatter.write_str("gate job was cancelled"),
            Self::Superseded => formatter.write_str("gate job was superseded by a newer input"),
            Self::TimedOut => formatter.write_str("gate scheduler deadline elapsed"),
            Self::ResourceBlocked(reason) => {
                write!(formatter, "resource admission rejected: {reason}")
            }
            Self::Lease(reason) => write!(formatter, "gate lease failed: {reason}"),
            Self::Internal(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for SchedulerError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelReason {
    Cancelled,
    Superseded,
    Shutdown,
    TimedOut,
}

impl CancelReason {
    const fn error(self) -> SchedulerError {
        match self {
            Self::Cancelled => SchedulerError::Cancelled,
            Self::Superseded => SchedulerError::Superseded,
            Self::Shutdown => SchedulerError::Closing,
            Self::TimedOut => SchedulerError::TimedOut,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct JobTiming {
    pub requested_at: Instant,
    pub started_at: Instant,
    pub queue_ms: u64,
}

#[derive(Clone)]
pub struct ProgressHub {
    callbacks: Arc<Mutex<BTreeMap<u64, ProgressCallback>>>,
    next_callback_id: Arc<AtomicU64>,
    started_at: Instant,
}

pub struct ProgressRegistration {
    callbacks: Arc<Mutex<BTreeMap<u64, ProgressCallback>>>,
    id: u64,
}

impl fmt::Debug for ProgressRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgressRegistration")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for ProgressRegistration {
    fn drop(&mut self) {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.remove(&self.id);
        }
    }
}

impl fmt::Debug for ProgressHub {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgressHub")
            .field(
                "callbacks",
                &self.callbacks.lock().map_or(0, |callbacks| callbacks.len()),
            )
            .finish()
    }
}

impl ProgressHub {
    fn new(started_at: Instant) -> Self {
        Self {
            callbacks: Arc::new(Mutex::new(BTreeMap::new())),
            next_callback_id: Arc::new(AtomicU64::new(0)),
            started_at,
        }
    }

    pub fn add_callback(&self, callback: ProgressCallback) -> Option<ProgressRegistration> {
        let mut callbacks = self.callbacks.lock().ok()?;
        if callbacks.len() >= 32 {
            return None;
        }
        let id = self.next_callback_id.fetch_add(1, Ordering::Relaxed);
        callbacks.insert(id, callback);
        Some(ProgressRegistration {
            callbacks: Arc::clone(&self.callbacks),
            id,
        })
    }

    pub fn emit(
        &self,
        stage: ProgressStage,
        target: Option<super::types::GateTargetId>,
        progress: f64,
        total: Option<f64>,
        message: impl Into<String>,
        heartbeat: bool,
    ) {
        let event = ProgressEvent {
            stage,
            target,
            progress: progress.clamp(0.0, 1.0),
            total,
            message: bounded_message(message.into()),
            heartbeat,
            elapsed_ms: self
                .started_at
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        };
        let callbacks = self.callbacks.lock().map_or_else(
            |_| Vec::new(),
            |callbacks| callbacks.values().cloned().collect(),
        );
        for callback in callbacks {
            callback(event.clone());
        }
    }
}

#[derive(Clone)]
pub struct ScheduledJobContext {
    pub id: String,
    pub root: PathBuf,
    pub generation: u64,
    pub timing: JobTiming,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    pub progress: ProgressHub,
}

impl fmt::Debug for ScheduledJobContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledJobContext")
            .field("id", &self.id)
            .field("root", &self.root)
            .field("generation", &self.generation)
            .field("timing", &self.timing)
            .field("deadline", &self.deadline)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish()
    }
}

struct JobState {
    id: String,
    root: PathBuf,
    key: String,
    identity_key: String,
    generation: u64,
    requested_at: Instant,
    cancellation: CancellationToken,
    cancel_reason: Mutex<Option<CancelReason>>,
    subscribers: AtomicUsize,
    result: Mutex<Option<Result<GateEvidence, SchedulerError>>>,
    complete: AtomicBool,
    notify: Notify,
    progress: ProgressHub,
}

impl fmt::Debug for JobState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobState")
            .field("id", &self.id)
            .field("root", &self.root)
            .field("key", &self.key)
            .field("identity_key", &self.identity_key)
            .field("generation", &self.generation)
            .field("subscribers", &self.subscribers.load(Ordering::Acquire))
            .field("complete", &self.complete.load(Ordering::Acquire))
            .finish()
    }
}

impl JobState {
    fn cancel(&self, reason: CancelReason) {
        if let Ok(mut current) = self.cancel_reason.lock() {
            if current.is_none() {
                *current = Some(reason);
            }
        }
        self.cancellation.cancel();
    }

    fn reason(&self) -> Option<CancelReason> {
        self.cancel_reason.lock().ok().and_then(|reason| *reason)
    }

    fn is_joinable(&self) -> bool {
        !self.complete.load(Ordering::Acquire)
            && !self.cancellation.is_cancelled()
            && self.reason().is_none()
    }

    fn finish(&self, result: Result<GateEvidence, SchedulerError>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(result);
        }
        self.complete.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn detach(&self) {
        let previous =
            self.subscribers
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                });
        if previous == Ok(1) && !self.complete.load(Ordering::Acquire) {
            self.cancel(CancelReason::Cancelled);
        }
    }
}

#[derive(Clone)]
pub struct ScheduledJob {
    state: Arc<JobState>,
}

impl fmt::Debug for ScheduledJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.state.fmt(formatter)
    }
}

impl ScheduledJob {
    pub fn id(&self) -> &str {
        &self.state.id
    }

    pub fn root(&self) -> &Path {
        &self.state.root
    }

    pub fn key(&self) -> &str {
        &self.state.key
    }

    pub fn generation(&self) -> u64 {
        self.state.generation
    }

    pub fn progress(&self) -> ProgressHub {
        self.state.progress.clone()
    }

    pub fn subscribe(&self, cancellation: Option<CancellationToken>) -> JobSubscription {
        self.state.subscribers.fetch_add(1, Ordering::AcqRel);
        JobSubscription {
            state: Arc::clone(&self.state),
            cancellation,
            detached: AtomicBool::new(false),
        }
    }

    pub fn cancel(&self) {
        self.state.cancel(CancelReason::Cancelled);
    }

    pub fn is_complete(&self) -> bool {
        self.state.complete.load(Ordering::Acquire)
    }
}

pub struct JobSubscription {
    state: Arc<JobState>,
    cancellation: Option<CancellationToken>,
    detached: AtomicBool,
}

impl fmt::Debug for JobSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobSubscription")
            .field("job_id", &self.state.id)
            .field("detached", &self.detached.load(Ordering::Acquire))
            .finish()
    }
}

impl JobSubscription {
    pub async fn wait(&self) -> Result<GateEvidence, SchedulerError> {
        loop {
            let notified = self.state.notify.notified();
            if let Some(result) = self
                .state
                .result
                .lock()
                .ok()
                .and_then(|result| result.clone())
            {
                return result;
            }
            tokio::select! {
                () = notified => {}
                () = async {
                    if let Some(cancellation) = &self.cancellation {
                        cancellation.cancelled().await;
                    }
                }, if self.cancellation.is_some() => {
                    self.detach();
                    return Err(SchedulerError::Cancelled);
                }
            }
        }
    }

    pub fn detach(&self) {
        if !self.detached.swap(true, Ordering::AcqRel) {
            self.state.detach();
        }
    }

    pub fn cancel(&self) {
        self.detach();
    }

    pub fn job_id(&self) -> &str {
        &self.state.id
    }
}

impl Drop for JobSubscription {
    fn drop(&mut self) {
        if !self.detached.swap(true, Ordering::AcqRel) {
            self.state.detach();
        }
    }
}

#[derive(Debug, Default)]
struct SchedulerState {
    generations: BTreeMap<PathBuf, u64>,
    jobs: BTreeMap<String, Arc<JobState>>,
    closing: bool,
}

#[derive(Clone)]
pub struct GateScheduler {
    options: SchedulerOptions,
    state: Arc<Mutex<SchedulerState>>,
}

impl fmt::Debug for GateScheduler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().ok();
        formatter
            .debug_struct("GateScheduler")
            .field("options", &self.options)
            .field("jobs", &state.as_ref().map_or(0, |state| state.jobs.len()))
            .field(
                "closing",
                &state.as_ref().is_some_and(|state| state.closing),
            )
            .finish()
    }
}

impl GateScheduler {
    pub fn new(options: SchedulerOptions) -> Self {
        Self {
            options,
            state: Arc::new(Mutex::new(SchedulerState::default())),
        }
    }

    pub fn options(&self) -> &SchedulerOptions {
        &self.options
    }

    pub fn generation(&self, root: &Path) -> u64 {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.generations.get(root).copied())
            .unwrap_or(0)
    }

    pub fn mark_dirty(&self, root: impl Into<PathBuf>) -> u64 {
        let root = root.into();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = {
            let generation = state.generations.entry(root.clone()).or_insert(0);
            *generation = generation.saturating_add(1);
            *generation
        };
        for job in state.jobs.values() {
            if job.root == root && !job.complete.load(Ordering::Acquire) {
                job.cancel(CancelReason::Superseded);
            }
        }
        generation
    }

    pub fn active_count(&self) -> usize {
        self.state.lock().map_or(0, |state| {
            state
                .jobs
                .values()
                .filter(|job| !job.complete.load(Ordering::Acquire))
                .count()
        })
    }

    pub fn assert_admitted(&self, path: &Path) -> Result<ResourceSnapshot, SchedulerError> {
        let snapshot = resource_snapshot(path)
            .map_err(|error| SchedulerError::ResourceBlocked(error.to_string()))?;
        let mut reasons = Vec::new();
        if snapshot.free_disk_mb < self.options.min_free_disk_mb {
            reasons.push(format!(
                "free disk {} MiB < {} MiB",
                snapshot.free_disk_mb, self.options.min_free_disk_mb
            ));
        }
        if memory_below_floor(
            snapshot.available_memory_mb,
            self.options.min_available_memory_mb,
        ) {
            reasons.push(format!(
                "available memory {} MiB < {} MiB",
                snapshot.available_memory_mb.unwrap_or_default(),
                self.options.min_available_memory_mb
            ));
        }
        if reasons.is_empty() {
            Ok(snapshot)
        } else {
            Err(SchedulerError::ResourceBlocked(reasons.join("; ")))
        }
    }

    pub fn submit<F>(
        &self,
        root: impl Into<PathBuf>,
        key: String,
        identity_key: String,
        work: F,
    ) -> Result<ScheduledJob, SchedulerError>
    where
        F: FnOnce(ScheduledJobContext) -> BoxFuture<'static, Result<GateEvidence, SchedulerError>>
            + Send
            + 'static,
    {
        self.submit_at(root, key, identity_key, Instant::now(), work)
    }

    /// Submit work while preserving the request acceptance time in the global
    /// gate deadline. Callers that perform preflight before singleflight use
    /// this path so preflight cannot extend the configured hard timeout.
    pub fn submit_at<F>(
        &self,
        root: impl Into<PathBuf>,
        key: String,
        identity_key: String,
        requested_at: Instant,
        work: F,
    ) -> Result<ScheduledJob, SchedulerError>
    where
        F: FnOnce(ScheduledJobContext) -> BoxFuture<'static, Result<GateEvidence, SchedulerError>>
            + Send
            + 'static,
    {
        let root = root.into();
        let (job, owner) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closing {
                return Err(SchedulerError::Closing);
            }
            let generation = state.generations.get(&root).copied().unwrap_or(0);
            let composite = composite_key(&root, &key, generation);
            if let Some(existing) = state.jobs.get(&composite)
                && existing.is_joinable()
            {
                return Ok(ScheduledJob {
                    state: Arc::clone(existing),
                });
            }
            let mut generation = generation;
            let same_root_new_input = state.jobs.values().any(|existing| {
                existing.root == root
                    && existing.identity_key != identity_key
                    && !existing.complete.load(Ordering::Acquire)
            });
            if same_root_new_input {
                generation = generation.saturating_add(1);
                state.generations.insert(root.clone(), generation);
                for existing in state.jobs.values() {
                    if existing.root == root
                        && existing.identity_key != identity_key
                        && !existing.complete.load(Ordering::Acquire)
                    {
                        existing.cancel(CancelReason::Superseded);
                    }
                }
            }
            let composite = composite_key(&root, &key, generation);
            if let Some(existing) = state.jobs.get(&composite)
                && existing.is_joinable()
            {
                return Ok(ScheduledJob {
                    state: Arc::clone(existing),
                });
            }
            let progress = ProgressHub::new(requested_at);
            let job = Arc::new(JobState {
                id: make_job_id(),
                root: root.clone(),
                key,
                identity_key,
                generation,
                requested_at,
                cancellation: CancellationToken::new(),
                cancel_reason: Mutex::new(None),
                subscribers: AtomicUsize::new(0),
                result: Mutex::new(None),
                complete: AtomicBool::new(false),
                notify: Notify::new(),
                progress,
            });
            state.jobs.insert(composite, Arc::clone(&job));
            (job, true)
        };

        if owner {
            let state = Arc::clone(&self.state);
            let options = self.options.clone();
            let task_job = Arc::clone(&job);
            tokio::spawn(async move {
                let result = execute_job(&options, &task_job, work).await;
                task_job.finish(result);
                if let Ok(mut state) = state.lock() {
                    state
                        .jobs
                        .retain(|_, candidate| !Arc::ptr_eq(candidate, &task_job));
                }
            });
        }
        Ok(ScheduledJob { state: job })
    }

    pub async fn close(&self) {
        let jobs = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closing = true;
            let jobs = state.jobs.values().cloned().collect::<Vec<_>>();
            for job in &jobs {
                job.cancel(CancelReason::Shutdown);
            }
            jobs
        };
        for job in jobs {
            let _ = tokio::time::timeout(self.options.hard_timeout, wait_for_job(&job)).await;
        }
    }
}

async fn execute_job<F>(
    options: &SchedulerOptions,
    job: &Arc<JobState>,
    work: F,
) -> Result<GateEvidence, SchedulerError>
where
    F: FnOnce(ScheduledJobContext) -> BoxFuture<'static, Result<GateEvidence, SchedulerError>>
        + Send
        + 'static,
{
    let deadline = job.requested_at + options.hard_timeout;
    wait_debounce(options.debounce, deadline, &job.cancellation)
        .await
        .map_err(|error| cancellation_error(job, error))?;
    let lease_root = options.lease_dir.join("worktrees");
    let worktree_lease = acquire_lease(
        &lease_root,
        &job.root.to_string_lossy(),
        Some(deadline),
        Some(&job.cancellation),
    )
    .await
    .map_err(|error| cancellation_error(job, map_lease_error(error)))?;
    let host_lease = match acquire_host_slot(
        options.lease_dir.join("host"),
        options.host_concurrency,
        Some(deadline),
        Some(&job.cancellation),
    )
    .await
    {
        Ok(lease) => lease,
        Err(error) => {
            drop(worktree_lease);
            return Err(cancellation_error(job, map_lease_error(error)));
        }
    };
    if let Some(reason) = job.reason() {
        drop(host_lease);
        drop(worktree_lease);
        return Err(reason.error());
    }
    if Instant::now() >= deadline {
        drop(host_lease);
        drop(worktree_lease);
        job.cancel(CancelReason::TimedOut);
        return Err(SchedulerError::TimedOut);
    }
    let started_at = Instant::now();
    let timing = JobTiming {
        requested_at: job.requested_at,
        started_at,
        queue_ms: started_at
            .duration_since(job.requested_at)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    };
    job.progress
        .emit(ProgressStage::Running, None, 0.0, None, "running", false);
    let context = ScheduledJobContext {
        id: job.id.clone(),
        root: job.root.clone(),
        generation: job.generation,
        timing,
        deadline,
        cancellation: job.cancellation.clone(),
        progress: job.progress.clone(),
    };
    let heartbeat = spawn_heartbeat(
        job.progress.clone(),
        job.cancellation.clone(),
        options.heartbeat,
    );
    let result = work(context).await;
    heartbeat.abort();
    drop(host_lease);
    drop(worktree_lease);
    job.reason().map_or(result, |reason| Err(reason.error()))
}

fn spawn_heartbeat(
    progress: ProgressHub,
    cancellation: CancellationToken,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if interval.is_zero() {
            return;
        }
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    if cancellation.is_cancelled() {
                        return;
                    }
                    progress.emit(ProgressStage::Heartbeat, None, 0.0, None, "heartbeat", true);
                }
                () = cancellation.cancelled() => return,
            }
        }
    })
}

async fn wait_for_job(job: &JobState) {
    loop {
        let notified = job.notify.notified();
        if job.complete.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn wait_debounce(
    debounce: Duration,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(), SchedulerError> {
    if debounce.is_zero() {
        if cancellation.is_cancelled() {
            return Err(SchedulerError::Cancelled);
        }
        return Ok(());
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    tokio::select! {
        () = tokio::time::sleep(debounce.min(remaining)) => {
            if cancellation.is_cancelled() { Err(SchedulerError::Cancelled) } else if Instant::now() >= deadline { Err(SchedulerError::TimedOut) } else { Ok(()) }
        }
        () = cancellation.cancelled() => Err(SchedulerError::Cancelled),
    }
}

fn map_lease_error(error: LeaseError) -> SchedulerError {
    match error {
        LeaseError::Cancelled => SchedulerError::Cancelled,
        LeaseError::TimedOut { .. } => SchedulerError::TimedOut,
        LeaseError::UnsafeDirectory(path) => SchedulerError::Lease(path.display().to_string()),
        LeaseError::Io { path, message } => {
            SchedulerError::Lease(format!("{}: {message}", path.display()))
        }
    }
}

fn cancellation_error(job: &JobState, fallback: SchedulerError) -> SchedulerError {
    job.reason().map_or(fallback, CancelReason::error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSnapshot {
    pub free_disk_mb: u64,
    pub available_memory_mb: Option<u64>,
}

fn resource_snapshot(path: &Path) -> std::io::Result<ResourceSnapshot> {
    let free_disk = fs4::available_space(path)?;
    let available_memory = available_memory_bytes()?;
    Ok(ResourceSnapshot {
        free_disk_mb: free_disk / (1024 * 1024),
        available_memory_mb: available_memory.map(|bytes| bytes / (1024 * 1024)),
    })
}

fn memory_below_floor(available_memory_mb: Option<u64>, minimum_mb: u64) -> bool {
    available_memory_mb.is_some_and(|available| available < minimum_mb)
}

fn available_memory_bytes() -> std::io::Result<Option<u64>> {
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string(RESOURCE_MEMORY_PATH)?;
        if let Some(value) = contents.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next() == Some("MemAvailable:"))
                .then(|| parts.next()?.parse::<u64>().ok())
                .flatten()
        }) {
            return Ok(Some(value.saturating_mul(1024)));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "MemAvailable was missing from /proc/meminfo",
        ))
    }
    #[cfg(not(target_os = "linux"))]
    Ok(None)
}

fn composite_key(root: &Path, key: &str, generation: u64) -> String {
    format!("{}\0{}\0{generation}", root.display(), key)
}

fn make_job_id() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "gate-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn bounded_message(mut message: String) -> String {
    const MAX_MESSAGE_BYTES: usize = 512;
    if message.len() <= MAX_MESSAGE_BYTES {
        return message;
    }
    message.truncate(MAX_MESSAGE_BYTES);
    while !message.is_char_boundary(message.len()) {
        message.pop();
    }
    message
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn dropping_a_progress_registration_detaches_its_callback() {
        let hub = ProgressHub::new(Instant::now());
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let registration = hub
            .add_callback(Arc::new(move |_| {
                callback_calls.fetch_add(1, Ordering::SeqCst);
            }))
            .expect("register progress callback");
        hub.emit(
            ProgressStage::Running,
            None,
            0.5,
            Some(1.0),
            "running",
            false,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(registration);
        hub.emit(
            ProgressStage::Running,
            None,
            0.75,
            Some(1.0),
            "still running",
            false,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use crate::gate::types::{GateAuthority, GateRequest, GateStatus, GateTargetId};

    fn retained_job(root: &Path, key: &str, identity_key: &str) -> Arc<JobState> {
        Arc::new(JobState {
            id: "retained".to_owned(),
            root: root.to_owned(),
            key: key.to_owned(),
            identity_key: identity_key.to_owned(),
            generation: 0,
            requested_at: Instant::now(),
            cancellation: CancellationToken::new(),
            cancel_reason: Mutex::new(None),
            subscribers: AtomicUsize::new(0),
            result: Mutex::new(None),
            complete: AtomicBool::new(false),
            notify: Notify::new(),
            progress: ProgressHub::new(Instant::now()),
        })
    }

    #[tokio::test]
    async fn completed_or_cancelled_jobs_are_not_joined_while_retained() {
        for cancelled in [false, true] {
            let lease_dir = std::fs::canonicalize(std::env::temp_dir())
                .expect("canonical temp directory")
                .join(format!(
                    "agz-rust-coder-scheduler-retained-{}-{}",
                    std::process::id(),
                    i32::from(cancelled)
                ));
            let mut options = SchedulerOptions::new(&lease_dir);
            options.debounce = Duration::ZERO;
            options.hard_timeout = Duration::from_secs(5);
            let scheduler = GateScheduler::new(options);
            let root = lease_dir.join("workspace");
            let key = "check:epoch:1";
            let identity_key = "identity";
            let request = GateRequest::new(&root, GateTargetId::Check);
            let retained = retained_job(&root, key, identity_key);
            if cancelled {
                retained.cancel(CancelReason::Cancelled);
            } else {
                let mut evidence = GateEvidence::pending("retained", &request);
                evidence.status = GateStatus::FastPass;
                evidence.authority = GateAuthority::Fast;
                retained.finish(Ok(evidence));
            }
            scheduler
                .state
                .lock()
                .expect("scheduler state lock")
                .jobs
                .insert(composite_key(&root, key, 0), Arc::clone(&retained));

            let replacement = scheduler
                .submit_at(
                    root,
                    key.to_owned(),
                    identity_key.to_owned(),
                    Instant::now(),
                    move |context| {
                        let request = request.clone();
                        Box::pin(async move { Ok(GateEvidence::pending(context.id, &request)) })
                    },
                )
                .expect("submit replacement job");
            assert_ne!(replacement.id(), retained.id.as_str());
            let subscription = replacement.subscribe(None);
            assert!(subscription.wait().await.is_ok());
            scheduler.close().await;
            let _ = std::fs::remove_dir_all(lease_dir);
        }
    }

    #[test]
    fn unknown_memory_does_not_block_resource_admission() {
        assert!(!memory_below_floor(None, 512));
        assert!(memory_below_floor(Some(511), 512));
        assert!(!memory_below_floor(Some(512), 512));
    }
}
