//! Canonical paths, local file URIs, and the rust-analyzer config probe.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

use super::client::DocumentSyncOptions;
use crate::workspace::{RootError, WorkspaceRoot, parse_file_uri};

pub const BUILD_SCRIPTS_ENABLE_KEY: &str = "rust-analyzer.cargo.buildScripts.enable";
pub const PROC_MACRO_ENABLE_KEY: &str = "rust-analyzer.procMacro.enable";
pub const CHECK_ON_SAVE_KEY: &str = "rust-analyzer.checkOnSave";

const DEFAULT_BINARY_NAME: &str = "rust-analyzer";

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum NormalizeError {
    #[error("path must be absolute: {0}")]
    RelativePath(PathBuf),
    #[error("parent path components are not allowed: {0}")]
    ParentComponent(PathBuf),
    #[error("path contains a symlink component: {0}")]
    Symlink(PathBuf),
    #[error("path was not found: {0}")]
    NotFound(PathBuf),
    #[error("path is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("path is not a regular file: {0}")]
    NotRegularFile(PathBuf),
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("binary was not found: {0}")]
    BinaryNotFound(String),
    #[error("invalid file URI: {0}")]
    InvalidFileUri(String),
    #[error("path I/O failed for {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("workspace root error: {0}")]
    Root(#[from] RootError),
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum SchemaError {
    #[error("schema output is not valid UTF-8")]
    InvalidUtf8,
    #[error("schema output is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("schema output is empty")]
    Empty,
}

/// The keys advertised by `rust-analyzer --print-config-schema`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BinaryConfigSchema {
    keys: BTreeSet<String>,
}

impl BinaryConfigSchema {
    pub fn new(keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            keys: keys.into_iter().map(Into::into).collect(),
        }
    }

    pub fn keys(&self) -> &BTreeSet<String> {
        &self.keys
    }

    pub fn supports(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    pub fn supports_workspace_code_deny(&self) -> bool {
        self.supports(BUILD_SCRIPTS_ENABLE_KEY)
            && self.supports(PROC_MACRO_ENABLE_KEY)
            && self.supports(CHECK_ON_SAVE_KEY)
    }

    pub fn from_json(value: &Value) -> Result<Self, SchemaError> {
        let mut keys = BTreeSet::new();
        collect_schema_keys(value, &mut keys);
        if keys.is_empty() {
            return Err(SchemaError::Empty);
        }
        Ok(Self { keys })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SchemaError> {
        let text = std::str::from_utf8(bytes).map_err(|_| SchemaError::InvalidUtf8)?;
        let value = serde_json::from_str(text)
            .map_err(|error| SchemaError::InvalidJson(error.to_string()))?;
        Self::from_json(&value)
    }
}

fn collect_schema_keys(value: &Value, keys: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_schema_keys(value, keys)),
        Value::Object(object) => {
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                keys.extend(properties.keys().cloned());
                properties
                    .values()
                    .for_each(|value| collect_schema_keys(value, keys));
            }
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "properties")
                .for_each(|(_, value)| collect_schema_keys(value, keys));
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Canonicalize an existing path while rejecting symlink components before the
/// canonicalization step.  This keeps caller-visible paths deterministic and
/// prevents a symlinked workspace or executable from being admitted.
pub fn canonical_path(path: &Path) -> Result<PathBuf, NormalizeError> {
    if !path.is_absolute() {
        return Err(NormalizeError::RelativePath(path.to_owned()));
    }
    reject_parent_components(path)?;
    reject_symlink_components(path)?;
    fs::canonicalize(path).map_err(|error| map_path_error(path, error))
}

pub fn canonical_workspace_path(path: &Path) -> Result<PathBuf, NormalizeError> {
    let canonical = canonical_path(path)?;
    let metadata = fs::metadata(&canonical).map_err(|error| map_path_error(&canonical, error))?;
    if !metadata.is_dir() {
        return Err(NormalizeError::NotDirectory(canonical));
    }
    Ok(canonical)
}

pub fn canonical_binary_path(path: &Path) -> Result<PathBuf, NormalizeError> {
    let canonical = canonical_path(path)?;
    let metadata = fs::metadata(&canonical).map_err(|error| map_path_error(&canonical, error))?;
    if !metadata.is_file() {
        return Err(NormalizeError::NotRegularFile(canonical));
    }
    Ok(canonical)
}

