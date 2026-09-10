//! End-to-end tests for `change(action=migrate)`.
//!
//! Each fixture drives planning through an injected fake analyzer (the real
//! rust-analyzer path is exercised by `lsp_real.rs`) and validates the
//! transformed candidate with real Cargo. The original workspace must stay
//! byte-identical and the candidate must never live inside it.

use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use agz_rust_mcp::{
    Config,
    change::{
        AnalyzeRequest, AnalyzedSite, AnalyzerError, AnchorAnalysis, ChangeAction, ChangeOutcome,
        ChangeRequest, ChangeService, MigrateAnchorInput, MigrateConstraintsInput, MigrateRequest,
        MigrateTransformationInput, MigrateTransformationKind, MigrationAnalyzer,
    },
    gate::{GateDetail, GateTargetId, ValidationOptions},
    process::ProcessSupervisor,
    workspace::{ClientRoots, RootGuard, WorkspaceRoot},
};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// One fake analyzer site: the needle and which occurrence to use.
#[derive(Clone)]
struct SiteSpec {
    file: &'static str,
    needle: &'static str,
    occurrence: usize,
    identity: Option<bool>,
}

#[derive(Clone)]
struct AnalysisSpec {
    symbol: &'static str,
    definition: SiteSpec,
    implementations: Vec<SiteSpec>,
    references: Vec<SiteSpec>,
    references_total: u64,
}

struct FakeAnalyzer {
    analyses: Vec<AnalysisSpec>,
}

impl FakeAnalyzer {
    fn locate(&self, root: &Path, spec: &SiteSpec) -> Result<AnalyzedSite, AnalyzerError> {
        let path = root.join(spec.file);
        let content = fs::read_to_string(&path)
            .map_err(|error| AnalyzerError::Invalid(format!("{}: {error}", spec.file)))?;
        let mut offset = 0usize;
        for _ in 0..spec.occurrence {
            let found = content[offset..]
                .find(spec.needle)
                .map(|found| offset + found)
                .ok_or_else(|| {
                    AnalyzerError::NotFound(format!("{} needle not found", spec.file))
                })?;
            offset = found + spec.needle.len();
        }
        let start = offset
            .checked_sub(spec.needle.len())
            .ok_or_else(|| AnalyzerError::Invalid("needle offset".to_owned()))?;
        let line_start = content[..start].rfind('\n').map_or(0, |index| index + 1);
        let line = content[..start].matches('\n').count();
        let character = content[line_start..start].encode_utf16().count();
        let end_character = character + spec.needle.encode_utf16().count();
        Ok(AnalyzedSite {
            file: spec.file.to_owned(),
            start_line: u32::try_from(line).unwrap_or(0),
            start_character: u32::try_from(character).unwrap_or(0),
            end_line: u32::try_from(line).unwrap_or(0),
            end_character: u32::try_from(end_character).unwrap_or(0),
            identity: spec.identity,
        })
    }
}

impl MigrationAnalyzer for FakeAnalyzer {
    fn analyze<'a>(
        &'a self,
        request: AnalyzeRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AnchorAnalysis, AnalyzerError>> + Send + 'a>> {
        Box::pin(async move {
            let spec = self
                .analyses
                .iter()
                .find(|analysis| analysis.symbol == request.anchor_symbol)
                .ok_or_else(|| {
                    AnalyzerError::NotFound(format!("no analysis for {}", request.anchor_symbol))
                })?;
            let definition = self.locate(&request.root, &spec.definition)?;
            let implementations = spec
                .implementations
                .iter()
                .map(|site| self.locate(&request.root, site))
                .collect::<Result<Vec<_>, _>>()?;
            let references = spec
                .references
                .iter()
                .map(|site| self.locate(&request.root, site))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(AnchorAnalysis {
                definition,
                implementations,
                references,
                references_total: spec.references_total,
                implementations_total: u64::try_from(spec.implementations.len()).unwrap_or(0),
                identity_checks: spec
                    .references
                    .iter()
                    .filter(|site| site.identity.is_some())
                    .count()
                    .try_into()
                    .unwrap_or(0),
                signature: None,
                notes: Vec::new(),
            })
        })
    }
}

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    guard: Arc<RootGuard>,
    config: Config,
}

