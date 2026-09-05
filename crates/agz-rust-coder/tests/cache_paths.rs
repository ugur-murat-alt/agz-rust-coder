use agz_rust_coder::{
    cache::{OwnedCacheRetention, RetentionLimits},
    docs::{CacheIdentity, DocsCache, GeneratedPage, page_candidates},
};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

struct TestRoot(PathBuf);
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn canonical_cache_paths_support_the_complete_docs_and_retention_lifecycle() {
    // canonicalize produces the verbatim drive path on Windows, including
    // expansion of short user names. Do not strip that prefix in this test.
    let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let root = TestRoot(base.join(format!("agz-cache-paths-{}-{stamp}", std::process::id())));
    fs::create_dir(&root.0).expect("create isolated fixture");
    let cache_root = root.0.join("nested").join("docs");
    let cache = DocsCache::new(&cache_root);
    let identity = CacheIdentity {
        crate_name: "demo-crate".into(),
        version: "1.2.3".into(),
        source: "path".into(),
        fingerprint: "canonical-path-fixture".into(),
    };
    let deadline = Some(Instant::now() + Duration::from_secs(10));
    let generation = cache
        .prepare_generation_bounded(&identity, deadline, None)
        .expect("prepare generation through canonical drive root");
    generation
        .validate()
        .expect("retain cache directory identity");
    drop(generation);
    let page = GeneratedPage {
        path: "struct.Widget.html".into(),
        html: b"<html><main><p>Canonical cache Widget.</p></main></html>".to_vec(),
    };
    let entry = cache
        .publish_pages(&identity, "demo-crate", &[page], deadline)
        .expect("publish through canonical drive root");
    assert!(cache.is_complete(&identity));
    assert!(
        cache
            .read_page(&identity, "demo-crate", &page_candidates(Some("Widget")))
            .expect("read completed page")
            .text
            .contains("Canonical cache Widget")
    );
    let retention = OwnedCacheRetention::with_limits(
        &cache_root,
        RetentionLimits {
            max_age: Duration::ZERO,
            max_bytes: u64::MAX,
            max_entries: 10,
            max_nodes: 100,
        },
    );
    assert!(
        retention.touch(&entry),
        "canonical root must be accepted for retention"
    );
    let lease = retention.lease(&entry);
    let now = SystemTime::now() + Duration::from_secs(1);
    assert_eq!(
        retention.prune_at(now).removed,
        0,
        "active entry stays protected"
    );
    assert!(entry.exists());
    drop(lease);
    assert_eq!(
        retention.prune_at(now).removed,
        1,
        "unleased completed entry is pruned"
    );
    assert!(!entry.exists());
}
