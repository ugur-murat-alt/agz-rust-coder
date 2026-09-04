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

const CRATES_IO_CANCELLED: &str = "crates.io request cancelled";

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

impl ReqwestCratesIoClient {
    async fn get_cancellable(
        &self,
        request: &CratesIoRequest,
        request_cancellation: &tokio_util::sync::CancellationToken,
        shutdown_cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<CratesIoResponse, CratesIoError> {
        if !same_https_host(&request.url, "crates.io") {
            return Err(CratesIoError::Transport(
                "crates.io request left the fixed HTTPS host".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(request.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(map_reqwest_error)?;
        let mut builder = client.get(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response =
            send_cancellable(builder, request_cancellation, shutdown_cancellation).await?;
        let status = response.status().as_u16();
        let effective_url = response.url().as_str().to_owned();
        if !same_https_host(&effective_url, "crates.io") {
            return Err(CratesIoError::Transport(
                "crates.io response left the fixed HTTPS host".to_owned(),
            ));
        }
        let body = read_cancellable_body(
            response,
            request.max_body_bytes,
            request_cancellation,
            shutdown_cancellation,
        )
        .await?;
        Ok(CratesIoResponse {
            status,
            body,
            effective_url: Some(effective_url),
        })
    }
}

async fn send_cancellable(
    builder: reqwest::RequestBuilder,
    request_cancellation: &tokio_util::sync::CancellationToken,
    shutdown_cancellation: &tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, CratesIoError> {
    tokio::select! {
        response = builder.send() => response.map_err(map_reqwest_error),
        () = request_cancellation.cancelled() => Err(cancelled_error()),
        () = shutdown_cancellation.cancelled() => Err(cancelled_error()),
    }
}

async fn read_cancellable_body(
    mut response: reqwest::Response,
    max_body_bytes: usize,
    request_cancellation: &tokio_util::sync::CancellationToken,
    shutdown_cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>, CratesIoError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_body_bytes as u64)
    {
        return Err(body_limit_error());
    }
    let mut body = Vec::with_capacity(max_body_bytes.min(8 * 1024));
    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk.map_err(map_reqwest_error)?,
            () = request_cancellation.cancelled() => return Err(cancelled_error()),
            () = shutdown_cancellation.cancelled() => return Err(cancelled_error()),
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if chunk.len() > max_body_bytes.saturating_sub(body.len()) {
            return Err(body_limit_error());
        }
        body.extend_from_slice(&chunk);
    }
}

fn cancelled_error() -> CratesIoError {
    CratesIoError::Transport(CRATES_IO_CANCELLED.to_owned())
}

fn body_limit_error() -> CratesIoError {
    CratesIoError::Transport("crates.io response exceeded the body limit".to_owned())
}

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
    let (name, requested_version, request) = match prepare_lookup(raw_name, requested_version) {
        Ok(request) => request,
        Err(result) => return result,
    };
    let response = match client.get(&request) {
        Ok(response) => response,
        Err(error) => return unavailable_transport_result(name, error),
    };
    lookup_response(name, requested_version, response)
}

/// Cancellation-aware production path used by the MCP server only.
pub(crate) async fn lookup_crate_cancellable(
    raw_name: &str,
    requested_version: Option<&str>,
    request_cancellation: &tokio_util::sync::CancellationToken,
    shutdown_cancellation: &tokio_util::sync::CancellationToken,
) -> CrateLookupResult {
    let (name, requested_version, request) = match prepare_lookup(raw_name, requested_version) {
        Ok(request) => request,
        Err(result) => return result,
    };
    let response = match ReqwestCratesIoClient
        .get_cancellable(&request, request_cancellation, shutdown_cancellation)
        .await
    {
        Ok(response) => response,
        Err(error) => return unavailable_transport_result(name, error),
    };
    lookup_response(name, requested_version, response)
}

fn prepare_lookup(
    raw_name: &str,
    requested_version: Option<&str>,
) -> Result<(String, Option<String>, CratesIoRequest), CrateLookupResult> {
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
        return Err(result);
    }
    let Some(name) = valid_crate_name(&name) else {
        let mut result = CrateLookupResult::new(name, CrateLookupStatus::Invalid);
        result.suggestion =
            Some("Crate name must contain only letters, numbers, '-' or '_'.".to_owned());
        return Err(result);
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
            return Err(result);
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
    Ok((name, requested_version, request))
}

fn unavailable_transport_result(name: String, error: CratesIoError) -> CrateLookupResult {
    let mut result = CrateLookupResult::new(name.clone(), CrateLookupStatus::Unavailable);
    result.suggestion = Some(format!(
        "crates.io is unreachable; crate \"{name}\" could not be verified ({error})."
    ));
    result
}

fn lookup_response(
    name: String,
    requested_version: Option<String>,
    response: CratesIoResponse,
) -> CrateLookupResult {
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

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use super::*;

    fn local_url(listener: &TcpListener) -> String {
        format!(
            "http://{}",
            listener.local_addr().expect("local listener address")
        )
    }

    fn read_request_headers(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buffer).expect("read request headers");
            assert_ne!(read, 0, "client closed before finishing request headers");
            request.extend_from_slice(&buffer[..read]);
            assert!(
                request.len() <= 8 * 1024,
                "request headers exceeded test bound"
            );
        }
    }

    fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) {
        write!(stream, "{:X}\r\n", chunk.len()).expect("write chunk size");
        stream.write_all(chunk).expect("write chunk body");
        stream.write_all(b"\r\n").expect("write chunk terminator");
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_response_header() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let url = local_url(&listener);
        let (release_tx, release_rx) = mpsc::channel();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            read_request_headers(&mut stream);
            accepted_tx.send(()).expect("notify accepted request");
            let _ = release_rx.recv();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        });
        let request_cancellation = tokio_util::sync::CancellationToken::new();
        let shutdown_cancellation = tokio_util::sync::CancellationToken::new();
        let request = request_cancellation.clone();
        let shutdown = shutdown_cancellation.clone();
        let lookup = tokio::spawn(async move {
            send_cancellable(reqwest::Client::new().get(url), &request, &shutdown).await
        });

