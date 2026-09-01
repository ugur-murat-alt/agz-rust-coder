use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{ChildStderr, ChildStdout},
    sync::mpsc,
    task::JoinHandle,
};

pub type DiagnosticCallback = Arc<dyn Fn(&str) -> bool + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub(crate) enum OutputEvent {
    Chunk(StreamKind, Vec<u8>),
    Eof(StreamKind),
    Error(StreamKind, String),
}

pub(crate) fn spawn_stdout_reader(
    reader: ChildStdout,
    sender: mpsc::Sender<OutputEvent>,
) -> JoinHandle<()> {
    spawn_reader(reader, StreamKind::Stdout, sender)
}

pub(crate) fn spawn_stderr_reader(
    reader: ChildStderr,
    sender: mpsc::Sender<OutputEvent>,
) -> JoinHandle<()> {
    spawn_reader(reader, StreamKind::Stderr, sender)
}

fn spawn_reader<R>(
    mut reader: R,
    stream: StreamKind,
    sender: mpsc::Sender<OutputEvent>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            let read = match reader.read(&mut buffer).await {
                Ok(read) => read,
                Err(error) => {
                    let _ = sender
                        .send(OutputEvent::Error(stream, error.to_string()))
                        .await;
                    break;
                }
            };
            if read == 0 {
                break;
            }
            if sender
                .send(OutputEvent::Chunk(stream, buffer[..read].to_vec()))
                .await
                .is_err()
            {
                // Keep consuming if the owner gave up while the child still has
                // the pipe open. The owner will abort this bounded task later.
                while reader
                    .read(&mut buffer)
                    .await
                    .ok()
                    .is_some_and(|size| size > 0)
                {}
                return;
            }
        }
        let _ = sender.send(OutputEvent::Eof(stream)).await;
    })
}

#[derive(Debug)]
pub(crate) struct OutputSnapshot {
    pub(crate) output: String,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) output_truncated: bool,
    pub(crate) first_diagnostic: Option<Duration>,
    pub(crate) read_errors: Vec<String>,
}

pub(crate) struct OutputCollector {
    started: Instant,
    max_bytes: usize,
    callback: Option<DiagnosticCallback>,
    combined: TailBuffer,
    stdout: TailBuffer,
    stderr: TailBuffer,
    stdout_decoder: Utf8Decoder,
    stderr_decoder: Utf8Decoder,
    stdout_sanitizer: TerminalSanitizer,
    stderr_sanitizer: TerminalSanitizer,
    line_buffer: String,
    first_diagnostic: Option<Duration>,
    read_errors: Vec<String>,
}

impl OutputCollector {
    pub(crate) fn new(
        started: Instant,
        max_bytes: usize,
        callback: Option<DiagnosticCallback>,
    ) -> Self {
        Self {
            started,
            max_bytes,
            callback,
            combined: TailBuffer::new(max_bytes),
            stdout: TailBuffer::new(max_bytes),
            stderr: TailBuffer::new(max_bytes),
            stdout_decoder: Utf8Decoder::default(),
            stderr_decoder: Utf8Decoder::default(),
            stdout_sanitizer: TerminalSanitizer::default(),
            stderr_sanitizer: TerminalSanitizer::default(),
            line_buffer: String::new(),
            first_diagnostic: None,
            read_errors: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, stream: StreamKind, bytes: &[u8]) {
        let text = match stream {
            StreamKind::Stdout => self.stdout_decoder.push(bytes),
            StreamKind::Stderr => self.stderr_decoder.push(bytes),
        };
        let text = match stream {
            StreamKind::Stdout => self.stdout_sanitizer.push(&text),
            StreamKind::Stderr => self.stderr_sanitizer.push(&text),
        };
        self.append_clean(stream, &text);
    }

    pub(crate) fn finish_stream(&mut self, stream: StreamKind) {
        let text = match stream {
            StreamKind::Stdout => self.stdout_decoder.finish(),
            StreamKind::Stderr => self.stderr_decoder.finish(),
        };
        let text = match stream {
            StreamKind::Stdout => {
                let mut text = self.stdout_sanitizer.push(&text);
                text.push_str(&self.stdout_sanitizer.finish());
                text
            }
            StreamKind::Stderr => {
                let mut text = self.stderr_sanitizer.push(&text);
                text.push_str(&self.stderr_sanitizer.finish());
                text
            }
        };
        self.append_clean(stream, &text);
    }

    pub(crate) fn push_error(&mut self, stream: StreamKind, error: &str) {
        if self.read_errors.len() < 4 {
            self.read_errors.push(format!("{stream:?}: {error}"));
        }
    }

    pub(crate) fn finish(mut self) -> OutputSnapshot {
        self.finish_stream(StreamKind::Stdout);
        self.finish_stream(StreamKind::Stderr);
        self.inspect_remaining_line();
        OutputSnapshot {
            output: self.combined.as_string(),
            stdout: self.stdout.as_string(),
            stderr: self.stderr.as_string(),
            output_truncated: self.combined.truncated(),
            first_diagnostic: self.first_diagnostic,
            read_errors: self.read_errors,
        }
    }

    fn append_clean(&mut self, stream: StreamKind, text: &str) {
        if text.is_empty() {
            return;
        }
        self.combined.push(text.as_bytes());
        match stream {
            StreamKind::Stdout => self.stdout.push(text.as_bytes()),
            StreamKind::Stderr => self.stderr.push(text.as_bytes()),
        }
        self.inspect_lines(text);
    }

    fn inspect_lines(&mut self, text: &str) {
        if self.first_diagnostic.is_some() || self.callback.is_none() {
            return;
        }
        self.line_buffer.push_str(text);
        if self.line_buffer.len() > self.max_bytes {
            self.line_buffer = utf8_tail(&self.line_buffer, self.max_bytes);
        }
        self.inspect_complete_lines();
    }

    fn inspect_complete_lines(&mut self) {
        let callback = match self.callback.as_ref() {
            Some(callback) => Arc::clone(callback),
            None => return,
        };
        let mut line_start = 0;
        let mut matched = false;
        for (index, character) in self.line_buffer.char_indices() {
            if character != '\n' {
                continue;
            }
            let line = self.line_buffer[line_start..index]
                .strip_suffix('\r')
                .unwrap_or(&self.line_buffer[line_start..index]);
            if callback(line) {
                self.first_diagnostic = Some(self.started.elapsed());
                matched = true;
                break;
            }
            line_start = index + character.len_utf8();
        }
        if matched {
            return;
        }
        if line_start > 0 {
            self.line_buffer = self.line_buffer[line_start..].to_owned();
        }
    }

    fn inspect_remaining_line(&mut self) {
        if self.first_diagnostic.is_some() {
            return;
        }
        let callback = match self.callback.as_ref() {
            Some(callback) => Arc::clone(callback),
            None => return,
        };
        let line = self
            .line_buffer
            .strip_suffix('\r')
            .unwrap_or(&self.line_buffer);
        if !line.is_empty() && callback(line) {
            self.first_diagnostic = Some(self.started.elapsed());
        }
    }
}

#[derive(Debug, Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        if bytes.is_empty() && self.pending.is_empty() {
            return String::new();
        }
        let mut input = Vec::with_capacity(self.pending.len() + bytes.len());
        input.extend_from_slice(&self.pending);
        input.extend_from_slice(bytes);
        self.pending.clear();

        let mut output = String::new();
        let mut offset = 0;
        while offset < input.len() {
            match std::str::from_utf8(&input[offset..]) {
                Ok(text) => {
                    output.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        output.push_str(
                            std::str::from_utf8(&input[offset..offset + valid])
                                .expect("the UTF-8 validator reported valid bytes"),
                        );
                        offset += valid;
                    }
                    if let Some(length) = error.error_len() {
                        output.push('\u{fffd}');
                        offset += length;
                    } else {
                        self.pending.extend_from_slice(&input[offset..]);
                        break;
                    }
                }
            }
        }
        output
    }

    fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let pending = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&pending).into_owned()
    }
}

