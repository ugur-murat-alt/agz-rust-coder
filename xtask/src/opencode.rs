use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command as TokioCommand;

use crate::child_process;

const EXPECTED_OPENCODE_VERSION: &str = "opencode2 v0.0.0-beta-18743";
const FAKE_PROVIDER_MODEL: &str = "stage7-fake-model";
const MCP_TOOL_NAMES: [&str; 12] = [
    "check",
    "audit",
    "crate_lookup",
    "docs",
    "symbol",
    "references",
    "definition",
    "symbols",
    "implementations",
    "hierarchy",
    "rename",
    "refactor",
];
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize)]
pub struct OpencodeEvidence {
    pub status: &'static str,
    pub direct_config: ConfigEvidence,
    pub grouped_config: ConfigEvidence,
    pub direct_tool_names: Vec<String>,
    pub grouped_surface: Vec<&'static str>,
    pub opencode_version: VersionEvidence,
    pub direct_opencode: OpenCodeRunEvidence,
    pub grouped_opencode: OpenCodeRunEvidence,
    pub provider_free_boundary: ProviderFreeBoundary,
}

#[derive(Debug, Serialize)]
pub struct ConfigEvidence {
    pub schema: &'static str,
    pub server_key: &'static str,
    pub server_type: &'static str,
    pub codemode: bool,
    pub command_placeholder: &'static str,
    pub cwd: &'static str,
    pub startup_timeout_ms: u64,
    pub catalog_timeout_ms: u64,
    pub execution_timeout_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct VersionEvidence {
    pub expected: &'static str,
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct OpenCodeToolCallEvidence {
    pub tool_name: String,
    pub status: String,
    pub is_error: bool,
    pub structured_text_equivalent: bool,
}

#[derive(Debug, Serialize)]
pub struct OpenCodeRunEvidence {
    pub codemode: bool,
    pub connected: bool,
    pub visible_tools: Vec<String>,
    pub code_mode_namespaces: Vec<String>,
    pub tool_calls: u32,
    pub tool_call: Option<OpenCodeToolCallEvidence>,
    pub provider_requests: u32,
    pub external_network_requests: u32,
    pub process_exit_code: i32,
}

#[derive(Debug, Serialize)]
pub struct ProviderFreeBoundary {
    pub model_request_attempted: bool,
    pub local_fake_model: bool,
    pub opencode_tool_calls: u32,
    pub provider_requests: u32,
    pub external_network_requests: u32,
    pub paid: bool,
    pub status: &'static str,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceMode {
    Direct,
    Grouped,
}

#[derive(Debug, Default)]
struct FakeProviderState {
    provider_requests: u32,
    request_debug: Vec<String>,
    visible_tools: Vec<String>,
    code_mode_namespace_seen: bool,
    tool_calls: u32,
    tool_call_name: Option<String>,
    tool_status: Option<String>,
    tool_is_error: bool,
    tool_output: Option<Value>,
    tool_representations_equivalent: Option<bool>,
    protocol_error: Option<String>,
}

#[derive(Clone, Debug)]
struct ToolResultObservation {
    status: String,
    is_error: bool,
    structured_text_equivalent: bool,
    output: Value,
}

struct FakeProvider {
    address: String,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<FakeProviderState>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeProvider {
    fn start(mode: SurfaceMode) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("bind local fake provider")?;
        listener
            .set_nonblocking(true)
            .context("configure local fake provider")?;
        let address = listener
            .local_addr()
            .context("read local fake provider address")?
            .to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(FakeProviderState::default()));
        let thread_stop = Arc::clone(&stop);
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("stage7-fake-provider".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if !peer.ip().is_loopback() {
                                record_provider_error(
                                    &thread_state,
                                    "fake provider received a non-loopback request",
                                );
                                continue;
                            }
                            handle_provider_request(stream, mode, &thread_state);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => {
                            record_provider_error(
                                &thread_state,
                                &format!("fake provider listener failed: {error}"),
                            );
                            break;
                        }
                    }
                }
            })
            .context("start local fake provider")?;
        Ok(Self {
            address,
            stop,
            state,
            thread: Some(thread),
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn snapshot(&self) -> FakeProviderState {
        self.state.lock().map_or_else(
            |_| FakeProviderState {
                protocol_error: Some("fake provider state lock poisoned".to_owned()),
                ..FakeProviderState::default()
            },
            |state| FakeProviderState {
                provider_requests: state.provider_requests,
                request_debug: state.request_debug.clone(),
                visible_tools: state.visible_tools.clone(),
                code_mode_namespace_seen: state.code_mode_namespace_seen,
                tool_calls: state.tool_calls,
                tool_call_name: state.tool_call_name.clone(),
                tool_status: state.tool_status.clone(),
                tool_is_error: state.tool_is_error,
                tool_output: state.tool_output.clone(),
                tool_representations_equivalent: state.tool_representations_equivalent,
                protocol_error: state.protocol_error.clone(),
            },
        )
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(&self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub async fn run(root: &Path) -> Result<OpencodeEvidence> {
    let direct = read_config(include_str!(
        "../../tests/fixtures/stage7/opencode/direct.jsonc"
    ))?;
    let grouped = read_config(include_str!(
        "../../tests/fixtures/stage7/opencode/grouped.jsonc"
    ))?;
    let direct_config = validate_config(&direct, false)?;
    let grouped_config = validate_config(&grouped, true)?;
    let opencode_version = probe_opencode_version()?;
    require_verified_opencode(&opencode_version)?;

    let grouped_opencode = run_opencode_trial(root, grouped, SurfaceMode::Grouped).await?;
    let direct_opencode = run_opencode_trial(root, direct, SurfaceMode::Direct).await?;
    if !(1..=2).contains(&direct_opencode.tool_calls)
        || direct_opencode
            .tool_call
            .as_ref()
            .is_none_or(|call| call.tool_name != "rust_check" || call.status != "FAST_PASS")
    {
        bail!("OpenCode direct mode did not complete the rust_check call");
    }
    if grouped_opencode.tool_calls != 2
        || grouped_opencode
            .tool_call
            .as_ref()
            .is_none_or(|call| call.tool_name != "execute" || call.status != "CLEAN")
    {
        bail!("OpenCode Code Mode did not complete the grouped Rust check call");
    }

    let direct_tool_names = direct_opencode.visible_tools.clone();
    let grouped_namespaces = grouped_opencode.code_mode_namespaces.clone();
    let opencode_tool_calls = direct_opencode.tool_calls + grouped_opencode.tool_calls;
    let provider_requests = direct_opencode.provider_requests + grouped_opencode.provider_requests;
    Ok(OpencodeEvidence {
        status: "PASS",
        direct_config,
        grouped_config,
        direct_tool_names,
        grouped_surface: grouped_namespaces
            .iter()
            .filter_map(|name| (name == "rust").then_some("rust"))
            .collect(),
        opencode_version,
        direct_opencode,
        grouped_opencode,
        provider_free_boundary: ProviderFreeBoundary {
            model_request_attempted: true,
            local_fake_model: true,
            opencode_tool_calls,
            provider_requests,
            external_network_requests: 0,
            paid: false,
            status: "verified",
            reason: "all provider traffic was handled by the loopback fake model; no paid or external endpoint was configured",
        },
    })
}

#[allow(clippy::too_many_lines)]
async fn run_opencode_trial(
    root: &Path,
    config: Value,
    mode: SurfaceMode,
) -> Result<OpenCodeRunEvidence> {
    let trial = IsolatedOpenCodeTrial::new(root, config, mode)?;
    let mut command = TokioCommand::new(&trial.opencode_binary);
    command
        .current_dir(&trial.workspace)
        .args([
            "run",
            "--standalone",
            "--log-level",
            "error",
            "--auto",
            "--agent",
            "build",
            "--format",
            "json",
            "--model",
            "stage7-local/fake",
        ])
        .arg(match mode {
            SurfaceMode::Direct => "Call rust_check exactly once with no arguments, then finish.",
            SurfaceMode::Grouped => {
                "Use the Rust Code Mode namespace to call audit exactly once, then finish."
            }
        });
    trial.configure_command(&mut command)?;
    let output = child_process::output(command, None, Duration::from_secs(90), 16 * 1024 * 1024)
        .await
        .context("run OpenCode smoke")?;
    let state = trial.provider.snapshot();
    if let Some(error) = state.protocol_error {
        bail!("OpenCode fake provider protocol error: {error}");
    }
    if !output.status.success() {
        bail!(
            "OpenCode smoke exited with {}: {}",
            output.status,
            bounded_text(&output.stderr)
        );
    }
    let completed_expected_exchange = match mode {
        SurfaceMode::Direct => (1..=2).contains(&state.tool_calls),
        SurfaceMode::Grouped => state.tool_calls == 2,
    };
    if state.provider_requests < 2 || !completed_expected_exchange {
        bail!(
            "OpenCode smoke did not complete the expected model/tool exchange (provider_requests={}, tool_calls={}, visible_tools={:?}, request_debug={:?}, stdout={}, stderr={})",
            state.provider_requests,
            state.tool_calls,
            state.visible_tools,
            state.request_debug,
            diagnostic_text(&output.stdout),
            diagnostic_text(&output.stderr)
        );
    }
    let expected_tool = match mode {
        SurfaceMode::Direct => "rust_check",
        SurfaceMode::Grouped => "execute",
    };
    if state.tool_call_name.as_deref() != Some(expected_tool) {
        bail!(
            "OpenCode offered or called the wrong tool: expected {expected_tool}, got {:?}",
            state.tool_call_name
        );
    }
    let provider_tool_status = state.tool_status.clone().with_context(|| {
        format!(
            "OpenCode did not return a Rust MCP tool result to the provider (request_debug={:?}, stdout={})",
            state.request_debug,
            diagnostic_text(&output.stdout)
        )
    })?;
    let event_result =
        find_opencode_tool_event(&output.stdout, expected_tool).with_context(|| {
            format!(
                "OpenCode JSON event stream omitted the completed {expected_tool} result: {}",
                diagnostic_text(&output.stdout)
            )
        })?;
    let provider_tool_output = state
        .tool_output
        .as_ref()
        .context("OpenCode provider request omitted the tool output JSON")?;
    validate_tool_observations(
        &provider_tool_status,
        state.tool_is_error,
        provider_tool_output,
        state.tool_representations_equivalent,
        &event_result,
    )?;
    let visible_tools = state.visible_tools.clone();
    let code_mode_namespaces = if state.code_mode_namespace_seen {
        vec!["rust".to_owned()]
    } else {
        Vec::new()
    };
    if mode == SurfaceMode::Direct {
        for tool in MCP_TOOL_NAMES {
            if !visible_tools
                .iter()
                .any(|name| name == &format!("rust_{tool}"))
            {
                bail!("OpenCode direct surface is missing rust_{tool}");
            }
        }
    } else if !visible_tools.iter().any(|name| name == "execute") || !state.code_mode_namespace_seen
    {
        bail!("OpenCode grouped surface did not expose the Rust Code Mode namespace");
    }
    Ok(OpenCodeRunEvidence {
        codemode: mode == SurfaceMode::Grouped,
        connected: true,
        visible_tools,
        code_mode_namespaces,
        tool_calls: state.tool_calls,
        tool_call: Some(OpenCodeToolCallEvidence {
            tool_name: state.tool_call_name.unwrap_or_default(),
            status: event_result.status,
            is_error: event_result.is_error,
            structured_text_equivalent: event_result.structured_text_equivalent,
        }),
        provider_requests: state.provider_requests,
        external_network_requests: 0,
        process_exit_code: output.status.code().unwrap_or(-1),
    })
}

struct IsolatedOpenCodeTrial {
    root: PathBuf,
    workspace: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    cache_home: PathBuf,
    state_home: PathBuf,
    tmp_home: PathBuf,
    provider: FakeProvider,
    opencode_binary: String,
}

impl IsolatedOpenCodeTrial {
    fn new(root: &Path, mut config: Value, mode: SurfaceMode) -> Result<Self> {
        let fixture_workspace = root.join("tests/fixtures/stage7/opencode/workspace");
        let temporary = unique_directory("stage7-opencode")?;
        let workspace = temporary.join("workspace");
        let config_home = temporary.join("config");
        let data_home = temporary.join("data");
        let cache_home = temporary.join("cache");
        let state_home = temporary.join("state");
        let tmp_home = temporary.join("tmp");
        fs::create_dir(&workspace).context("create OpenCode smoke workspace")?;
        for directory in [
            &config_home,
            &data_home,
            &cache_home,
            &state_home,
            &tmp_home,
        ] {
            fs::create_dir(directory).context("create isolated OpenCode directory")?;
        }
        copy_workspace(&fixture_workspace, &workspace)?;
        let provider = FakeProvider::start(mode)?;
        let server_command = server_command(root);
        let server = config
            .pointer_mut("/mcp/servers/rust")
            .and_then(Value::as_object_mut)
            .context("OpenCode config has no rust server")?;
        server.insert("command".to_owned(), Value::Array(server_command));
        let providers = json!({
            "stage7-local": {
                "name": "Stage 7 loopback fake provider",
                "package": "@opencode-ai/ai/providers/openai-compatible",
                "settings": {"baseURL": provider.base_url()},
                "models": {
                    "fake": {
                        "modelID": FAKE_PROVIDER_MODEL,
                        "name": "Stage 7 fake model",
                        "capabilities": {
                            "tools": true,
                            "input": ["text"],
                            "output": ["text"]
                        },
                        "limit": {"context": 32768, "output": 4096}
                    }
                }
            }
        });
        config["model"] = Value::String("stage7-local/fake".to_owned());
        config["providers"] = providers;
        config["permissions"] = json!([
            {"action": "rust_*", "resource": "*", "effect": "allow"},
            {"action": "execute", "resource": "*", "effect": "allow"}
        ]);
        let config_path = workspace.join("opencode.json");
        let mut config_bytes =
            serde_json::to_vec_pretty(&config).context("serialize OpenCode config")?;
        config_bytes.push(b'\n');
        fs::write(&config_path, config_bytes).context("write isolated OpenCode config")?;
        Ok(Self {
            root: temporary,
            workspace,
            config_home,
            data_home,
            cache_home,
            state_home,
            tmp_home,
            provider,
            opencode_binary: opencode_binary(),
        })
    }

    fn configure_command(&self, command: &mut TokioCommand) -> Result<()> {
        let current_path = std::env::var_os("PATH");
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        let path = cargo_path(current_path.as_deref(), home.as_deref())?;
        command
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CACHE_HOME", &self.cache_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("TMPDIR", &self.tmp_home)
            .env("TMP", &self.tmp_home)
            .env("TEMP", &self.tmp_home)
            .env("CARGO_HOME", self.root.join("cargo-home"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_TERM_COLOR", "never")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9");
        // Retain the selected toolchain, not user credentials or configuration.
        if let Some(toolchain) = std::env::var_os("RUSTUP_TOOLCHAIN") {
            command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        #[cfg(windows)]
        {
            for name in [
                "SystemRoot",
                "WINDIR",
                "ComSpec",
                "PATHEXT",
                "LIB",
                "LIBPATH",
                "INCLUDE",
                "VCToolsInstallDir",
                "WindowsSdkDir",
                "WindowsSDKVersion",
            ] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command
                .env("USERPROFILE", &self.root)
                .env("APPDATA", &self.config_home)
                .env("LOCALAPPDATA", &self.data_home);
        }
        if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME") {
            command.env("RUSTUP_HOME", rustup_home);
        } else if let Some(home) = home {
            command.env("RUSTUP_HOME", PathBuf::from(home).join(".rustup"));
        }
        Ok(())
    }
}

impl Drop for IsolatedOpenCodeTrial {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[allow(clippy::too_many_lines)]
fn handle_provider_request(
    mut stream: TcpStream,
    mode: SurfaceMode,
    state: &Arc<Mutex<FakeProviderState>>,
) {
    let request = match read_http_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            record_provider_error(
                state,
                &format!("read fake provider request failed: {error}"),
            );
            return;
        }
    };
    let body: Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(error) => {
            record_provider_error(
                state,
                &format!("parse fake provider request failed: {error}"),
            );
            return;
        }
    };
    let request_number = match state.lock() {
        Ok(mut state) => {
            state.provider_requests = state.provider_requests.saturating_add(1);
            state.provider_requests
        }
        Err(_) => return,
    };
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Ok(mut state) = state.lock() {
        state.request_debug.push(format!(
            "#{request_number} tools={} messages={}",
            body.get("tools")
                .map_or_else(|| "<none>".to_owned(), Value::to_string),
            messages
                .iter()
                .map(|message| {
                    format!(
                        "{}:{}",
                        message.get("role").and_then(Value::as_str).unwrap_or("?"),
                        bounded_text(message_text(message).as_bytes())
                    )
                })
                .collect::<Vec<_>>()
                .join("|")
        ));
        for message in messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        {
            state.request_debug.push(format!(
                "#{request_number} raw_tool={}",
                diagnostic_text(message.to_string().as_bytes())
            ));
        }
    }
    let visible_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .or_else(|| tool.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let code_mode_namespace_seen = messages.iter().any(|message| {
        let text = message_text(message);
        text.contains("tools[\"rust\"]")
            || text.contains("tools.rust")
            || (text.contains("rust") && text.contains("Code Mode"))
    });
    if let Ok(mut state) = state.lock() {
        for tool in visible_tools {
            if !state.visible_tools.contains(&tool) {
                state.visible_tools.push(tool);
            }
        }
        state.code_mode_namespace_seen |= code_mode_namespace_seen;
    }
    let expected_tool = match mode {
        SurfaceMode::Direct => "rust_check",
        SurfaceMode::Grouped => "execute",
    };
    let offered = state
        .lock()
        .ok()
        .is_some_and(|state| state.visible_tools.iter().any(|name| name == expected_tool));
    let tool_call_sent = state.lock().ok().is_some_and(|state| state.tool_calls != 0);
    if mode == SurfaceMode::Direct && request_number >= 2 && !tool_call_sent && !offered {
        if let Ok(mut state) = state.lock() {
            state.tool_calls = 1;
            state.tool_call_name = Some("read".to_owned());
        }
        respond_stream(
            &mut stream,
            stream_tool_call("read", r#"{"path":"Cargo.toml","limit":1}"#),
        );
        return;
    }
    if mode == SurfaceMode::Direct && offered {
        let bootstrap_complete = state.lock().ok().is_some_and(|state| {
            state.tool_calls == 1 && state.tool_call_name.as_deref() == Some("read")
        });
        if !tool_call_sent || bootstrap_complete {
            if let Ok(mut state) = state.lock() {
                state.tool_calls = if bootstrap_complete { 2 } else { 1 };
                state.tool_call_name = Some(expected_tool.to_owned());
            }
            respond_stream(&mut stream, stream_tool_call("rust_check", "{}"));
            return;
        }
    }
    if mode == SurfaceMode::Grouped && offered && !tool_call_sent {
        if let Ok(mut state) = state.lock() {
            state.tool_calls = 1;
            state.tool_call_name = Some(expected_tool.to_owned());
        }
        respond_stream(
            &mut stream,
            stream_tool_call(
                "execute",
                r#"{"code":"return await search({ namespace: \"rust\", query: \"audit Rust source scan\", limit: 100 });"}"#,
            ),
        );
        return;
    }
    if mode == SurfaceMode::Grouped && tool_call_sent {
        let calls = state.lock().ok().map_or(0, |state| state.tool_calls);
        let search_finished = messages
            .iter()
            .any(|message| message.get("role").and_then(Value::as_str) == Some("tool"));
        if calls == 1 && search_finished {
            if let Ok(mut state) = state.lock() {
                state.tool_calls = 2;
            }
            respond_stream(
                &mut stream,
                stream_tool_call(
                    "execute",
                    r#"{"code":"return await tools.rust.audit({});"}"#,
                ),
            );
            return;
        }
    }
    if tool_call_sent {
        let tool_result = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .find_map(|message| {
                let (status, is_error, _) = find_status_value(message)?;
                let output = find_tool_output_value(message)?;
                let equivalent = find_tool_representation_equivalence(message);
                Some((status, is_error, output, equivalent))
            });
        if let Some((status, is_error, output, equivalent)) = tool_result {
            if let Ok(mut state) = state.lock() {
                state.tool_status = Some(status);
                state.tool_is_error = is_error;
                state.tool_output = Some(output);
                state.tool_representations_equivalent = equivalent;
            }
            respond_stream(&mut stream, stream_text("STAGE7_TOOL_CALL_COMPLETE"));
            return;
        }
    }
    if request_number >= 8 {
        let visible = state
            .lock()
            .ok()
            .map(|state| state.visible_tools.join(", "))
            .unwrap_or_default();
        record_provider_error(
            state,
            &format!("OpenCode did not offer {expected_tool}; visible tools: {visible}"),
        );
        let response = json!({"error": {"message": "expected smoke tool was not offered"}});
        respond_json(&mut stream, &response);
        return;
    }
    if request_number == 1 {
        thread::sleep(Duration::from_secs(3));
    }
    respond_stream(&mut stream, stream_text("STAGE7_PRELUDE_COMPLETE"));
}

struct HttpRequest {
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<HttpRequest> {
    // accept() may inherit the listener's nonblocking flag on BSD/macOS.
    // Explicit blocking mode keeps fragmented requests under the read timeout.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break Some(position + 4);
        }
        if bytes.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "fake provider headers exceed limit",
            ));
        }
    };
    let Some(header_end) = header_end else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "fake provider request ended before headers",
        ));
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if content_length > 8 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "fake provider body exceeds limit",
        ));
    }
    while bytes.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "fake provider request ended inside body",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(HttpRequest {
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn respond_json(stream: &mut TcpStream, value: &Value) {
    let body = value.to_string();
    respond(stream, "application/json", body.as_bytes());
}

fn respond_stream(stream: &mut TcpStream, chunks: Vec<Value>) {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    respond(stream, "text/event-stream", body.as_bytes());
}

fn respond(stream: &mut TcpStream, content_type: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn stream_tool_call(tool_name: &str, arguments: &str) -> Vec<Value> {
    vec![
        json!({
            "id": "stage7-fake-tool-call",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": FAKE_PROVIDER_MODEL,
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "stage7-fake-tool-call",
                        "type": "function",
                        "function": {"name": tool_name, "arguments": arguments}
                    }]
                },
                "finish_reason": null
            }]
        }),
        json!({
            "id": "stage7-fake-tool-call",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": FAKE_PROVIDER_MODEL,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        }),
    ]
}

