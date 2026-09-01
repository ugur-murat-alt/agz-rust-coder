use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use agz_rust_coder::tools::audit::{
    AuditCancellation, AuditError, AuditFinding, AuditLimits, AuditRequest, AuditService,
    AuditSkipReason, AuditSummary, InvalidPathReason,
};
use agz_rust_coder::workspace::{ClientRoots, RootGuard, WorkspaceRoot};
use tokio_util::sync::CancellationToken;

struct TestRoot(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

impl TestRoot {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = fs::canonicalize(std::env::temp_dir())
            .expect("canonical temp directory")
            .join(format!(
                "agz-rust-coder-audit-{label}-{}-{stamp}-{sequence}",
                std::process::id()
            ));
        fs::create_dir(&path).expect("create audit test root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create audit test parent");
        }
        fs::write(&path, source).expect("write audit test source");
        path
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace_root(path: &Path) -> WorkspaceRoot {
    let guard = RootGuard::new([path.to_owned()], std::iter::empty()).expect("create root guard");
    guard
        .snapshot(ClientRoots::unsupported())
        .expect("create root snapshot")
        .select(None)
        .expect("select root")
}

fn fixture_root() -> PathBuf {
    fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/audit"))
        .expect("canonicalize audit fixtures")
}

fn findings_with_pattern<'a>(summary: &'a AuditSummary, pattern: &str) -> Vec<&'a AuditFinding> {
    summary
        .findings
        .iter()
        .filter(|finding| finding.pattern_id() == pattern)
        .collect()
}

#[test]
fn fixture_audit_matches_the_reference_patterns_and_skips_generated_code() {
    let root = workspace_root(&fixture_root());
    let summary = AuditService::default()
        .audit(&AuditRequest::new(&root))
        .expect("audit fixture");

    assert_eq!(summary.scanned_files, 3);
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "clone-tax")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "unwrap")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "string-param")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "vec-param")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "pathbuf-param")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "index-loop")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "arc-mutex-stack")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "std-mutex-await")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "unsafe-block")
    );
    assert!(
        summary
            .findings
            .iter()
            .any(|finding| finding.pattern_id() == "casual-safety-comment")
    );
    assert!(
        !summary
            .findings
            .iter()
            .any(|finding| finding.file.ends_with("generated.rs"))
    );
    assert!(summary.findings.windows(2).all(|pair| (
        pair[0].file.clone(),
        pair[0].line,
        pair[0].pattern
    ) <= (
        pair[1].file.clone(),
        pair[1].line,
        pair[1].pattern
    )));
}

#[test]
fn masker_ignores_comments_strings_raw_strings_and_test_modules() {
    let root = TestRoot::new("masking");
    root.write(
        "src/masking.rs",
        r##"use std::sync::Mutex;
// async .await None::<u8>.unwrap(); value.clone(); unsafe { }
/* Arc<Mutex<u8>> &String &Vec<u8> &PathBuf */
const NORMAL: &str = "None::<u8>.unwrap(); unsafe { }";
const BYTES: &[u8] = b"std::sync::Mutex; Arc<Mutex<u8>>";
const RAW: &str = r#"None::<u8>.unwrap(); unsafe { }"#;
fn production() { None::<u8>.unwrap(); }
#[cfg(test)]
mod tests {
    async fn only_test() { work().await; }
    fn ignored() { None::<u8>.unwrap(); }
}
        "##,
    );
    let root = workspace_root(root.path());
    let summary = AuditService::default()
        .audit(&AuditRequest::new(&root))
        .expect("audit masking fixture");

    assert_eq!(findings_with_pattern(&summary, "unwrap").len(), 1);
    assert!(findings_with_pattern(&summary, "std-mutex-await").is_empty());
    assert!(findings_with_pattern(&summary, "clone-tax").is_empty());
    assert!(findings_with_pattern(&summary, "unsafe-block").is_empty());
}

#[test]
fn async_mutex_detection_ignores_test_only_async_code() {
    let root = TestRoot::new("async-test");
    root.write(
        "src/only_tests.rs",
        "use std::sync::Mutex;\n#[cfg(test)]\nmod tests { async fn test() { work().await; } }\n",
    );
    root.write(
        "src/production.rs",
        "use std::sync::Mutex;\nasync fn work() { other().await; }\n",
    );
    let root = workspace_root(root.path());
    let summary = AuditService::default()
        .audit(&AuditRequest::new(&root))
        .expect("audit async fixture");

    assert_eq!(
        summary
            .findings
            .iter()
            .filter(|finding| finding.pattern_id() == "std-mutex-await")
            .count(),
        1
    );
    assert_eq!(
        summary
            .findings
            .iter()
            .find(|finding| finding.pattern_id() == "std-mutex-await")
            .map(|finding| finding.file.clone()),
        Some(PathBuf::from("src/production.rs"))
    );
}

