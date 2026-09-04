//! Workspace selection, authorization, metadata, and bounded input identity.

pub mod graph;
pub mod identity;
pub mod metadata;
pub mod roots;
pub mod select;

pub use graph::{PackageEdge, PackageGraph, PackageNode, build_package_graph};
pub use identity::{
    GitOutput, GitProbe, IdentityError, IdentityIncompleteReason, IdentityInput, IdentityLimits,
    InputIdentity, StdGitProbe, compute_input_identity, compute_input_identity_authorized,
};
pub use metadata::{
    CargoMetadataRunner, DependencyClosure, MetadataCacheState, MetadataCommandSpec, MetadataError,
    MetadataLoad, MetadataRun, MetadataRunner, MetadataService, WorkspaceSnapshot,
};
pub use roots::{
    AuthorizedRoot, BoundedFile, ClientRoots, DirectoryEntry, DirectoryEntryKind, ResolvedPath,
    RootError, RootGuard, RootKind, RootSnapshot, WalkFile, WalkIssue, WalkIssueKind, WalkLimits,
    WalkResult, WorkspaceRoot, parse_file_uri,
};
pub use select::{SelectionError, WorkspaceSelection, select_in_root, select_workspace};