        tokio::time::timeout(Duration::from_secs(1), accepted_rx)
            .await
            .expect("client must reach the stalled header server")
            .expect("server must notify accepted request");
        request_cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_millis(250), lookup)
            .await
            .expect("header cancellation must not wait for the request timeout")
            .expect("lookup task must not panic")
            .expect_err("cancelled request must not produce a response");
        assert_eq!(error, cancelled_error());
        release_tx.send(()).expect("release server");
        server.join().expect("join local server");
    }

    #[tokio::test]
    async fn shutdown_interrupts_a_stalled_response_header() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let url = local_url(&listener);
        let (release_tx, release_rx) = mpsc::channel();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            read_request_headers(&mut stream);
            accepted_tx.send(()).expect("notify accepted request");
            let _ = release_rx.recv();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        });
        let request_cancellation = tokio_util::sync::CancellationToken::new();
        let shutdown_cancellation = tokio_util::sync::CancellationToken::new();
        let request = request_cancellation.clone();
        let shutdown = shutdown_cancellation.clone();
        let lookup = tokio::spawn(async move {
            send_cancellable(reqwest::Client::new().get(url), &request, &shutdown).await
        });

        tokio::time::timeout(Duration::from_secs(1), accepted_rx)
            .await
            .expect("client must reach the stalled header server")
            .expect("server must notify accepted request");
        shutdown_cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_millis(250), lookup)
            .await
            .expect("shutdown cancellation must not wait for the request timeout")
            .expect("lookup task must not panic")
            .expect_err("shutdown request must not produce a response");
        assert_eq!(error, cancelled_error());
        release_tx.send(()).expect("release server");
        server.join().expect("join local server");
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let url = local_url(&listener);
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            read_request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nConnection: close\r\n\r\npartial",
                )
                .expect("write partial response");
            stream.flush().expect("flush partial response");
            let _ = release_rx.recv();
        });
        let request_cancellation = tokio_util::sync::CancellationToken::new();
        let shutdown_cancellation = tokio_util::sync::CancellationToken::new();
        let response = send_cancellable(
            reqwest::Client::new().get(url),
            &request_cancellation,
            &shutdown_cancellation,
        )
        .await
        .expect("receive response headers");
        let request = request_cancellation.clone();
        let shutdown = shutdown_cancellation.clone();
        let body = tokio::spawn(async move {
            read_cancellable_body(response, CRATES_IO_MAX_BODY_BYTES, &request, &shutdown).await
        });

        tokio::task::yield_now().await;
        request_cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_millis(250), body)
            .await
            .expect("body cancellation must not wait for the request timeout")
            .expect("body task must not panic")
            .expect_err("cancelled body must not complete");
        assert_eq!(error, cancelled_error());
        release_tx.send(()).expect("release server");
        server.join().expect("join local server");
    }

    #[tokio::test]
    async fn chunked_response_stops_before_the_body_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let url = local_url(&listener);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            read_request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .expect("write chunked response headers");
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..(CRATES_IO_MAX_BODY_BYTES / chunk.len()) {
                write_chunk(&mut stream, &chunk);
            }
            write_chunk(&mut stream, b"x");
            stream.write_all(b"0\r\n\r\n").expect("finish chunked body");
            stream.flush().expect("flush chunked response");
        });
        let request_cancellation = tokio_util::sync::CancellationToken::new();
        let shutdown_cancellation = tokio_util::sync::CancellationToken::new();
        let response = send_cancellable(
            reqwest::Client::new().get(url),
            &request_cancellation,
            &shutdown_cancellation,
        )
        .await
        .expect("receive chunked response headers");
        let error = read_cancellable_body(
            response,
            CRATES_IO_MAX_BODY_BYTES,
            &request_cancellation,
            &shutdown_cancellation,
        )
        .await
        .expect_err("response over the body limit must fail");
        assert_eq!(error, body_limit_error());
        server.join().expect("join local server");
    }
}