impl Fixture {
    fn new(label: &str, files: &[(&str, &str)]) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let base = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-mcp-migrate-{label}-{}-{stamp}",
                std::process::id()
            ));
        let root = base.join("workspace");
        let state = base.join("state");
        fs::create_dir_all(&root).expect("create workspace");
        fs::create_dir_all(&state).expect("create state");
        for (relative, contents) in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture parent");
            }
            fs::write(path, contents).expect("write fixture file");
        }
        let mut config = Config::defaults_at(&root);
        config.gate.debounce_ms = 10;
        config.gate.hard_timeout_ms = 300_000;
        config.gate.cache_dir = state.join("gate-cache");
        config.gate.lease_dir = state.join("leases");
        config.change.scratch_dir = state.join("change");
        config.telemetry.enabled = false;
        config.telemetry.path = state.join("activity.jsonl");
        let dependencies = config.server.allow_dependency_roots.clone();
        let guard =
            Arc::new(RootGuard::new([root.clone()], dependencies).expect("create root guard"));
        Self {
            root,
            state,
            guard,
            config,
        }
    }

    fn service(&self, analyzer: Option<Arc<dyn MigrationAnalyzer>>) -> Arc<ChangeService> {
        let service = ChangeService::new(
            self.config.clone(),
            Arc::clone(&self.guard),
            ProcessSupervisor::without_journal(),
        )
        .expect("create change service");
        let service = match analyzer {
            Some(analyzer) => service.with_migration_analyzer(analyzer),
            None => service,
        };
        Arc::new(service)
    }

    fn workspace(&self) -> WorkspaceRoot {
        self.guard
            .snapshot(ClientRoots::unsupported())
            .expect("root snapshot")
            .select(Some(&self.root))
            .expect("select workspace")
    }

    fn candidate(&self, id: &str) -> PathBuf {
        self.state.join("change").join(id).join("candidate")
    }

    fn snapshot_tree(&self) -> BTreeMap<String, String> {
        let mut snapshot = BTreeMap::new();
        let mut pending = vec![self.root.clone()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("read fixture") {
                let entry = entry.expect("fixture entry");
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path
                        .strip_prefix(&self.root)
                        .expect("relative path")
                        .to_string_lossy()
                        .replace('\\', "/");
                    snapshot.insert(relative, digest(&path));
                }
            }
        }
        snapshot
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

fn digest(path: &Path) -> String {
    let bytes = fs::read(path).expect("read digest target");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("{:x}", hasher.finalize())
}

async fn execute(
    service: &ChangeService,
    fixture: &Fixture,
    request: ChangeRequest,
) -> ChangeOutcome {
    service
        .execute(
            request,
            &fixture.workspace(),
            CancellationToken::new(),
            None,
        )
        .await
}

fn create_request() -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Create,
        change_id: None,
        expected_revision: None,
        base_identity: None,
        patches: Vec::new(),
        new_files: Vec::new(),
        migration: None,
        target: GateTargetId::Check,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn migrate_request(
    id: &str,
    revision: u64,
    base: &str,
    symbol: &str,
    argument: &str,
    target: GateTargetId,
    constraints: MigrateConstraintsInput,
) -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Migrate,
        change_id: Some(id.to_owned()),
        expected_revision: Some(revision),
        base_identity: Some(base.to_owned()),
        patches: Vec::new(),
        new_files: Vec::new(),
        migration: Some(MigrateRequest {
            anchor: MigrateAnchorInput {
                file: "api/src/lib.rs".to_owned(),
                symbol: symbol.to_owned(),
                line: None,
            },
            transformation: MigrateTransformationInput {
                kind: MigrateTransformationKind::AddParameter,
                parameter: "_factor: u32".to_owned(),
                argument: argument.to_owned(),
                position: None,
            },
            consumer_scope: Some("workspace".to_owned()),
            constraints,
        }),
        target,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn inspect_request(id: &str) -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Inspect,
        change_id: Some(id.to_owned()),
        ..create_request()
    }
}

