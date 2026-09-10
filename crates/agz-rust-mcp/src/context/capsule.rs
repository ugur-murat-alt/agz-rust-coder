//! Typed anchors, revision-bound capsule identity, and bounded wire data.
//!
//! Every field in this module is emitted as untrusted evidence. Source text,
//! hover text, and repository prose are data; they are never interpreted as
//! instructions, and no field is fed back into server guidance.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Wire schema version for context capsules.
pub const CONTEXT_SCHEMA_VERSION: u8 = 1;
/// Maximum typed anchors accepted by one request.
pub const MAX_ANCHORS: usize = 8;
/// Maximum purpose label length in characters.
pub const MAX_PURPOSE_CHARS: usize = 512;
/// Maximum change label length in characters.
pub const MAX_CHANGE_ID_CHARS: usize = 256;
/// Minimum effective byte budget; mirrors the smallest wire limit.
pub const MIN_BYTE_BUDGET: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ContextAction {
    #[default]
    Prepare,
    Expand,
    Delta,
}

impl ContextAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Expand => "expand",
            Self::Delta => "delta",
        }
    }
}

/// A 1-based inclusive line range for a file anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnchorRange {
    #[schemars(range(min = 1))]
    pub start_line: u32,
    #[schemars(range(min = 1))]
    pub end_line: u32,
}

impl AnchorRange {
    /// Returns the inclusive, ordered line bounds.
    pub fn bounds(self) -> (u32, u32) {
        let start = self.start_line.min(self.end_line).max(1);
        let end = self.end_line.max(self.start_line).max(start);
        (start, end)
    }
}

/// A typed context anchor. Free-text natural-language understanding is not
/// performed; every anchor names a file or a symbol position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ContextAnchor {
    #[serde(rename_all = "camelCase")]
    File {
        #[schemars(length(min = 1))]
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<AnchorRange>,
    },
    #[serde(rename_all = "camelCase")]
    Symbol {
        #[schemars(length(min = 1))]
        symbol: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(length(min = 1))]
        file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(range(min = 1))]
        line: Option<u32>,
    },
}

