//! Testable, fail-open crates.io lookup domain.

use std::{fmt, io::Read, time::Duration};

use serde::Deserialize;

use crate::knowledge::std_modules;

/// Fixed crates.io endpoint used by the lookup domain.
pub const CRATES_IO_API: &str = "https://crates.io/api/v1/crates";
/// Maximum response body accepted from the injected HTTP client.
pub const CRATES_IO_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Default request timeout conveyed to the HTTP client.
pub const CRATES_IO_TIMEOUT: Duration = Duration::from_secs(8);
/// Stable user-agent value for an eventual HTTP adapter.
pub const CRATES_IO_USER_AGENT: &str = "agz-rust-coder crate_lookup (advisory)";

/// Typed lookup outcome. No outcome claims that a dependency was added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrateLookupStatus {
    Invalid,
    Std,
    Found,
    VersionMismatch,
    NotFound,
    Unavailable,
}

/// Protocol-shaped input kept separate from the HTTP and result traits.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrateLookupInput {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Validation failures are returned before any external request is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrateLookupValidationError {
    EmptyName,
    InvalidName,
    EmptyVersion,
    InvalidVersion,
}

impl fmt::Display for CrateLookupValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyName => "name cannot be empty",
            Self::InvalidName => "name contains unsupported characters",
            Self::EmptyVersion => "version cannot be empty",
            Self::InvalidVersion => "version contains control characters or is too long",
        })
    }
}

impl std::error::Error for CrateLookupValidationError {}

/// Validates a request without contacting crates.io.
pub fn validate_crate_lookup_input(
    input: &CrateLookupInput,
) -> Result<(), CrateLookupValidationError> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(CrateLookupValidationError::EmptyName);
    }
    if valid_crate_name(&name.to_ascii_lowercase()).is_none() {
        return Err(CrateLookupValidationError::InvalidName);
    }
    if let Some(version) = input.version.as_deref() {
        if version.trim().is_empty() {
            return Err(CrateLookupValidationError::EmptyVersion);
        }
        if !valid_version(version.trim()) {
            return Err(CrateLookupValidationError::InvalidVersion);
        }
    }
    Ok(())
}

impl CrateLookupStatus {
    /// Stable wire spelling used by the server response adapter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Std => "std",
            Self::Found => "found",
            Self::VersionMismatch => "version-mismatch",
            Self::NotFound => "not-found",
            Self::Unavailable => "unreachable",
        }
    }
}

/// A request sent to an injected crates.io client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CratesIoRequest {
    pub url: String,
    pub crate_name: String,
    pub timeout: Duration,
    pub max_body_bytes: usize,
    pub headers: Vec<(String, String)>,
}

/// A response returned by an injected crates.io client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CratesIoResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// If the client followed redirects, this must remain on `crates.io`.
    pub effective_url: Option<String>,
}

/// Failures are deliberately separate from invalid/not-found data outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CratesIoError {
    Offline,
    Timeout,
    Transport(String),
}