fn stream_text(text: &str) -> Vec<Value> {
    vec![
        json!({
            "id": "stage7-fake-final",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": FAKE_PROVIDER_MODEL,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
        }),
        json!({
            "id": "stage7-fake-final",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": FAKE_PROVIDER_MODEL,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        }),
    ]
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn find_status_value(value: &Value) -> Option<(String, bool, bool)> {
    if let Some(result) = exact_call_tool_result(value) {
        return Some(result);
    }
    if let Value::Object(map) = value {
        if let Some(result) = paired_tool_result(map) {
            return Some(result);
        }
        if let Some(status) = map.get("status").and_then(Value::as_str)
            && map.contains_key("data")
        {
            let is_error = map
                .get("isError")
                .or_else(|| map.get("is_error"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            return Some((status.to_owned(), is_error, false));
        }
        for child in map.values() {
            if let Some(status) = find_status_value(child) {
                return Some(status);
            }
        }
    } else if let Value::Array(values) = value {
        for child in values {
            if let Some(status) = find_status_value(child) {
                return Some(status);
            }
        }
    } else if let Value::String(text) = value
        && let Ok(child) = serde_json::from_str::<Value>(text)
    {
        return find_status_value(&child);
    }
    None
}

fn find_opencode_tool_event(bytes: &[u8], expected_tool: &str) -> Option<ToolResultObservation> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter(|event| event.pointer("/part/tool").and_then(Value::as_str) == Some(expected_tool))
        .filter(|event| {
            event.pointer("/part/state/status").and_then(Value::as_str) == Some("completed")
        })
        .find_map(|event| {
            let state = event.pointer("/part/state")?;
            let output = serde_json::from_str::<Value>(state.get("output")?.as_str()?).ok()?;
            let status = output.get("status")?.as_str()?.to_owned();
            let text = state
                .pointer("/metadata/content")?
                .as_array()?
                .iter()
                .find_map(|block| block.get("text").and_then(Value::as_str))?;
            let text = serde_json::from_str::<Value>(text).ok()?;
            let structured = state
                .pointer("/metadata/structuredContent")
                .or_else(|| state.pointer("/metadata/structured_content"))
                .unwrap_or(&output);
            let is_error = state
                .get("isError")
                .or_else(|| state.get("is_error"))
                .or_else(|| state.pointer("/metadata/isError"))
                .or_else(|| state.pointer("/metadata/is_error"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(ToolResultObservation {
                status,
                is_error,
                structured_text_equivalent: output == text && output == *structured,
                output,
            })
        })
}

fn validate_tool_observations(
    provider_status: &str,
    provider_is_error: bool,
    provider_output: &Value,
    provider_representations_equivalent: Option<bool>,
    event: &ToolResultObservation,
) -> Result<()> {
    if event.status != provider_status {
        bail!(
            "OpenCode provider and JSON event statuses differ: provider={provider_status}, event={}",
            event.status
        );
    }
    if provider_is_error || event.is_error {
        bail!("OpenCode tool result was marked as an error");
    }
    if provider_representations_equivalent == Some(false)
        || !event.structured_text_equivalent
        || provider_output != &event.output
    {
        bail!("OpenCode tool result text and structured representations differ");
    }
    Ok(())
}

fn find_tool_representation_equivalence(value: &Value) -> Option<bool> {
    if let Ok(result) = serde_json::from_value::<CallToolResult>(value.clone())
        && let Some(structured) = result.structured_content
    {
        let text = result.content.iter().find_map(|block| {
            serde_json::to_value(block)
                .ok()
                .and_then(|block| block.get("text").and_then(Value::as_str).map(str::to_owned))
        })?;
        let text = serde_json::from_str::<Value>(&text).ok()?;
        return Some(text == structured);
    }
    match value {
        Value::Object(map) => {
            if let Some(structured) = map
                .get("structuredContent")
                .or_else(|| map.get("structured_content"))
                .or_else(|| {
                    map.get("metadata")
                        .and_then(|metadata| metadata.get("structuredContent"))
                })
                .or_else(|| {
                    map.get("metadata")
                        .and_then(|metadata| metadata.get("structured_content"))
                })
            {
                let text = map
                    .get("output")
                    .or_else(|| map.get("text"))
                    .and_then(Value::as_str)?;
                let text = serde_json::from_str::<Value>(text).ok()?;
                return Some(text == *structured);
            }
            map.values().find_map(find_tool_representation_equivalence)
        }
        Value::Array(values) => values.iter().find_map(find_tool_representation_equivalence),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| find_tool_representation_equivalence(&parsed)),
        _ => None,
    }
}

fn find_tool_output_value(value: &Value) -> Option<Value> {
    if let Ok(result) = serde_json::from_value::<CallToolResult>(value.clone())
        && let Some(structured) = result.structured_content
    {
        return Some(structured);
    }
    match value {
        Value::Object(map) => {
            if let Some(structured) = map
                .get("structuredContent")
                .or_else(|| map.get("structured_content"))
                .or_else(|| {
                    map.get("metadata")
                        .and_then(|metadata| metadata.get("structuredContent"))
                })
                .or_else(|| {
                    map.get("metadata")
                        .and_then(|metadata| metadata.get("structured_content"))
                })
            {
                return Some(structured.clone());
            }
            if map.get("status").and_then(Value::as_str).is_some() && map.contains_key("data") {
                return Some(value.clone());
            }
            map.values().find_map(find_tool_output_value)
        }
        Value::Array(values) => values.iter().find_map(find_tool_output_value),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| find_tool_output_value(&parsed)),
        _ => None,
    }
}

fn exact_call_tool_result(value: &Value) -> Option<(String, bool, bool)> {
    let result = serde_json::from_value::<CallToolResult>(value.clone()).ok()?;
    let structured = result.structured_content.as_ref()?;
    let status = structured.get("status")?.as_str()?.to_owned();
    let text = value
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|block| block.get("text").and_then(Value::as_str))?;
    let text_value = serde_json::from_str::<Value>(text).ok()?;
    Some((
        status,
        result.is_error.unwrap_or(false),
        text_value == *structured,
    ))
}

fn paired_tool_result(map: &serde_json::Map<String, Value>) -> Option<(String, bool, bool)> {
    let structured = map
        .get("structuredContent")
        .or_else(|| map.get("structured_content"))
        .or_else(|| {
            map.get("metadata")
                .and_then(|metadata| metadata.get("structuredContent"))
        })
        .or_else(|| {
            map.get("metadata")
                .and_then(|metadata| metadata.get("structured_content"))
        })?;
    let text = map
        .get("output")
        .or_else(|| map.get("text"))
        .and_then(Value::as_str)?;
    let text_value = serde_json::from_str::<Value>(text).ok()?;
    let status = structured.get("status")?.as_str()?.to_owned();
    let is_error = map
        .get("isError")
        .or_else(|| map.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some((status, is_error, text_value == *structured))
}

fn record_provider_error(state: &Arc<Mutex<FakeProviderState>>, message: &str) {
    if let Ok(mut state) = state.lock() {
        if state.protocol_error.is_none() {
            state.protocol_error = Some(message.to_owned());
        }
    }
}

fn copy_workspace(source: &Path, destination: &Path) -> Result<()> {
    for relative in ["Cargo.toml", "Cargo.lock", "src/lib.rs"] {
        let source_path = source.join(relative);
        let metadata = fs::symlink_metadata(&source_path)
            .with_context(|| format!("read OpenCode fixture metadata for {relative}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("OpenCode fixture entry is not a regular file: {relative}");
        }
        let destination_path = destination.join(relative);
        if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent).context("create OpenCode fixture parent")?;
        }
        fs::copy(source_path, destination_path).context("copy OpenCode fixture file")?;
    }
    Ok(())
}

fn server_command(root: &Path) -> Vec<Value> {
    if let Some(binary) = mcp_binary(root) {
        return vec![Value::String(binary.to_string_lossy().into_owned())];
    }
    vec![
        Value::String("cargo".to_owned()),
        Value::String("run".to_owned()),
        Value::String("--quiet".to_owned()),
        Value::String("--locked".to_owned()),
        Value::String("--manifest-path".to_owned()),
        Value::String(root.join("Cargo.toml").to_string_lossy().into_owned()),
        Value::String("-p".to_owned()),
        Value::String("agz-rust-coder".to_owned()),
        Value::String("--".to_owned()),
    ]
}

fn mcp_binary(root: &Path) -> Option<PathBuf> {
    let configured = std::env::var_os("AGZ_RUST_CODER_BIN").map(PathBuf::from);
    let mut candidates = configured.into_iter().chain([
        root.join(format!(
            "target/debug/agz-rust-coder{}",
            std::env::consts::EXE_SUFFIX
        )),
        root.join(format!(
            "target/release/agz-rust-coder{}",
            std::env::consts::EXE_SUFFIX
        )),
    ]);
    candidates.find(|path| {
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    })
}

fn opencode_binary() -> String {
    std::env::var("OPENCODE2_BIN").unwrap_or_else(|_| {
        // npm installs a .cmd shim on Windows; Rust only infers .exe.
        if cfg!(windows) {
            "opencode2.cmd"
        } else {
            "opencode2"
        }
        .to_owned()
    })
}

fn cargo_path(
    current: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<OsString> {
    let mut paths = current
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let Some(home) = home else {
        return std::env::join_paths(paths).context("join isolated OpenCode PATH");
    };
    let cargo_bin = PathBuf::from(home).join(".cargo/bin");
    if cargo_bin.is_dir() && !paths.contains(&cargo_bin) {
        paths.insert(0, cargo_bin);
    }
    std::env::join_paths(paths).context("join isolated OpenCode PATH")
}

fn unique_directory(label: &str) -> Result<PathBuf> {
    let base = fs::canonicalize(std::env::temp_dir())
        .context("canonical isolated OpenCode temp directory")?;
    for _ in 0..16 {
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = base.join(format!("agz-rust-coder-{label}-{timestamp}-{nonce}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create isolated OpenCode directory"),
        }
    }
    bail!("could not allocate an isolated OpenCode directory")
}

fn bounded_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.chars()
        .rev()
        .take(512)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn diagnostic_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let chars: Vec<_> = text.chars().collect();
    if chars.len() <= 12_000 {
        return text.into_owned();
    }
    let head: String = chars[..6_000].iter().collect();
    let tail: String = chars[chars.len() - 6_000..].iter().collect();
    format!("{head}\n... diagnostic output elided ...\n{tail}")
}

fn require_verified_opencode(evidence: &VersionEvidence) -> Result<()> {
    if evidence.status != "verified" {
        bail!(
            "opencode2 {}: expected {}",
            evidence.status,
            evidence.expected
        );
    }
    Ok(())
}

fn read_config(text: &str) -> Result<Value> {
    serde_json::from_str(text).context("parse OpenCode JSONC fixture")
}

fn validate_config(value: &Value, expected_codemode: bool) -> Result<ConfigEvidence> {
    if value.get("$schema").and_then(Value::as_str) != Some("https://opencode.ai/config.json") {
        bail!("OpenCode fixture has an unexpected schema URL");
    }
    let server = value
        .pointer("/mcp/servers/rust")
        .context("OpenCode fixture is missing the rust server")?;
    if server.get("type").and_then(Value::as_str) != Some("local")
        || server.get("codemode").and_then(Value::as_bool) != Some(expected_codemode)
        || server.get("cwd").and_then(Value::as_str) != Some(".")
    {
        bail!("OpenCode local server fields do not match the fixture contract");
    }
    let command = server
        .get("command")
        .and_then(Value::as_array)
        .context("OpenCode fixture command")?;
    if command.len() != 1 || command[0].as_str() != Some("<agz-rust-coder>") {
        bail!("OpenCode fixture must use the redacted binary placeholder");
    }
    let timeout = server.get("timeout").context("OpenCode fixture timeout")?;
    let startup = timeout
        .get("startup")
        .and_then(Value::as_u64)
        .context("OpenCode startup timeout")?;
    let catalog = timeout
        .get("catalog")
        .and_then(Value::as_u64)
        .context("OpenCode catalog timeout")?;
    let execution = timeout
        .get("execution")
        .and_then(Value::as_u64)
        .context("OpenCode execution timeout")?;
    if startup != 30_000 || catalog != 30_000 || execution != 720_000 {
        bail!("OpenCode fixture timeout policy changed");
    }
    Ok(ConfigEvidence {
        schema: "https://opencode.ai/config.json",
        server_key: "rust",
        server_type: "local",
        codemode: expected_codemode,
        command_placeholder: "<agz-rust-coder>",
        cwd: ".",
        startup_timeout_ms: startup,
        catalog_timeout_ms: catalog,
        execution_timeout_ms: execution,
    })
}

fn probe_opencode_version() -> Result<VersionEvidence> {
    let binary = opencode_binary();
    let output = match std::process::Command::new(&binary)
        .arg("--version")
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(VersionEvidence {
                expected: EXPECTED_OPENCODE_VERSION,
                status: "not_available",
            });
        }
        Err(error) => return Err(error).context("run opencode2 version probe"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let status = if output.status.success()
        && stdout
            .lines()
            .any(|line| line.trim() == EXPECTED_OPENCODE_VERSION)
    {
        "verified"
    } else {
        "mismatch"
    };
    Ok(VersionEvidence {
        expected: EXPECTED_OPENCODE_VERSION,
        status,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn fake_provider_reads_fragmented_requests_from_nonblocking_accepted_sockets() {
        use super::*;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let mut client = TcpStream::connect(listener.local_addr().expect("fixture address"))
            .expect("connect fixture");
        let (mut accepted, _) = listener.accept().expect("accept fixture");
        accepted
            .set_nonblocking(true)
            .expect("reproduce inherited socket mode");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            ready_tx.send(()).expect("signal reader");
            read_http_request(&mut accepted)
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader started");
        thread::sleep(Duration::from_millis(30));
        client
            .write_all(b"POST / HTTP/1.1\r\nContent-Length: 2\r\n")
            .expect("partial headers");
        thread::sleep(Duration::from_millis(30));
        client.write_all(b"\r\n{").expect("partial body");
        thread::sleep(Duration::from_millis(30));
        client.write_all(b"}").expect("body end");
        let request = reader
            .join()
            .expect("reader joined")
            .expect("bounded request read");
        assert_eq!(request.body, b"{}");
    }

    use super::*;

    #[test]
    fn smoke_requires_the_pinned_opencode_binary() {
        assert!(
            require_verified_opencode(&VersionEvidence {
                expected: EXPECTED_OPENCODE_VERSION,
                status: "verified",
            })
            .is_ok()
        );
        for status in ["not_available", "mismatch"] {
            assert!(
                require_verified_opencode(&VersionEvidence {
                    expected: EXPECTED_OPENCODE_VERSION,
                    status,
                })
                .is_err()
            );
        }
    }

    #[test]
    fn fake_provider_extracts_nested_rust_status() {
        let structured = json!({"status": "FAST_PASS", "data": {}});
        let value = serde_json::to_value(CallToolResult::structured(structured))
            .expect("serialize structured tool result");
        assert_eq!(
            find_status_value(&value),
            Some(("FAST_PASS".to_owned(), false, true))
        );
    }

    #[test]
    fn fake_provider_rejects_mismatched_text_and_structured_results() {
        let value = json!({
            "content": [{"type": "text", "text": "{\"status\":\"FAIL\",\"data\":{}}"}],
            "structuredContent": {"status": "FAST_PASS", "data": {}},
            "isError": false
        });
        assert_eq!(
            find_status_value(&value),
            Some(("FAST_PASS".to_owned(), false, false))
        );
    }

    #[test]
    fn opencode_event_compares_output_and_content_json() {
        let event = json!({
            "type": "tool_use",
            "part": {
                "tool": "rust_check",
                "state": {
                    "status": "completed",
                    "output": "{\"status\":\"FAST_PASS\",\"data\":{}}",
                    "metadata": {
                        "content": [{"type": "text", "text": "{\"status\":\"FAST_PASS\",\"data\":{}}"}]
                    }
                }
            }
        });
        let mut bytes = event.to_string().into_bytes();
        bytes.push(b'\n');
        let observed = find_opencode_tool_event(&bytes, "rust_check").expect("tool event");
        assert_eq!(observed.status, "FAST_PASS");
        assert!(!observed.is_error);
        assert!(observed.structured_text_equivalent);

        let mut mismatch = event;
        mismatch["part"]["state"]["metadata"]["content"][0]["text"] =
            Value::String("{\"status\":\"FAIL\",\"data\":{}}".to_owned());
        let observed = find_opencode_tool_event(mismatch.to_string().as_bytes(), "rust_check")
            .expect("mismatched tool event");
        assert!(!observed.structured_text_equivalent);
    }

    #[test]
    fn smoke_rejects_error_and_mismatched_tool_observations() {
        let provider = json!({"status": "FAST_PASS", "data": {"source": "provider"}});
        let matching_event = ToolResultObservation {
            status: "FAST_PASS".to_owned(),
            is_error: false,
            structured_text_equivalent: true,
            output: provider.clone(),
        };
        assert!(
            validate_tool_observations("FAST_PASS", false, &provider, None, &matching_event)
                .is_ok()
        );

        let mismatched_event = ToolResultObservation {
            output: json!({"status": "FAST_PASS", "data": {"source": "event"}}),
            ..matching_event.clone()
        };
        assert!(
            validate_tool_observations("FAST_PASS", false, &provider, None, &mismatched_event)
                .is_err()
        );
        assert!(
            validate_tool_observations("FAST_PASS", true, &provider, None, &mismatched_event)
                .is_err()
        );

        let provider_mismatch = json!({
            "content": [{"type": "text", "text": "{\"status\":\"FAIL\",\"data\":{}}"}],
            "structuredContent": {"status": "FAST_PASS", "data": {"source": "provider"}},
            "isError": false
        });
        assert_eq!(
            find_tool_representation_equivalence(&provider_mismatch),
            Some(false)
        );
        assert!(
            validate_tool_observations("FAST_PASS", false, &provider, Some(false), &matching_event)
                .is_err()
        );
    }

    #[test]
    fn isolated_path_uses_platform_path_joining() {
        let root = unique_directory("path-test").expect("path test root");
        let cargo_bin = root.join(".cargo/bin");
        fs::create_dir_all(&cargo_bin).expect("cargo bin");
        let existing =
            std::env::join_paths([root.join("one"), root.join("two")]).expect("existing PATH");

        let joined =
            cargo_path(Some(existing.as_os_str()), Some(root.as_os_str())).expect("isolated PATH");
        let paths = std::env::split_paths(&joined).collect::<Vec<_>>();

        assert_eq!(paths.first(), Some(&cargo_bin));
        assert_eq!(paths.get(1), Some(&root.join("one")));
        assert_eq!(paths.get(2), Some(&root.join("two")));
        fs::remove_dir_all(root).expect("remove path test root");
    }
}
