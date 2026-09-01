//! Complete-marker guarded rustdoc cache built on the shared atomic publisher.

use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use cap_std::fs::{Dir, OpenOptions as CapabilityOpenOptions};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::{
    OwnedCacheRetention, PublishOptions, complete_marker_path, has_complete_marker, publish,
};

use super::html::{
    DOCS_MAX_HTML_BYTES, is_safe_page_path, package_folder_names, strip_rustdoc_html,
};

pub const DOCS_COMPLETE_MARKER: &str = ".complete.json";
const MAX_MARKER_BYTES: u64 = 16 * 1024;
const MAX_GENERATED_PAGES: usize = 2_000;
const MAX_GENERATED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GENERATION_LOCKS: usize = 256;
const MAX_GENERATION_LOCK_SCAN: usize = MAX_GENERATION_LOCKS * 4;
const GENERATION_LOCK_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheIdentity {
    pub crate_name: String,
    pub version: String,
    pub source: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPage {
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedPage {
    pub path: String,
    pub html: Vec<u8>,
}

pub struct PreparedGeneration {
    entry: PathBuf,
    directory: Dir,
    complete: bool,
    _generation_lock: File,
}

impl std::fmt::Debug for PreparedGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedGeneration")
            .field("entry", &self.entry)
            .finish_non_exhaustive()
    }
}

impl PreparedGeneration {
    pub fn target_dir(&self) -> PathBuf {
        self.entry.join("target")
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn validate(&self) -> Result<(), DocsCacheError> {
        validate_directory_identity(&self.entry, &self.directory)
    }
}

#[derive(Debug)]
pub enum DocsCacheError {
    InvalidRoot(PathBuf),
    UnsafePath(PathBuf),
    Io { path: PathBuf, message: String },
    Publication(String),
    Validation(String),
    Cancelled,
    DeadlineExceeded,
}

impl std::fmt::Display for DocsCacheError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRoot(path) => {
                write!(formatter, "docs cache root is invalid: {}", path.display())
            }
            Self::UnsafePath(path) => {
                write!(formatter, "unsafe docs cache path: {}", path.display())
            }
            Self::Io { path, message } => write!(
                formatter,
                "docs cache I/O failed for {}: {message}",
                path.display()
            ),
            Self::Publication(message) => {
                write!(formatter, "docs cache publication failed: {message}")
            }
            Self::Validation(message) => {
                write!(formatter, "docs cache validation failed: {message}")
            }
            Self::Cancelled => formatter.write_str("docs cache operation was cancelled"),
            Self::DeadlineExceeded => formatter.write_str("docs cache operation deadline elapsed"),
        }
    }
}

impl std::error::Error for DocsCacheError {}

#[derive(Debug, Clone)]
pub struct DocsCache {
    root: PathBuf,
    retention: OwnedCacheRetention,
}

