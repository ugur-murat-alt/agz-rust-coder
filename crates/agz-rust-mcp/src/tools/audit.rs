//! Bounded, source-write-free Rust pitfall audit.
//!
//! This module intentionally owns its public domain types.  The server can
//! later re-export them at its protocol boundary without making the scanner
//! depend on RMCP types or on a particular handler layout.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::{
    knowledge::pitfalls::{PatternId, PitfallDefinition, Severity, pattern_by_id},
    workspace::{ClientRoots, DirectoryEntryKind, RootError, RootGuard, WalkLimits, WorkspaceRoot},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_FILES: usize = 10_000;
const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_FINDINGS: usize = 200;
const MAX_REPORTED_SKIPS: usize = 256;

const IGNORED_DIRECTORIES: &[&str] = &[
    ".git",
    "target",
    "generated",
    "node_modules",
    "vendor",
    "test",
    "tests",
    "fixtures",
];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct AuditLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_findings: usize,
}

impl Default for AuditLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_findings: DEFAULT_MAX_FINDINGS,
        }
    }
}

impl AuditLimits {
    pub const fn new(
        max_files: usize,
        max_file_bytes: u64,
        max_total_bytes: u64,
        max_findings: usize,
    ) -> Self {
        Self {
            max_files,
            max_file_bytes,
            max_total_bytes,
            max_findings,
        }
    }

