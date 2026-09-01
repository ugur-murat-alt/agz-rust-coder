use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};

use crate::workspace::ClientRoots;

/// The terminal and non-terminal states emitted by the validation domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateStatus {
    FastPass,
    FullPass,
    Fail,
    Pending,
    Timeout,
    Stale,
    Superseded,
    Cancelled,
    Unavailable,
    Inconclusive,
    ResourceBlocked,
}

impl GateStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FastPass => "FAST_PASS",
            Self::FullPass => "FULL_PASS",
            Self::Fail => "FAIL",
            Self::Pending => "PENDING",
            Self::Timeout => "TIMEOUT",
            Self::Stale => "STALE",
            Self::Superseded => "SUPERSEDED",
            Self::Cancelled => "CANCELLED",
            Self::Unavailable => "UNAVAILABLE",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::ResourceBlocked => "RESOURCE_BLOCKED",
        }
    }

    pub const fn authority(self) -> GateAuthority {
        match self {
            Self::FullPass => GateAuthority::Full,
            Self::FastPass => GateAuthority::Fast,
            Self::Fail
            | Self::Pending
            | Self::Timeout
            | Self::Stale
            | Self::Superseded
            | Self::Cancelled
            | Self::Unavailable
            | Self::Inconclusive
            | Self::ResourceBlocked => GateAuthority::None,
        }
    }

    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// The validation authority a result is allowed to grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateAuthority {
    None,
    Fast,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateMode {
    Fast,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateDetail {
    Compact,
    Standard,
    Full,
}

impl Default for GateDetail {
    fn default() -> Self {
        Self::Compact
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateTargetId {
    Check,
    Clippy,
    Test,
    Doc,
    Fmt,
    All,
}

impl GateTargetId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Doc => "doc",
            Self::Fmt => "fmt",
            Self::All => "all",
        }
    }

    pub const fn mode(self) -> GateMode {
        if matches!(self, Self::All) {
            GateMode::Full
        } else {
            GateMode::Fast
        }
    }
}

