//! Strict configuration loading for the standalone MCP server.
//!
//! Configuration is deliberately assembled in one direction only:
//! defaults, explicit TOML, environment, then command-line options.  In
//! particular, lists are replaced by the first higher-precedence layer that
//! supplies them; they are never appended implicitly.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
};

use clap::{ArgAction, Parser};
use serde::Deserialize;
use thiserror::Error;

const ENV_PREFIX: &str = "AGZ_RUST_CODER_";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TOML configuration: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid command-line configuration: {0}")]
    Cli(#[from] clap::Error),
    #[error("unknown environment variable {0}")]
    UnknownEnvironment(String),
    #[error("invalid environment variable {name}: {message}")]
    InvalidEnvironment { name: String, message: String },
    #[error("invalid configuration field {field}: {message}")]
    InvalidField {
        field: &'static str,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub server: ServerConfig,
    pub tools: ToolConfig,
    pub cargo: CargoConfig,
    pub gate: GateConfig,
    pub change: ChangeConfig,
    pub verify: VerifyConfig,
    pub work: WorkConfig,
    pub rust_analyzer: RustAnalyzerConfig,
    pub docs: DocsConfig,
    pub context: ContextConfig,
    pub api: ApiConfig,
    pub limits: LimitsConfig,
    pub profile: ProfileConfig,
    pub repair: RepairConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub allow_roots: Vec<PathBuf>,
    pub allow_dependency_roots: Vec<PathBuf>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolConfig {
    pub check: bool,
    pub profile: bool,
    pub audit: bool,
    pub crate_lookup: bool,
    pub docs: bool,
    pub context: bool,
    pub api: bool,
    pub verify: bool,
    pub lsp: bool,
    pub explain: bool,
    pub rename: bool,
    pub refactor: bool,
    pub change: bool,
    pub repair: bool,
    pub work: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoConfig {
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateConfig {
    pub hard_timeout_ms: u64,
    pub debounce_ms: u64,
    pub host_concurrency: u64,
    pub scope: GateScope,
    pub cache: GateCache,
    pub min_free_disk_mb: u64,
    pub min_available_memory_mb: u64,
    pub cache_dir: PathBuf,
    pub lease_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyConfig {
    pub max_cells: u64,
    pub max_wall_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GateScope {
    Workspace,
    #[default]
    Shadow,
    Affected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GateCache {
    #[default]
    Auto,
    Project,
    Isolated,
}

/// Bounds and paths for the revision-bound changeset scratch area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeConfig {
    pub scratch_dir: PathBuf,
    pub max_active: u64,
    pub max_files: u64,
    pub max_bytes: u64,
    pub ttl_ms: u64,
    pub max_revisions: u64,
}

/// Bounds for the bounded work executor. `wall_time_ms` is the per-work wall
/// budget; `continuation_ttl_ms` bounds how long a single-use host handoff token
/// stays valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkConfig {
    pub max_active: u64,
    pub max_compiles: u64,
    pub max_candidates: u64,
    pub max_handoffs: u64,
    pub wall_time_ms: u64,
    pub continuation_ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustAnalyzerConfig {
    pub path: Option<PathBuf>,
    pub timeout_ms: u64,
    pub idle_ms: u64,
    pub max_instances: u64,
    pub check_hint: bool,
    pub workspace_code: WorkspaceCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceCode {
    #[default]
    Deny,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsConfig {
    pub timeout_ms: u64,
    pub fallback: DocsFallback,
    pub cache_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocsFallback {
    #[default]
    Auto,
    Local,
    Network,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextConfig {
    pub max_capsules: u64,
    pub capsule_ttl_ms: u64,
    pub max_items: u64,
}

/// Bounds for `api` resolution evidence and compile-only probes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConfig {
    pub max_snippets: u64,
    pub max_snippet_bytes: u64,
    pub compile_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitsConfig {
    pub max_rename_edits: u64,
    pub max_refactor_edits: u64,
    pub process_output_bytes: u64,
    pub tool_output_bytes: u64,
    pub max_in_flight_tools: u64,
    pub max_active_tasks: u64,
    pub max_retained_tasks: u64,
    pub identity_files: u64,
    pub identity_file_bytes: u64,
    pub identity_total_bytes: u64,
    pub external_files: u64,
    pub external_bytes: u64,
    pub git_output_bytes: u64,
    pub audit_files: u64,
    pub audit_file_bytes: u64,
    pub audit_total_bytes: u64,
    pub audit_findings: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileConfig {
    pub max_report_bytes: u64,
    pub max_runs: u64,
    pub compare_samples: u64,
}

/// Bounds for the compiler-driven `repair` tool. Request budgets may only
/// narrow these configured caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairConfig {
    pub max_candidates: u32,
    pub max_compiles: u32,
    pub wall_time_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub path: PathBuf,
    pub retention_bytes: u64,
    pub retention_days: u64,
    pub max_archives: u64,
}

impl Config {
    /// Build the documented defaults for a specific process CWD.
    pub fn defaults_at(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let namespace = platform_cache_dir();
        let docs_dir = namespace.join("docs");
        let state_dir = namespace.join("state");
        Self {
            server: ServerConfig {
                allow_roots: vec![cwd],
                allow_dependency_roots: Vec::new(),
            },
            tools: ToolConfig {
                check: true,
                profile: true,
                audit: true,
                crate_lookup: true,
                docs: true,
                context: true,
                api: true,
                verify: true,
                lsp: true,
                explain: true,
                rename: true,
                refactor: true,
                change: true,
                repair: true,
                work: true,
            },
            cargo: CargoConfig { path: None },
            gate: GateConfig {
                hard_timeout_ms: 600_000,
                debounce_ms: 500,
                host_concurrency: 1,
                scope: GateScope::Shadow,
                cache: GateCache::Auto,
                min_free_disk_mb: 1_024,
                min_available_memory_mb: 512,
                cache_dir: state_dir.join("gate"),
                lease_dir: state_dir.join("leases"),
            },
            change: ChangeConfig {
                scratch_dir: state_dir.join("change"),
                max_active: 4,
                max_files: 20_000,
                max_bytes: 268_435_456,
                ttl_ms: 86_400_000,
                max_revisions: 32,
            },
            verify: VerifyConfig {
                max_cells: 8,
                max_wall_ms: 120_000,
            },
            work: WorkConfig {
                max_active: 4,
                max_compiles: 12,
                max_candidates: 4,
                max_handoffs: 3,
                wall_time_ms: 600_000,
                continuation_ttl_ms: 900_000,
            },
            rust_analyzer: RustAnalyzerConfig {
                path: None,
                timeout_ms: 30_000,
                idle_ms: 900_000,
                max_instances: 2,
                check_hint: false,
                workspace_code: WorkspaceCode::Deny,
            },
            docs: DocsConfig {
                timeout_ms: 300_000,
                fallback: DocsFallback::Auto,
                cache_dir: docs_dir,
            },
            context: ContextConfig {
                max_capsules: 32,
                capsule_ttl_ms: 900_000,
                max_items: 64,
            },
            api: ApiConfig {
                max_snippets: 4,
                max_snippet_bytes: 32_768,
                compile_timeout_ms: 120_000,
            },
            limits: LimitsConfig {
                max_rename_edits: 200,
                max_refactor_edits: 200,
                process_output_bytes: 8_388_608,
                tool_output_bytes: 49_152,
                max_in_flight_tools: 32,
                max_active_tasks: 16,
                max_retained_tasks: 128,
                identity_files: 20_000,
                identity_file_bytes: 33_554_432,
                identity_total_bytes: 268_435_456,
                external_files: 5_000,
                external_bytes: 67_108_864,
                git_output_bytes: 8_388_608,
                audit_files: 10_000,
                audit_file_bytes: 2_097_152,
                audit_total_bytes: 67_108_864,
                audit_findings: 200,
            },
            profile: ProfileConfig {
                max_report_bytes: 4 * 1024 * 1024,
                max_runs: 4,
                compare_samples: 3,
            },
            repair: RepairConfig {
                max_candidates: 4,
                max_compiles: 4,
                wall_time_ms: 120_000,
            },
            telemetry: TelemetryConfig {
                enabled: true,
                path: state_dir.join("activity.jsonl"),
                retention_bytes: 8 * 1024 * 1024,
                retention_days: 7,
                max_archives: 3,
            },
        }
    }

    pub fn defaults() -> Self {
        let cwd = env::current_dir().unwrap_or_else(|_| env::temp_dir());
        Self::defaults_at(cwd)
    }

    /// Load only explicit sources. This form is deterministic and is used by
    /// protocol/configuration tests without mutating the process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when TOML, environment, command-line, or range
    /// validation fails.
    pub fn from_sources<I, K, V>(
        cwd: impl Into<PathBuf>,
        toml_text: Option<&str>,
        environment: I,
        cli: &CliOptions,
    ) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut config = Self::defaults_at(cwd);
        if let Some(toml_text) = toml_text {
            let file = toml::from_str::<FileConfig>(toml_text)?;
            apply_file(&mut config, file);
        }
        for (key, value) in environment {
            let key = key.into();
            let value = value.into();
            if key.starts_with(ENV_PREFIX) {
                apply_environment(&mut config, &key, &value)?;
            }
        }
        apply_cli(&mut config, cli)?;
        config.validate()?;
        Ok(config)
    }

    /// Load CLI, the explicitly named TOML file, and the process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when command-line arguments, the config file,
    /// environment values, or range validation is invalid.
    pub fn load_from<I, T>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let cli = CliOptions::try_parse_from(args)?;
        let toml_text = cli
            .config
            .as_ref()
            .map(|path| {
                fs::read_to_string(path).map_err(|source| ConfigError::ReadFile {
                    path: path.clone(),
                    source,
                })
            })
            .transpose()?;
        let cwd = env::current_dir().unwrap_or_else(|_| env::temp_dir());
        Self::from_sources(cwd, toml_text.as_deref(), process_environment()?, &cli)
    }

    /// # Errors
    ///
    /// Returns an error when a configured value is outside its supported
    /// range, task limits are inconsistent, or server-owned paths overlap an
    /// authorized workspace or dependency root.
    pub fn validate(&self) -> Result<(), ConfigError> {
        check_range(
            "gate.hard_timeout_ms",
            self.gate.hard_timeout_ms,
            1,
            u64::MAX,
        )?;
        check_range("gate.debounce_ms", self.gate.debounce_ms, 0, 5_000)?;
        check_range("gate.host_concurrency", self.gate.host_concurrency, 1, 64)?;
        check_range("verify.max_cells", self.verify.max_cells, 1, 64)?;
        check_range(
            "verify.max_wall_ms",
            self.verify.max_wall_ms,
            1_000,
            3_600_000,
        )?;
        check_range(
            "rust_analyzer.timeout_ms",
            self.rust_analyzer.timeout_ms,
            1,
            900_000,
        )?;
        check_range(
            "rust_analyzer.idle_ms",
            self.rust_analyzer.idle_ms,
            1,
            86_400_000,
        )?;
        check_range(
            "rust_analyzer.max_instances",
            self.rust_analyzer.max_instances,
            1,
            16,
        )?;
        check_range("docs.timeout_ms", self.docs.timeout_ms, 1, 3_600_000)?;
        check_range("change.max_active", self.change.max_active, 1, 64)?;
        check_range("change.max_files", self.change.max_files, 1, 1_000_000)?;
        check_range("change.max_bytes", self.change.max_bytes, 1_024, u64::MAX)?;
        check_range("change.ttl_ms", self.change.ttl_ms, 1, u64::MAX)?;
        check_range("change.max_revisions", self.change.max_revisions, 1, 4_096)?;
        check_range("work.max_active", self.work.max_active, 1, 64)?;
        check_range("work.max_compiles", self.work.max_compiles, 1, 4_096)?;
        check_range("work.max_candidates", self.work.max_candidates, 1, 256)?;
        check_range("work.max_handoffs", self.work.max_handoffs, 0, 64)?;
        check_range("work.wall_time_ms", self.work.wall_time_ms, 1, 86_400_000)?;
        check_range(
            "work.continuation_ttl_ms",
            self.work.continuation_ttl_ms,
            1_000,
            86_400_000,
        )?;
        check_range("context.max_capsules", self.context.max_capsules, 1, 1_024)?;
        check_range(
            "context.capsule_ttl_ms",
            self.context.capsule_ttl_ms,
            1_000,
            86_400_000,
        )?;
        check_range("context.max_items", self.context.max_items, 1, 256)?;
        check_range("api.max_snippets", self.api.max_snippets, 1, 16)?;
        check_range(
            "api.max_snippet_bytes",
            self.api.max_snippet_bytes,
            256,
            1_048_576,
        )?;
        check_range(
            "api.compile_timeout_ms",
            self.api.compile_timeout_ms,
            1_000,
            3_600_000,
        )?;
        check_range(
            "profile.max_report_bytes",
            self.profile.max_report_bytes,
            1_024,
            67_108_864,
        )?;
        check_range("profile.max_runs", self.profile.max_runs, 1, 16)?;
        check_range(
            "profile.compare_samples",
            self.profile.compare_samples,
            1,
            8,
        )?;
        check_range(
            "repair.max_candidates",
            u64::from(self.repair.max_candidates),
            1,
            32,
        )?;
        check_range(
            "repair.max_compiles",
            u64::from(self.repair.max_compiles),
            1,
            64,
        )?;
        check_range(
            "repair.wall_time_ms",
            self.repair.wall_time_ms,
            1_000,
            3_600_000,
        )?;
        check_range(
            "limits.tool_output_bytes",
            self.limits.tool_output_bytes,
            512,
            49_152,
        )?;
        check_range(
            "limits.max_in_flight_tools",
            self.limits.max_in_flight_tools,
            1,
            4_096,
        )?;
        check_range(
            "limits.max_active_tasks",
            self.limits.max_active_tasks,
            1,
            65_536,
        )?;
        check_range(
            "limits.max_retained_tasks",
            self.limits.max_retained_tasks,
            1,
            65_536,
        )?;
        if self.limits.max_active_tasks > self.limits.max_retained_tasks {
            return Err(ConfigError::InvalidField {
                field: "limits.max_active_tasks",
                message: "cannot exceed limits.max_retained_tasks".to_owned(),
            });
        }
        for (field, value) in [
            ("limits.max_rename_edits", self.limits.max_rename_edits),
            ("limits.max_refactor_edits", self.limits.max_refactor_edits),
            (
                "limits.process_output_bytes",
                self.limits.process_output_bytes,
            ),
            ("limits.identity_files", self.limits.identity_files),
            (
                "limits.identity_file_bytes",
                self.limits.identity_file_bytes,
            ),
            (
                "limits.identity_total_bytes",
                self.limits.identity_total_bytes,
            ),
            ("limits.external_files", self.limits.external_files),
            ("limits.external_bytes", self.limits.external_bytes),
            ("limits.git_output_bytes", self.limits.git_output_bytes),
            ("limits.audit_files", self.limits.audit_files),
            ("limits.audit_file_bytes", self.limits.audit_file_bytes),
            ("limits.audit_total_bytes", self.limits.audit_total_bytes),
            ("limits.audit_findings", self.limits.audit_findings),
            ("telemetry.retention_days", self.telemetry.retention_days),
            ("telemetry.max_archives", self.telemetry.max_archives),
        ] {
            check_range(field, value, 1, u64::MAX)?;
        }
        check_range(
            "telemetry.retention_bytes",
            self.telemetry.retention_bytes,
            4 * 1024,
            u64::MAX,
        )?;
        let path_checks = [
            (
                "gate.cache_dir",
                self.gate.cache_dir.as_path(),
                !matches!(self.gate.cache, GateCache::Project),
            ),
            ("gate.lease_dir", self.gate.lease_dir.as_path(), true),
            ("docs.cache_dir", self.docs.cache_dir.as_path(), true),
            (
                "change.scratch_dir",
                self.change.scratch_dir.as_path(),
                self.tools.change,
            ),
            (
                "telemetry.path",
                self.telemetry.path.as_path(),
                self.telemetry.enabled,
            ),
        ];
        for (field, path, enabled) in path_checks {
            if enabled {
                reject_path_overlap(
                    field,
                    path,
                    &self.server.allow_roots,
                    &self.server.allow_dependency_roots,
                )?;
            }
        }
        Ok(())
    }

    /// The startup tool set is static for the lifetime of a server.
    pub fn enabled_tool_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.tools.check {
            names.push("check");
        }
        if self.tools.profile {
            names.push("profile");
        }
        if self.tools.audit {
            names.push("audit");
        }
        if self.tools.crate_lookup {
            names.push("crate_lookup");
        }
        if self.tools.docs {
            names.push("docs");
        }
        if self.tools.context {
            names.push("context");
        }
        if self.tools.api {
            names.push("api");
        }
        if self.tools.explain {
            names.push("explain");
        }
        if self.tools.verify {
            names.push("verify");
        }
        if self.tools.lsp {
            names.extend([
                "symbol",
                "references",
                "definition",
                "symbols",
                "implementations",
                "hierarchy",
            ]);
            if self.tools.rename {
                names.push("rename");
            }
            if self.tools.refactor {
                names.push("refactor");
            }
        }
        if self.tools.change {
            names.push("change");
        }
        if self.tools.repair && self.tools.change {
            names.push("repair");
        }
        if self.tools.work {
            names.push("work");
        }
        names
    }

    pub fn tasks_enabled(&self) -> bool {
        self.tools.check || self.tools.docs
    }

    pub fn task_ttl_ms(&self) -> u64 {
        self.gate
            .hard_timeout_ms
            .max(self.docs.timeout_ms)
            .saturating_add(30_000)
    }
}

#[derive(Debug, Clone, Parser, Default)]
#[command(name = "agz-rust-coder", version, disable_help_subcommand = true)]
pub struct CliOptions {
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long = "allow-root", action = ArgAction::Append, value_name = "PATH")]
    pub allow_root: Option<Vec<PathBuf>>,
    #[arg(long = "allow-dependency-root", action = ArgAction::Append, value_name = "PATH")]
    pub allow_dependency_root: Option<Vec<PathBuf>>,
    #[arg(long = "tools-check")]
    pub tools_check: Option<bool>,
    #[arg(long = "tools-profile")]
    pub tools_profile: Option<bool>,
    #[arg(long = "tools-audit")]
    pub tools_audit: Option<bool>,
    #[arg(long = "tools-crate-lookup")]
    pub tools_crate_lookup: Option<bool>,
    #[arg(long = "tools-docs")]
    pub tools_docs: Option<bool>,
    #[arg(long = "tools-context")]
    pub tools_context: Option<bool>,
    #[arg(long = "tools-api")]
    pub tools_api: Option<bool>,
    #[arg(long = "tools-verify")]
    pub tools_verify: Option<bool>,
    #[arg(long = "tools-lsp")]
    pub tools_lsp: Option<bool>,
    #[arg(long = "tools-explain")]
    pub tools_explain: Option<bool>,
    #[arg(long = "tools-rename")]
    pub tools_rename: Option<bool>,
    #[arg(long = "tools-refactor")]
    pub tools_refactor: Option<bool>,
    #[arg(long = "tools-change")]
    pub tools_change: Option<bool>,
    #[arg(long = "tools-repair")]
    pub tools_repair: Option<bool>,
    #[arg(long = "tools-work")]
    pub tools_work: Option<bool>,
    #[arg(long = "cargo-path")]
    pub cargo_path: Option<PathBuf>,
    #[arg(long = "gate-hard-timeout-ms")]
    pub gate_hard_timeout_ms: Option<u64>,
    #[arg(long = "gate-debounce-ms")]
    pub gate_debounce_ms: Option<u64>,
    #[arg(long = "gate-host-concurrency")]
    pub gate_host_concurrency: Option<u64>,
    #[arg(long = "gate-scope")]
    pub gate_scope: Option<String>,
    #[arg(long = "gate-cache")]
    pub gate_cache: Option<String>,
    #[arg(long = "gate-min-free-disk-mb")]
    pub gate_min_free_disk_mb: Option<u64>,
    #[arg(long = "gate-min-available-memory-mb")]
    pub gate_min_available_memory_mb: Option<u64>,
    #[arg(long = "gate-cache-dir")]
    pub gate_cache_dir: Option<PathBuf>,
    #[arg(long = "gate-lease-dir")]
    pub gate_lease_dir: Option<PathBuf>,
    #[arg(long = "change-scratch-dir")]
    pub change_scratch_dir: Option<PathBuf>,
    #[arg(long = "change-max-active")]
    pub change_max_active: Option<u64>,
    #[arg(long = "change-max-files")]
    pub change_max_files: Option<u64>,
    #[arg(long = "change-max-bytes")]
    pub change_max_bytes: Option<u64>,
    #[arg(long = "change-ttl-ms")]
    pub change_ttl_ms: Option<u64>,
    #[arg(long = "change-max-revisions")]
    pub change_max_revisions: Option<u64>,
    #[arg(long = "verify-max-cells")]
    pub verify_max_cells: Option<u64>,
    #[arg(long = "verify-max-wall-ms")]
    pub verify_max_wall_ms: Option<u64>,
    #[arg(long = "work-max-active")]
    pub work_max_active: Option<u64>,
    #[arg(long = "work-max-compiles")]
    pub work_max_compiles: Option<u64>,
    #[arg(long = "work-max-candidates")]
    pub work_max_candidates: Option<u64>,
    #[arg(long = "work-max-handoffs")]
    pub work_max_handoffs: Option<u64>,
    #[arg(long = "work-wall-time-ms")]
    pub work_wall_time_ms: Option<u64>,
    #[arg(long = "work-continuation-ttl-ms")]
    pub work_continuation_ttl_ms: Option<u64>,
    #[arg(long = "rust-analyzer-path")]
    pub rust_analyzer_path: Option<PathBuf>,
    #[arg(long = "rust-analyzer-timeout-ms")]
    pub rust_analyzer_timeout_ms: Option<u64>,
    #[arg(long = "rust-analyzer-idle-ms")]
    pub rust_analyzer_idle_ms: Option<u64>,
    #[arg(long = "rust-analyzer-max-instances")]
    pub rust_analyzer_max_instances: Option<u64>,
    #[arg(long = "rust-analyzer-check-hint")]
    pub rust_analyzer_check_hint: Option<bool>,
    #[arg(long = "rust-analyzer-workspace-code")]
    pub rust_analyzer_workspace_code: Option<String>,
    #[arg(long = "docs-timeout-ms")]
    pub docs_timeout_ms: Option<u64>,
    #[arg(long = "docs-fallback")]
    pub docs_fallback: Option<String>,
    #[arg(long = "docs-cache-dir")]
    pub docs_cache_dir: Option<PathBuf>,
    #[arg(long = "context-max-capsules")]
    pub context_max_capsules: Option<u64>,
    #[arg(long = "context-capsule-ttl-ms")]
    pub context_capsule_ttl_ms: Option<u64>,
    #[arg(long = "context-max-items")]
    pub context_max_items: Option<u64>,
    #[arg(long = "api-max-snippets")]
    pub api_max_snippets: Option<u64>,
    #[arg(long = "api-max-snippet-bytes")]
    pub api_max_snippet_bytes: Option<u64>,
    #[arg(long = "api-compile-timeout-ms")]
    pub api_compile_timeout_ms: Option<u64>,
    #[arg(long = "profile-max-report-bytes")]
    pub profile_max_report_bytes: Option<u64>,
    #[arg(long = "profile-max-runs")]
    pub profile_max_runs: Option<u64>,
    #[arg(long = "profile-compare-samples")]
    pub profile_compare_samples: Option<u64>,
    #[arg(long = "repair-max-candidates")]
    pub repair_max_candidates: Option<u64>,
    #[arg(long = "repair-max-compiles")]
    pub repair_max_compiles: Option<u64>,
    #[arg(long = "repair-wall-time-ms")]
    pub repair_wall_time_ms: Option<u64>,
    #[arg(long = "max-rename-edits")]
    pub max_rename_edits: Option<u64>,
    #[arg(long = "max-refactor-edits")]
    pub max_refactor_edits: Option<u64>,
    #[arg(long = "tool-output-bytes")]
    pub tool_output_bytes: Option<u64>,
    #[arg(long = "process-output-bytes")]
    pub process_output_bytes: Option<u64>,
    #[arg(long = "max-in-flight-tools")]
    pub max_in_flight_tools: Option<u64>,
    #[arg(long = "max-active-tasks")]
    pub max_active_tasks: Option<u64>,
    #[arg(long = "max-retained-tasks")]
    pub max_retained_tasks: Option<u64>,
    #[arg(long = "identity-files")]
    pub identity_files: Option<u64>,
    #[arg(long = "identity-file-bytes")]
    pub identity_file_bytes: Option<u64>,
    #[arg(long = "identity-total-bytes")]
    pub identity_total_bytes: Option<u64>,
    #[arg(long = "external-files")]
    pub external_files: Option<u64>,
    #[arg(long = "external-bytes")]
    pub external_bytes: Option<u64>,
    #[arg(long = "git-output-bytes")]
    pub git_output_bytes: Option<u64>,
    #[arg(long = "audit-files")]
    pub audit_files: Option<u64>,
    #[arg(long = "audit-file-bytes")]
    pub audit_file_bytes: Option<u64>,
    #[arg(long = "audit-total-bytes")]
    pub audit_total_bytes: Option<u64>,
    #[arg(long = "audit-findings")]
    pub audit_findings: Option<u64>,
    #[arg(long = "telemetry-enabled")]
    pub telemetry_enabled: Option<bool>,
    #[arg(long = "telemetry-path")]
    pub telemetry_path: Option<PathBuf>,
    #[arg(long = "telemetry-retention-bytes")]
    pub telemetry_retention_bytes: Option<u64>,
    #[arg(long = "telemetry-retention-days")]
    pub telemetry_retention_days: Option<u64>,
    #[arg(long = "telemetry-max-archives")]
    pub telemetry_max_archives: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    server: Option<FileServerConfig>,
    tools: Option<FileToolConfig>,
    cargo: Option<FileCargoConfig>,
    gate: Option<FileGateConfig>,
    change: Option<FileChangeConfig>,
    verify: Option<FileVerifyConfig>,
    work: Option<FileWorkConfig>,
    rust_analyzer: Option<FileRustAnalyzerConfig>,
    docs: Option<FileDocsConfig>,
    context: Option<FileContextConfig>,
    api: Option<FileApiConfig>,
    limits: Option<FileLimitsConfig>,
    profile: Option<FileProfileConfig>,
    repair: Option<FileRepairConfig>,
    telemetry: Option<FileTelemetryConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileServerConfig {
    allow_roots: Option<Vec<PathBuf>>,
    allow_dependency_roots: Option<Vec<PathBuf>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileToolConfig {
    check: Option<bool>,
    profile: Option<bool>,
    audit: Option<bool>,
    crate_lookup: Option<bool>,
    docs: Option<bool>,
    context: Option<bool>,
    api: Option<bool>,
    verify: Option<bool>,
    lsp: Option<bool>,
    explain: Option<bool>,
    rename: Option<bool>,
    refactor: Option<bool>,
    change: Option<bool>,
    repair: Option<bool>,
    work: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileCargoConfig {
    path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileGateConfig {
    hard_timeout_ms: Option<u64>,
    debounce_ms: Option<u64>,
    host_concurrency: Option<u64>,
    scope: Option<GateScopeFile>,
    cache: Option<GateCacheFile>,
    min_free_disk_mb: Option<u64>,
    min_available_memory_mb: Option<u64>,
    cache_dir: Option<PathBuf>,
    lease_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GateScopeFile {
    Workspace,
    Shadow,
    Affected,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileVerifyConfig {
    max_cells: Option<u64>,
    max_wall_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GateCacheFile {
    Auto,
    Project,
    Isolated,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileChangeConfig {
    scratch_dir: Option<PathBuf>,
    max_active: Option<u64>,
    max_files: Option<u64>,
    max_bytes: Option<u64>,
    ttl_ms: Option<u64>,
    max_revisions: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileWorkConfig {
    max_active: Option<u64>,
    max_compiles: Option<u64>,
    max_candidates: Option<u64>,
    max_handoffs: Option<u64>,
    wall_time_ms: Option<u64>,
    continuation_ttl_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileRustAnalyzerConfig {
    path: Option<PathBuf>,
    timeout_ms: Option<u64>,
    idle_ms: Option<u64>,
    max_instances: Option<u64>,
    check_hint: Option<bool>,
    workspace_code: Option<WorkspaceCodeFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WorkspaceCodeFile {
    Deny,
    Allow,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileDocsConfig {
    timeout_ms: Option<u64>,
    fallback: Option<DocsFallbackFile>,
    cache_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DocsFallbackFile {
    Auto,
    Local,
    Network,
    Off,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileContextConfig {
    max_capsules: Option<u64>,
    capsule_ttl_ms: Option<u64>,
    max_items: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileApiConfig {
    max_snippets: Option<u64>,
    max_snippet_bytes: Option<u64>,
    compile_timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileLimitsConfig {
    max_rename_edits: Option<u64>,
    max_refactor_edits: Option<u64>,
    process_output_bytes: Option<u64>,
    tool_output_bytes: Option<u64>,
    max_in_flight_tools: Option<u64>,
    max_active_tasks: Option<u64>,
    max_retained_tasks: Option<u64>,
    identity_files: Option<u64>,
    identity_file_bytes: Option<u64>,
    identity_total_bytes: Option<u64>,
    external_files: Option<u64>,
    external_bytes: Option<u64>,
    git_output_bytes: Option<u64>,
    audit_files: Option<u64>,
    audit_file_bytes: Option<u64>,
    audit_total_bytes: Option<u64>,
    audit_findings: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileProfileConfig {
    max_report_bytes: Option<u64>,
    max_runs: Option<u64>,
    compare_samples: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileRepairConfig {
    max_candidates: Option<u32>,
    max_compiles: Option<u32>,
    wall_time_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileTelemetryConfig {
    enabled: Option<bool>,
    path: Option<PathBuf>,
    retention_bytes: Option<u64>,
    retention_days: Option<u64>,
    max_archives: Option<u64>,
}

fn platform_cache_dir() -> PathBuf {
    if let Some(base) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(base).join("agz-rust-coder");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("agz-rust-coder");
    }
    env::temp_dir().join("agz-rust-coder")
}

#[allow(clippy::too_many_lines)]
fn apply_file(config: &mut Config, file: FileConfig) {
    if let Some(server) = file.server {
        if let Some(value) = server.allow_roots {
            config.server.allow_roots = value;
        }
        if let Some(value) = server.allow_dependency_roots {
            config.server.allow_dependency_roots = value;
        }
    }
    if let Some(tools) = file.tools {
        apply_opt(&mut config.tools.check, tools.check);
        apply_opt(&mut config.tools.profile, tools.profile);
        apply_opt(&mut config.tools.audit, tools.audit);
        apply_opt(&mut config.tools.crate_lookup, tools.crate_lookup);
        apply_opt(&mut config.tools.docs, tools.docs);
        apply_opt(&mut config.tools.context, tools.context);
        apply_opt(&mut config.tools.api, tools.api);
        apply_opt(&mut config.tools.verify, tools.verify);
        apply_opt(&mut config.tools.lsp, tools.lsp);
        apply_opt(&mut config.tools.explain, tools.explain);
        apply_opt(&mut config.tools.rename, tools.rename);
        apply_opt(&mut config.tools.refactor, tools.refactor);
        apply_opt(&mut config.tools.change, tools.change);
        apply_opt(&mut config.tools.repair, tools.repair);
        apply_opt(&mut config.tools.work, tools.work);
    }
    if let Some(cargo) = file.cargo {
        if let Some(path) = cargo.path {
            config.cargo.path = Some(path);
        }
    }
    if let Some(gate) = file.gate {
        apply_opt(&mut config.gate.hard_timeout_ms, gate.hard_timeout_ms);
        apply_opt(&mut config.gate.debounce_ms, gate.debounce_ms);
        apply_opt(&mut config.gate.host_concurrency, gate.host_concurrency);
        if let Some(value) = gate.scope {
            config.gate.scope = value.into();
        }
        if let Some(value) = gate.cache {
            config.gate.cache = value.into();
        }
        apply_opt(&mut config.gate.min_free_disk_mb, gate.min_free_disk_mb);
        apply_opt(
            &mut config.gate.min_available_memory_mb,
            gate.min_available_memory_mb,
        );
        apply_opt(&mut config.gate.cache_dir, gate.cache_dir);
        apply_opt(&mut config.gate.lease_dir, gate.lease_dir);
    }
    if let Some(change) = file.change {
        apply_opt(&mut config.change.scratch_dir, change.scratch_dir);
        apply_opt(&mut config.change.max_active, change.max_active);
        apply_opt(&mut config.change.max_files, change.max_files);
        apply_opt(&mut config.change.max_bytes, change.max_bytes);
        apply_opt(&mut config.change.ttl_ms, change.ttl_ms);
        apply_opt(&mut config.change.max_revisions, change.max_revisions);
    }
    if let Some(verify) = file.verify {
        apply_opt(&mut config.verify.max_cells, verify.max_cells);
        apply_opt(&mut config.verify.max_wall_ms, verify.max_wall_ms);
    }
    if let Some(work) = file.work {
        apply_opt(&mut config.work.max_active, work.max_active);
        apply_opt(&mut config.work.max_compiles, work.max_compiles);
        apply_opt(&mut config.work.max_candidates, work.max_candidates);
        apply_opt(&mut config.work.max_handoffs, work.max_handoffs);
        apply_opt(&mut config.work.wall_time_ms, work.wall_time_ms);
        apply_opt(
            &mut config.work.continuation_ttl_ms,
            work.continuation_ttl_ms,
        );
    }
    if let Some(ra) = file.rust_analyzer {
        if let Some(path) = ra.path {
            config.rust_analyzer.path = Some(path);
        }
        apply_opt(&mut config.rust_analyzer.timeout_ms, ra.timeout_ms);
        apply_opt(&mut config.rust_analyzer.idle_ms, ra.idle_ms);
        apply_opt(&mut config.rust_analyzer.max_instances, ra.max_instances);
        apply_opt(&mut config.rust_analyzer.check_hint, ra.check_hint);
        if let Some(value) = ra.workspace_code {
            config.rust_analyzer.workspace_code = value.into();
        }
    }
    if let Some(docs) = file.docs {
        apply_opt(&mut config.docs.timeout_ms, docs.timeout_ms);
        if let Some(value) = docs.fallback {
            config.docs.fallback = value.into();
        }
        apply_opt(&mut config.docs.cache_dir, docs.cache_dir);
    }
    if let Some(context) = file.context {
        apply_opt(&mut config.context.max_capsules, context.max_capsules);
        apply_opt(&mut config.context.capsule_ttl_ms, context.capsule_ttl_ms);
        apply_opt(&mut config.context.max_items, context.max_items);
    }
    if let Some(api) = file.api {
        apply_opt(&mut config.api.max_snippets, api.max_snippets);
        apply_opt(&mut config.api.max_snippet_bytes, api.max_snippet_bytes);
        apply_opt(&mut config.api.compile_timeout_ms, api.compile_timeout_ms);
    }
    if let Some(limits) = file.limits {
        apply_opt(&mut config.limits.max_rename_edits, limits.max_rename_edits);
        apply_opt(
            &mut config.limits.max_refactor_edits,
            limits.max_refactor_edits,
        );
        apply_opt(
            &mut config.limits.process_output_bytes,
            limits.process_output_bytes,
        );
        apply_opt(
            &mut config.limits.tool_output_bytes,
            limits.tool_output_bytes,
        );
        apply_opt(
            &mut config.limits.max_in_flight_tools,
            limits.max_in_flight_tools,
        );
        apply_opt(&mut config.limits.max_active_tasks, limits.max_active_tasks);
        apply_opt(
            &mut config.limits.max_retained_tasks,
            limits.max_retained_tasks,
        );
        apply_opt(&mut config.limits.identity_files, limits.identity_files);
        apply_opt(
            &mut config.limits.identity_file_bytes,
            limits.identity_file_bytes,
        );
        apply_opt(
            &mut config.limits.identity_total_bytes,
            limits.identity_total_bytes,
        );
        apply_opt(&mut config.limits.external_files, limits.external_files);
        apply_opt(&mut config.limits.external_bytes, limits.external_bytes);
        apply_opt(&mut config.limits.git_output_bytes, limits.git_output_bytes);
        apply_opt(&mut config.limits.audit_files, limits.audit_files);
        apply_opt(&mut config.limits.audit_file_bytes, limits.audit_file_bytes);
        apply_opt(
            &mut config.limits.audit_total_bytes,
            limits.audit_total_bytes,
        );
        apply_opt(&mut config.limits.audit_findings, limits.audit_findings);
    }
    if let Some(profile) = file.profile {
        apply_opt(
            &mut config.profile.max_report_bytes,
            profile.max_report_bytes,
        );
        apply_opt(&mut config.profile.max_runs, profile.max_runs);
        apply_opt(&mut config.profile.compare_samples, profile.compare_samples);
    }
    if let Some(repair) = file.repair {
        apply_opt(&mut config.repair.max_candidates, repair.max_candidates);
        apply_opt(&mut config.repair.max_compiles, repair.max_compiles);
        apply_opt(&mut config.repair.wall_time_ms, repair.wall_time_ms);
    }
    if let Some(telemetry) = file.telemetry {
        apply_opt(&mut config.telemetry.enabled, telemetry.enabled);
        apply_opt(&mut config.telemetry.path, telemetry.path);
        apply_opt(
            &mut config.telemetry.retention_bytes,
            telemetry.retention_bytes,
        );
        apply_opt(
            &mut config.telemetry.retention_days,
            telemetry.retention_days,
        );
        apply_opt(&mut config.telemetry.max_archives, telemetry.max_archives);
    }
}

#[allow(clippy::too_many_lines)]
fn apply_environment(config: &mut Config, key: &str, value: &str) -> Result<(), ConfigError> {
    let field = key.strip_prefix(ENV_PREFIX).unwrap_or(key);
    let invalid = |message: String| ConfigError::InvalidEnvironment {
        name: key.to_owned(),
        message,
    };
    match field {
        "SERVER__ALLOW_ROOTS" => {
            config.server.allow_roots = parse_path_list(value).map_err(invalid)?;
        }
        "SERVER__ALLOW_DEPENDENCY_ROOTS" => {
            config.server.allow_dependency_roots = parse_path_list(value).map_err(invalid)?;
        }
        "TOOLS__CHECK" => config.tools.check = parse_bool(value).map_err(invalid)?,
        "TOOLS__PROFILE" => config.tools.profile = parse_bool(value).map_err(invalid)?,
        "TOOLS__AUDIT" => config.tools.audit = parse_bool(value).map_err(invalid)?,
        "TOOLS__CRATE_LOOKUP" => config.tools.crate_lookup = parse_bool(value).map_err(invalid)?,
        "TOOLS__DOCS" => config.tools.docs = parse_bool(value).map_err(invalid)?,
        "TOOLS__CONTEXT" => config.tools.context = parse_bool(value).map_err(invalid)?,
        "TOOLS__API" => config.tools.api = parse_bool(value).map_err(invalid)?,
        "TOOLS__VERIFY" => config.tools.verify = parse_bool(value).map_err(invalid)?,
        "TOOLS__LSP" => config.tools.lsp = parse_bool(value).map_err(invalid)?,
        "TOOLS__EXPLAIN" => config.tools.explain = parse_bool(value).map_err(invalid)?,
        "TOOLS__RENAME" => config.tools.rename = parse_bool(value).map_err(invalid)?,
        "TOOLS__REFACTOR" => config.tools.refactor = parse_bool(value).map_err(invalid)?,
        "TOOLS__CHANGE" => config.tools.change = parse_bool(value).map_err(invalid)?,
        "TOOLS__REPAIR" => config.tools.repair = parse_bool(value).map_err(invalid)?,
        "TOOLS__WORK" => config.tools.work = parse_bool(value).map_err(invalid)?,
        "CARGO__PATH" => config.cargo.path = Some(nonempty_path(value).map_err(invalid)?),
        "GATE__HARD_TIMEOUT_MS" => {
            config.gate.hard_timeout_ms = parse_u64(value).map_err(invalid)?;
        }
        "GATE__DEBOUNCE_MS" => config.gate.debounce_ms = parse_u64(value).map_err(invalid)?,
        "GATE__HOST_CONCURRENCY" => {
            config.gate.host_concurrency = parse_u64(value).map_err(invalid)?;
        }
        "GATE__SCOPE" => config.gate.scope = parse_scope(value).map_err(invalid)?,
        "GATE__CACHE" => config.gate.cache = parse_cache(value).map_err(invalid)?,
        "GATE__MIN_FREE_DISK_MB" => {
            config.gate.min_free_disk_mb = parse_u64(value).map_err(invalid)?;
        }
        "GATE__MIN_AVAILABLE_MEMORY_MB" => {
            config.gate.min_available_memory_mb = parse_u64(value).map_err(invalid)?;
        }
        "GATE__CACHE_DIR" => config.gate.cache_dir = nonempty_path(value).map_err(invalid)?,
        "GATE__LEASE_DIR" => config.gate.lease_dir = nonempty_path(value).map_err(invalid)?,
        "CHANGE__SCRATCH_DIR" => {
            config.change.scratch_dir = nonempty_path(value).map_err(invalid)?;
        }
        "CHANGE__MAX_ACTIVE" => {
            config.change.max_active = parse_u64(value).map_err(invalid)?;
        }
        "CHANGE__MAX_FILES" => {
            config.change.max_files = parse_u64(value).map_err(invalid)?;
        }
        "CHANGE__MAX_BYTES" => {
            config.change.max_bytes = parse_u64(value).map_err(invalid)?;
        }
        "CHANGE__TTL_MS" => {
            config.change.ttl_ms = parse_u64(value).map_err(invalid)?;
        }
        "CHANGE__MAX_REVISIONS" => {
            config.change.max_revisions = parse_u64(value).map_err(invalid)?;
        }
        "VERIFY__MAX_CELLS" => config.verify.max_cells = parse_u64(value).map_err(invalid)?,
        "VERIFY__MAX_WALL_MS" => config.verify.max_wall_ms = parse_u64(value).map_err(invalid)?,
        "WORK__MAX_ACTIVE" => {
            config.work.max_active = parse_u64(value).map_err(invalid)?;
        }
        "WORK__MAX_COMPILES" => {
            config.work.max_compiles = parse_u64(value).map_err(invalid)?;
        }
        "WORK__MAX_CANDIDATES" => {
            config.work.max_candidates = parse_u64(value).map_err(invalid)?;
        }
        "WORK__MAX_HANDOFFS" => {
            config.work.max_handoffs = parse_u64(value).map_err(invalid)?;
        }
        "WORK__WALL_TIME_MS" => {
            config.work.wall_time_ms = parse_u64(value).map_err(invalid)?;
        }
        "WORK__CONTINUATION_TTL_MS" => {
            config.work.continuation_ttl_ms = parse_u64(value).map_err(invalid)?;
        }
        "RUST_ANALYZER__PATH" => {
            config.rust_analyzer.path = Some(nonempty_path(value).map_err(invalid)?);
        }
        "RUST_ANALYZER__TIMEOUT_MS" => {
            config.rust_analyzer.timeout_ms = parse_u64(value).map_err(invalid)?;
        }
        "RUST_ANALYZER__IDLE_MS" => {
            config.rust_analyzer.idle_ms = parse_u64(value).map_err(invalid)?;
        }
        "RUST_ANALYZER__MAX_INSTANCES" => {
            config.rust_analyzer.max_instances = parse_u64(value).map_err(invalid)?;
        }
        "RUST_ANALYZER__CHECK_HINT" => {
            config.rust_analyzer.check_hint = parse_bool(value).map_err(invalid)?;
        }
        "RUST_ANALYZER__WORKSPACE_CODE" => {
            config.rust_analyzer.workspace_code = parse_workspace_code(value).map_err(invalid)?;
        }
        "DOCS__TIMEOUT_MS" => config.docs.timeout_ms = parse_u64(value).map_err(invalid)?,
        "DOCS__FALLBACK" => config.docs.fallback = parse_fallback(value).map_err(invalid)?,
        "DOCS__CACHE_DIR" => config.docs.cache_dir = nonempty_path(value).map_err(invalid)?,
        "CONTEXT__MAX_CAPSULES" => {
            config.context.max_capsules = parse_u64(value).map_err(invalid)?;
        }
        "CONTEXT__CAPSULE_TTL_MS" => {
            config.context.capsule_ttl_ms = parse_u64(value).map_err(invalid)?;
        }
        "CONTEXT__MAX_ITEMS" => {
            config.context.max_items = parse_u64(value).map_err(invalid)?;
        }
        "API__MAX_SNIPPETS" => {
            config.api.max_snippets = parse_u64(value).map_err(invalid)?;
        }
        "API__MAX_SNIPPET_BYTES" => {
            config.api.max_snippet_bytes = parse_u64(value).map_err(invalid)?;
        }
        "API__COMPILE_TIMEOUT_MS" => {
            config.api.compile_timeout_ms = parse_u64(value).map_err(invalid)?;
        }
        "PROFILE__MAX_REPORT_BYTES" => {
            config.profile.max_report_bytes = parse_u64(value).map_err(invalid)?;
        }
        "PROFILE__MAX_RUNS" => config.profile.max_runs = parse_u64(value).map_err(invalid)?,
        "PROFILE__COMPARE_SAMPLES" => {
            config.profile.compare_samples = parse_u64(value).map_err(invalid)?;
        }
        "REPAIR__MAX_CANDIDATES" => {
            config.repair.max_candidates = parse_u32(value).map_err(invalid)?;
        }
        "REPAIR__MAX_COMPILES" => {
            config.repair.max_compiles = parse_u32(value).map_err(invalid)?;
        }
        "REPAIR__WALL_TIME_MS" => {
            config.repair.wall_time_ms = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__MAX_RENAME_EDITS" => {
            config.limits.max_rename_edits = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__MAX_REFACTOR_EDITS" => {
            config.limits.max_refactor_edits = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__PROCESS_OUTPUT_BYTES" => {
            config.limits.process_output_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__TOOL_OUTPUT_BYTES" => {
            config.limits.tool_output_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__MAX_IN_FLIGHT_TOOLS" => {
            config.limits.max_in_flight_tools = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__MAX_ACTIVE_TASKS" => {
            config.limits.max_active_tasks = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__MAX_RETAINED_TASKS" => {
            config.limits.max_retained_tasks = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__IDENTITY_FILES" => {
            config.limits.identity_files = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__IDENTITY_FILE_BYTES" => {
            config.limits.identity_file_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__IDENTITY_TOTAL_BYTES" => {
            config.limits.identity_total_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__EXTERNAL_FILES" => {
            config.limits.external_files = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__EXTERNAL_BYTES" => {
            config.limits.external_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__GIT_OUTPUT_BYTES" => {
            config.limits.git_output_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__AUDIT_FILES" => config.limits.audit_files = parse_u64(value).map_err(invalid)?,
        "LIMITS__AUDIT_FILE_BYTES" => {
            config.limits.audit_file_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__AUDIT_TOTAL_BYTES" => {
            config.limits.audit_total_bytes = parse_u64(value).map_err(invalid)?;
        }
        "LIMITS__AUDIT_FINDINGS" => {
            config.limits.audit_findings = parse_u64(value).map_err(invalid)?;
        }
        "TELEMETRY__ENABLED" => config.telemetry.enabled = parse_bool(value).map_err(invalid)?,
        "TELEMETRY__PATH" => config.telemetry.path = nonempty_path(value).map_err(invalid)?,
        "TELEMETRY__RETENTION_BYTES" => {
            config.telemetry.retention_bytes = parse_u64(value).map_err(invalid)?;
        }
        "TELEMETRY__RETENTION_DAYS" => {
            config.telemetry.retention_days = parse_u64(value).map_err(invalid)?;
        }
        "TELEMETRY__MAX_ARCHIVES" => {
            config.telemetry.max_archives = parse_u64(value).map_err(invalid)?;
        }
        _ => return Err(ConfigError::UnknownEnvironment(key.to_owned())),
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn apply_cli(config: &mut Config, cli: &CliOptions) -> Result<(), ConfigError> {
    let invalid =
        |field: &'static str, message: String| ConfigError::InvalidField { field, message };
    if let Some(value) = &cli.allow_root {
        value.clone_into(&mut config.server.allow_roots);
    }
    if let Some(value) = &cli.allow_dependency_root {
        value.clone_into(&mut config.server.allow_dependency_roots);
    }
    apply_opt(&mut config.tools.check, cli.tools_check);
    apply_opt(&mut config.tools.profile, cli.tools_profile);
    apply_opt(&mut config.tools.audit, cli.tools_audit);
    apply_opt(&mut config.tools.crate_lookup, cli.tools_crate_lookup);
    apply_opt(&mut config.tools.docs, cli.tools_docs);
    apply_opt(&mut config.tools.context, cli.tools_context);
    apply_opt(&mut config.tools.api, cli.tools_api);
    apply_opt(&mut config.tools.verify, cli.tools_verify);
    apply_opt(&mut config.tools.lsp, cli.tools_lsp);
    apply_opt(&mut config.tools.explain, cli.tools_explain);
    apply_opt(&mut config.tools.rename, cli.tools_rename);
    apply_opt(&mut config.tools.refactor, cli.tools_refactor);
    apply_opt(&mut config.tools.change, cli.tools_change);
    apply_opt(&mut config.tools.repair, cli.tools_repair);
    apply_opt(&mut config.tools.work, cli.tools_work);
    if let Some(path) = cli.cargo_path.clone() {
        config.cargo.path = Some(path);
    }
    apply_opt(&mut config.gate.hard_timeout_ms, cli.gate_hard_timeout_ms);
    apply_opt(&mut config.gate.debounce_ms, cli.gate_debounce_ms);
    apply_opt(&mut config.gate.host_concurrency, cli.gate_host_concurrency);
    if let Some(value) = cli.gate_scope.as_deref() {
        config.gate.scope = parse_scope(value).map_err(|message| invalid("gate.scope", message))?;
    }
    if let Some(value) = cli.gate_cache.as_deref() {
        config.gate.cache = parse_cache(value).map_err(|message| invalid("gate.cache", message))?;
    }
    apply_opt(&mut config.gate.min_free_disk_mb, cli.gate_min_free_disk_mb);
    apply_opt(
        &mut config.gate.min_available_memory_mb,
        cli.gate_min_available_memory_mb,
    );
    apply_opt(&mut config.gate.cache_dir, cli.gate_cache_dir.clone());
    apply_opt(&mut config.gate.lease_dir, cli.gate_lease_dir.clone());
    apply_opt(
        &mut config.change.scratch_dir,
        cli.change_scratch_dir.clone(),
    );
    apply_opt(&mut config.change.max_active, cli.change_max_active);
    apply_opt(&mut config.change.max_files, cli.change_max_files);
    apply_opt(&mut config.change.max_bytes, cli.change_max_bytes);
    apply_opt(&mut config.change.ttl_ms, cli.change_ttl_ms);
    apply_opt(&mut config.change.max_revisions, cli.change_max_revisions);
    apply_opt(&mut config.verify.max_cells, cli.verify_max_cells);
    apply_opt(&mut config.verify.max_wall_ms, cli.verify_max_wall_ms);
    apply_opt(&mut config.work.max_active, cli.work_max_active);
    apply_opt(&mut config.work.max_compiles, cli.work_max_compiles);
    apply_opt(&mut config.work.max_candidates, cli.work_max_candidates);
    apply_opt(&mut config.work.max_handoffs, cli.work_max_handoffs);
    apply_opt(&mut config.work.wall_time_ms, cli.work_wall_time_ms);
    apply_opt(
        &mut config.work.continuation_ttl_ms,
        cli.work_continuation_ttl_ms,
    );
    if let Some(path) = cli.rust_analyzer_path.clone() {
        config.rust_analyzer.path = Some(path);
    }
    apply_opt(
        &mut config.rust_analyzer.timeout_ms,
        cli.rust_analyzer_timeout_ms,
    );
    apply_opt(&mut config.rust_analyzer.idle_ms, cli.rust_analyzer_idle_ms);
    apply_opt(
        &mut config.rust_analyzer.max_instances,
        cli.rust_analyzer_max_instances,
    );
    apply_opt(
        &mut config.rust_analyzer.check_hint,
        cli.rust_analyzer_check_hint,
    );
    if let Some(value) = cli.rust_analyzer_workspace_code.as_deref() {
        config.rust_analyzer.workspace_code = parse_workspace_code(value)
            .map_err(|message| invalid("rust_analyzer.workspace_code", message))?;
    }
    apply_opt(&mut config.docs.timeout_ms, cli.docs_timeout_ms);
    if let Some(value) = cli.docs_fallback.as_deref() {
        config.docs.fallback =
            parse_fallback(value).map_err(|message| invalid("docs.fallback", message))?;
    }
    apply_opt(&mut config.docs.cache_dir, cli.docs_cache_dir.clone());
    apply_opt(&mut config.context.max_capsules, cli.context_max_capsules);
    apply_opt(
        &mut config.context.capsule_ttl_ms,
        cli.context_capsule_ttl_ms,
    );
    apply_opt(&mut config.context.max_items, cli.context_max_items);
    apply_opt(&mut config.api.max_snippets, cli.api_max_snippets);
    apply_opt(&mut config.api.max_snippet_bytes, cli.api_max_snippet_bytes);
    apply_opt(
        &mut config.api.compile_timeout_ms,
        cli.api_compile_timeout_ms,
    );
    apply_opt(
        &mut config.profile.max_report_bytes,
        cli.profile_max_report_bytes,
    );
    apply_opt(&mut config.profile.max_runs, cli.profile_max_runs);
    apply_opt(
        &mut config.profile.compare_samples,
        cli.profile_compare_samples,
    );
    if let Some(value) = cli.repair_max_candidates {
        config.repair.max_candidates = u32::try_from(value)
            .map_err(|_| invalid("repair.max_candidates", "value exceeds u32".to_owned()))?;
    }
    if let Some(value) = cli.repair_max_compiles {
        config.repair.max_compiles = u32::try_from(value)
            .map_err(|_| invalid("repair.max_compiles", "value exceeds u32".to_owned()))?;
    }
    apply_opt(&mut config.repair.wall_time_ms, cli.repair_wall_time_ms);
    apply_opt(&mut config.limits.max_rename_edits, cli.max_rename_edits);
    apply_opt(
        &mut config.limits.max_refactor_edits,
        cli.max_refactor_edits,
    );
    apply_opt(
        &mut config.limits.process_output_bytes,
        cli.process_output_bytes,
    );
    apply_opt(&mut config.limits.tool_output_bytes, cli.tool_output_bytes);
    apply_opt(
        &mut config.limits.max_in_flight_tools,
        cli.max_in_flight_tools,
    );
    apply_opt(&mut config.limits.max_active_tasks, cli.max_active_tasks);
    apply_opt(
        &mut config.limits.max_retained_tasks,
        cli.max_retained_tasks,
    );
    apply_opt(&mut config.limits.identity_files, cli.identity_files);
    apply_opt(
        &mut config.limits.identity_file_bytes,
        cli.identity_file_bytes,
    );
    apply_opt(
        &mut config.limits.identity_total_bytes,
        cli.identity_total_bytes,
    );
    apply_opt(&mut config.limits.external_files, cli.external_files);
    apply_opt(&mut config.limits.external_bytes, cli.external_bytes);
    apply_opt(&mut config.limits.git_output_bytes, cli.git_output_bytes);
    apply_opt(&mut config.limits.audit_files, cli.audit_files);
    apply_opt(&mut config.limits.audit_file_bytes, cli.audit_file_bytes);
    apply_opt(&mut config.limits.audit_total_bytes, cli.audit_total_bytes);
    apply_opt(&mut config.limits.audit_findings, cli.audit_findings);
    apply_opt(&mut config.telemetry.enabled, cli.telemetry_enabled);
    apply_opt(&mut config.telemetry.path, cli.telemetry_path.clone());
    apply_opt(
        &mut config.telemetry.retention_bytes,
        cli.telemetry_retention_bytes,
    );
    apply_opt(
        &mut config.telemetry.retention_days,
        cli.telemetry_retention_days,
    );
    apply_opt(
        &mut config.telemetry.max_archives,
        cli.telemetry_max_archives,
    );
    Ok(())
}

fn apply_opt<T>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

fn check_range(field: &'static str, value: u64, min: u64, max: u64) -> Result<(), ConfigError> {
    if value < min || value > max {
        return Err(ConfigError::InvalidField {
            field,
            message: format!("{value} is outside [{min}, {max}]"),
        });
    }
    Ok(())
}

fn reject_path_overlap(
    field: &'static str,
    path: &Path,
    workspace_roots: &[PathBuf],
    dependency_roots: &[PathBuf],
) -> Result<(), ConfigError> {
    let candidate = normalize_config_path(path)
        .map_err(|message| ConfigError::InvalidField { field, message })?;
    for root in workspace_roots.iter().chain(dependency_roots.iter()) {
        let root = normalize_config_path(root)
            .map_err(|message| ConfigError::InvalidField { field, message })?;
        if paths_overlap(&candidate, &root) {
            return Err(ConfigError::InvalidField {
                field,
                message: format!(
                    "path {} overlaps configured workspace/dependency root {} in either direction",
                    candidate.display(),
                    root.display()
                ),
            });
        }
    }
    Ok(())
}

fn normalize_config_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = lexical_absolute(path)?;
    match fs::canonicalize(&absolute) {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            canonicalize_existing_ancestor(&absolute)
        }
        Err(error) => Err(format!(
            "could not resolve configured path {}: {error}",
            absolute.display()
        )),
    }
}

fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or_else(|| {
                    format!(
                        "could not resolve any existing ancestor of configured path {}",
                        path.display()
                    )
                })?;
                suffix.push(name.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    format!(
                        "configured path has no resolvable parent: {}",
                        path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "could not resolve configured path ancestor {}: {error}",
                    ancestor.display()
                ));
            }
        }
    }
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()
            .map_err(|error| format!("could not read current directory: {error}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "configured path escapes its absolute root: {}",
                        path.display()
                    ));
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    if !normalized.is_absolute() {
        return Err(format!(
            "configured path could not be made absolute: {}",
            path.display()
        ));
    }
    Ok(normalized)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    path_is_within(left, right) || path_is_within(right, left)
}

fn path_is_within(root: &Path, candidate: &Path) -> bool {
    candidate == root
        || candidate
            .strip_prefix(root)
            .is_ok_and(|relative| !relative.is_absolute())
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err("expected true or false".to_owned()),
    }
}

fn parse_u64(value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| "expected an unsigned integer".to_owned())
}

fn parse_u32(value: &str) -> Result<u32, String> {
    u32::try_from(parse_u64(value)?).map_err(|_| "expected an unsigned 32-bit integer".to_owned())
}

fn parse_path_list(value: &str) -> Result<Vec<PathBuf>, String> {
    if value.trim().is_empty() {
        return Err("path list cannot be empty".to_owned());
    }
    let paths: Vec<_> = env::split_paths(OsStr::new(value)).collect();
    if paths.is_empty() || paths.iter().any(|path| path.as_os_str().is_empty()) {
        return Err("path list contains no usable paths".to_owned());
    }
    Ok(paths)
}

fn nonempty_path(value: &str) -> Result<PathBuf, String> {
    let value = value.trim();
    if value.is_empty() {
        Err("path cannot be empty".to_owned())
    } else {
        Ok(PathBuf::from(value))
    }
}

fn process_environment() -> Result<Vec<(String, String)>, ConfigError> {
    environment_from_os(env::vars_os())
}

fn environment_from_os<I, K, V>(values: I) -> Result<Vec<(String, String)>, ConfigError>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let mut environment = Vec::new();
    for (key, value) in values {
        let key = key.into();
        if !key
            .as_os_str()
            .as_encoded_bytes()
            .starts_with(ENV_PREFIX.as_bytes())
        {
            continue;
        }
        let name = key
            .into_string()
            .map_err(|key| ConfigError::InvalidEnvironment {
                name: String::from_utf8_lossy(key.as_os_str().as_encoded_bytes())
                    .chars()
                    .take(128)
                    .collect(),
                message: "name is not valid Unicode".to_owned(),
            })?;
        let value = value
            .into()
            .into_string()
            .map_err(|_| ConfigError::InvalidEnvironment {
                name: name.clone(),
                message: "value is not valid Unicode".to_owned(),
            })?;
        environment.push((name, value));
    }
    Ok(environment)
}

fn parse_scope(value: &str) -> Result<GateScope, String> {
    match value.trim() {
        "workspace" => Ok(GateScope::Workspace),
        "shadow" => Ok(GateScope::Shadow),
        "affected" => Ok(GateScope::Affected),
        _ => Err("expected workspace, shadow, or affected".to_owned()),
    }
}

fn parse_cache(value: &str) -> Result<GateCache, String> {
    match value.trim() {
        "auto" => Ok(GateCache::Auto),
        "project" => Ok(GateCache::Project),
        "isolated" => Ok(GateCache::Isolated),
        _ => Err("expected auto, project, or isolated".to_owned()),
    }
}

fn parse_workspace_code(value: &str) -> Result<WorkspaceCode, String> {
    match value.trim() {
        "deny" => Ok(WorkspaceCode::Deny),
        "allow" => Ok(WorkspaceCode::Allow),
        _ => Err("expected deny or allow".to_owned()),
    }
}

fn parse_fallback(value: &str) -> Result<DocsFallback, String> {
    match value.trim() {
        "auto" => Ok(DocsFallback::Auto),
        "local" => Ok(DocsFallback::Local),
        "network" => Ok(DocsFallback::Network),
        "off" => Ok(DocsFallback::Off),
        _ => Err("expected auto, local, network, or off".to_owned()),
    }
}

impl From<GateScopeFile> for GateScope {
    fn from(value: GateScopeFile) -> Self {
        match value {
            GateScopeFile::Workspace => Self::Workspace,
            GateScopeFile::Shadow => Self::Shadow,
            GateScopeFile::Affected => Self::Affected,
        }
    }
}

impl From<GateCacheFile> for GateCache {
    fn from(value: GateCacheFile) -> Self {
        match value {
            GateCacheFile::Auto => Self::Auto,
            GateCacheFile::Project => Self::Project,
            GateCacheFile::Isolated => Self::Isolated,
        }
    }
}

impl From<WorkspaceCodeFile> for WorkspaceCode {
    fn from(value: WorkspaceCodeFile) -> Self {
        match value {
            WorkspaceCodeFile::Deny => Self::Deny,
            WorkspaceCodeFile::Allow => Self::Allow,
        }
    }
}

impl From<DocsFallbackFile> for DocsFallback {
    fn from(value: DocsFallbackFile) -> Self {
        match value {
            DocsFallbackFile::Auto => Self::Auto,
            DocsFallbackFile::Local => Self::Local,
            DocsFallbackFile::Network => Self::Network,
            DocsFallbackFile::Off => Self::Off,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn cli() -> CliOptions {
        CliOptions::default()
    }

    fn test_path(suffix: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        fs::canonicalize(env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-coder-config-{stamp}-{}-{suffix}",
                std::process::id()
            ))
    }

    fn config_for_path_tests(root: PathBuf, safe_base: &Path) -> Config {
        let mut config = Config::defaults_at(test_path("defaults"));
        config.server.allow_roots = vec![root];
        config.server.allow_dependency_roots = Vec::new();
        config.gate.cache = GateCache::Isolated;
        config.gate.cache_dir = safe_base.join("gate-cache");
        config.gate.lease_dir = safe_base.join("leases");
        config.docs.cache_dir = safe_base.join("docs-cache");
        config.telemetry.path = safe_base.join("activity.jsonl");
        config
    }

    fn assert_invalid_field(result: Result<(), ConfigError>, expected: &'static str) {
        assert!(matches!(
            result,
            Err(ConfigError::InvalidField { field, .. }) if field == expected
        ));
    }

    #[test]
    fn precedence_is_cli_over_environment_over_toml_over_defaults() {
        let mut cli = cli();
        cli.gate_hard_timeout_ms = Some(7_000);
        let config = Config::from_sources(
            "/workspace",
            Some("[gate]\nhard_timeout_ms = 2_000\n"),
            [("AGZ_RUST_CODER_GATE__HARD_TIMEOUT_MS", "3000")],
            &cli,
        )
        .unwrap();
        assert_eq!(config.gate.hard_timeout_ms, 7_000);
    }

    #[test]
    fn profile_config_accepts_toml_environment_and_cli_layers() {
        let mut cli = cli();
        cli.profile_max_runs = Some(3);
        let config = Config::from_sources(
            "/workspace",
            Some(
                "[tools]\nprofile = false\n[profile]\nmax_report_bytes = 8192\ncompare_samples = 2\n",
            ),
            [("AGZ_RUST_CODER_PROFILE__MAX_RUNS", "5")],
            &cli,
        )
        .unwrap();
        assert!(!config.tools.profile);
        assert!(!config.enabled_tool_names().contains(&"profile"));
        assert_eq!(config.profile.max_report_bytes, 8_192);
        assert_eq!(config.profile.max_runs, 3);
        assert_eq!(config.profile.compare_samples, 2);

        let out_of_range = Config::from_sources(
            "/workspace",
            Some("[profile]\nmax_report_bytes = 1\n"),
            std::iter::empty::<(String, String)>(),
            &cli,
        );
        assert!(matches!(
            out_of_range,
            Err(ConfigError::InvalidField { .. })
        ));
    }

    #[test]
    fn repair_config_accepts_toml_environment_and_cli_layers() {
        let plain_cli = cli();
        let mut cli = cli();
        cli.repair_max_candidates = Some(1);
        let config = Config::from_sources(
            "/workspace",
            Some("[tools]\nrepair = false\n[repair]\nmax_compiles = 2\nwall_time_ms = 5_000\n"),
            [("AGZ_RUST_CODER_REPAIR__MAX_CANDIDATES", "3")],
            &cli,
        )
        .unwrap();
        assert!(!config.tools.repair);
        assert!(!config.enabled_tool_names().contains(&"repair"));
        assert_eq!(config.repair.max_candidates, 1);
        assert_eq!(config.repair.max_compiles, 2);
        assert_eq!(config.repair.wall_time_ms, 5_000);

        let defaults = Config::defaults_at("/workspace");
        assert!(defaults.tools.repair);
        let names = defaults.enabled_tool_names();
        let change_index = names
            .iter()
            .position(|name| *name == "change")
            .expect("change tool");
        assert_eq!(names.get(change_index + 1), Some(&"repair"));

        let mut change_disabled = defaults;
        change_disabled.tools.change = false;
        assert!(!change_disabled.enabled_tool_names().contains(&"repair"));

        let out_of_range = Config::from_sources(
            "/workspace",
            Some("[repair]\nmax_candidates = 33\n"),
            std::iter::empty::<(String, String)>(),
            &plain_cli,
        );
        assert!(matches!(
            out_of_range,
            Err(ConfigError::InvalidField { .. })
        ));

        let environment_out_of_range = Config::from_sources(
            "/workspace",
            None,
            [("AGZ_RUST_CODER_REPAIR__MAX_COMPILES", "0")],
            &plain_cli,
        );
        assert!(matches!(
            environment_out_of_range,
            Err(ConfigError::InvalidField { .. })
        ));
    }

    #[test]
    fn work_config_accepts_toml_environment_and_cli_layers() {
        let mut cli = cli();
        cli.work_wall_time_ms = Some(5_000);
        let config = Config::from_sources(
            "/workspace",
            Some("[tools]\nwork = false\n[work]\nmax_handoffs = 1\n"),
            [("AGZ_RUST_CODER_WORK__MAX_CANDIDATES", "2")],
            &cli,
        )
        .unwrap();
        assert!(!config.tools.work);
        assert!(!config.enabled_tool_names().contains(&"work"));
        assert_eq!(config.work.max_handoffs, 1);
        assert_eq!(config.work.max_candidates, 2);
        assert_eq!(config.work.wall_time_ms, 5_000);

        let mut enabled = cli.clone();
        enabled.tools_work = Some(true);
        let config = Config::from_sources(
            "/workspace",
            Some("[tools]\nwork = false\n"),
            std::iter::empty::<(String, String)>(),
            &enabled,
        )
        .unwrap();
        assert!(config.tools.work);
        assert!(config.enabled_tool_names().contains(&"work"));

        let out_of_range = Config::from_sources(
            "/workspace",
            Some("[work]\ncontinuation_ttl_ms = 1\n"),
            std::iter::empty::<(String, String)>(),
            &cli,
        );
        assert_invalid_field(out_of_range.map(|_| ()), "work.continuation_ttl_ms");
    }

    #[test]
    fn list_layers_replace_instead_of_append() {
        let cli = cli();
        let toml_root = test_path("toml-root");
        let env_a = test_path("env-a");
        let env_b = test_path("env-b");
        let toml = format!(
            "[server]\nallow_roots = [{}]\n",
            toml::Value::String(toml_root.display().to_string())
        );
        let environment = env::join_paths([&env_a, &env_b])
            .expect("join environment paths")
            .into_string()
            .expect("UTF-8 environment paths");
        let config = Config::from_sources(
            test_path("default"),
            Some(&toml),
            [("AGZ_RUST_CODER_SERVER__ALLOW_ROOTS", environment)],
            &cli,
        )
        .unwrap();
        assert_eq!(config.server.allow_roots, vec![env_a, env_b]);
    }

    #[test]
    fn verify_configuration_uses_the_declared_precedence() {
        let mut cli = cli();
        cli.verify_max_cells = Some(3);
        cli.tools_verify = Some(false);
        let config = Config::from_sources(
            "/workspace",
            Some("[verify]\nmax_cells = 2\nmax_wall_ms = 5000\n"),
            [
                ("AGZ_RUST_CODER_VERIFY__MAX_CELLS", "4"),
                ("AGZ_RUST_CODER_TOOLS__VERIFY", "true"),
            ],
            &cli,
        )
        .expect("verify configuration is valid");
        assert_eq!(config.verify.max_cells, 3);
        assert_eq!(config.verify.max_wall_ms, 5_000);
        assert!(!config.tools.verify);
        assert!(!config.enabled_tool_names().contains(&"verify"));
    }

    #[test]
    fn verify_configuration_ranges_fail_closed() {
        for toml in [
            "[verify]\nmax_cells = 0\n",
            "[verify]\nmax_cells = 65\n",
            "[verify]\nmax_wall_ms = 999\n",
            "[verify]\nmax_wall_ms = 3600001\n",
        ] {
            let config = Config::from_sources(
                "/workspace",
                Some(toml),
                std::iter::empty::<(String, String)>(),
                &cli(),
            );
            assert!(
                matches!(config, Err(ConfigError::InvalidField { .. })),
                "expected rejection for {toml}"
            );
        }
    }

    #[test]
    fn unknown_toml_field_wrong_type_and_out_of_range_are_rejected() {
        let cli = cli();
        let unknown = Config::from_sources(
            "/workspace",
            Some("[tools]\nwat = true\n"),
            std::iter::empty::<(String, String)>(),
            &cli,
        );
        assert!(matches!(unknown, Err(ConfigError::Toml(_))));

        let wrong_type = Config::from_sources(
            "/workspace",
            Some("[gate]\nhard_timeout_ms = \"slow\"\n"),
            std::iter::empty::<(String, String)>(),
            &cli,
        );
        assert!(matches!(wrong_type, Err(ConfigError::Toml(_))));

        let out_of_range = Config::from_sources(
            "/workspace",
            Some("[limits]\ntool_output_bytes = 50000\n"),
            std::iter::empty::<(String, String)>(),
            &cli,
        );
        assert!(matches!(
            out_of_range,
            Err(ConfigError::InvalidField { .. })
        ));
    }

    #[test]
    fn malformed_environment_is_not_silently_ignored() {
        let result = Config::from_sources(
            "/workspace",
            None,
            [("AGZ_RUST_CODER_GATE__HOST_CONCURRENCY", "many")],
            &cli(),
        );
        assert!(matches!(
            result,
            Err(ConfigError::InvalidEnvironment { .. })
        ));
    }

    #[test]
    fn invalid_cli_enum_and_impossible_wire_limit_are_rejected() {
        let mut invalid_enum = cli();
        invalid_enum.gate_scope = Some("wide".to_owned());
        let enum_result = Config::from_sources(
            "/workspace",
            None,
            std::iter::empty::<(String, String)>(),
            &invalid_enum,
        );
        assert!(matches!(enum_result, Err(ConfigError::InvalidField { .. })));

        let mut invalid_limit = cli();
        invalid_limit.tool_output_bytes = Some(511);
        let limit_result = Config::from_sources(
            "/workspace",
            None,
            std::iter::empty::<(String, String)>(),
            &invalid_limit,
        );
        assert!(matches!(
            limit_result,
            Err(ConfigError::InvalidField { .. })
        ));
    }

    #[test]
    fn reserved_paths_reject_both_directions_for_workspace_and_dependency_roots() {
        let root = test_path("containment-root");
        let safe_base = test_path("containment-safe");
        let mut config = config_for_path_tests(root.clone(), &safe_base);
        config.gate.cache_dir = root.join("nested").join("..").join("gate-cache");
        assert_invalid_field(config.validate(), "gate.cache_dir");

        let dependency = test_path("containment-dependency");
        let mut config = config_for_path_tests(root.clone(), &safe_base);
        config.server.allow_dependency_roots = vec![dependency.clone()];
        config.gate.lease_dir = dependency.join("leases");
        assert_invalid_field(config.validate(), "gate.lease_dir");

        let mut config = config_for_path_tests(root.clone(), &safe_base);
        config.server.allow_dependency_roots = vec![dependency.clone()];
        config.docs.cache_dir = dependency.join("docs-cache");
        assert_invalid_field(config.validate(), "docs.cache_dir");

        let mut config = config_for_path_tests(root.clone(), &safe_base);
        config.telemetry.path = root.join("activity.jsonl");
        assert_invalid_field(config.validate(), "telemetry.path");

        let containing_path = test_path("containing-path");
        let mut config = config_for_path_tests(containing_path.join("workspace"), &safe_base);
        config.gate.cache_dir = containing_path;
        assert_invalid_field(config.validate(), "gate.cache_dir");
    }

    #[test]
    fn project_cache_is_exempt_but_disabled_telemetry_is_not_checked() {
        let root = test_path("project-root");
        let safe_base = test_path("project-safe");
        let mut config = config_for_path_tests(root.clone(), &safe_base);
        config.gate.cache = GateCache::Project;
        config.gate.cache_dir = root.join("project-cache");
        assert!(config.validate().is_ok());

        config.gate.lease_dir = root.join("leases");
        assert_invalid_field(config.validate(), "gate.lease_dir");

        let mut config = config_for_path_tests(root.clone(), &safe_base);
        config.telemetry.enabled = false;
        config.telemetry.path = root.join("activity.jsonl");
        assert!(config.validate().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn existing_paths_are_canonicalized_before_overlap_check() {
        use std::os::unix::fs::symlink;

        let base = test_path("canonical");
        let root = base.join("workspace");
        let alias = base.join("cache-alias");
        fs::create_dir_all(&root).expect("create canonical test root");
        symlink(&root, &alias).expect("create canonical test alias");

        let safe_base = test_path("canonical-safe");
        let mut config = config_for_path_tests(root, &safe_base);
        config.gate.cache_dir = alias;
        let result = config.validate();
        fs::remove_dir_all(&base).expect("remove canonical test root");

        assert_invalid_field(result, "gate.cache_dir");
    }

    #[cfg(unix)]
    #[test]
    fn missing_paths_resolve_existing_symlinked_ancestors_before_overlap_check() {
        use std::os::unix::fs::symlink;

        let base = test_path("canonical-missing");
        let root = base.join("workspace");
        let alias = base.join("cache-alias");
        fs::create_dir_all(&root).expect("create canonical missing test root");
        symlink(&root, &alias).expect("create canonical missing test alias");

        let safe_base = test_path("canonical-missing-safe");
        let mut config = config_for_path_tests(root, &safe_base);
        config.gate.cache_dir = alias.join("not-created");
        let result = config.validate();
        fs::remove_dir_all(&base).expect("remove canonical missing test root");

        assert_invalid_field(result, "gate.cache_dir");
    }

    #[test]
    fn context_configuration_wires_toml_environment_and_cli() {
        let mut cli = cli();
        cli.tools_context = Some(false);
        cli.context_max_capsules = Some(8);
        cli.context_capsule_ttl_ms = Some(9_000);
        let config = Config::from_sources(
            "/workspace",
            Some("[context]\nmax_capsules = 4\ncapsule_ttl_ms = 5000\nmax_items = 8\n"),
            [
                ("AGZ_RUST_CODER_CONTEXT__MAX_CAPSULES", "6"),
                ("AGZ_RUST_CODER_CONTEXT__MAX_ITEMS", "12"),
                ("AGZ_RUST_CODER_CONTEXT__CAPSULE_TTL_MS", "7000"),
            ],
            &cli,
        )
        .expect("context configuration layers resolve");
        assert!(!config.tools.context);
        assert_eq!(config.context.max_capsules, 8);
        assert_eq!(config.context.capsule_ttl_ms, 9_000);
        assert_eq!(config.context.max_items, 12);
        assert!(
            !config.enabled_tool_names().contains(&"context"),
            "tools.context=false must remove the tool from the catalog"
        );

        // Without the CLI layer, environment beats TOML for the TTL.
        let env_ttl = Config::from_sources(
            "/workspace",
            Some("[context]\ncapsule_ttl_ms = 5000\n"),
            [("AGZ_RUST_CODER_CONTEXT__CAPSULE_TTL_MS", "7000")],
            &cli_without_context(),
        )
        .expect("context environment TTL resolves");
        assert_eq!(env_ttl.context.capsule_ttl_ms, 7_000);

        let mut invalid = config;
        invalid.context.capsule_ttl_ms = 10;
        assert_invalid_field(invalid.validate(), "context.capsule_ttl_ms");

        let mut invalid_capsules = Config::defaults_at(test_path("context-defaults"));
        invalid_capsules.context.max_capsules = 0;
        assert_invalid_field(invalid_capsules.validate(), "context.max_capsules");

        let mut invalid_items = Config::defaults_at(test_path("context-items"));
        invalid_items.context.max_items = 257;
        assert_invalid_field(invalid_items.validate(), "context.max_items");
    }

    fn cli_without_context() -> CliOptions {
        let mut cli = cli();
        cli.tools_context = None;
        cli.context_max_capsules = None;
        cli.context_capsule_ttl_ms = None;
        cli.context_max_items = None;
        cli
    }

    #[test]
    fn api_configuration_wires_toml_environment_and_cli() {
        let mut cli = cli();
        cli.tools_api = Some(false);
        cli.api_max_snippets = Some(2);
        cli.api_max_snippet_bytes = Some(4_096);
        let config = Config::from_sources(
            "/workspace",
            Some("[api]\nmax_snippets = 3\nmax_snippet_bytes = 8192\ncompile_timeout_ms = 9000\n"),
            [
                ("AGZ_RUST_CODER_API__MAX_SNIPPETS", "1"),
                ("AGZ_RUST_CODER_API__COMPILE_TIMEOUT_MS", "7000"),
            ],
            &cli,
        )
        .expect("api configuration layers resolve");
        assert!(!config.tools.api);
        assert_eq!(config.api.max_snippets, 2);
        assert_eq!(config.api.max_snippet_bytes, 4_096);
        assert_eq!(config.api.compile_timeout_ms, 7_000);
        assert!(
            !config.enabled_tool_names().contains(&"api"),
            "tools.api=false must remove the tool from the catalog"
        );

        let mut invalid_snippets = Config::defaults_at(test_path("api-snippets"));
        invalid_snippets.api.max_snippets = 0;
        assert_invalid_field(invalid_snippets.validate(), "api.max_snippets");

        let mut invalid_bytes = Config::defaults_at(test_path("api-bytes"));
        invalid_bytes.api.max_snippet_bytes = 4;
        assert_invalid_field(invalid_bytes.validate(), "api.max_snippet_bytes");

        let mut invalid_timeout = Config::defaults_at(test_path("api-timeout"));
        invalid_timeout.api.compile_timeout_ms = 10;
        assert_invalid_field(invalid_timeout.validate(), "api.compile_timeout_ms");
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_environment_is_filtered_or_reported_without_panicking() {
        use std::os::unix::ffi::OsStringExt;

        let irrelevant = OsString::from_vec(vec![0xff]);
        let relevant = OsString::from("AGZ_RUST_CODER_GATE__HOST_CONCURRENCY");
        let filtered = environment_from_os([
            (irrelevant, OsString::from_vec(vec![0xfe])),
            (relevant.clone(), OsString::from("2")),
        ])
        .expect("irrelevant non-Unicode variables are ignored");
        assert_eq!(filtered.len(), 1);

        let invalid_value = environment_from_os([(relevant, OsString::from_vec(vec![0xff]))]);
        assert!(matches!(
            invalid_value,
            Err(ConfigError::InvalidEnvironment { .. })
        ));

        let mut invalid_name = ENV_PREFIX.as_bytes().to_vec();
        invalid_name.push(0xff);
        let invalid_name =
            environment_from_os([(OsString::from_vec(invalid_name), OsString::from("value"))]);
        assert!(matches!(
            invalid_name,
            Err(ConfigError::InvalidEnvironment { .. })
        ));
    }
}
