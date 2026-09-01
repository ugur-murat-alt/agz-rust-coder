use std::{
    env,
    fs::OpenOptions,
    io::{self, Read, Write},
    path::PathBuf,
};

fn main() {
    let args = env::args().collect::<Vec<_>>();
    let executable = args.first().map_or("", String::as_str);
    let has_mode = |flag: &str, name: &str| {
        args.iter().any(|arg| arg == flag) || executable.contains(name)
    };
    let hierarchical = has_mode("--symbols=hierarchical", "hierarchical");
    let reject_rename = has_mode("--prepare-rename=reject", "reject");
    let default_rename = has_mode("--prepare-rename=default", "default");
    let reciprocal = has_mode("--reciprocal-hierarchy", "reciprocal");
    let retry_method = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--content-modified-once="));
    let retry_method = retry_method.or_else(|| {
        executable
            .contains("retry-hover")
            .then_some("textDocument/hover")
    });
    let message_error = executable.contains("message-retry");
    let log_path = args
        .iter()
        .position(|arg| arg == "--log")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from);
    let mut input = io::stdin();
    let mut buffer = Vec::new();
    let mut retried = false;

    loop {
        let Some(message) = next_frame(&mut input, &mut buffer).expect("read semantic request")
        else {
            return;
        };
        let method = string_field(&message, "method");
        append_log(log_path.as_deref(), method.as_deref().unwrap_or("response"));
        let Some(id) = id_field(&message) else {
            if method.as_deref() == Some("exit") {
                return;
            }
            continue;
        };
        let method = method.unwrap_or_default();
        if retry_method == Some(method.as_str()) && !retried {
            retried = true;
            send_raw(&response_error(&id, -32_801, "content modified"));
            continue;
        }
        if message_error && method == "textDocument/hover" {
            send_raw(&response_error(
                &id,
                -32_602,
                "No references found at position",
            ));
            continue;
        }
        let uri = string_field(&message, "uri").unwrap_or_else(|| "file:///mock/lib.rs".to_owned());
        let result = response_value(
            &method,
            &message,
            &uri,
            hierarchical,
            reject_rename,
            default_rename,
            reciprocal,
        );
        send_raw(&response(&id, &result));
    }
}