    pub fn from_u64(
        max_files: u64,
        max_file_bytes: u64,
        max_total_bytes: u64,
        max_findings: u64,
    ) -> Self {
        Self {
            max_files: usize::try_from(max_files).unwrap_or(usize::MAX),
            max_file_bytes,
            max_total_bytes,
            max_findings: usize::try_from(max_findings).unwrap_or(usize::MAX),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditRequest<'a> {
    pub root: &'a WorkspaceRoot,
    pub path: Option<PathBuf>,
}

impl<'a> AuditRequest<'a> {
    pub fn new(root: &'a WorkspaceRoot) -> Self {
        Self { root, path: None }
    }

    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AuditFinding {
    pub pattern: PatternId,
    pub severity: Severity,
    pub file: PathBuf,
    pub line: u32,
    pub snippet: String,
    pub fix: Option<&'static str>,
}

impl AuditFinding {
    pub fn pattern_id(&self) -> &'static str {
        self.pattern.as_str()
    }

    pub fn severity_name(&self) -> &'static str {
        self.severity.as_str()
    }

    pub fn definition(&self) -> &'static PitfallDefinition {
        pattern_by_id(self.pattern)
    }
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub enum AuditSkipReason {
    Generated,
    IgnoredPath,
    Symlink,
    Unreadable,
    NonRegular,
    FileTooLarge,
    FileLimit,
    ByteLimit,
    DepthLimit,
    InvalidUtf8,
}

impl fmt::Display for AuditSkipReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Generated => "generated",
            Self::IgnoredPath => "ignored path",
            Self::Symlink => "symlink",
            Self::Unreadable => "unreadable",
            Self::NonRegular => "not a regular file",
            Self::FileTooLarge => "file size budget",
            Self::FileLimit => "file count budget",
            Self::ByteLimit => "total byte budget",
            Self::DepthLimit => "walk depth budget",
            Self::InvalidUtf8 => "invalid UTF-8",
        };
        formatter.write_str(name)
    }
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct AuditSkip {
    pub path: PathBuf,
    pub reason: AuditSkipReason,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AuditSummary {
    pub scanned_files: u64,
    pub scanned_bytes: u64,
    pub findings: Vec<AuditFinding>,
    pub skipped: Vec<AuditSkip>,
    pub truncated: bool,
    pub skipped_truncated: bool,
}

impl AuditSummary {
    fn new() -> Self {
        Self {
            scanned_files: 0,
            scanned_bytes: 0,
            findings: Vec::new(),
            skipped: Vec::new(),
            truncated: false,
            skipped_truncated: false,
        }
    }

    pub fn finding_count(&self) -> usize {
        self.findings.len()
    }

    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    pub fn files(&self) -> u64 {
        self.scanned_files
    }

    pub fn bytes(&self) -> u64 {
        self.scanned_bytes
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum InvalidPathReason {
    Absolute,
    ParentComponent,
    Empty,
}

impl fmt::Display for InvalidPathReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absolute => formatter.write_str("absolute path is not allowed"),
            Self::ParentComponent => formatter.write_str("parent path components are not allowed"),
            Self::Empty => formatter.write_str("path cannot be empty"),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AuditError {
    Root(RootError),
    InvalidPath {
        path: PathBuf,
        reason: InvalidPathReason,
    },
    Cancelled(AuditCancellationReason),
    Worker(String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AuditCancellationReason {
    Request,
    RootEpoch,
    Shutdown,
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(error) => error.fmt(formatter),
            Self::InvalidPath { path, reason } => {
                write!(formatter, "invalid audit path {}: {reason}", path.display())
            }
            Self::Cancelled(reason) => write!(formatter, "audit was cancelled ({reason:?})"),
            Self::Worker(error) => write!(formatter, "audit worker failed: {error}"),
        }
    }
}

impl std::error::Error for AuditError {}

impl From<RootError> for AuditError {
    fn from(error: RootError) -> Self {
        Self::Root(error)
    }
}

#[derive(Debug, Clone)]
pub struct AuditCancellation {
    request: CancellationToken,
    root_epoch: CancellationToken,
    shutdown: CancellationToken,
}

impl AuditCancellation {
    pub fn new(
        request: CancellationToken,
        root_epoch: CancellationToken,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            request,
            root_epoch,
            shutdown,
        }
    }

    pub fn request(&self) -> &CancellationToken {
        &self.request
    }

    pub fn root_epoch(&self) -> &CancellationToken {
        &self.root_epoch
    }

    pub fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    fn reason(&self) -> Option<AuditCancellationReason> {
        if self.request.is_cancelled() {
            Some(AuditCancellationReason::Request)
        } else if self.root_epoch.is_cancelled() {
            Some(AuditCancellationReason::RootEpoch)
        } else if self.shutdown.is_cancelled() {
            Some(AuditCancellationReason::Shutdown)
        } else {
            None
        }
    }
}

impl Default for AuditCancellation {
    fn default() -> Self {
        Self::new(
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct AuditService {
    limits: AuditLimits,
}

impl Default for AuditService {
    fn default() -> Self {
        Self::new(AuditLimits::default())
    }
}

impl AuditService {
    pub const fn new(limits: AuditLimits) -> Self {
        Self { limits }
    }

    pub const fn limits(&self) -> AuditLimits {
        self.limits
    }

    pub fn audit(&self, request: &AuditRequest<'_>) -> Result<AuditSummary, AuditError> {
        self.audit_inner(request, None)
    }

    pub fn audit_with_cancellation(
        &self,
        request: &AuditRequest<'_>,
        cancellation: &AuditCancellation,
    ) -> Result<AuditSummary, AuditError> {
        self.audit_inner(request, Some(cancellation))
    }

    fn audit_inner(
        &self,
        request: &AuditRequest<'_>,
        cancellation: Option<&AuditCancellation>,
    ) -> Result<AuditSummary, AuditError> {
        check_cancellation(cancellation)?;
        match request.path.as_deref() {
            Some(path) => {
                validate_relative_path(path)?;
                self.audit_file(request.root, path, cancellation)
            }
            None => self.audit_tree(request.root, cancellation),
        }
    }

    /// Run a potentially long audit on Tokio's blocking pool.
    pub async fn scan_async(
        &self,
        root: WorkspaceRoot,
        path: Option<PathBuf>,
        cancellation: AuditCancellation,
    ) -> Result<AuditSummary, AuditError> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            service.scan_with_cancellation(&root, path.as_deref(), &cancellation)
        })
        .await
        .map_err(|error| AuditError::Worker(error.to_string()))?
    }

    pub fn scan_with_cancellation(
        &self,
        root: &WorkspaceRoot,
        path: Option<&Path>,
        cancellation: &AuditCancellation,
    ) -> Result<AuditSummary, AuditError> {
        let request = AuditRequest {
            root,
            path: path.map(Path::to_owned),
        };
        self.audit_with_cancellation(&request, cancellation)
    }

    /// Resolve an authorized directory through `RootGuard` before scanning it.
    pub fn audit_with_guard(
        &self,
        guard: &RootGuard,
        directory: Option<&Path>,
        path: Option<&Path>,
    ) -> Result<AuditSummary, AuditError> {
        let snapshot = guard.snapshot(ClientRoots::unsupported())?;
        let root = snapshot.select(directory)?;
        let request = AuditRequest {
            root: &root,
            path: path.map(Path::to_owned),
        };
        self.audit(&request)
    }

    pub fn scan(
        &self,
        root: &WorkspaceRoot,
        path: Option<&Path>,
    ) -> Result<AuditSummary, AuditError> {
        let request = AuditRequest {
            root,
            path: path.map(Path::to_owned),
        };
        self.audit(&request)
    }

    fn audit_tree(
        &self,
        root: &WorkspaceRoot,
        cancellation: Option<&AuditCancellation>,
    ) -> Result<AuditSummary, AuditError> {
        let mut summary = AuditSummary::new();

        let max_depth = WalkLimits::default().max_depth;
        let mut pending = vec![(PathBuf::new(), 0usize)];
        let mut accepted_files = 0usize;
        let mut total_bytes = 0u64;
        while let Some((relative, depth)) = pending.pop() {
            check_cancellation(cancellation)?;
            let entries = match root.list_directory(&relative) {
                Ok(entries) => entries,
                Err(_) => {
                    push_skip(&mut summary, relative, AuditSkipReason::Unreadable);
                    continue;
                }
            };
            for entry in entries {
                check_cancellation(cancellation)?;
                if entry.kind == DirectoryEntryKind::Directory && is_ignored_directory(&entry.name)
                {
                    continue;
                }
                let child = relative.join(&entry.name);
                match entry.kind {
                    DirectoryEntryKind::Symlink => {
                        if !is_ignored_path(&child) {
                            push_skip(&mut summary, child, AuditSkipReason::Symlink);
                        }
                    }
                    DirectoryEntryKind::Directory => {
                        if depth >= max_depth {
                            if !is_ignored_path(&child) {
                                summary.truncated = true;
                                push_skip(&mut summary, child, AuditSkipReason::DepthLimit);
                            }
                        } else {
                            pending.push((child, depth + 1));
                        }
                    }
                    DirectoryEntryKind::RegularFile => {
                        if !has_rs_extension(&child) || is_ignored_path(&child) {
                            continue;
                        }
                        if accepted_files >= self.limits.max_files {
                            summary.truncated = true;
                            push_skip(&mut summary, child, AuditSkipReason::FileLimit);
                            continue;
                        }
                        let bytes = match root.read_file(&child, self.limits.max_file_bytes) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                let reason = skip_reason_for_root_error(&error);
                                if matches!(
                                    reason,
                                    AuditSkipReason::FileTooLarge
                                        | AuditSkipReason::FileLimit
                                        | AuditSkipReason::ByteLimit
                                        | AuditSkipReason::DepthLimit
                                ) {
                                    summary.truncated = true;
                                }
                                push_skip(&mut summary, child, reason);
                                continue;
                            }
                        };
                        let size = bytes.len() as u64;
                        if total_bytes.saturating_add(size) > self.limits.max_total_bytes {
                            summary.truncated = true;
                            push_skip(&mut summary, child, AuditSkipReason::ByteLimit);
                            continue;
                        }
                        total_bytes = total_bytes.saturating_add(size);
                        accepted_files = accepted_files.saturating_add(1);
                        self.audit_source(child, bytes, &mut summary, cancellation)?;
                    }
                    DirectoryEntryKind::Other => {}
                }
            }
        }
        check_cancellation(cancellation)?;
        finish_summary(&mut summary);
        Ok(summary)
    }

    fn audit_file(
        &self,
        root: &WorkspaceRoot,
        path: &Path,
        cancellation: Option<&AuditCancellation>,
    ) -> Result<AuditSummary, AuditError> {
        check_cancellation(cancellation)?;
        let mut summary = AuditSummary::new();
        if is_ignored_path(path) {
            push_skip(&mut summary, path.to_owned(), AuditSkipReason::IgnoredPath);
            finish_summary(&mut summary);
            return Ok(summary);
        }
        if !has_rs_extension(path) {
            return Ok(summary);
        }
        self.audit_walk_file(root, path.to_owned(), &mut summary, cancellation)?;
        finish_summary(&mut summary);
        Ok(summary)
    }

    fn audit_walk_file(
        &self,
        root: &WorkspaceRoot,
        path: PathBuf,
        summary: &mut AuditSummary,
        cancellation: Option<&AuditCancellation>,
    ) -> Result<(), AuditError> {
        check_cancellation(cancellation)?;
        let bytes = match root.read_file(&path, self.limits.max_file_bytes) {
            Ok(bytes) => bytes,
            Err(error) => {
                push_skip(summary, path, skip_reason_for_root_error(&error));
                return Ok(());
            }
        };
        self.audit_source(path, bytes, summary, cancellation)
    }

    fn audit_source(
        &self,
        path: PathBuf,
        bytes: Vec<u8>,
        summary: &mut AuditSummary,
        cancellation: Option<&AuditCancellation>,
    ) -> Result<(), AuditError> {
        check_cancellation(cancellation)?;
        let Ok(source) = String::from_utf8(bytes) else {
            push_skip(summary, path, AuditSkipReason::InvalidUtf8);
            return Ok(());
        };
        let views = mask_source(&source);
        if is_generated_source(&views.comments) {
            push_skip(summary, path, AuditSkipReason::Generated);
            return Ok(());
        }

        summary.scanned_files = summary.scanned_files.saturating_add(1);
        summary.scanned_bytes = summary.scanned_bytes.saturating_add(source.len() as u64);
        let findings = findings_for_source(&source, &views, &path);
        let remaining = self
            .limits
            .max_findings
            .saturating_sub(summary.findings.len());
        if findings.len() > remaining {
            summary.truncated = true;
        }
        summary
            .findings
            .extend(findings.into_iter().take(remaining));
        check_cancellation(cancellation)
    }
}