impl DocsCache {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = absolute_path(root.as_ref());
        Self {
            retention: OwnedCacheRetention::new(&root),
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entry_path(&self, identity: &CacheIdentity) -> PathBuf {
        self.root.join(cache_key(identity))
    }

    pub fn read_page(
        &self,
        identity: &CacheIdentity,
        crate_name: &str,
        candidates: &[String],
    ) -> Option<CachedPage> {
        let entry = self.entry_path(identity);
        if !self.complete_identity_matches(&entry, identity) {
            return None;
        }
        let lease = self.retention.lease(&entry);
        let result = self.read_page_inner(&entry, crate_name, candidates);
        drop(lease);
        if result.is_some() {
            let _ = self.retention.touch(&entry);
        }
        result
    }

    pub fn is_complete(&self, identity: &CacheIdentity) -> bool {
        self.complete_identity_matches(&self.entry_path(identity), identity)
    }

    /// Prepare an incomplete entry for an isolated local documentation run.
    ///
    /// The generator receives a target directory inside this validated entry,
    /// so an invalid cache root is rejected before it can write through it.
    pub fn prepare_generation(
        &self,
        identity: &CacheIdentity,
    ) -> Result<PreparedGeneration, DocsCacheError> {
        self.prepare_generation_bounded(identity, None, None)
    }

    pub fn prepare_generation_bounded(
        &self,
        identity: &CacheIdentity,
        deadline: Option<Instant>,
        cancellation: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<PreparedGeneration, DocsCacheError> {
        ensure_directory(&self.root)?;
        let entry = self.entry_path(identity);
        let generation_lock = acquire_generation_lock(&self.root, &entry, deadline, cancellation)?;
        let lock_path = generation_lock_path(&self.root, &entry)?;
        cleanup_generation_locks(&self.root, &lock_path);
        let lease = self.retention.lease(&entry);
        self.retention.prune();
        ensure_directory(&entry)?;
        let directory = open_cache_directory(&entry)?;
        if self.complete_identity_matches(&entry, identity) {
            validate_directory_identity(&entry, &directory)?;
            drop(lease);
            return Ok(PreparedGeneration {
                entry,
                directory,
                complete: true,
                _generation_lock: generation_lock,
            });
        }
        clear_entry_contents(&entry, &directory)?;
        directory
            .create_dir("target")
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| DocsCacheError::Io {
                path: entry.join("target"),
                message: error.to_string(),
            })?;
        validate_directory_identity(&entry, &directory)?;
        drop(lease);
        Ok(PreparedGeneration {
            entry,
            directory,
            complete: false,
            _generation_lock: generation_lock,
        })
    }

    pub fn publish_pages(
        &self,
        identity: &CacheIdentity,
        crate_name: &str,
        pages: &[GeneratedPage],
        deadline: Option<Instant>,
    ) -> Result<PathBuf, DocsCacheError> {
        if pages.is_empty() {
            return Err(DocsCacheError::Validation(
                "no generated documentation pages".to_owned(),
            ));
        }
        if pages.len() > MAX_GENERATED_PAGES {
            return Err(DocsCacheError::Validation(format!(
                "generated documentation contains more than {MAX_GENERATED_PAGES} pages"
            )));
        }
        let mut total_bytes = 0u64;
        for page in pages {
            check_deadline(deadline)?;
            if !is_safe_page_path(&page.path) {
                return Err(DocsCacheError::UnsafePath(PathBuf::from(&page.path)));
            }
            if page.html.len() > DOCS_MAX_HTML_BYTES {
                return Err(DocsCacheError::Validation(format!(
                    "page {} exceeds {} bytes",
                    page.path, DOCS_MAX_HTML_BYTES
                )));
            }
            total_bytes = total_bytes.saturating_add(page.html.len() as u64);
            if total_bytes > MAX_GENERATED_BYTES {
                return Err(DocsCacheError::Validation(format!(
                    "generated documentation exceeds {MAX_GENERATED_BYTES} bytes"
                )));
            }
            if strip_rustdoc_html(std::str::from_utf8(&page.html).map_err(|_| {
                DocsCacheError::Validation(format!("page {} is not UTF-8", page.path))
            })?)
            .is_empty()
            {
                return Err(DocsCacheError::Validation(format!(
                    "page {} has no text",
                    page.path
                )));
            }
        }
        ensure_directory(&self.root)?;
        let entry = self.entry_path(identity);
        let lease = self.retention.lease(&entry);
        self.retention.prune();
        ensure_directory(&entry)?;
        let directory = open_cache_directory(&entry)?;
        if !self.complete_identity_matches(&entry, identity) {
            clear_entry_contents(&entry, &directory)?;
        }
        let package = package_folder_names(crate_name)
            .into_iter()
            .next()
            .unwrap_or_else(|| "_".to_owned());
        let mut published = 0usize;
        for page in pages {
            check_deadline(deadline)?;
            let final_path = entry.join("doc").join(&package).join(&page.path);
            let parent = final_path
                .parent()
                .ok_or_else(|| DocsCacheError::UnsafePath(final_path.clone()))?;
            ensure_directory(parent)?;
            publish(
                &final_path,
                &page.html,
                PublishOptions {
                    deadline,
                    temp_prefix: Some("docs"),
                    ..PublishOptions::default()
                },
                |path| {
                    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
                    if !metadata.is_file() || metadata.file_type().is_symlink() {
                        return Err("generated page is not a regular file".to_owned());
                    }
                    Ok::<(), String>(())
                },
            )
            .map_err(|error| map_publish_error(error.to_string()))?;
            published += 1;
        }
        check_deadline(deadline)?;
        if published == 0 {
            return Err(DocsCacheError::Validation(
                "no page was published".to_owned(),
            ));
        }
        let marker = serde_json::to_vec(&Marker {
            version: 1,
            identity: identity.clone(),
        })
        .map_err(|error| DocsCacheError::Validation(error.to_string()))?;
        let marker_path = complete_marker_path(&entry);
        publish(
            &marker_path,
            &marker,
            PublishOptions {
                deadline,
                temp_prefix: Some("docs-marker"),
                ..PublishOptions::default()
            },
            |path| {
                let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err("complete marker is not a regular file".to_owned());
                }
                if metadata.len() > MAX_MARKER_BYTES {
                    return Err("complete marker exceeded its limit".to_owned());
                }
                Ok::<(), String>(())
            },
        )
        .map_err(|error| map_publish_error(error.to_string()))?;
        self.retention.touch(&entry);
        drop(lease);
        Ok(entry)
    }

