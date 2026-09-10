//! Bounded in-memory capsule store.
//!
//! The store is a best-effort revision cache: entries expire by TTL, the ring
//! evicts the oldest insertion when full, and a root-epoch change invalidates
//! every stored capsule. Lookups never fabricate an empty previous capsule.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

use super::capsule::Capsule;

#[derive(Debug, Clone)]
pub struct StoredCapsule {
    pub capsule: Capsule,
    stored_at: Instant,
}

#[derive(Debug, Clone)]
pub enum StoreLookup {
    Found(Box<StoredCapsule>),
    /// The entry was evicted by TTL or a root-epoch change.
    Expired {
        reason: String,
    },
    NotFound,
}

#[derive(Debug, Default)]
struct StoreState {
    entries: HashMap<String, StoredCapsule>,
    order: VecDeque<String>,
}

/// A bounded, insertion-ordered capsule cache.
#[derive(Debug)]
pub struct CapsuleStore {
    state: Mutex<StoreState>,
    max_capsules: usize,
    ttl: Duration,
}

impl CapsuleStore {
    pub fn new(max_capsules: usize, ttl: Duration) -> Self {
        Self {
            state: Mutex::new(StoreState::default()),
            max_capsules: max_capsules.max(1),
            ttl,
        }
    }

    /// Insert or replace one capsule, evicting the oldest entries when full.
    pub fn insert(&self, capsule: Capsule) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let id = capsule.capsule_id.clone();
        let entry = StoredCapsule {
            capsule,
            stored_at: Instant::now(),
        };
        if state.entries.insert(id.clone(), entry).is_none() {
            state.order.push_back(id.clone());
        } else {
            // Move a replaced capsule to the newest position.
            state.order.retain(|candidate| candidate != &id);
            state.order.push_back(id.clone());
        }
        while state.entries.len() > self.max_capsules {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            state.entries.remove(&oldest);
        }
    }

    /// Look up a capsule for the current root epoch.
    pub fn lookup(&self, capsule_id: &str, root_epoch: u64) -> StoreLookup {
        let Ok(mut state) = self.state.lock() else {
            return StoreLookup::NotFound;
        };
        let Some(entry) = state.entries.get(capsule_id).cloned() else {
            return StoreLookup::NotFound;
        };
        if entry.stored_at.elapsed() > self.ttl {
            state.entries.remove(capsule_id);
            state.order.retain(|candidate| candidate != capsule_id);
            return StoreLookup::Expired {
                reason: format!(
                    "capsule expired after {} ms; prepare a new capsule",
                    self.ttl.as_millis()
                ),
            };
        }
        if entry.capsule.identity.root_epoch != root_epoch {
            state.entries.remove(capsule_id);
            state.order.retain(|candidate| candidate != capsule_id);
            return StoreLookup::Expired {
                reason: format!(
                    "root epoch changed from {} to {root_epoch}; stored capsule handles are invalidated",
                    entry.capsule.identity.root_epoch
                ),
            };
        }
        StoreLookup::Found(Box::new(entry))
    }

    pub fn len(&self) -> usize {
        self.state.lock().map_or(0, |state| state.entries.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::context::capsule::{
        CapsuleIdentity, CapsuleItemKind, ContextItem, ItemProvenance, ItemResolution,
    };

    fn capsule(id: &str, epoch: u64) -> Capsule {
        Capsule {
            schema_version: 1,
            capsule_id: id.to_owned(),
            identity: CapsuleIdentity {
                schema_version: 1,
                capsule_id: id.to_owned(),
                root_epoch: epoch,
                workspace_root: "/workspace".to_owned(),
                toolchain: None,
                analyzer: None,
                source_hashes: BTreeMap::new(),
                anchors_hash: String::new(),
                purpose_hash: None,
                change_id: None,
                byte_budget: 1_024,
                selected_packages: Vec::new(),
                enabled_features: Vec::new(),
                workspace_only: true,
            },
            anchors: Vec::new(),
            purpose: None,
            change_id: None,
            items: vec![ContextItem::new(
                CapsuleItemKind::Definition,
                "definition",
                ItemProvenance::WorkspaceSource,
                ItemResolution::Resolved,
            )],
            omitted: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn ring_evicts_the_oldest_capsule() {
        let store = CapsuleStore::new(2, Duration::from_secs(60));
        store.insert(capsule("a", 1));
        store.insert(capsule("b", 1));
        store.insert(capsule("c", 1));
        assert_eq!(store.len(), 2);
        assert!(matches!(store.lookup("a", 1), StoreLookup::NotFound));
        assert!(matches!(store.lookup("b", 1), StoreLookup::Found(_)));
        assert!(matches!(store.lookup("c", 1), StoreLookup::Found(_)));
    }

    #[test]
    fn ttl_expires_and_never_returns_a_fake_entry() {
        let store = CapsuleStore::new(4, Duration::from_millis(1));
        store.insert(capsule("a", 1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(store.lookup("a", 1), StoreLookup::Expired { .. }));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn epoch_change_invalidates_stored_capsules() {
        let store = CapsuleStore::new(4, Duration::from_secs(60));
        store.insert(capsule("a", 1));
        assert!(matches!(
            store.lookup("a", 2),
            StoreLookup::Expired { reason } if reason.contains("root epoch changed")
        ));
        assert_eq!(store.len(), 0);
    }
}