fn check_cancellation(cancellation: Option<&AuditCancellation>) -> Result<(), AuditError> {
    if let Some(reason) = cancellation.and_then(AuditCancellation::reason) {
        return Err(AuditError::Cancelled(reason));
    }
    Ok(())
}

fn is_ignored_directory(name: &OsStr) -> bool {
    IGNORED_DIRECTORIES
        .iter()
        .any(|ignored| name == OsStr::new(ignored))
}

fn skip_reason_for_root_error(error: &RootError) -> AuditSkipReason {
    match error {
        RootError::Symlink(_) => AuditSkipReason::Symlink,
        RootError::NotRegularFile(_) => AuditSkipReason::NonRegular,
        RootError::TooLarge { .. } => AuditSkipReason::FileTooLarge,
        RootError::PathNotFound(_) | RootError::Io { .. } | RootError::Poisoned => {
            AuditSkipReason::Unreadable
        }
        RootError::PathOutsideRoot(_)
        | RootError::NotDirectory(_)
        | RootError::RelativePath
        | RootError::AbsolutePath
        | RootError::ParentComponent
        | RootError::InvalidPath(_)
        | RootError::InvalidFileUri(_)
        | RootError::EmptyConfiguredRoots
        | RootError::ClientRootsEmpty
        | RootError::ClientRootsUnavailable
        | RootError::NoRootIntersection
        | RootError::MultipleRoots => AuditSkipReason::Unreadable,
    }
}

