#![allow(clippy::struct_field_names)]

//! Bounded retention for a plugin-owned cache directory.
//!
//! Only direct, non-symlink children of the configured root are candidates.
//! Recursive accounting uses `symlink_metadata`, so a symlink is never
//! traversed. Entries containing a symlink or an uninspectable node are kept
//! rather than risking deletion outside the owned tree.

use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime},
};

use super::atomic::{COMPLETE_MARKER, has_complete_marker};

const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_ENTRIES: usize = 256;
const DEFAULT_MAX_NODES: usize = 100_000;
const MAX_SCAN_NODES: usize = 100_000;

/// Quotas applied to non-active, plugin-owned direct children.
#[derive(Debug, Clone, Copy)]
pub struct RetentionLimits {
    /// Entries older than this are removed before quota enforcement.
    pub max_age: Duration,
    /// Aggregate logical file bytes retained under direct children.
    pub max_bytes: u64,
    /// Maximum number of direct entries retained.
    pub max_entries: usize,
    /// Maximum aggregate regular-file and directory nodes retained.
    pub max_nodes: usize,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            max_age: DEFAULT_MAX_AGE,
            max_bytes: DEFAULT_MAX_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_nodes: DEFAULT_MAX_NODES,
        }
    }
}

/// The result of one best-effort retention pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Number of direct entries removed by this pass.
    pub removed: usize,
    /// Number of direct entries still visible after this pass.
    pub retained: usize,
    /// Aggregate logical bytes observed after this pass.
    pub bytes: u64,
    /// Aggregate regular-file and directory nodes observed after this pass.
    pub nodes: usize,
}

/// A cloneable retention coordinator whose active leases are process-local.
#[derive(Clone, Debug)]
pub struct OwnedCacheRetention {
    root: PathBuf,
    limits: RetentionLimits,
    active: Arc<Mutex<HashMap<PathBuf, usize>>>,
    touched: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
}

impl OwnedCacheRetention {
    /// Creates a retention coordinator with the documented default quotas.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self::with_limits(root, RetentionLimits::default())
    }

    /// Creates a retention coordinator with explicit age, LRU, byte, entry,
    /// and node budgets.
    pub fn with_limits(root: impl AsRef<Path>, limits: RetentionLimits) -> Self {
        Self {
            root: absolute_path(root.as_ref()),
            limits,
            active: Arc::new(Mutex::new(HashMap::new())),
            touched: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns the lexically absolute plugin-owned root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Acquires an active lease for one direct child.
    ///
    /// Invalid paths and symlink children receive a no-op lease. Retention is
    /// advisory, so refusing an unverifiable lease is safer than widening its
    /// scope.
    pub fn lease(&self, path: impl AsRef<Path>) -> CacheLease {
        if !safe_owned_root(&self.root) {
            return CacheLease::noop();
        }
        let Some(entry) = self.safe_direct_entry(path.as_ref()) else {
            return CacheLease::noop();
        };
        if let Ok(metadata) = fs::symlink_metadata(&entry)
            && (metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()))
        {
            return CacheLease::noop();
        }

        let mut active = lock_unpoisoned(&self.active);
        *active.entry(entry.clone()).or_insert(0) += 1;
        CacheLease {
            path: Some(entry),
            active: Arc::clone(&self.active),
            released: false,
        }
    }

    /// Records an in-process LRU touch without following the entry.
    pub fn touch(&self, path: impl AsRef<Path>) -> bool {
        if !safe_owned_root(&self.root) {
            return false;
        }
        let Some(entry) = self.safe_direct_entry(path.as_ref()) else {
            return false;
        };
        let Ok(metadata) = fs::symlink_metadata(&entry) else {
            return false;
        };
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return false;
        }
        lock_unpoisoned(&self.touched).insert(entry, SystemTime::now());
        true
    }

    /// Runs retention using the current wall clock for age comparisons.
    pub fn prune(&self) -> RetentionReport {
        self.prune_at(SystemTime::now())
    }

    /// Deterministic form of [`Self::prune`] for tests and callers with a
    /// captured clock.
    pub fn prune_at(&self, now: SystemTime) -> RetentionReport {
        // Holding this guard through deletion makes lease acquisition and
        // candidate removal one process-local critical section.
        let active = lock_unpoisoned(&self.active);
        if !safe_owned_root(&self.root) {
            return RetentionReport::default();
        }

        let mut removed = 0;
        for entry in self.entries() {
            if Self::is_active_locked(&entry.path, &active)
                || !is_old(entry.last_used, now, self.limits.max_age)
            {
                continue;
            }
            if remove_entry(&entry.path) {
                removed += 1;
                lock_unpoisoned(&self.touched).remove(&entry.path);
            }
        }

        let mut entries = self.entries();
        let mut bytes = entries
            .iter()
            .map(|entry| entry.stats.bytes)
            .fold(0, u64::saturating_add);
        let mut nodes = entries
            .iter()
            .map(|entry| entry.stats.nodes)
            .fold(0, usize::saturating_add);
        let mut entry_count = entries.len();

        entries.sort_by(|left, right| {
            right
                .last_used
                .cmp(&left.last_used)
                .then_with(|| left.path.cmp(&right.path))
        });

        for entry in entries.into_iter().rev() {
            let over_budget = entry_count > self.limits.max_entries
                || bytes > self.limits.max_bytes
                || nodes > self.limits.max_nodes;
            if !over_budget || Self::is_active_locked(&entry.path, &active) {
                continue;
            }
            if remove_entry(&entry.path) {
                removed += 1;
                entry_count = entry_count.saturating_sub(1);
                bytes = bytes.saturating_sub(entry.stats.bytes);
                nodes = nodes.saturating_sub(entry.stats.nodes);
                lock_unpoisoned(&self.touched).remove(&entry.path);
            }
        }

        let final_entries = self.entries();
        RetentionReport {
            removed,
            retained: final_entries.len(),
            bytes: final_entries
                .iter()
                .map(|entry| entry.stats.bytes)
                .fold(0, u64::saturating_add),
            nodes: final_entries
                .iter()
                .map(|entry| entry.stats.nodes)
                .fold(0, usize::saturating_add),
        }
    }

    fn entries(&self) -> Vec<CacheEntry> {
        if !safe_owned_root(&self.root) {
            return Vec::new();
        }
        let touched = lock_unpoisoned(&self.touched);
        let Ok(children) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        children
            .flatten()
            .filter_map(|child| {
                let path = child.path();
                let name = path.file_name()?.to_string_lossy();
                if is_reserved_name(&name) {
                    return None;
                }
                let metadata = fs::symlink_metadata(&path).ok()?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || !has_complete_marker(&path)
                {
                    return None;
                }
                let stats = inspect_tree(&path);
                let mtime = metadata.modified().ok()?;
                let last_used = touched
                    .get(&path)
                    .copied()
                    .map_or(mtime, |touch| touch.max(mtime));
                Some(CacheEntry {
                    path,
                    stats,
                    last_used,
                })
            })
            .collect()
    }

    fn safe_direct_entry(&self, path: &Path) -> Option<PathBuf> {
        let candidate = absolute_path(path);
        let relative = candidate.strip_prefix(&self.root).ok()?;
        let mut components = relative.components();
        let Component::Normal(_) = components.next()? else {
            return None;
        };
        if components.next().is_some() {
            return None;
        }
        Some(candidate)
    }

    fn is_active_locked(path: &Path, active: &MutexGuard<'_, HashMap<PathBuf, usize>>) -> bool {
        active.get(path).is_some_and(|count| *count > 0)
    }
}

