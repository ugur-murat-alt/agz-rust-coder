use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

fn main() {
    let (mode, log_path, pid_path) = arguments();
    if let Some(path) = &pid_path {
        fs::write(path, std::process::id().to_string()).expect("write mock process ID");
    }
    if mode == "stderr" {
        let payload = "s".repeat(4_096);
        eprint!("{payload}FINAL-STDERR");
        io::stderr().flush().expect("flush mock stderr");
    }

    let output = Arc::new(Mutex::new(io::stdout()));
    let mut input = io::stdin();
    let mut buffer = Vec::new();
    let mut retry_count = 0;
    let mut trigger_id = None;

    loop {
        let message = match next_frame(&mut input, &mut buffer) {
            Ok(Some(message)) => message,
            Ok(None) => {
                if mode == "ignore" || mode == "ignore-eof" {
                    thread::sleep(Duration::from_millis(25));
                    continue;
                }
                return;
            }
            Err(error) => panic!("read mock request: {error}"),
        };
        append_log(log_path.as_deref(), &message);

        let method = string_field(&message, "method");
        let id = id_field(&message);
        if method.as_deref() == Some("$/cancelRequest") {
            continue;
        }
        let Some(id) = id else {
            if method.as_deref() == Some("exit")
                && mode != "ignore"
                && mode != "ignore-eof"
            {
                return;
            }
            continue;
        };

        match mode.as_str() {
            "partial" => send_response(&output, &id, r#"{"ok":true}"#, true),
            "concat" => {
                let notification = frame(
                    r#"{"jsonrpc":"2.0","method":"mock/notification","params":{"ok":true}}"#,
                );
                send_many(
                    &output,
                    &[notification.to_owned(), response(&id, r#"{"ok":true}"#)],
                );
            }
            "malformed" => {
                send_many(
                    &output,
                        &[
                        frame(r#"{"jsonrpc":"2.0","id":}"#),
                        response(&id, r#"{"ok":true}"#),
                    ],
                );
            }
            "error" => {
                if method.as_deref() == Some("error") {
                    send_raw(
                        &output,
                        &response_error(&id, -32_042, "mock failure", r#"{"marker":true}"#),
                        false,
                    );
                } else {
                    send_response(&output, &id, r#"{"ok":true}"#, false);
                }
            }
            "retry" => {
                if method.as_deref() == Some("retry") {
                    retry_count += 1;
                    if retry_count == 1 {
                        send_raw(
                            &output,
                            &response_error(&id, -32_801, "content modified", "null"),
                            false,
                        );
                    } else {
                        send_response(&output, &id, r#"{"ok":true}"#, false);
                    }
                } else {
                    send_response(&output, &id, r#"{"ok":true}"#, false);
                }
            }
            "server" => {
                if method.as_deref() == Some("trigger") {
                    let notification = frame(
                        r#"{"jsonrpc":"2.0","method":"mock/notification","params":{"value":7}}"#,
                    );
                    let request = frame(
                        r#"{"jsonrpc":"2.0","id":700,"method":"mock/custom","params":{"value":7}}"#,
                    );
                    send_many(
                        &output,
                        &[notification.to_owned(), request.to_owned(), response(&id, "null")],
                    );
                } else if id == "700" {
                    continue;
                } else {
                    send_response(&output, &id, r#"{"ok":true}"#, false);
                }
            }
            "server-cancel" => {
                if method.as_deref() == Some("trigger") {
                    trigger_id = Some(id.clone());
                    let request = frame(
                        r#"{"jsonrpc":"2.0","id":701,"method":"mock/slowServerRequest","params":{}}"#,
                    );
                    send_raw(&output, &request, false);
                    let output_for_cancel = Arc::clone(&output);
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(40));
                        send_raw(
                            &output_for_cancel,
                            &frame(
                                r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":701}}"#,
                            ),
                            false,
                        );
                    });
                } else if id == "701" {
                    if let Some(trigger_id) = trigger_id.take() {
                        send_response(&output, &trigger_id, r#"{"cancelled":true}"#, false);
                    }
                } else {
                    send_response(&output, &id, r#"{"ok":true}"#, false);
                }
            }
            "server-unsupported" => {
                if method.as_deref() == Some("trigger") {
                    let request = frame(
                        r#"{"jsonrpc":"2.0","id":702,"method":"mock/unsupported","params":{}}"#,
                    );
                    send_many(&output, &[request, response(&id, "null")]);
                } else if id == "702" {
                    continue;
                } else {
                    send_response(&output, &id, r#"{"ok":true}"#, false);
                }
            }
            "slow" => {
                if method.as_deref() != Some("slow") {
                    send_response(&output, &id, "null", false);
                }
            }
            "ignore" | "ignore-eof" => {}
            "graceful" => {
                if method.as_deref() == Some("shutdown") {
                    send_response(&output, &id, "null", false);
                } else if method.as_deref() == Some("exit") {
                    return;
                } else {
                    send_response(&output, &id, r#"{"ok":true}"#, false);
                }
            }
            _ => {
                if method.as_deref() == Some("shutdown") {
                    send_response(&output, &id, "null", false);
                } else if method.as_deref() == Some("exit") {
                    return;
                } else {
                    send_response(&output, &id, r#"{"ok":true}"#, false);
                }
            }
        }
    }
}

fn arguments() -> (String, Option<PathBuf>, Option<PathBuf>) {
    let mut mode = "echo".to_owned();
    let mut log_path = None;
    let mut pid_path = None;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--mode" => mode = args.next().expect("mock mode").to_owned(),
            "--log" => log_path = Some(PathBuf::from(args.next().expect("mock log path"))),
            "--pid" => pid_path = Some(PathBuf::from(args.next().expect("mock PID path"))),
            other => panic!("unknown mock argument: {other}"),
        }
    }
    (mode, log_path, pid_path)
}

fn next_frame(reader: &mut impl Read, buffer: &mut Vec<u8>) -> io::Result<Option<String>> {
    loop {
        if let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
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

        let mut chunk = [0u8; 1_024];
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
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
    if let Some(value) = value.strip_prefix('"') {
        let end = value.find('"')?;
        return Some(format!("\"{}\"", &value[..end]));
    }
    let end = value
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(value.len());
    let id = &value[..end];
    (!id.is_empty()).then(|| id.to_owned())
}

fn append_log(path: Option<&Path>, message: &str) {
    let Some(path) = path else {
        return;
    };
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open mock log");
    writeln!(log, "{message}").expect("write mock log");
}

fn frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

fn response(id: &str, result: &str) -> String {
    frame(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#))
}

fn response_error(id: &str, code: i64, message: &str, data: &str) -> String {
    frame(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{code},"message":"{message}","data":{data}}}}}"#
    ))
}

fn send_response(output: &Arc<Mutex<io::Stdout>>, id: &str, result: &str, partial: bool) {
    send_raw(output, &response(id, result), partial);
}

fn send_many(output: &Arc<Mutex<io::Stdout>>, bodies: &[String]) {
    let mut bytes = Vec::new();
    for body in bodies {
        bytes.extend_from_slice(body.as_bytes());
    }
    let mut output = output.lock().expect("lock mock stdout");
    output.write_all(&bytes).expect("write mock output");
    output.flush().expect("flush mock output");
}

fn send_raw(output: &Arc<Mutex<io::Stdout>>, body: &str, partial: bool) {
    let bytes = body.as_bytes();
    let mut output = output.lock().expect("lock mock stdout");
    if partial {
        for chunk in bytes.chunks(3) {
            output.write_all(chunk).expect("write partial mock output");
            output.flush().expect("flush partial mock output");
            thread::sleep(Duration::from_millis(1));
        }
    } else {
        output.write_all(bytes).expect("write mock output");
        output.flush().expect("flush mock output");
    }
}