fn push_skip(summary: &mut AuditSummary, path: PathBuf, reason: AuditSkipReason) {
    if summary
        .skipped
        .iter()
        .any(|skip| skip.path == path && skip.reason == reason)
    {
        return;
    }
    if summary.skipped.len() >= MAX_REPORTED_SKIPS {
        summary.skipped_truncated = true;
        summary.truncated = true;
        return;
    }
    summary.skipped.push(AuditSkip { path, reason });
}

fn finish_summary(summary: &mut AuditSummary) {
    summary.skipped.sort();
    summary.findings.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.line.cmp(&right.line))
            .then(left.pattern.cmp(&right.pattern))
            .then(left.snippet.cmp(&right.snippet))
    });
}

fn validate_relative_path(path: &Path) -> Result<(), AuditError> {
    if path.as_os_str().is_empty() {
        return Err(AuditError::InvalidPath {
            path: path.to_owned(),
            reason: InvalidPathReason::Empty,
        });
    }
    if path.is_absolute() {
        return Err(AuditError::InvalidPath {
            path: path.to_owned(),
            reason: InvalidPathReason::Absolute,
        });
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(AuditError::InvalidPath {
                    path: path.to_owned(),
                    reason: InvalidPathReason::ParentComponent,
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(AuditError::InvalidPath {
                    path: path.to_owned(),
                    reason: InvalidPathReason::Absolute,
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn has_rs_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == OsStr::new("rs"))
}

fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        name.to_string_lossy().starts_with('.')
            || IGNORED_DIRECTORIES
                .iter()
                .any(|ignored| name == OsStr::new(ignored))
    })
}

#[derive(Debug)]
struct SourceViews {
    code: String,
    comments: String,
}

fn mask_source(source: &str) -> SourceViews {
    let bytes = source.as_bytes();
    let mut code = bytes.to_vec();
    let mut comments = vec![b' '; bytes.len()];
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            comments[index] = b'\n';
        }
    }

    let mut index = 0;
    let mut block_depth = 0usize;
    while index < bytes.len() {
        if block_depth > 0 {
            if starts_with(bytes, index, b"/*") {
                mask_range(&mut code, index, index.saturating_add(2));
                copy_comment_range(&mut comments, bytes, index, index.saturating_add(2));
                block_depth = block_depth.saturating_add(1);
                index = index.saturating_add(2);
            } else if starts_with(bytes, index, b"*/") {
                mask_range(&mut code, index, index.saturating_add(2));
                copy_comment_range(&mut comments, bytes, index, index.saturating_add(2));
                block_depth = block_depth.saturating_sub(1);
                index = index.saturating_add(2);
            } else {
                mask_byte(&mut code, index);
                copy_comment_byte(&mut comments, bytes, index);
                index += 1;
            }
            continue;
        }

        if starts_with(bytes, index, b"//") {
            let mut end = index.saturating_add(2);
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            mask_range(&mut code, index, end);
            copy_comment_range(&mut comments, bytes, index, end);
            index = end;
            continue;
        }
        if starts_with(bytes, index, b"/*") {
            mask_range(&mut code, index, index.saturating_add(2));
            copy_comment_range(&mut comments, bytes, index, index.saturating_add(2));
            block_depth = 1;
            index = index.saturating_add(2);
            continue;
        }
        if let Some((start, end)) = raw_string_range(bytes, index) {
            mask_range(&mut code, start, end);
            index = end;
            continue;
        }
        if let Some((start, end)) = normal_string_range(bytes, index) {
            mask_range(&mut code, start, end);
            index = end;
            continue;
        }
        if let Some(end) = character_literal_end(bytes, index) {
            mask_range(&mut code, index, end);
            index = end;
            continue;
        }
        index += 1;
    }

    SourceViews {
        code: String::from_utf8_lossy(&code).into_owned(),
        comments: String::from_utf8_lossy(&comments).into_owned(),
    }
}