fn four_crate_fixture() -> Fixture {
    Fixture::new(
        "workspace",
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"api\", \"imp\", \"reexport\", \"consumer\"]\nresolver = \"2\"\n",
            ),
            (
                "Cargo.lock",
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"api\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"consumer\"\nversion = \"0.1.0\"\ndependencies = [\n \"imp\",\n \"reexport\",\n]\n\n[[package]]\nname = \"imp\"\nversion = \"0.1.0\"\ndependencies = [\n \"api\",\n]\n\n[[package]]\nname = \"reexport\"\nversion = \"0.1.0\"\ndependencies = [\n \"api\",\n]\n",
            ),
            (
                "api/Cargo.toml",
                "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "api/src/lib.rs",
                "pub trait Scale {\n    fn scale(&self, base: u32) -> u32;\n}\n\npub fn compute(base: u32) -> u32 {\n    base + 1\n}\n",
            ),
            (
                "imp/Cargo.toml",
                "[package]\nname = \"imp\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\napi = { path = \"../api\" }\n",
            ),
            (
                "imp/src/lib.rs",
                "pub struct Gauge(pub u32);\n\nimpl api::Scale for Gauge {\n    fn scale(&self, base: u32) -> u32 {\n        self.0 * base\n    }\n}\n",
            ),
            (
                "reexport/Cargo.toml",
                "[package]\nname = \"reexport\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\napi = { path = \"../api\" }\n",
            ),
            ("reexport/src/lib.rs", "pub use api::{compute, Scale};\n"),
            (
                "consumer/Cargo.toml",
                "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nimp = { path = \"../imp\" }\nreexport = { path = \"../reexport\" }\n",
            ),
            (
                "consumer/src/lib.rs",
                "use imp::Gauge;\nuse reexport::{compute, Scale};\n\npub fn read() -> u32 {\n    Gauge(2).scale(3) + compute(1)\n}\n\nfn scale(value: u32) -> u32 {\n    value + 7\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn reads() {\n        let first = Gauge(4).scale(2);\n        let total = compute(2);\n        assert_eq!(first, 8);\n        assert_eq!(total, 3);\n        assert_eq!(scale(1), 8);\n    }\n}\n",
            ),
        ],
    )
}

fn workspace_analyzer() -> FakeAnalyzer {
    FakeAnalyzer {
        analyses: vec![
            AnalysisSpec {
                symbol: "scale",
                definition: SiteSpec {
                    file: "api/src/lib.rs",
                    needle: "scale",
                    occurrence: 1,
                    identity: Some(true),
                },
                implementations: vec![SiteSpec {
                    file: "imp/src/lib.rs",
                    needle: "scale",
                    occurrence: 1,
                    identity: Some(true),
                }],
                references: vec![
                    SiteSpec {
                        file: "consumer/src/lib.rs",
                        needle: "scale",
                        occurrence: 1,
                        identity: Some(true),
                    },
                    SiteSpec {
                        file: "consumer/src/lib.rs",
                        needle: "scale",
                        occurrence: 2,
                        identity: Some(false),
                    },
                    SiteSpec {
                        file: "consumer/src/lib.rs",
                        needle: "scale",
                        occurrence: 3,
                        identity: Some(true),
                    },
                    SiteSpec {
                        file: "consumer/src/lib.rs",
                        needle: "scale",
                        occurrence: 4,
                        identity: Some(false),
                    },
                ],
                references_total: 4,
            },
            AnalysisSpec {
                symbol: "compute",
                definition: SiteSpec {
                    file: "api/src/lib.rs",
                    needle: "compute",
                    occurrence: 1,
                    identity: Some(true),
                },
                implementations: Vec::new(),
                references: vec![
                    SiteSpec {
                        file: "reexport/src/lib.rs",
                        needle: "compute",
                        occurrence: 1,
                        identity: Some(true),
                    },
                    SiteSpec {
                        file: "consumer/src/lib.rs",
                        needle: "compute",
                        occurrence: 1,
                        identity: Some(true),
                    },
                    SiteSpec {
                        file: "consumer/src/lib.rs",
                        needle: "compute",
                        occurrence: 2,
                        identity: Some(true),
                    },
                    SiteSpec {
                        file: "consumer/src/lib.rs",
                        needle: "compute",
                        occurrence: 3,
                        identity: Some(true),
                    },
                ],
                references_total: 4,
            },
        ],
    }
}