impl Default for GateTargetId {
    fn default() -> Self {
        Self::Check
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateSource {
    Explicit,
    Automatic,
    HostFinalization,
}

impl Default for GateSource {
    fn default() -> Self {
        Self::Explicit
    }
}

/// A protocol-independent request accepted by [`crate::gate::CheckService`].
#[derive(Debug, Clone)]
pub struct GateRequest {
    pub directory: Option<PathBuf>,
    pub target: GateTargetId,
    pub timings: bool,
    pub detail: GateDetail,
    pub client_roots: ClientRoots,
    /// Authorization snapshot epoch supplied by the protocol boundary.
    ///
    /// Zero is the unbound default used by domain tests and configured-root
    /// callers that do not have a client-roots snapshot.
    pub root_epoch: u64,
    pub source: GateSource,
}

impl GateRequest {
    pub fn new(directory: impl Into<PathBuf>, target: GateTargetId) -> Self {
        Self {
            directory: Some(directory.into()),
            target,
            timings: false,
            detail: GateDetail::Compact,
            client_roots: ClientRoots::unsupported(),
            root_epoch: 0,
            source: GateSource::Explicit,
        }
    }

    pub fn for_all(directory: impl Into<PathBuf>) -> Self {
        Self::new(directory, GateTargetId::All)
    }

    pub fn without_directory(target: GateTargetId) -> Self {
        Self {
            directory: None,
            target,
            timings: false,
            detail: GateDetail::Compact,
            client_roots: ClientRoots::unsupported(),
            root_epoch: 0,
            source: GateSource::Explicit,
        }
    }

    pub fn with_timings(mut self, timings: bool) -> Self {
        self.timings = timings;
        self
    }

    pub fn with_detail(mut self, detail: GateDetail) -> Self {
        self.detail = detail;
        self
    }

    pub fn with_client_roots(mut self, client_roots: ClientRoots) -> Self {
        self.client_roots = client_roots;
        self
    }

    pub fn with_root_epoch(mut self, root_epoch: u64) -> Self {
        self.root_epoch = root_epoch;
        self
    }

    pub fn with_source(mut self, source: GateSource) -> Self {
        self.source = source;
        self
    }

    pub const fn mode(&self) -> GateMode {
        self.target.mode()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressStage {
    Accepted,
    Preflight,
    Queued,
    Running,
    Finished,
    Completed,
    Failed,
    Cancelled,
    Heartbeat,
}

/// A bounded progress event. The callback is domain-local and has no RMCP
/// dependency, which lets protocol adapters translate it to their own wire
/// notification type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub stage: ProgressStage,
    pub target: Option<GateTargetId>,
    pub progress: f64,
    pub total: Option<f64>,
    pub message: String,
    pub heartbeat: bool,
    pub elapsed_ms: u64,
}

pub type ProgressCallback = Arc<dyn Fn(ProgressEvent) + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateScopeStrategy {
    Workspace,
    Affected,
    Shadow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateScope {
    pub strategy: GateScopeStrategy,
    pub packages: Vec<String>,
    pub package_ids: Vec<String>,
    pub changed_paths: Vec<PathBuf>,
    pub widened_because: Vec<String>,
}

impl GateScope {
    pub fn workspace(changed_paths: Vec<PathBuf>, reason: impl Into<String>) -> Self {
        Self {
            strategy: GateScopeStrategy::Workspace,
            packages: Vec::new(),
            package_ids: Vec::new(),
            changed_paths,
            widened_because: vec![reason.into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionApplicability {
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticSpan {
    pub file: String,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    pub line_start: u64,
    pub line_end: u64,
    pub column_start: u64,
    pub column_end: u64,
    pub is_primary: bool,
    pub label: Option<String>,
    pub suggested_replacement: Option<String>,
    pub suggestion_applicability: Option<SuggestionApplicability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticChild {
    pub level: String,
    pub message: String,
    pub rendered: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<DiagnosticChild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionEdit {
    pub file: String,
    pub line_start: u64,
    pub line_end: u64,
    pub column_start: u64,
    pub column_end: u64,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompilerSuggestion {
    pub message: String,
    pub applicability: SuggestionApplicability,
    pub edits: Vec<SuggestionEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateDiagnostic {
    pub code: Option<String>,
    pub level: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub message: String,
    pub rendered: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<DiagnosticChild>,
    pub suggestions: Vec<CompilerSuggestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionPatch {
    pub file: String,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionPackage {
    pub patches: Vec<SuggestionPatch>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CargoBuildTelemetry {
    pub total_units: u64,
    pub fresh_units: u64,
    pub rebuilt_units: u64,
    pub build_scripts: u64,
    pub linked_units: u64,
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateStepResult {
    pub target: GateTargetId,
    pub command: String,
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub first_diagnostic_ms: Option<u64>,
    pub diagnostics: Vec<GateDiagnostic>,
    pub suggestion_package: Option<SuggestionPackage>,
    pub tail: String,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub drain_complete: bool,
    pub cleanup_complete: bool,
    pub build: Option<CargoBuildTelemetry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationProfile {
    pub id: String,
    pub command_hash: String,
    pub cache_mode: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateBuildInfo {
    pub metadata_cache: String,
    pub target_directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateEvidence {
    pub version: u8,
    pub job_id: String,
    pub status: GateStatus,
    pub authority: GateAuthority,
    pub mode: GateMode,
    pub generation: u64,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub response_ms: u64,
    pub queue_ms: u64,
    pub first_diagnostic_ms: Option<u64>,
    pub requested_dir: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub input_hash: String,
    pub command_hash: String,
    pub environment_hash: String,
    pub cache_mode: String,
    pub scope: GateScope,
    pub steps: Vec<GateStepResult>,
    pub build: Option<GateBuildInfo>,
    pub profile: Option<ValidationProfile>,
    pub source: GateSource,
    pub message: Option<String>,
    pub warnings: Vec<String>,
}

impl GateEvidence {
    pub fn pending(job_id: impl Into<String>, request: &GateRequest) -> Self {
        Self {
            version: 1,
            job_id: job_id.into(),
            status: GateStatus::Pending,
            authority: GateAuthority::None,
            mode: request.mode(),
            generation: 0,
            requested_at: String::new(),
            started_at: None,
            finished_at: None,
            response_ms: 0,
            queue_ms: 0,
            first_diagnostic_ms: None,
            requested_dir: request.directory.clone().unwrap_or_default(),
            workspace_root: None,
            manifest_path: None,
            input_hash: "pending".to_owned(),
            command_hash: "pending".to_owned(),
            environment_hash: "pending".to_owned(),
            cache_mode: "pending".to_owned(),
            scope: GateScope {
                strategy: GateScopeStrategy::Workspace,
                packages: Vec::new(),
                package_ids: Vec::new(),
                changed_paths: Vec::new(),
                widened_because: vec!["preflight is still running".to_owned()],
            },
            steps: Vec::new(),
            build: None,
            profile: None,
            source: request.source,
            message: None,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GateTarget {
    pub id: GateTargetId,
    pub label: &'static str,
    pub args: Vec<OsString>,
    pub timeout: Duration,
}

impl GateTarget {
    pub fn command_string(&self, cargo: &std::path::Path) -> String {
        std::iter::once(cargo.to_string_lossy().into_owned())
            .chain(
                self.args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned()),
            )
            .collect::<Vec<_>>()
            .join(" ")
    }
}