fn starts_with(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes
        .get(index..index.saturating_add(needle.len()))
        .is_some_and(|candidate| candidate == needle)
}

fn mask_byte(code: &mut [u8], index: usize) {
    if code.get(index).copied() != Some(b'\n') {
        code[index] = b' ';
    }
}

fn mask_range(code: &mut [u8], start: usize, end: usize) {
    for index in start..end.min(code.len()) {
        mask_byte(code, index);
    }
}

fn copy_comment_byte(comments: &mut [u8], source: &[u8], index: usize) {
    if source.get(index).copied() != Some(b'\n') {
        comments[index] = source[index];
    }
}

fn copy_comment_range(comments: &mut [u8], source: &[u8], start: usize, end: usize) {
    for index in start..end.min(source.len()) {
        copy_comment_byte(comments, source, index);
    }
}

fn raw_string_range(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let raw_index = if bytes.get(index) == Some(&b'r') {
        index
    } else if matches!(bytes.get(index), Some(b'b' | b'c'))
        && bytes.get(index.saturating_add(1)) == Some(&b'r')
    {
        index.saturating_add(1)
    } else {
        return None;
    };
    let mut quote = raw_index.saturating_add(1);
    let mut hashes = 0usize;
    while bytes.get(quote) == Some(&b'#') && hashes <= 255 {
        hashes += 1;
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }

    let mut end = quote.saturating_add(1);
    while end < bytes.len() {
        if bytes[end] == b'"'
            && (0..hashes).all(|offset| bytes.get(end.saturating_add(1 + offset)) == Some(&b'#'))
        {
            return Some((index, end.saturating_add(1 + hashes)));
        }
        end += 1;
    }
    Some((index, bytes.len()))
}