#[tokio::test]
async fn workspace_migration_covers_api_impl_reexport_and_consumers() {
    let fixture = four_crate_fixture();
    let originals = fixture.snapshot_tree();
    let service = fixture.service(Some(Arc::new(workspace_analyzer())));
    let created = execute(&service, &fixture, create_request()).await;
    assert_eq!(created.status, "CREATED", "{created:#?}");
    let id = created.data.change_id.clone().expect("change id");
    let base = created.data.base_identity.clone().expect("base identity");

    // 1) The trait method: definition + implementation + call sites + one
    //    same-named unrelated free function in the consumer.
    let migrated = execute(
        &service,
        &fixture,
        migrate_request(
            &id,
            0,
            &base,
            "scale",
            "2",
            GateTargetId::Test,
            MigrateConstraintsInput::default(),
        ),
    )
    .await;
    assert_eq!(migrated.status, "MIGRATED", "{migrated:#?}");
    assert!(!migrated.is_error, "{migrated:#?}");
    let report = migrated.data.migration.clone().expect("migration report");
    assert!(report.complete, "{report:#?}");
    assert_eq!(report.obligations_total, 0);
    assert_eq!(report.impact.implementations_total, 1);
    assert_eq!(report.impact.consumers_total, 2);
    assert_eq!(report.impact.unrelated_total, 2);
    assert_eq!(report.impact.unrelated[0].file, "consumer/src/lib.rs");
    assert!(report.impact.packages.contains(&"api".to_owned()));
    assert!(report.impact.packages.contains(&"imp".to_owned()));
    assert!(report.api_diff.changed);
    assert!(report.api_diff.public_api);
    assert!(report.flags.behavior_change);
    assert_eq!(report.flags.semantic_equivalence_claim, "notClaimed");
    assert!(
        migrated
            .data
            .evidence
            .iter()
            .any(|row| row.fresh && row.authoritative && row.target == "test"),
        "{:#?}",
        migrated.data.evidence
    );
    let candidate = fixture.candidate(&id);
    let api = fs::read_to_string(candidate.join("api/src/lib.rs")).expect("candidate api");
    assert!(
        api.contains("fn scale(&self, base: u32, _factor: u32) -> u32;"),
        "{api}"
    );
    let imp = fs::read_to_string(candidate.join("imp/src/lib.rs")).expect("candidate imp");
    assert!(
        imp.contains("fn scale(&self, base: u32, _factor: u32) -> u32"),
        "{imp}"
    );
    let consumer =
        fs::read_to_string(candidate.join("consumer/src/lib.rs")).expect("candidate consumer");
    assert!(consumer.contains("Gauge(2).scale(3, 2)"), "{consumer}");
    assert!(consumer.contains("Gauge(4).scale(2, 2)"), "{consumer}");
    assert!(
        consumer.contains("fn scale(value: u32) -> u32"),
        "{consumer}"
    );
    assert!(consumer.contains("assert_eq!(scale(1), 8)"), "{consumer}");

    // 2) A free function re-exported by the reexport crate and used by the
    //    consumer, including its import site.
    let migrated = execute(
        &service,
        &fixture,
        migrate_request(
            &id,
            1,
            &base,
            "compute",
            "1",
            GateTargetId::Check,
            MigrateConstraintsInput::default(),
        ),
    )
    .await;
    assert_eq!(migrated.status, "MIGRATED", "{migrated:#?}");
    let report = migrated.data.migration.clone().expect("migration report");
    assert!(report.complete, "{report:#?}");
    assert!(
        report.impact.packages.contains(&"reexport".to_owned()),
        "{:#?}",
        report.impact.packages
    );
    assert!(report.impact.reexports_total >= 1, "{:#?}", report.impact);
    assert_eq!(report.compatibility_scope, "capturedWorkspaceOnly");
    let candidate = fixture.candidate(&id);
    let consumer =
        fs::read_to_string(candidate.join("consumer/src/lib.rs")).expect("candidate consumer");
    assert!(consumer.contains("compute(1, 1)"), "{consumer}");
    assert!(consumer.contains("compute(2, 1)"), "{consumer}");
    let reexport =
        fs::read_to_string(candidate.join("reexport/src/lib.rs")).expect("candidate reexport");
    assert_eq!(reexport, "pub use api::{compute, Scale};\n");

    // 3) The original workspace was never written and hosts no target dir.
    assert_eq!(fixture.snapshot_tree(), originals);
    assert!(!fixture.root.join("target").exists());
    let inspected = execute(&service, &fixture, inspect_request(&id)).await;
    assert_eq!(inspected.data.revision, 2, "{inspected:#?}");
    assert!(inspected.data.migration.is_some());
}

fn macro_fixture() -> Fixture {
    Fixture::new(
        "macro",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"macro-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
            ),
            (
                "Cargo.lock",
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"macro-fixture\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/lib.rs",
                "pub fn scale(value: u32) -> u32 {\n    value + 1\n}\n\n#[cfg(test)]\nmod tests {\n    use super::scale;\n\n    #[test]\n    fn works() {\n        assert_eq!(scale(1), 2);\n    }\n}\n",
            ),
        ],
    )
}