#[derive(Debug, Default)]
struct TerminalSanitizer {
    state: EscapeState,
}

impl TerminalSanitizer {
    fn push(&mut self, text: &str) -> String {
        let mut clean = String::with_capacity(text.len());
        for character in text.chars() {
            match self.state {
                EscapeState::Normal => match character {
                    '\u{1b}' => self.state = EscapeState::Escape,
                    '\n' | '\r' | '\t' => clean.push(character),
                    character if is_control(character) => {}
                    character => clean.push(character),
                },
                EscapeState::Escape => match character {
                    '[' => self.state = EscapeState::Csi,
                    ']' => self.state = EscapeState::Osc,
                    _ => self.state = EscapeState::Normal,
                },
                EscapeState::Csi => {
                    if ('@'..='~').contains(&character) {
                        self.state = EscapeState::Normal;
                    }
                }
                EscapeState::Osc => match character {
                    '\u{7}' => self.state = EscapeState::Normal,
                    '\u{1b}' => self.state = EscapeState::OscEscape,
                    _ => {}
                },
                EscapeState::OscEscape => {
                    self.state = if character == '\\' {
                        EscapeState::Normal
                    } else if character == '\u{1b}' {
                        EscapeState::OscEscape
                    } else {
                        EscapeState::Osc
                    };
                }
            }
        }
        clean
    }

    fn finish(&mut self) -> String {
        self.state = EscapeState::Normal;
        String::new()
    }
}

#[derive(Debug, Default)]
enum EscapeState {
    #[default]
    Normal,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

fn is_control(character: char) -> bool {
    character <= '\u{1f}' || character == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&character)
}

#[derive(Debug)]
struct TailBuffer {
    max_bytes: usize,
    bytes: Vec<u8>,
    truncated: bool,
}

impl TailBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.max_bytes == 0 {
            self.truncated = true;
            self.bytes.clear();
            return;
        }
        if self.bytes.len().saturating_add(bytes.len()) > self.max_bytes {
            self.truncated = true;
        }
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > self.max_bytes {
            self.bytes = utf8_tail_bytes(&self.bytes, self.max_bytes);
        }
    }

    fn as_string(&self) -> String {
        String::from_utf8(self.bytes.clone()).expect("the output tail is valid UTF-8")
    }

    fn truncated(&self) -> bool {
        self.truncated
    }
}

fn utf8_tail(value: &str, max_bytes: usize) -> String {
    String::from_utf8(utf8_tail_bytes(value.as_bytes(), max_bytes))
        .expect("the output tail is valid UTF-8")
}

fn utf8_tail_bytes(value: &[u8], max_bytes: usize) -> Vec<u8> {
    if value.len() <= max_bytes {
        return value.to_vec();
    }
    let mut start = value.len() - max_bytes;
    while start < value.len() && (value[start] & 0xc0) == 0x80 {
        start += 1;
    }
    value[start..].to_vec()
}