impl ContextAnchor {
    pub fn file(&self) -> Option<&str> {
        match self {
            Self::File { file, .. } => Some(file),
            Self::Symbol { file, .. } => file.as_deref(),
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum CapsuleItemKind {
    Definition,
    Signature,
    Implementation,
    Consumer,
    TestReference,
    CallHierarchy,
    FileExcerpt,
    Package,
    EnabledFeature,
    Dependency,
}

impl CapsuleItemKind {
    /// Lower priority values are more load-bearing during budget trimming.
    pub const fn priority(self) -> u8 {
        match self {
            Self::Definition => 0,
            Self::Consumer => 1,
            Self::TestReference => 2,
            Self::Implementation => 3,
            Self::Signature => 4,
            Self::FileExcerpt => 5,
            Self::CallHierarchy => 6,
            Self::Package => 7,
            Self::EnabledFeature => 8,
            Self::Dependency => 9,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ItemProvenance {
    RustAnalyzer,
    WorkspaceSource,
    CargoMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ItemResolution {
    Resolved,
    Advisory,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OmissionReason {
    Budget,
    ItemLimit,
    Ambiguity,
    Unavailable,
    OutsideWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextItem {
    /// Stable within one capsule; a sha256 prefix over kind, file, line and symbol.
    pub id: String,
    pub kind: CapsuleItemKind,
    /// Why this item was selected, in one bounded sentence.
    pub reason: String,
    pub provenance: ItemProvenance,
    pub resolution: ItemResolution,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
    /// True when the file hash no longer matches the stored capsule identity.
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub excerpt_bytes: u64,
    pub excerpt_chars: u64,
}

impl ContextItem {
    pub fn new(
        kind: CapsuleItemKind,
        reason: impl Into<String>,
        provenance: ItemProvenance,
        resolution: ItemResolution,
    ) -> Self {
        Self {
            id: String::new(),
            kind,
            reason: reason.into(),
            provenance,
            resolution,
            file: None,
            line: None,
            symbol: None,
            source_hash: None,
            stale: false,
            excerpt: None,
            excerpt_bytes: 0,
            excerpt_chars: 0,
        }
    }

    #[must_use]
    pub fn with_location(mut self, file: impl Into<String>, line: u32) -> Self {
        self.file = Some(file.into());
        self.line = Some(line);
        self
    }

    #[must_use]
    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }

    #[must_use]
    pub fn with_hash(mut self, hash: impl Into<String>) -> Self {
        self.source_hash = Some(hash.into());
        self
    }

    #[must_use]
    pub fn with_excerpt(mut self, excerpt: impl Into<String>) -> Self {
        let excerpt = excerpt.into();
        self.excerpt_bytes = excerpt.len() as u64;
        self.excerpt_chars = excerpt.chars().count() as u64;
        self.excerpt = Some(excerpt);
        self
    }

    #[must_use]
    pub fn with_id(mut self) -> Self {
        self.id = item_id(
            self.kind,
            self.file.as_deref(),
            self.line,
            self.symbol.as_deref(),
        );
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OmittedItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<CapsuleItemKind>,
    pub reason: OmissionReason,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

impl OmittedItem {
    pub fn new(reason: OmissionReason, detail: impl Into<String>) -> Self {
        Self {
            kind: None,
            reason,
            detail: detail.into(),
            file: None,
            line: None,
            symbol: None,
        }
    }

    #[must_use]
    pub fn with_kind(mut self, kind: CapsuleItemKind) -> Self {
        self.kind = Some(kind);
        self
    }

    #[must_use]
    pub fn with_location(mut self, file: Option<String>, line: Option<u32>) -> Self {
        self.file = file;
        self.line = line;
        self
    }

    #[must_use]
    pub fn with_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackageIdentity {
    pub package_id: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapsuleIdentity {
    pub schema_version: u8,
    pub capsule_id: String,
    pub root_epoch: u64,
    pub workspace_root: String,
    /// Optional toolchain label from the environment or rustup settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// `rust-analyzer` when a manager is available for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyzer: Option<String>,
    /// Workspace-relative file path to sha256 of the full file contents.
    pub source_hashes: BTreeMap<String, String>,
    pub anchors_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose_hash: Option<String>,
    /// Recorded as a label only; no candidate service exists in this version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub byte_budget: u64,
    pub selected_packages: Vec<PackageIdentity>,
    /// `<package-id>:<feature>` entries from cargo metadata.
    pub enabled_features: Vec<String>,
    /// Always true in this version; workspace hashes are used, not candidate hashes.
    pub workspace_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Capsule {
    pub schema_version: u8,
    pub capsule_id: String,
    pub identity: CapsuleIdentity,
    pub anchors: Vec<ContextAnchor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub items: Vec<ContextItem>,
    pub omitted: Vec<OmittedItem>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeltaEntry {
    pub id: String,
    pub kind: CapsuleItemKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeltaReport {
    pub added: Vec<DeltaEntry>,
    pub changed: Vec<DeltaEntry>,
    pub removed: Vec<DeltaEntry>,
    pub unchanged: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub offset: u32,
    pub limit: u32,
    pub total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SizeReport {
    /// Serialized UTF-8 bytes of this payload before the MCP envelope is added.
    pub bytes: u64,
    /// UTF-8 characters across returned item excerpts.
    pub chars: u64,
    pub item_count: u32,
    pub returned_items: u32,
    pub byte_budget: u64,
    /// Always false: this server ships no tokenizer and never reports tokens.
    pub tokenizer_available: bool,
}

impl Default for SizeReport {
    fn default() -> Self {
        Self {
            bytes: 0,
            chars: 0,
            item_count: 0,
            returned_items: 0,
            byte_budget: 0,
            tokenizer_available: false,
        }
    }
}

impl SizeReport {
    pub fn measure(data: &ContextData, byte_budget: u64) -> Self {
        let bytes = serde_json::to_vec(data).map_or(0, |encoded| encoded.len() as u64);
        let chars = data
            .items
            .iter()
            .filter_map(|item| item.excerpt.as_deref())
            .map(|excerpt| excerpt.chars().count() as u64)
            .sum();
        Self {
            bytes,
            chars,
            item_count: u32::try_from(data.items.len()).unwrap_or(u32::MAX),
            returned_items: u32::try_from(
                data.items
                    .iter()
                    .filter(|item| item.excerpt.is_some())
                    .count(),
            )
            .unwrap_or(u32::MAX),
            byte_budget,
            tokenizer_available: false,
        }
    }
}

/// The stable structured payload for the `context` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextData {
    pub action: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_capsule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub anchors: Vec<ContextAnchor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<CapsuleIdentity>,
    pub items: Vec<ContextItem>,
    pub omitted: Vec<OmittedItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<DeltaReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<PageInfo>,
    pub sizes: SizeReport,
    pub notes: Vec<String>,
}

impl ContextData {
    /// A typed failure such as `NOT_FOUND` or `EXPIRED`; never a fake empty delta.
    pub fn failure(action: ContextAction, status: &str, reason: impl Into<String>) -> Self {
        Self {
            action: action.as_str().to_owned(),
            status: status.to_owned(),
            capsule_id: None,
            previous_capsule_id: None,
            root_epoch: None,
            purpose: None,
            change_id: None,
            anchors: Vec::new(),
            identity: None,
            items: Vec::new(),
            omitted: Vec::new(),
            delta: None,
            page: None,
            sizes: SizeReport::default(),
            notes: vec![reason.into()],
        }
    }
}

/// Canonical identity inputs hashed into the capsule id.
#[derive(Debug)]
pub struct CapsuleIdentityInput<'a> {
    pub root_epoch: u64,
    pub workspace_root: &'a str,
    pub toolchain: Option<&'a str>,
    pub analyzer: Option<&'a str>,
    pub source_hashes: &'a BTreeMap<String, String>,
    pub anchors: &'a [ContextAnchor],
    pub purpose: Option<&'a str>,
    pub change_id: Option<&'a str>,
    pub byte_budget: u64,
    pub selected_packages: &'a [PackageIdentity],
    pub enabled_features: &'a [String],
}

/// `capsuleId` = sha256 over root epoch, toolchain/analyzer identity, source
/// hashes, the typed anchor set, purpose, change label, feature selection, and
/// byte budget. The same inputs always produce the same id.
pub fn compute_capsule_id(input: &CapsuleIdentityInput<'_>) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Canonical<'a> {
        schema_version: u8,
        root_epoch: u64,
        workspace_root: &'a str,
        toolchain: Option<&'a str>,
        analyzer: Option<&'a str>,
        source_hashes: &'a BTreeMap<String, String>,
        anchors: &'a [ContextAnchor],
        purpose: Option<&'a str>,
        change_id: Option<&'a str>,
        byte_budget: u64,
        selected_packages: &'a [PackageIdentity],
        enabled_features: &'a [String],
    }
    let canonical = Canonical {
        schema_version: CONTEXT_SCHEMA_VERSION,
        root_epoch: input.root_epoch,
        workspace_root: input.workspace_root,
        toolchain: input.toolchain,
        analyzer: input.analyzer,
        source_hashes: input.source_hashes,
        anchors: input.anchors,
        purpose: input.purpose,
        change_id: input.change_id,
        byte_budget: input.byte_budget,
        selected_packages: input.selected_packages,
        enabled_features: input.enabled_features,
    };
    let encoded = serde_json::to_vec(&canonical).unwrap_or_default();
    sha256_hex(&encoded)
}

/// A 24-character sha256 prefix used as a stable per-capsule item handle.
pub fn item_id(
    kind: CapsuleItemKind,
    file: Option<&str>,
    line: Option<u32>,
    symbol: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{kind:?}").as_bytes());
    hasher.update(b"\0");
    hasher.update(file.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    hasher.update(line.unwrap_or(0).to_le_bytes());
    hasher.update(b"\0");
    hasher.update(symbol.unwrap_or("").as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest.chars().take(24).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn anchors_hash(anchors: &[ContextAnchor]) -> String {
    let encoded = serde_json::to_vec(anchors).unwrap_or_default();
    sha256_hex(&encoded)
}

pub fn purpose_hash(purpose: Option<&str>) -> Option<String> {
    purpose.map(|purpose| sha256_hex(purpose.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> ContextAnchor {
        ContextAnchor::Symbol {
            symbol: "Widget".to_owned(),
            file: Some("src/lib.rs".to_owned()),
            line: Some(4),
        }
    }

    #[test]
    fn anchors_round_trip_as_tagged_camel_case() {
        let encoded = serde_json::to_value(anchor()).expect("serialize anchor");
        assert_eq!(encoded["kind"], "symbol");
        assert_eq!(encoded["symbol"], "Widget");
        assert!(encoded.get("line").is_some());
        let decoded: ContextAnchor = serde_json::from_value(encoded).expect("deserialize anchor");
        assert_eq!(decoded, anchor());
    }

    #[test]
    fn capsule_identity_changes_with_source_hash_and_epoch() {
        let mut hashes = BTreeMap::new();
        hashes.insert("src/lib.rs".to_owned(), "a".repeat(64));
        let packages = Vec::new();
        let features = Vec::new();
        let anchors = [anchor()];
        let base = compute_capsule_id(&CapsuleIdentityInput {
            root_epoch: 1,
            workspace_root: "/workspace",
            toolchain: None,
            analyzer: Some("rust-analyzer"),
            source_hashes: &hashes,
            anchors: &anchors,
            purpose: Some("fix api"),
            change_id: Some("change-1"),
            byte_budget: 4_096,
            selected_packages: &packages,
            enabled_features: &features,
        });
        let same = compute_capsule_id(&CapsuleIdentityInput {
            root_epoch: 1,
            workspace_root: "/workspace",
            toolchain: None,
            analyzer: Some("rust-analyzer"),
            source_hashes: &hashes,
            anchors: &anchors,
            purpose: Some("fix api"),
            change_id: Some("change-1"),
            byte_budget: 4_096,
            selected_packages: &packages,
            enabled_features: &features,
        });
        assert_eq!(base, same);

        let mut changed = hashes.clone();
        changed.insert("src/lib.rs".to_owned(), "b".repeat(64));
        let mutated = compute_capsule_id(&CapsuleIdentityInput {
            root_epoch: 1,
            workspace_root: "/workspace",
            toolchain: None,
            analyzer: Some("rust-analyzer"),
            source_hashes: &changed,
            anchors: &anchors,
            purpose: Some("fix api"),
            change_id: Some("change-1"),
            byte_budget: 4_096,
            selected_packages: &packages,
            enabled_features: &features,
        });
        assert_ne!(base, mutated);

        let epoch_changed = compute_capsule_id(&CapsuleIdentityInput {
            root_epoch: 2,
            workspace_root: "/workspace",
            toolchain: None,
            analyzer: Some("rust-analyzer"),
            source_hashes: &hashes,
            anchors: &anchors,
            purpose: Some("fix api"),
            change_id: Some("change-1"),
            byte_budget: 4_096,
            selected_packages: &packages,
            enabled_features: &features,
        });
        assert_ne!(base, epoch_changed);
    }

    #[test]
    fn capsule_identity_binds_toolchain_analyzer_and_feature_selection() {
        let hashes = BTreeMap::new();
        let anchors = [anchor()];
        let packages = vec![PackageIdentity {
            package_id: "p1".to_owned(),
            name: "demo".to_owned(),
            version: "0.1.0".to_owned(),
        }];
        let features = vec!["demo:serde".to_owned()];
        let identity = |toolchain: Option<&'static str>,
                        analyzer: Option<&'static str>,
                        packages: &[PackageIdentity],
                        features: &[String]| {
            compute_capsule_id(&CapsuleIdentityInput {
                root_epoch: 1,
                workspace_root: "/workspace",
                toolchain,
                analyzer,
                source_hashes: &hashes,
                anchors: &anchors,
                purpose: None,
                change_id: None,
                byte_budget: 4_096,
                selected_packages: packages,
                enabled_features: features,
            })
        };
        let base = identity(Some("1.88.0"), Some("rust-analyzer"), &packages, &features);
        assert_ne!(
            base,
            identity(Some("1.89.0"), Some("rust-analyzer"), &packages, &features),
            "toolchain identity must bind"
        );
        assert_ne!(
            base,
            identity(Some("1.88.0"), None, &packages, &features),
            "analyzer identity must bind"
        );
        assert_ne!(
            base,
            identity(Some("1.88.0"), Some("rust-analyzer"), &packages, &[]),
            "enabled feature selection must bind"
        );
        assert_ne!(
            base,
            identity(Some("1.88.0"), Some("rust-analyzer"), &[], &features),
            "selected packages must bind"
        );
    }

    #[test]
    fn item_ids_are_stable_but_kind_and_location_sensitive() {
        let first = item_id(
            CapsuleItemKind::Consumer,
            Some("src/app.rs"),
            Some(3),
            Some("Widget"),
        );
        let second = item_id(
            CapsuleItemKind::Consumer,
            Some("src/app.rs"),
            Some(3),
            Some("Widget"),
        );
        assert_eq!(first, second);
        assert_eq!(first.len(), 24);
        assert_ne!(
            first,
            item_id(
                CapsuleItemKind::TestReference,
                Some("src/app.rs"),
                Some(3),
                Some("Widget")
            )
        );
    }
}