fn single_crate_anchor(symbol: &str) -> MigrateAnchorInput {
    MigrateAnchorInput {
        file: "src/lib.rs".to_owned(),
        symbol: symbol.to_owned(),
        line: None,
    }
}

fn single_crate_migrate_request(
    id: &str,
    revision: u64,
    base: &str,
    symbol: &str,
    constraints: MigrateConstraintsInput,
) -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Migrate,
        change_id: Some(id.to_owned()),
        expected_revision: Some(revision),
        base_identity: Some(base.to_owned()),
        patches: Vec::new(),
        new_files: Vec::new(),
        migration: Some(MigrateRequest {
            anchor: single_crate_anchor(symbol),
            transformation: MigrateTransformationInput {
                kind: MigrateTransformationKind::AddParameter,
                parameter: "_factor: u32".to_owned(),
                argument: "1".to_owned(),
                position: None,
            },
            consumer_scope: Some("workspace".to_owned()),
            constraints,
        }),
        target: GateTargetId::Check,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

#[tokio::test]
async fn macro_sites_are_obligations_and_partial_is_never_complete() {
    let fixture = macro_fixture();
    let originals = fixture.snapshot_tree();
    let analyzer = FakeAnalyzer {
        analyses: vec![AnalysisSpec {
            symbol: "scale",
            definition: SiteSpec {
                file: "src/lib.rs",
                needle: "scale",
                occurrence: 1,
                identity: Some(true),
            },
            implementations: Vec::new(),
            references: vec![SiteSpec {
                file: "src/lib.rs",
                needle: "scale",
                occurrence: 3,
                identity: Some(true),
            }],
            references_total: 1,
        }],
    };
    let service = fixture.service(Some(Arc::new(analyzer)));
    let created = execute(&service, &fixture, create_request()).await;
    let id = created.data.change_id.clone().expect("change id");
    let base = created.data.base_identity.clone().expect("base identity");

    let migrated = execute(
        &service,
        &fixture,
        single_crate_migrate_request(&id, 0, &base, "scale", MigrateConstraintsInput::default()),
    )
    .await;
    assert_eq!(migrated.status, "MIGRATION_PARTIAL", "{migrated:#?}");
    assert!(migrated.is_error);
    let report = migrated.data.migration.clone().expect("report");
    assert!(!report.complete);
    assert_eq!(report.obligations_total, 1);
    assert_eq!(report.obligations[0].kind, "macro");
    assert!(
        report
            .impact
            .feature_hints
            .iter()
            .any(|hint| hint.contains("#[cfg(test)]")),
        "cfg-gated obligation sites must surface a feature boundary: {:#?}",
        report.impact.feature_hints
    );
    // The macro-hidden call site is not compiled by `check`, so the gate can
    // still pass; a passing gate never upgrades an incomplete plan.
    assert!(
        migrated.data.reason.contains("gate=PASS"),
        "{:#?}",
        migrated.data.reason
    );
    assert!(migrated.data.evidence.iter().any(|row| {
        row.fresh && row.authoritative && matches!(row.status.as_str(), "FAST_PASS" | "FULL_PASS")
    }));
    let candidate = fixture.candidate(&id);
    let source = fs::read_to_string(candidate.join("src/lib.rs")).expect("candidate source");
    assert!(
        source.contains("fn scale(value: u32, _factor: u32) -> u32"),
        "{source}"
    );
    assert!(source.contains("assert_eq!(scale(1), 2)"), "{source}");
    assert_eq!(fixture.snapshot_tree(), originals);
}