    fn complete_identity_matches(&self, entry: &Path, expected: &CacheIdentity) -> bool {
        if !has_complete_marker(entry) {
            return false;
        }
        let marker = complete_marker_path(entry);
        let Ok(bytes) = read_bounded(&marker, MAX_MARKER_BYTES) else {
            return false;
        };
        serde_json::from_slice::<Marker>(&bytes)
            .is_ok_and(|marker| marker.version == 1 && marker.identity == *expected)
    }

    fn read_page_inner(
        &self,
        entry: &Path,
        crate_name: &str,
        candidates: &[String],
    ) -> Option<CachedPage> {
        let folders = package_folder_names(crate_name);
        let roots = [
            entry.join("doc"),
            entry.join("target").join("doc"),
            entry.to_owned(),
        ];
        for root in roots {
            for folder in &folders {
                for candidate in candidates {
                    if !is_safe_page_path(candidate) {
                        continue;
                    }
                    let path = root.join(folder).join(candidate);
                    let Ok(bytes) = read_bounded_under(entry, &path, DOCS_MAX_HTML_BYTES as u64)
                    else {
                        continue;
                    };
                    let Ok(html) = std::str::from_utf8(&bytes) else {
                        continue;
                    };
                    let text = strip_rustdoc_html(html);
                    if !text.is_empty() {
                        return Some(CachedPage { path, text });
                    }
                }
            }
        }
        None
    }
}