impl fmt::Display for CratesIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline => formatter.write_str("crates.io is offline"),
            Self::Timeout => formatter.write_str("crates.io request timed out"),
            Self::Transport(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CratesIoError {}

/// Abstraction around HTTP so lookup behavior can be tested without a network.
pub trait CratesIoClient: Send + Sync {
    fn get(&self, request: &CratesIoRequest) -> Result<CratesIoResponse, CratesIoError>;
}

/// Default adapter until the host supplies its bounded HTTPS client.
#[derive(Debug, Default, Clone, Copy)]
pub struct OfflineCratesIoClient;

impl CratesIoClient for OfflineCratesIoClient {
    fn get(&self, _request: &CratesIoRequest) -> Result<CratesIoResponse, CratesIoError> {
        Err(CratesIoError::Offline)
    }
}

/// Production crates.io adapter. Redirects are rejected and the response body
/// is streamed through the request's byte limit.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReqwestCratesIoClient;

impl CratesIoClient for ReqwestCratesIoClient {
    fn get(&self, request: &CratesIoRequest) -> Result<CratesIoResponse, CratesIoError> {
        if !same_https_host(&request.url, "crates.io") {
            return Err(CratesIoError::Transport(
                "crates.io request left the fixed HTTPS host".to_owned(),
            ));
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(request.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(map_reqwest_error)?;
        let mut builder = client.get(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let mut response = builder.send().map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        let effective_url = response.url().as_str().to_owned();
        if !same_https_host(&effective_url, "crates.io") {
            return Err(CratesIoError::Transport(
                "crates.io response left the fixed HTTPS host".to_owned(),
            ));
        }
        let mut body = Vec::new();
        response
            .by_ref()
            .take((request.max_body_bytes as u64).saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|error| CratesIoError::Transport(error.to_string()))?;
        if body.len() > request.max_body_bytes {
            return Err(CratesIoError::Transport(
                "crates.io response exceeded the body limit".to_owned(),
            ));
        }
        Ok(CratesIoResponse {
            status,
            body,
            effective_url: Some(effective_url),
        })
    }
}

fn map_reqwest_error(error: reqwest::Error) -> CratesIoError {
    if error.is_timeout() {
        CratesIoError::Timeout
    } else {
        CratesIoError::Transport(error.to_string())
    }
}

/// Result data for one crate lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateLookupResult {
    pub name: String,
    pub status: CrateLookupStatus,
    /// Alias retained for callers that use the legacy result vocabulary.
    pub kind: CrateLookupStatus,
    pub crate_name: Option<String>,
    pub max_version: Option<String>,
    pub requested_version: Option<String>,
    pub description: Option<String>,
    pub downloads: Option<u64>,
    pub std_path: Option<String>,
    pub suggestion: Option<String>,
}

impl CrateLookupResult {
    fn new(name: String, status: CrateLookupStatus) -> Self {
        Self {
            name,
            status,
            kind: status,
            crate_name: None,
            max_version: None,
            requested_version: None,
            description: None,
            downloads: None,
            std_path: None,
            suggestion: None,
        }
    }
}

/// Looks up a crate using the bounded production crates.io adapter.
pub fn lookup_crate(raw_name: &str, requested_version: Option<&str>) -> CrateLookupResult {
    lookup_crate_with_client(&ReqwestCratesIoClient, raw_name, requested_version)
}

/// Executes a validated lookup with the bounded production adapter.
pub fn execute_crate_lookup(input: &CrateLookupInput) -> String {
    if let Err(error) = validate_crate_lookup_input(input) {
        return format!("rust.crate_lookup: invalid input ({error}).");
    }
    format_lookup_result(&lookup_crate(
        &input.name,
        input.version.as_deref().map(str::trim),
    ))
}

/// Looks up a crate against an injected, bounded crates.io client.
pub fn lookup_crate_with_client<C: CratesIoClient + ?Sized>(
    client: &C,
    raw_name: &str,
    requested_version: Option<&str>,
) -> CrateLookupResult {
    let name = raw_name.trim().to_ascii_lowercase();
    if let Some(entry) = std_modules::std_module_lookup(&name) {
        let mut result = CrateLookupResult::new(name.clone(), CrateLookupStatus::Std);
        result.std_path = Some(entry.module.to_owned());
        result.suggestion = Some(format!(
            "{name} is {} ({}). {} Do not add it as an external crate.",
            entry.module,
            entry.members,
            std_modules::STD_POLICY
        ));
        return result;
    }
    let Some(name) = valid_crate_name(&name) else {
        let mut result = CrateLookupResult::new(name, CrateLookupStatus::Invalid);
        result.suggestion =
            Some("Crate name must contain only letters, numbers, '-' or '_'.".to_owned());
        return result;
    };

    let requested_version = match requested_version
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) if valid_version(value) => Some(value.to_owned()),
        Some(_) => {
            let mut result = CrateLookupResult::new(name, CrateLookupStatus::Invalid);
            result.suggestion = Some(
                "Version must be a non-empty, bounded value without control characters.".to_owned(),
            );
            return result;
        }
        None => None,
    };

    let request = CratesIoRequest {
        url: format!("{CRATES_IO_API}/{}", encode_segment(&name)),
        crate_name: name.clone(),
        timeout: CRATES_IO_TIMEOUT,
        max_body_bytes: CRATES_IO_MAX_BODY_BYTES,
        headers: vec![("user-agent".to_owned(), CRATES_IO_USER_AGENT.to_owned())],
    };
    let response = match client.get(&request) {
        Ok(response) => response,
        Err(error) => {
            let mut result = CrateLookupResult::new(name.clone(), CrateLookupStatus::Unavailable);
            result.suggestion = Some(format!(
                "crates.io is unreachable; crate \"{name}\" could not be verified ({error})."
            ));
            return result;
        }
    };
    if response.body.len() > CRATES_IO_MAX_BODY_BYTES
        || response
            .effective_url
            .as_deref()
            .is_some_and(|url| !same_https_host(url, "crates.io"))
    {
        let mut result = CrateLookupResult::new(name, CrateLookupStatus::Unavailable);
        result.suggestion = Some(
            "The crates.io response exceeded a bound or left the fixed HTTPS host.".to_owned(),
        );
        return result;
    }
    if response.status == 404 {
        return not_found_result(name);
    }
    if !(200..300).contains(&response.status) {
        let mut result = CrateLookupResult::new(name, CrateLookupStatus::Unavailable);
        result.suggestion = Some(format!(
            "crates.io returned HTTP {}; treat the crate as unverified.",
            response.status
        ));
        return result;
    }

    let payload: CratesIoPayload = match serde_json::from_slice(&response.body) {
        Ok(payload) => payload,
        Err(error) => {
            let mut result = CrateLookupResult::new(name, CrateLookupStatus::Unavailable);
            result.suggestion = Some(format!(
                "crates.io returned invalid crate data; treat the crate as unverified ({error})."
            ));
            return result;
        }
    };
    let Some(crate_info) = payload.crate_data else {
        let mut result = CrateLookupResult::new(name, CrateLookupStatus::Unavailable);
        result.suggestion = Some(
            "crates.io response missing crate data; treat the crate as unverified.".to_owned(),
        );
        return result;
    };
    let max_version = crate_info
        .max_version
        .or(crate_info.newest_version)
        .unwrap_or_else(|| "unknown".to_owned());
    let published_versions = if requested_version.is_some() {
        let Some(versions) = payload.versions else {
            return unavailable_version_result(name);
        };
        if versions.is_empty() {
            return unavailable_version_result(name);
        }
        let mut published_versions = std::collections::BTreeSet::new();
        for version in versions {
            let Some(number) = version.num else {
                return unavailable_version_result(name);
            };
            if !valid_version(&number) {
                return unavailable_version_result(name);
            }
            published_versions.insert(number);
        }
        published_versions
    } else {
        std::collections::BTreeSet::new()
    };
    if let Some(requested) = requested_version.as_deref()
        && !published_versions.contains(requested)
    {
        let mut result = CrateLookupResult::new(name.clone(), CrateLookupStatus::VersionMismatch);
        result.crate_name = Some(name.clone());
        result.max_version = Some(max_version.clone());
        result.requested_version = Some(requested.to_owned());
        result.description = crate_info.description;
        result.downloads = crate_info.downloads;
        result.suggestion = Some(format!(
            "Crate \"{name}\" exists but version {requested} is not published; newest is {max_version}."
        ));
        return result;
    }

    let mut result = CrateLookupResult::new(name.clone(), CrateLookupStatus::Found);
    result.crate_name = Some(name.clone());
    result.max_version = Some(max_version.clone());
    result.requested_version = requested_version;
    result.description = crate_info.description;
    result.downloads = crate_info.downloads;
    result.suggestion = Some(match result.requested_version.as_deref() {
        Some(requested) => {
            format!("Crate \"{name}\" version {requested} is published (newest: {max_version}).")
        }
        None => format!(
            "Crate \"{name}\" exists; newest version {max_version}. Pin the version in Cargo.toml."
        ),
    });
    result
}

/// Formats lookup data without treating external text as instructions.
pub fn format_lookup_result(result: &CrateLookupResult) -> String {
    let mut lines = Vec::new();
    match result.status {
        CrateLookupStatus::Invalid => lines.push("STATUS: invalid crate lookup".to_owned()),
        CrateLookupStatus::Std => lines.push(format!(
            "STATUS: std module - {}",
            result.std_path.as_deref().unwrap_or("unknown")
        )),
        CrateLookupStatus::Found => {
            lines.push("STATUS: verified on crates.io".to_owned());
            if let Some(crate_name) = &result.crate_name {
                lines.push(format!("crate: {crate_name}"));
            }
            if let Some(version) = &result.max_version {
                lines.push(format!("newest version: {version}"));
            }
            if let Some(version) = &result.requested_version {
                lines.push(format!("requested version: {version}"));
            }
            if let Some(description) = &result.description {
                lines.push(format!(
                    "description: {}",
                    bounded_external_text(description, 200)
                ));
            }
            if let Some(downloads) = result.downloads {
                lines.push(format!("downloads: {downloads}"));
            }
        }
        CrateLookupStatus::VersionMismatch => {
            lines.push("STATUS: crate exists but requested version is NOT published".to_owned());
            lines.push(format!(
                "crate: {}, newest: {}, requested: {}",
                result.crate_name.as_deref().unwrap_or("unknown"),
                result.max_version.as_deref().unwrap_or("unknown"),
                result.requested_version.as_deref().unwrap_or("unknown")
            ));
        }
        CrateLookupStatus::NotFound => {
            lines.push("STATUS: crate not found on crates.io".to_owned())
        }
        CrateLookupStatus::Unavailable => {
            lines.push("STATUS: unverified (crates.io unreachable)".to_owned())
        }
    }
    if let Some(suggestion) = &result.suggestion {
        lines.push(format!(
            "advice: {}",
            bounded_external_text(suggestion, 1_000)
        ));
    }
    lines.join("\n").chars().take(6_000).collect()
}

fn not_found_result(name: String) -> CrateLookupResult {
    let mut result = CrateLookupResult::new(name.clone(), CrateLookupStatus::NotFound);
    let underscore_name = name.replace('-', "_");
    result.suggestion = Some(if underscore_name == name {
        format!(
            "No crate named \"{name}\" exists on crates.io. Verify the exact name before adding it to Cargo.toml; this may be a hallucinated crate."
        )
    } else {
        format!(
            "No crate named \"{name}\" exists. Did you mean \"{underscore_name}\"? crates.io names use underscores. Verify the exact name before adding it to Cargo.toml."
        )
    });
    result
}

fn unavailable_version_result(name: String) -> CrateLookupResult {
    let mut result = CrateLookupResult::new(name, CrateLookupStatus::Unavailable);
    result.suggestion = Some(
        "crates.io response did not contain a usable versions list; exact version could not be verified."
            .to_owned(),
    );
    result
}

fn valid_crate_name(name: &str) -> Option<String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(name.to_owned())
}

fn valid_version(version: &str) -> bool {
    !version.is_empty() && version.len() <= 128 && !version.chars().any(char::is_control)
}

fn encode_segment(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~') {
                vec![byte as char]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
}

fn same_https_host(url: &str, host: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains(['@', ':', '\\'])
        && !authority.chars().any(char::is_control)
        && authority.eq_ignore_ascii_case(host)
}

fn bounded_external_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
        .take(limit)
        .collect()
}

#[derive(Debug, serde::Deserialize)]
struct CratesIoPayload {
    #[serde(rename = "crate")]
    crate_data: Option<CrateInfo>,
    versions: Option<Vec<CrateVersion>>,
}

#[derive(Debug, serde::Deserialize)]
struct CrateInfo {
    newest_version: Option<String>,
    max_version: Option<String>,
    description: Option<String>,
    downloads: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct CrateVersion {
    num: Option<String>,
}