#[allow(clippy::too_many_arguments)]
fn response_value(
    method: &str,
    message: &str,
    uri: &str,
    hierarchical: bool,
    reject_rename: bool,
    default_rename: bool,
    reciprocal: bool,
) -> String {
    let uri = json_string(uri);
    match method {
        "initialize" => {
            r#"{"capabilities":{"hoverProvider":true,"textDocumentSync":2}}"#.to_owned()
        }
        "shutdown" => "null".to_owned(),
        "textDocument/documentSymbol" => {
            if hierarchical {
                r#"[{"name":"mock_mod","kind":2,"range":{"start":{"line":0,"character":0},"end":{"line":6,"character":1}},"children":[{"name":"mock_fn","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":2,"character":1}}}]}]"#.to_owned()
            } else {
                format!(r#"[{{"name":"mock_fn","kind":12,"location":{{"uri":{uri},"range":{{"start":{{"line":0,"character":7}},"end":{{"line":2,"character":1}}}}}},"containerName":"mock_mod"}}]"#)
            }
        }
        "textDocument/hover" => {
            r#"{"contents":{"kind":"markdown","value":"```rust\nmock_fn: fn() -> i32\n```\nMock hover docs."}}"#.to_owned()
        }
        "textDocument/references" => format!(
            r#"[{{"uri":{uri},"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":4}}}}}},{{"uri":"file:///mock/other.rs","range":{{"start":{{"line":3,"character":2}},"end":{{"line":3,"character":6}}}}}}]"#
        ),
        "textDocument/definition" => format!(
            r#"[{{"targetUri":{uri},"targetRange":{{"start":{{"line":0,"character":4}},"end":{{"line":0,"character":12}}}},"targetSelectionRange":{{"start":{{"line":0,"character":4}},"end":{{"line":0,"character":12}}}}}}]"#
        ),
        "textDocument/implementation" => format!(
            r#"[{{"targetUri":{uri},"targetRange":{{"start":{{"line":3,"character":0}},"end":{{"line":6,"character":1}}}},"targetSelectionRange":{{"start":{{"line":5,"character":11}},"end":{{"line":5,"character":18}}}}}}]"#
        ),
        "textDocument/prepareCallHierarchy" => format!(
            r#"[{{"name":"mock_fn","kind":12,"uri":{uri},"range":{{"start":{{"line":0,"character":0}},"end":{{"line":2,"character":1}}}},"selectionRange":{{"start":{{"line":0,"character":7}},"end":{{"line":0,"character":14}}}}}}]"#
        ),
        "callHierarchy/incomingCalls" => hierarchy_edge(message, &uri, reciprocal, true),
        "callHierarchy/outgoingCalls" => hierarchy_edge(message, &uri, reciprocal, false),
        "textDocument/prepareRename" => {
            if reject_rename {
                "null".to_owned()
            } else if default_rename {
                r#"{"defaultBehavior":true}"#.to_owned()
            } else {
                r#"{"range":{"start":{"line":0,"character":7},"end":{"line":0,"character":14}},"placeholder":"mock_fn"}"#.to_owned()
            }
        }
        "textDocument/rename" => {
            let new_name = json_string(
                &string_field(message, "newName").unwrap_or_else(|| "renamed".to_owned()),
            );
            format!(
                r#"{{"changes":{{{uri}:[{{"range":{{"start":{{"line":0,"character":7}},"end":{{"line":0,"character":14}}}},"newText":{new_name}}},{{"range":{{"start":{{"line":5,"character":11}},"end":{{"line":5,"character":18}}}},"newText":{new_name}}}]}}}}"#
            )
        }
        "textDocument/codeAction" => format!(
            r#"[{{"title":"Replace literal","kind":"refactor.rewrite","edit":{{"changes":{{{uri}:[{{"range":{{"start":{{"line":1,"character":4}},"end":{{"line":1,"character":6}}}},"newText":"43","annotationId":"safe"}}]}}}},"command":{{"title":"Format","command":"mock.format"}}}},{{"title":"Extract helper","command":"mock.extract","arguments":[]}},{{"title":"Unavailable","kind":"refactor.extract","disabled":{{"reason":"not valid here"}}}}]"#
        ),
        _ => "null".to_owned(),
    }
}

fn hierarchy_edge(message: &str, uri: &str, reciprocal: bool, incoming: bool) -> String {
    let name = string_field(message, "name").unwrap_or_default();
    let child = if reciprocal {
        match name.as_str() {
            "mock_fn" => "peer",
            "peer" => "mock_fn",
            _ => return "[]".to_owned(),
        }
    } else {
        if name != "mock_fn" {
            return "[]".to_owned();
        }
        if incoming { "caller" } else { "callee" }
    };
    let field = if incoming { "from" } else { "to" };
    let end_line = if child == "mock_fn" { 2 } else { 1 };
    format!(
        r#"[{{"{field}":{{"name":"{child}","kind":12,"uri":{uri},"range":{{"start":{{"line":0,"character":0}},"end":{{"line":{end_line},"character":1}}}}}}}}]"#
    )
}

fn next_frame(reader: &mut impl Read, buffer: &mut Vec<u8>) -> io::Result<Option<String>> {
    loop {
        let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
        else {
            let mut chunk = [0u8; 1_024];
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                return Ok(None);
            }
            buffer.extend_from_slice(&chunk[..read]);
            continue;
        };
        let header = String::from_utf8_lossy(&buffer[..header_end]);
        let length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        let body_end = body_start.saturating_add(length);
        if buffer.len() < body_end {
            let mut chunk = [0u8; 1_024];
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                return Ok(None);
            }
            buffer.extend_from_slice(&chunk[..read]);
            continue;
        }
        let body = buffer[body_start..body_end].to_vec();
        buffer.drain(..body_end);
        return Ok(Some(String::from_utf8_lossy(&body).into_owned()));
    }
}

fn string_field(message: &str, field: &str) -> Option<String> {
    let marker = format!("\"{field}\"");
    let start = message.find(&marker)? + marker.len();
    let value = message[start..].trim_start().strip_prefix(':')?.trim_start();
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_owned())
}

fn id_field(message: &str) -> Option<String> {
    let marker = "\"id\"";
    let start = message.find(marker)? + marker.len();
    let value = message[start..].trim_start().strip_prefix(':')?.trim_start();
    let end = value
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(value.len());
    let id = &value[..end];
    (!id.is_empty()).then(|| id.to_owned())
}

fn json_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn append_log(path: Option<&std::path::Path>, method: &str) {
    let Some(path) = path else {
        return;
    };
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open semantic mock log");
    writeln!(log, "{method}").expect("write semantic mock log");
}

fn frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

fn response(id: &str, result: &str) -> String {
    frame(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#))
}

fn response_error(id: &str, code: i64, message: &str) -> String {
    frame(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{code},"message":"{message}"}}}}"#
    ))
}

fn send_raw(message: &str) {
    let mut stdout = io::stdout();
    stdout.write_all(message.as_bytes()).expect("write semantic response");
    stdout.flush().expect("flush semantic response");
}