#[tokio::test]
async fn budget_omissions_are_visible_and_fail_closed() {
    let fixture = Fixture::new(
        "budget",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"budget-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
            ),
            (
                "Cargo.lock",
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"budget-fixture\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/lib.rs",
                "pub fn scale(value: u32) -> u32 {\n    value + 1\n}\n\npub fn run() -> u32 {\n    scale(1) + scale(2)\n}\n",
            ),
        ],
    );
    let originals = fixture.snapshot_tree();
    let analyzer = FakeAnalyzer {
        analyses: vec![AnalysisSpec {
            symbol: "scale",
            definition: SiteSpec {
                file: "src/lib.rs",
                needle: "scale",
                occurrence: 1,
                identity: Some(true),
            },
            implementations: Vec::new(),
            references: vec![
                SiteSpec {
                    file: "src/lib.rs",
                    needle: "scale",
                    occurrence: 2,
                    identity: Some(true),
                },
                SiteSpec {
                    file: "src/lib.rs",
                    needle: "scale",
                    occurrence: 3,
                    identity: Some(true),
                },
            ],
            references_total: 2,
        }],
    };
    let service = fixture.service(Some(Arc::new(analyzer)));
    let created = execute(&service, &fixture, create_request()).await;
    let id = created.data.change_id.clone().expect("change id");
    let base = created.data.base_identity.clone().expect("base identity");

    let migrated = execute(
        &service,
        &fixture,
        single_crate_migrate_request(
            &id,
            0,
            &base,
            "scale",
            MigrateConstraintsInput {
                max_references: None,
                max_edits: Some(2),
                max_identity_checks: None,
            },
        ),
    )
    .await;
    assert_eq!(migrated.status, "MIGRATION_PARTIAL", "{migrated:#?}");
    let report = migrated.data.migration.clone().expect("report");
    assert!(!report.complete, "{report:#?}");
    assert_eq!(report.budget.omitted_edits, 1);
    assert!(report.budget.truncated);
    assert!(
        report
            .obligations
            .iter()
            .any(|obligation| obligation.kind == "budget"),
        "{:#?}",
        report.obligations
    );
    assert!(
        migrated.data.reason.contains("gate=FAIL"),
        "{:#?}",
        migrated.data.reason
    );
    assert!(
        migrated
            .data
            .evidence
            .iter()
            .any(|row| row.fresh && row.authoritative && row.status == "FAIL"),
        "{:#?}",
        migrated.data.evidence
    );
    let candidate = fixture.candidate(&id);
    let source = fs::read_to_string(candidate.join("src/lib.rs")).expect("candidate source");
    assert!(
        source.contains("fn scale(value: u32, _factor: u32) -> u32"),
        "{source}"
    );
    assert!(source.contains("scale(1, 1) + scale(2)"), "{source}");
    assert_eq!(fixture.snapshot_tree(), originals);
}

#[tokio::test]
async fn unavailable_analyzer_and_unsupported_scope_are_typed() {
    let fixture = macro_fixture();
    let originals = fixture.snapshot_tree();
    let service = fixture.service(None);
    let created = execute(&service, &fixture, create_request()).await;
    let id = created.data.change_id.clone().expect("change id");
    let base = created.data.base_identity.clone().expect("base identity");

    let unavailable = execute(
        &service,
        &fixture,
        single_crate_migrate_request(&id, 0, &base, "scale", MigrateConstraintsInput::default()),
    )
    .await;
    assert_eq!(
        unavailable.status, "ANALYZER_UNAVAILABLE",
        "{unavailable:#?}"
    );
    assert!(unavailable.is_error);

    // A configured analyzer with an unsupported consumer scope is refused
    // before any plan or candidate byte is touched.
    let analyzer = FakeAnalyzer {
        analyses: vec![AnalysisSpec {
            symbol: "scale",
            definition: SiteSpec {
                file: "src/lib.rs",
                needle: "scale",
                occurrence: 1,
                identity: Some(true),
            },
            implementations: Vec::new(),
            references: Vec::new(),
            references_total: 0,
        }],
    };
    let scoped_service = fixture.service(Some(Arc::new(analyzer)));
    let mut request = fixture_scoped_request(&id, &base);
    request
        .migration
        .as_mut()
        .expect("migration")
        .consumer_scope = Some("package".to_owned());
    let refused = execute(&scoped_service, &fixture, request).await;
    assert_eq!(refused.status, "MIGRATION_REFUSED", "{refused:#?}");
    assert!(refused.data.reason.contains("consumerScope"));

    let inspected = execute(&scoped_service, &fixture, inspect_request(&id)).await;
    assert_eq!(inspected.data.revision, 0, "{inspected:#?}");
    assert!(inspected.data.migration.is_none());
    assert_eq!(fixture.snapshot_tree(), originals);
    assert!(!fixture.candidate(&id).join("target").exists());
}

fn fixture_scoped_request(id: &str, base: &str) -> ChangeRequest {
    single_crate_migrate_request(id, 0, base, "scale", MigrateConstraintsInput::default())
}