fn normal_string_range(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let quote = if bytes.get(index) == Some(&b'"') {
        index
    } else if matches!(bytes.get(index), Some(b'b' | b'c'))
        && bytes.get(index.saturating_add(1)) == Some(&b'"')
    {
        index.saturating_add(1)
    } else {
        return None;
    };

    let mut end = quote.saturating_add(1);
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end = end.saturating_add(2),
            b'"' => return Some((index, end.saturating_add(1))),
            _ => end += 1,
        }
    }
    Some((index, bytes.len()))
}

fn character_literal_end(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index) != Some(&b'\'') {
        return None;
    }
    let mut end = index.saturating_add(1);
    if bytes.get(end) == Some(&b'\\') {
        end = end.saturating_add(2);
    }
    while end < bytes.len() && bytes[end] != b'\'' && bytes[end] != b'\n' {
        end += 1;
    }
    (bytes.get(end) == Some(&b'\'')).then_some(end.saturating_add(1))
}

fn is_generated_source(comments: &str) -> bool {
    let lower = comments.to_ascii_lowercase();
    let markers = [
        "@generated",
        "automatically generated",
        "code is generated",
        "file is generated",
    ];
    markers.iter().any(|marker| {
        lower.match_indices(marker).any(|(start, _)| {
            lower
                .get(start..start.saturating_add(220).min(lower.len()))
                .is_some_and(|tail| tail.contains("do not edit"))
        })
    })
}

fn findings_for_source(source: &str, views: &SourceViews, file: &Path) -> Vec<AuditFinding> {
    let test_ranges = cfg_test_ranges(&views.code);
    let async_outside_tests = has_async_outside_tests(&views.code, &test_ranges);
    let mut findings = Vec::new();
    for &pattern_id in PatternId::all() {
        if pattern_id == PatternId::StdMutexAwait && !async_outside_tests {
            continue;
        }
        let searchable = if pattern_id == PatternId::CasualSafetyComment {
            &views.comments
        } else {
            &views.code
        };
        for candidate in matches_for_pattern(pattern_id, searchable) {
            if inside_ranges(candidate.offset, &test_ranges) {
                continue;
            }
            let definition = pattern_by_id(pattern_id);
            findings.push(AuditFinding {
                pattern: pattern_id,
                severity: definition.severity,
                file: file.to_owned(),
                line: line_at(source, candidate.offset),
                snippet: candidate.snippet,
                fix: Some(definition.fix),
            });
        }
    }
    findings.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then(left.pattern.cmp(&right.pattern))
            .then(left.snippet.cmp(&right.snippet))
    });
    findings
}

#[derive(Debug)]
struct Candidate {
    offset: usize,
    snippet: String,
}

fn matches_for_pattern(pattern: PatternId, source: &str) -> Vec<Candidate> {
    match pattern {
        PatternId::CloneTax => matches_clone_tax(source),
        PatternId::Unwrap => matches_literals(source, &[".unwrap(", ".expect("]),
        PatternId::StringParam => matches_parameter(source, "String", false),
        PatternId::VecParam => matches_parameter(source, "Vec", true),
        PatternId::PathBufParam => matches_parameter(source, "PathBuf", false),
        PatternId::IndexLoop => matches_index_loops(source),
        PatternId::ArcMutexStack => matches_arc_mutex(source),
        PatternId::StdMutexAwait => matches_literals(source, &["std::sync::Mutex"]),
        PatternId::UnsafeBlock => matches_unsafe_blocks(source),
        PatternId::CasualSafetyComment => matches_casual_safety_comments(source),
    }
}

fn matches_literals(source: &str, needles: &[&str]) -> Vec<Candidate> {
    let mut matches = Vec::new();
    for needle in needles {
        matches.extend(
            source
                .match_indices(needle)
                .map(|(offset, text)| Candidate {
                    offset,
                    snippet: text.to_owned(),
                }),
        );
    }
    matches.sort_by_key(|candidate| candidate.offset);
    matches
}

fn matches_clone_tax(source: &str) -> Vec<Candidate> {
    source
        .match_indices(".clone()")
        .filter(|(offset, _)| {
            !["Arc::", "Rc::", "Weak::"].iter().any(|prefix| {
                source
                    .get(..*offset)
                    .is_some_and(|before| before.ends_with(prefix))
            })
        })
        .map(|(offset, text)| Candidate {
            offset,
            snippet: text.to_owned(),
        })
        .collect()
}