#[test]
fn budgets_bound_files_bytes_findings_and_skips() {
    let root = TestRoot::new("budgets");
    root.write("a.rs", "fn a() { None::<u8>.unwrap(); }\n");
    root.write("b.rs", "fn b() { None::<u8>.unwrap(); }\n");
    root.write("large.rs", &"x".repeat(128));
    let root = workspace_root(root.path());
    let service = AuditService::new(AuditLimits::new(1, 64, 64, 1));
    let summary = service
        .audit(&AuditRequest::new(&root))
        .expect("bounded audit");

    assert!(summary.scanned_files <= 1);
    assert!(summary.scanned_bytes <= 64);
    assert!(summary.findings.len() <= 1);
    assert!(summary.truncated);
    assert!(summary.skipped.iter().any(|skip| matches!(
        skip.reason,
        AuditSkipReason::FileLimit | AuditSkipReason::ByteLimit
    )));
}

#[cfg(unix)]
#[test]
fn symlink_is_skipped_and_relative_parent_or_absolute_paths_are_rejected() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("paths");
    let outside = TestRoot::new("outside");
    outside.write("outside.rs", "fn outside() { None::<u8>.unwrap(); }\n");
    symlink(
        outside.path().join("outside.rs"),
        root.path().join("linked.rs"),
    )
    .expect("create source symlink");
    root.write("src/inside.rs", "fn inside() { None::<u8>.unwrap(); }\n");
    let root = workspace_root(root.path());
    let service = AuditService::default();

    let summary = service
        .audit(&AuditRequest::new(&root))
        .expect("audit symlink");
    assert!(
        !summary
            .findings
            .iter()
            .any(|finding| finding.file == Path::new("linked.rs"))
    );
    assert!(
        summary
            .skipped
            .iter()
            .any(|skip| skip.path == Path::new("linked.rs")
                && skip.reason == AuditSkipReason::Symlink)
    );

    let parent = service.audit(&AuditRequest::new(&root).with_path("../outside.rs"));
    assert!(matches!(
        parent,
        Err(AuditError::InvalidPath {
            reason: InvalidPathReason::ParentComponent,
            ..
        })
    ));
    let absolute = service.audit(&AuditRequest::new(&root).with_path(root.path()));
    assert!(matches!(
        absolute,
        Err(AuditError::InvalidPath {
            reason: InvalidPathReason::Absolute,
            ..
        })
    ));
}

#[test]
fn root_guard_selection_keeps_directory_and_file_path_authorized() {
    let root = TestRoot::new("guard");
    root.write("src/lib.rs", "pub fn value() { None::<u8>.unwrap(); }\n");
    let guard =
        Arc::new(RootGuard::new([root.path().to_owned()], std::iter::empty()).expect("guard"));
    let summary = AuditService::default()
        .audit_with_guard(&guard, Some(root.path()), Some(Path::new("src/lib.rs")))
        .expect("guarded audit");

    assert_eq!(summary.scanned_files, 1);
    assert_eq!(summary.findings.len(), 1);
    assert_eq!(summary.findings[0].file, PathBuf::from("src/lib.rs"));
}

#[test]
fn cancellation_has_a_distinct_reason_and_fails_before_scanning() {
    let root = TestRoot::new("cancelled");
    root.write("src/lib.rs", "fn inside() { None::<u8>.unwrap(); }\n");
    let root = workspace_root(root.path());
    let request = CancellationToken::new();
    request.cancel();
    let cancellation = agz_rust_coder::tools::audit::AuditCancellation::new(
        request,
        CancellationToken::new(),
        CancellationToken::new(),
    );

    let result = AuditService::default().scan_with_cancellation(&root, None, &cancellation);
    assert!(matches!(
        result,
        Err(AuditError::Cancelled(
            agz_rust_coder::tools::audit::AuditCancellationReason::Request
        ))
    ));
}

#[test]
fn root_epoch_cancellation_has_its_own_reason() {
    let root = TestRoot::new("root-epoch-cancel");
    root.write("src/lib.rs", "fn inside() { None::<u8>.unwrap(); }\n");
    let root = workspace_root(root.path());
    let root_epoch = CancellationToken::new();
    root_epoch.cancel();
    let cancellation = AuditCancellation::new(
        CancellationToken::new(),
        root_epoch,
        CancellationToken::new(),
    );

    let result = AuditService::default().scan_with_cancellation(&root, None, &cancellation);
    assert!(matches!(
        result,
        Err(AuditError::Cancelled(
            agz_rust_coder::tools::audit::AuditCancellationReason::RootEpoch
        ))
    ));
}

#[tokio::test]
async fn async_scan_runs_on_the_blocking_pool_and_observes_shutdown_cancellation() {
    let root = TestRoot::new("async");
    root.write("src/lib.rs", "fn inside() { None::<u8>.unwrap(); }\n");
    let root = workspace_root(root.path());
    let summary = AuditService::default()
        .scan_async(root.clone(), None, AuditCancellation::default())
        .await
        .expect("async audit");
    assert_eq!(summary.scanned_files, 1);

    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let cancellation =
        AuditCancellation::new(CancellationToken::new(), CancellationToken::new(), shutdown);
    let result = AuditService::default()
        .scan_async(root, None, cancellation)
        .await;
    assert!(matches!(
        result,
        Err(AuditError::Cancelled(
            agz_rust_coder::tools::audit::AuditCancellationReason::Shutdown
        ))
    ));
}