/// Resolve an explicitly configured binary or the fixed discovery candidates.
/// The returned executable has no symlink component and is always absolute.
pub fn resolve_binary_path(configured: Option<&Path>) -> Result<PathBuf, NormalizeError> {
    if let Some(configured) = configured {
        return canonical_binary_path(configured);
    }

    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            if directory.is_absolute() {
                candidates.push(directory.join(DEFAULT_BINARY_NAME));
            }
        }
    }
    if let Some(home) = env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".cargo")
                .join("bin")
                .join(DEFAULT_BINARY_NAME),
        );
    }
    candidates.sort();
    candidates.dedup();

    let mut first_error = None;
    for candidate in candidates {
        match canonical_binary_path(&candidate) {
            Ok(path) => return Ok(path),
            Err(error @ NormalizeError::Symlink(_)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(error) => {
                if first_error.is_none() && !matches!(error, NormalizeError::NotFound(_)) {
                    first_error = Some(error);
                }
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Err(NormalizeError::BinaryNotFound(
        DEFAULT_BINARY_NAME.to_owned(),
    ))
}

/// Capture only the environment needed by Cargo/rust-analyzer.  The child
/// client clears the inherited environment before applying this fixed map.
pub fn fixed_environment() -> BTreeMap<OsString, OsString> {
    let mut environment = BTreeMap::new();
    for key in ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(value) = env::var_os(key) {
            environment.insert(OsString::from(key), value);
        }
    }
    environment.insert(OsString::from("CARGO_TERM_COLOR"), OsString::from("never"));
    environment.insert(OsString::from("RUST_BACKTRACE"), OsString::from("0"));
    environment.insert(OsString::from("RUST_LOG"), OsString::from("error"));
    environment
}

pub fn path_to_file_uri(path: &Path) -> Result<String, NormalizeError> {
    if !path.is_absolute() {
        return Err(NormalizeError::RelativePath(path.to_owned()));
    }
    let path = path
        .to_str()
        .ok_or_else(|| NormalizeError::NonUtf8Path(path.to_owned()))?;
    let path = if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };
    let mut uri = String::from("file://");
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/')
            || (cfg!(windows) && *byte == b':')
        {
            uri.push(char::from(*byte));
        } else {
            uri.push('%');
            uri.push(hex_digit(byte >> 4));
            uri.push(hex_digit(byte & 0x0f));
        }
    }
    Ok(uri)
}

pub fn uri_path(uri: &str) -> Result<PathBuf, NormalizeError> {
    parse_file_uri(uri).map_err(|_| NormalizeError::InvalidFileUri(uri.to_owned()))
}

pub fn normalize_existing_file_uri(
    root: &WorkspaceRoot,
    uri: &str,
) -> Result<(PathBuf, PathBuf, String), NormalizeError> {
    let path = uri_path(uri)?;
    let resolved = root.resolve_existing(&path)?;
    let metadata = fs::metadata(&resolved.canonical)
        .map_err(|error| map_path_error(&resolved.canonical, error))?;
    if !metadata.is_file() {
        return Err(NormalizeError::NotRegularFile(resolved.canonical));
    }
    let uri = path_to_file_uri(&resolved.canonical)?;
    Ok((resolved.canonical, resolved.relative, uri))
}

pub fn document_sync_options(initialized: &Value) -> DocumentSyncOptions {
    let sync = initialized
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("textDocumentSync"));
    match sync {
        Some(Value::Number(value)) => {
            let change = value
                .as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .filter(|value| *value <= 2)
                .unwrap_or(0);
            DocumentSyncOptions {
                // The numeric form is the legacy shorthand used by
                // rust-analyzer for a server that accepts open/close events.
                open_close: change != 0,
                change,
            }
        }
        Some(Value::Object(object)) => DocumentSyncOptions {
            open_close: object
                .get("openClose")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            change: object
                .get("change")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .filter(|value| *value <= 2)
                .unwrap_or(0),
        },
        _ => DocumentSyncOptions::default(),
    }
}

fn reject_parent_components(path: &Path) -> Result<(), NormalizeError> {
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(NormalizeError::ParentComponent(path.to_owned()));
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> Result<(), NormalizeError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(NormalizeError::ParentComponent(path.to_owned()));
            }
            Component::Normal(name) => {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(NormalizeError::Symlink(current));
                    }
                    Ok(_) => {}
                    Err(error) => return Err(map_path_error(&current, error)),
                }
            }
        }
    }
    Ok(())
}

fn map_path_error(path: &Path, error: io::Error) -> NormalizeError {
    if error.kind() == io::ErrorKind::NotFound {
        NormalizeError::NotFound(path.to_owned())
    } else {
        NormalizeError::Io {
            path: path.to_owned(),
            message: error.to_string(),
        }
    }
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + value - 10),
        _ => unreachable!("hex digit is four bits"),
    }
}

pub fn configuration_value(method: &str, params: &Value, deny_workspace_code: bool) -> Value {
    if method != "workspace/configuration" {
        return Value::Null;
    }
    let Some(items) = params.get("items").and_then(Value::as_array) else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        items
            .iter()
            .map(|item| configuration_item(item, deny_workspace_code))
            .collect(),
    )
}

fn configuration_item(item: &Value, deny_workspace_code: bool) -> Value {
    let section = item
        .get("section")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !deny_workspace_code {
        return if section == CHECK_ON_SAVE_KEY
            || section.ends_with(".checkOnSave")
            || section == "checkOnSave"
        {
            Value::Bool(false)
        } else {
            Value::Null
        };
    }
    match section {
        BUILD_SCRIPTS_ENABLE_KEY | "cargo.buildScripts.enable" => Value::Bool(false),
        PROC_MACRO_ENABLE_KEY | "procMacro.enable" => Value::Bool(false),
        CHECK_ON_SAVE_KEY | "checkOnSave" => Value::Bool(false),
        "rust-analyzer.cargo.buildScripts" | "cargo.buildScripts" => {
            serde_json::json!({"enable": false})
        }
        "rust-analyzer.procMacro" | "procMacro" => serde_json::json!({"enable": false}),
        _ => Value::Null,
    }
}