fn matches_parameter(source: &str, type_name: &str, needs_angle: bool) -> Vec<Candidate> {
    let bytes = source.as_bytes();
    let mut matches = Vec::new();
    for (offset, _) in source.match_indices(':') {
        let mut end = offset.saturating_add(1);
        let whitespace = skip_whitespace(bytes, end);
        if whitespace == end || bytes.get(whitespace) != Some(&b'&') {
            continue;
        }
        end = skip_whitespace(bytes, whitespace.saturating_add(1));
        if !source
            .get(end..)
            .is_some_and(|tail| tail.starts_with(type_name))
        {
            continue;
        }
        let type_end = end.saturating_add(type_name.len());
        if bytes.get(type_end).is_some_and(|byte| is_word_byte(*byte)) {
            continue;
        }
        if needs_angle {
            let angle = skip_whitespace(bytes, type_end);
            if bytes.get(angle) != Some(&b'<') {
                continue;
            }
            end = angle.saturating_add(1);
        } else {
            end = type_end;
        }
        matches.push(Candidate {
            offset,
            snippet: source[offset..end].to_owned(),
        });
    }
    matches
}

fn matches_index_loops(source: &str) -> Vec<Candidate> {
    let bytes = source.as_bytes();
    let mut matches = Vec::new();
    for (offset, _) in source.match_indices("for") {
        if !word_at(bytes, offset, "for") {
            continue;
        }
        let mut end = require_whitespace(bytes, offset.saturating_add(3));
        if end == offset.saturating_add(3) {
            continue;
        }
        let variable_start = end;
        while bytes.get(end).is_some_and(|byte| is_word_byte(*byte)) {
            end += 1;
        }
        if end == variable_start {
            continue;
        }
        let after_variable = end;
        end = require_whitespace(bytes, end);
        if end == after_variable || !word_at(bytes, end, "in") {
            continue;
        }
        end = end.saturating_add(2);
        let after_in = end;
        end = require_whitespace(bytes, end);
        if end == after_in || !starts_with(bytes, end, b"0..") {
            continue;
        }
        end = end.saturating_add(3);
        let collection_start = end;
        while bytes.get(end).is_some_and(|byte| is_word_byte(*byte)) {
            end += 1;
        }
        if end == collection_start || !starts_with(bytes, end, b".len()") {
            continue;
        }
        end = end.saturating_add(6);
        matches.push(Candidate {
            offset,
            snippet: source[offset..end].to_owned(),
        });
    }
    matches
}

fn matches_arc_mutex(source: &str) -> Vec<Candidate> {
    let bytes = source.as_bytes();
    let mut matches = Vec::new();
    for (offset, _) in source.match_indices("Arc") {
        if !word_at(bytes, offset, "Arc") {
            continue;
        }
        let mut end = skip_whitespace(bytes, offset.saturating_add(3));
        if bytes.get(end) != Some(&b'<') {
            continue;
        }
        end = skip_whitespace(bytes, end.saturating_add(1));
        if !word_at(bytes, end, "Mutex") {
            continue;
        }
        end = skip_whitespace(bytes, end.saturating_add(5));
        if bytes.get(end) != Some(&b'<') {
            continue;
        }
        end += 1;
        matches.push(Candidate {
            offset,
            snippet: source[offset..end].to_owned(),
        });
    }
    matches
}

fn matches_unsafe_blocks(source: &str) -> Vec<Candidate> {
    let bytes = source.as_bytes();
    let mut matches = Vec::new();
    for (offset, _) in source.match_indices("unsafe") {
        if !word_at(bytes, offset, "unsafe") {
            continue;
        }
        let end = skip_whitespace(bytes, offset.saturating_add(6));
        if bytes.get(end) != Some(&b'{') {
            continue;
        }
        matches.push(Candidate {
            offset,
            snippet: source[offset..end.saturating_add(1)].to_owned(),
        });
    }
    matches
}

