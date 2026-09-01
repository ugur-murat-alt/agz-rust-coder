use std::sync::atomic::{AtomicUsize, Ordering};

use agz_rust_coder::tools::{
    CrateLookupStatus, CratesIoClient, CratesIoError, CratesIoRequest, CratesIoResponse,
    lookup_crate, lookup_crate_with_client,
};

struct FakeClient {
    calls: AtomicUsize,
    response: Result<CratesIoResponse, CratesIoError>,
}

impl FakeClient {
    fn new(response: Result<CratesIoResponse, CratesIoError>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            response,
        }
    }
}

impl CratesIoClient for FakeClient {
    fn get(&self, request: &CratesIoRequest) -> Result<CratesIoResponse, CratesIoError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        assert_eq!(request.crate_name, "serde_json");
        assert!(request.url.starts_with("https://crates.io/api/v1/crates/"));
        self.response.clone()
    }
}

fn response(body: serde_json::Value) -> CratesIoResponse {
    CratesIoResponse {
        status: 200,
        body: serde_json::to_vec(&body).expect("serialize fake crates.io response"),
        effective_url: Some("https://crates.io/api/v1/crates/serde_json".to_owned()),
    }
}

#[test]
fn std_modules_never_contact_the_registry() {
    let client = FakeClient::new(Err(CratesIoError::Offline));
    let result = lookup_crate_with_client(&client, "std", None);
    assert_eq!(result.status, CrateLookupStatus::Std);
    assert_eq!(client.calls.load(Ordering::Acquire), 0);
}

#[test]
fn exact_published_version_is_verified_from_bounded_registry_data() {
    let client = FakeClient::new(Ok(response(serde_json::json!({
        "crate": {
            "max_version": "1.0.145",
            "description": "JSON support",
            "downloads": 1234
        },
        "versions": [{"num": "1.0.145"}, {"num": "1.0.144"}]
    }))));
    let result = lookup_crate_with_client(&client, "serde_json", Some("1.0.144"));
    assert_eq!(result.status, CrateLookupStatus::Found);
    assert_eq!(result.requested_version.as_deref(), Some("1.0.144"));
    assert_eq!(result.max_version.as_deref(), Some("1.0.145"));
    assert_eq!(client.calls.load(Ordering::Acquire), 1);
}

#[test]
fn exact_version_with_empty_or_invalid_versions_is_unavailable() {
    for versions in [
        serde_json::json!([]),
        serde_json::json!([{}]),
        serde_json::json!([{"num": ""}]),
        serde_json::json!([{"num": "\u{1}"}]),
    ] {
        let client = FakeClient::new(Ok(response(serde_json::json!({
            "crate": {"max_version": "1.0.145"},
            "versions": versions,
        }))));
        let result = lookup_crate_with_client(&client, "serde_json", Some("1.0.145"));
        assert_eq!(result.status, CrateLookupStatus::Unavailable);
    }

    let client = FakeClient::new(Ok(response(serde_json::json!({
        "crate": {"max_version": "1.0.145"},
    }))));
    let result = lookup_crate_with_client(&client, "serde_json", Some("1.0.145"));
    assert_eq!(result.status, CrateLookupStatus::Unavailable);
}

#[test]
#[ignore = "requires crates.io network access"]
fn real_crates_io_adapter_verifies_an_exact_published_version() {
    let result = lookup_crate("reqwest", Some("0.13.2"));
    assert_eq!(result.status, CrateLookupStatus::Found, "{result:#?}");
    assert_eq!(result.requested_version.as_deref(), Some("0.13.2"));
}

#[test]
fn missing_version_and_cross_host_redirect_fail_open_as_unverified() {
    let mismatch = FakeClient::new(Ok(response(serde_json::json!({
        "crate": {"max_version": "1.0.145"},
        "versions": [{"num": "1.0.145"}]
    }))));
    let result = lookup_crate_with_client(&mismatch, "serde_json", Some("9.9.9"));
    assert_eq!(result.status, CrateLookupStatus::VersionMismatch);

    let inconsistent_max = FakeClient::new(Ok(response(serde_json::json!({
        "crate": {"max_version": "1.0.145"},
        "versions": [{"num": "1.0.144"}]
    }))));
    let result = lookup_crate_with_client(&inconsistent_max, "serde_json", Some("1.0.145"));
    assert_eq!(result.status, CrateLookupStatus::VersionMismatch);

    let redirect = FakeClient::new(Ok(CratesIoResponse {
        status: 200,
        body: b"{}".to_vec(),
        effective_url: Some("https://example.invalid/serde_json".to_owned()),
    }));
    let result = lookup_crate_with_client(&redirect, "serde_json", None);
    assert_eq!(result.status, CrateLookupStatus::Unavailable);
}