/// An RAII active-entry lease.
#[derive(Debug)]
pub struct CacheLease {
    path: Option<PathBuf>,
    active: Arc<Mutex<HashMap<PathBuf, usize>>>,
    released: bool,
}

impl CacheLease {
    fn noop() -> Self {
        Self {
            path: None,
            active: Arc::new(Mutex::new(HashMap::new())),
            released: true,
        }
    }

    /// Releases the lease immediately. Dropping it has the same effect.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let Some(path) = self.path.take() else {
            return;
        };
        let mut active = lock_unpoisoned(&self.active);
        let Some(count) = active.get_mut(&path) else {
            return;
        };
        if *count <= 1 {
            active.remove(&path);
        } else {
            *count -= 1;
        }
    }

    /// Returns the leased direct child, if this is not a no-op lease.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, Copy)]
struct TreeStats {
    bytes: u64,
    nodes: usize,
    contains_symlink: bool,
    safe_to_remove: bool,
}

#[derive(Debug)]
struct CacheEntry {
    path: PathBuf,
    stats: TreeStats,
    last_used: SystemTime,
}

fn inspect_tree(root: &Path) -> TreeStats {
    let mut stack = vec![root.to_path_buf()];
    let mut stats = TreeStats {
        bytes: 0,
        nodes: 0,
        contains_symlink: false,
        safe_to_remove: true,
    };

    while let Some(path) = stack.pop() {
        if stats.nodes >= MAX_SCAN_NODES {
            stats.safe_to_remove = false;
            break;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            stats.safe_to_remove = false;
            continue;
        };
        if metadata.file_type().is_symlink() {
            stats.contains_symlink = true;
            stats.safe_to_remove = false;
            continue;
        }
        if metadata.is_file() {
            stats.nodes += 1;
            stats.bytes = stats.bytes.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            stats.nodes += 1;
            let Ok(children) = fs::read_dir(&path) else {
                stats.safe_to_remove = false;
                continue;
            };
            for child in children {
                match child {
                    Ok(child) => stack.push(child.path()),
                    Err(_) => stats.safe_to_remove = false,
                }
            }
        } else {
            stats.safe_to_remove = false;
        }
    }

    stats
}

fn remove_entry(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return false;
    }
    if metadata.is_file() {
        return fs::remove_file(path).is_ok();
    }
    if !metadata.is_dir() {
        return false;
    }
    let stats = inspect_tree(path);
    if !stats.safe_to_remove || stats.contains_symlink {
        return false;
    }
    remove_directory(path)
}

fn remove_directory(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    let Ok(children) = fs::read_dir(path) else {
        return false;
    };
    for child in children {
        let Ok(child) = child else {
            return false;
        };
        let child_path = child.path();
        let Ok(metadata) = fs::symlink_metadata(&child_path) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            // Never unlink a symlink, even when it appeared after accounting.
            return false;
        }
        let removed = if metadata.is_dir() {
            remove_directory(&child_path)
        } else if metadata.is_file() {
            fs::remove_file(&child_path).is_ok()
        } else {
            false
        };
        if !removed {
            return false;
        }
    }
    fs::remove_dir(path).is_ok()
}

fn is_old(last_used: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    now.duration_since(last_used)
        .is_ok_and(|age| age >= max_age)
}

fn is_reserved_name(name: &str) -> bool {
    name == COMPLETE_MARKER || name.as_bytes().ends_with(b".lock") || name.contains(".tmp.")
}

fn safe_owned_root(root: &Path) -> bool {
    if !verify_directory_path(root) {
        return false;
    }
    match fs::symlink_metadata(root) {
        Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

fn verify_directory_path(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                // A Windows drive prefix is not a complete root until RootDir.
                current.push(prefix.as_os_str());
                continue;
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => return false,
            Component::Normal(name) => current.push(name),
        }
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            return false;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return false;
        }
    }
    true
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_or_else(|_| path.to_path_buf(), |directory| directory.join(path))
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