fn acquire_generation_lock(
    root: &Path,
    entry: &Path,
    deadline: Option<Instant>,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> Result<File, DocsCacheError> {
    let lock_path = generation_lock_path(root, entry)?;
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(DocsCacheError::Validation(
                "local generation lock is not a regular file".to_owned(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DocsCacheError::Io {
                path: lock_path,
                message: error.to_string(),
            });
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(&lock_path)
        .map_err(|error| DocsCacheError::Io {
            path: lock_path.clone(),
            message: error.to_string(),
        })?;
    validate_lock_identity(&lock_path, &lock)?;
    loop {
        if cancellation.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return Err(DocsCacheError::Cancelled);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(DocsCacheError::DeadlineExceeded);
        }
        match FileExt::try_lock(&lock) {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(TryLockError::Error(error)) => {
                return Err(DocsCacheError::Io {
                    path: lock_path,
                    message: error.to_string(),
                });
            }
        }
    }
}

fn generation_lock_path(root: &Path, entry: &Path) -> Result<PathBuf, DocsCacheError> {
    let name = entry
        .file_name()
        .ok_or_else(|| DocsCacheError::InvalidRoot(entry.to_owned()))?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".generation.lock");
    Ok(root.join(lock_name))
}

fn cleanup_generation_locks(root: &Path, current_lock: &Path) {
    let Ok(directory) = open_cache_directory(root) else {
        return;
    };
    let Ok(entries) = directory.entries() else {
        return;
    };
    let now = SystemTime::now();
    let mut candidates = entries
        .take(MAX_GENERATION_LOCK_SCAN)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            if !name.to_string_lossy().ends_with(".generation.lock") {
                return None;
            }
            let path = root.join(&name);
            if path == current_lock {
                return None;
            }
            let metadata = directory.symlink_metadata(&name).ok()?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return None;
            }
            Some(GenerationLockCandidate {
                name,
                path,
                modified: metadata.modified().ok()?.into_std(),
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    let excess = candidates.len().saturating_sub(MAX_GENERATION_LOCKS - 1);
    for (index, candidate) in candidates.into_iter().enumerate() {
        let expired = now
            .duration_since(candidate.modified)
            .is_ok_and(|age| age >= GENERATION_LOCK_MAX_AGE);
        if !expired && index >= excess {
            continue;
        }
        remove_unlocked_generation_lock(&directory, &candidate);
    }
}

struct GenerationLockCandidate {
    name: std::ffi::OsString,
    path: PathBuf,
    modified: SystemTime,
}

fn remove_unlocked_generation_lock(directory: &Dir, candidate: &GenerationLockCandidate) {
    let mut options = CapabilityOpenOptions::new();
    options.read(true).write(true);
    let Ok(file) = directory
        .open_with(&candidate.name, &options)
        .map(cap_std::fs::File::into_std)
    else {
        return;
    };
    if validate_lock_identity(&candidate.path, &file).is_err() {
        return;
    }
    if !matches!(FileExt::try_lock(&file), Ok(())) {
        return;
    }
    let regular = directory
        .symlink_metadata(&candidate.name)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
    if regular {
        let _ = directory.remove_file(&candidate.name);
    }
    let _ = FileExt::unlock(&file);
}

fn validate_lock_identity(path: &Path, file: &File) -> Result<(), DocsCacheError> {
    let path_metadata = fs::symlink_metadata(path).map_err(|error| DocsCacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let file_metadata = file.metadata().map_err(|error| DocsCacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !file_metadata.is_file()
    {
        return Err(DocsCacheError::Validation(
            "local generation lock changed while opening".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(DocsCacheError::Validation(
                "local generation lock identity changed while opening".to_owned(),
            ));
        }
    }
    Ok(())
}

fn clear_entry_contents(entry: &Path, directory: &Dir) -> Result<(), DocsCacheError> {
    let entries = directory.entries().map_err(|error| DocsCacheError::Io {
        path: entry.to_owned(),
        message: error.to_string(),
    })?;
    for directory_entry in entries {
        let directory_entry = directory_entry.map_err(|error| DocsCacheError::Io {
            path: entry.to_owned(),
            message: error.to_string(),
        })?;
        let name = directory_entry.file_name();
        let path = entry.join(&name);
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|error| DocsCacheError::Io {
                path: path.clone(),
                message: error.to_string(),
            })?;
        if metadata.file_type().is_symlink() {
            return Err(DocsCacheError::UnsafePath(path));
        }
        if metadata.is_file() {
            directory
                .remove_file(&name)
                .map_err(|error| DocsCacheError::Io {
                    path,
                    message: error.to_string(),
                })?;
            continue;
        }
        if !metadata.is_dir() {
            return Err(DocsCacheError::UnsafePath(path));
        }
        let child = directory
            .open_dir(&name)
            .map_err(|error| DocsCacheError::Io {
                path: path.clone(),
                message: error.to_string(),
            })?;
        child
            .remove_open_dir_all()
            .map_err(|error| DocsCacheError::Io {
                path,
                message: error.to_string(),
            })?;
    }
    validate_directory_identity(entry, directory)?;
    Ok(())
}

fn open_cache_directory(path: &Path) -> Result<Dir, DocsCacheError> {
    let directory = Dir::open_ambient_dir(path, cap_std::ambient_authority()).map_err(|error| {
        DocsCacheError::Io {
            path: path.to_owned(),
            message: error.to_string(),
        }
    })?;
    validate_directory_identity(path, &directory)?;
    Ok(directory)
}

fn validate_directory_identity(path: &Path, directory: &Dir) -> Result<(), DocsCacheError> {
    let ambient = fs::symlink_metadata(path).map_err(|error| DocsCacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if ambient.file_type().is_symlink() || !ambient.is_dir() {
        return Err(DocsCacheError::UnsafePath(path.to_owned()));
    }
    let opened = directory
        .dir_metadata()
        .map_err(|error| DocsCacheError::Io {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if ambient.dev() != cap_std::fs::MetadataExt::dev(&opened)
            || ambient.ino() != cap_std::fs::MetadataExt::ino(&opened)
        {
            return Err(DocsCacheError::UnsafePath(path.to_owned()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    version: u8,
    identity: CacheIdentity,
}

fn cache_key(identity: &CacheIdentity) -> String {
    let encoded = serde_json::to_vec(identity).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(encoded);
    format!("{:x}", hasher.finalize())
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, DocsCacheError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| DocsCacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes {
        return Err(DocsCacheError::UnsafePath(path.to_owned()));
    }
    let mut file = fs::File::open(path).map_err(|error| DocsCacheError::Io {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| DocsCacheError::Io {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(DocsCacheError::UnsafePath(path.to_owned()));
    }
    Ok(bytes)
}

fn read_bounded_under(root: &Path, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DocsCacheError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| DocsCacheError::UnsafePath(path.to_owned()))?;
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return Err(DocsCacheError::UnsafePath(path.to_owned()));
    }
    let mut current = root.to_owned();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let Component::Normal(name) = component else {
            return Err(DocsCacheError::UnsafePath(path.to_owned()));
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current).map_err(|error| DocsCacheError::Io {
            path: current.clone(),
            message: error.to_string(),
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DocsCacheError::UnsafePath(current));
        }
    }
    read_bounded(path, max_bytes)
}

fn ensure_directory(path: &Path) -> Result<(), DocsCacheError> {
    if path.as_os_str().is_empty() {
        return Err(DocsCacheError::InvalidRoot(path.to_owned()));
    }
    let absolute = absolute_path(path);
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => return Err(DocsCacheError::UnsafePath(absolute)),
            Component::Normal(name) => current.push(name),
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(DocsCacheError::UnsafePath(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|error| DocsCacheError::Io {
                    path: current.clone(),
                    message: error.to_string(),
                })?;
            }
            Err(error) => {
                return Err(DocsCacheError::Io {
                    path: current,
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |directory| directory.join(path))
    }
}

fn check_deadline(deadline: Option<Instant>) -> Result<(), DocsCacheError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(DocsCacheError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

fn map_publish_error(message: String) -> DocsCacheError {
    if message.contains("cancelled") {
        DocsCacheError::Cancelled
    } else if message.contains("deadline") {
        DocsCacheError::DeadlineExceeded
    } else {
        DocsCacheError::Publication(message)
    }
}