fn matches_casual_safety_comments(source: &str) -> Vec<Candidate> {
    let lower = source.to_ascii_lowercase();
    let mut matches = Vec::new();
    for (offset, _) in lower.match_indices("safety:") {
        let end = source[offset..]
            .find('\n')
            .map_or(source.len(), |relative| offset.saturating_add(relative));
        let line = &lower[offset..end];
        if ["caller", "guaranteed", "obviously", "trivially"]
            .iter()
            .any(|word| line.contains(word))
        {
            matches.push(Candidate {
                offset,
                snippet: source[offset..end].to_owned(),
            });
        }
    }
    matches
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn require_whitespace(bytes: &[u8], index: usize) -> usize {
    if !bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        return index;
    }
    skip_whitespace(bytes, index)
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn word_at(bytes: &[u8], offset: usize, word: &str) -> bool {
    let word_bytes = word.as_bytes();
    if !starts_with(bytes, offset, word_bytes) {
        return false;
    }
    let before_is_word = offset
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| is_word_byte(*byte));
    let end = offset.saturating_add(word_bytes.len());
    let after_is_word = bytes.get(end).is_some_and(|byte| is_word_byte(*byte));
    !before_is_word && !after_is_word
}

fn line_at(source: &str, offset: usize) -> u32 {
    let line = source
        .as_bytes()
        .get(..offset.min(source.len()))
        .map_or(1usize, |prefix| {
            1usize.saturating_add(prefix.iter().fold(0usize, |count, byte| {
                if *byte == b'\n' { count + 1 } else { count }
            }))
        });
    u32::try_from(line).unwrap_or(u32::MAX)
}

fn cfg_test_ranges(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'#' {
            index += 1;
            continue;
        }
        let Some(attribute_end) = cfg_test_attribute_end(bytes, index) else {
            index += 1;
            continue;
        };
        let Some(open) = cfg_test_module_open(bytes, attribute_end) else {
            index = attribute_end;
            continue;
        };
        if let Some(close) = matching_brace(bytes, open) {
            ranges.push((index, close));
            index = close.saturating_add(1);
        } else {
            index = attribute_end;
        }
    }
    ranges
}

fn cfg_test_attribute_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start.saturating_add(1);
    index = skip_whitespace(bytes, index);
    if bytes.get(index) != Some(&b'[') {
        return None;
    }
    index = skip_whitespace(bytes, index.saturating_add(1));
    if !word_at(bytes, index, "cfg") {
        return None;
    }
    index = skip_whitespace(bytes, index.saturating_add(3));
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    index = skip_whitespace(bytes, index.saturating_add(1));
    if !word_at(bytes, index, "test") {
        return None;
    }
    index = skip_whitespace(bytes, index.saturating_add(4));
    if bytes.get(index) != Some(&b')') {
        return None;
    }
    index = skip_whitespace(bytes, index.saturating_add(1));
    (bytes.get(index) == Some(&b']')).then_some(index.saturating_add(1))
}

fn cfg_test_module_open(bytes: &[u8], attribute_end: usize) -> Option<usize> {
    let mut index = skip_whitespace(bytes, attribute_end);
    if word_at(bytes, index, "pub") {
        index = index.saturating_add(3);
        index = skip_whitespace(bytes, index);
        if bytes.get(index) == Some(&b'(') {
            let mut depth = 1usize;
            index = index.saturating_add(1);
            while index < bytes.len() && depth > 0 {
                match bytes[index] {
                    b'(' => depth = depth.saturating_add(1),
                    b')' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                index += 1;
            }
        }
        index = skip_whitespace(bytes, index);
    }
    if !word_at(bytes, index, "mod") {
        return None;
    }
    index = index.saturating_add(3);
    index = skip_whitespace(bytes, index);
    if !bytes.get(index).is_some_and(|byte| is_word_byte(*byte)) {
        return None;
    }
    while bytes.get(index).is_some_and(|byte| is_word_byte(*byte)) {
        index += 1;
    }
    index = skip_whitespace(bytes, index);
    (bytes.get(index) == Some(&b'{')).then_some(index)
}

fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth = depth.saturating_add(1),
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn has_async_outside_tests(source: &str, ranges: &[(usize, usize)]) -> bool {
    let bytes = source.as_bytes();
    source
        .match_indices("async")
        .any(|(offset, _)| word_at(bytes, offset, "async") && !inside_ranges(offset, ranges))
        || source.match_indices(".await").any(|(offset, _)| {
            let end = offset.saturating_add(6);
            bytes.get(end).is_none_or(|byte| !is_word_byte(*byte)) && !inside_ranges(offset, ranges)
        })
}

fn inside_ranges(offset: usize, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|(start, end)| offset >= *start && offset <= *end)
}
