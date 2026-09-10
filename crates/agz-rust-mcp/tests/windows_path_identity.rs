#![cfg(windows)]
use agz_rust_mcp::workspace::{ClientRoots, RootError, RootGuard, build_package_graph};
use cargo_metadata::Metadata;
use serde_json::json;
use std::{fs, path::PathBuf, time::SystemTime};

/// Cargo reports ordinary drive paths for packages while authorized roots are
/// canonical. The package graph must expose the canonical spelling so anchor
/// containment and target selection compare one identity.
#[test]
fn package_graph_roots_use_the_canonical_verbatim_spelling() {
    let id = "fixture 0.1.0 (path+file:///C:/fake/workspace)";
    let metadata: Metadata = serde_json::from_value(json!({
        "packages": [{
            "name": "fixture",
            "version": "0.1.0",
            "id": id,
            "license": null,
            "license_file": null,
            "description": null,
            "source": null,
            "dependencies": [],
            "targets": [{
                "kind": ["lib"],
                "crate_types": ["lib"],
                "name": "fixture",
                "src_path": "C:\\fake\\workspace\\src\\lib.rs",
                "edition": "2024",
                "doc": true,
                "doctest": true,
                "test": true
            }],
            "features": {},
            "manifest_path": "C:\\fake\\workspace\\Cargo.toml",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": null,
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }],
        "workspace_members": [id],
        "workspace_default_members": [id],
        "resolve": null,
        "workspace_root": "C:\\fake\\workspace",
        "target_directory": "C:\\fake\\workspace\\target",
        "metadata": null,
        "version": 1
    }))
    .expect("metadata fixture");

    let graph = build_package_graph(&metadata);
    let node = graph.nodes().values().next().expect("package node");
    assert_eq!(node.root, PathBuf::from(r"\\?\C:\fake\workspace"));
    assert_eq!(
        node.manifest_path,
        PathBuf::from(r"\\?\C:\fake\workspace\Cargo.toml")
    );
}

#[test]
fn ordinary_and_verbatim_drive_paths_share_authority_without_expanding_it() {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let base = fs::canonicalize(std::env::temp_dir())
        .expect("canonical temporary directory")
        .join(format!("agz-path-identity-{}-{stamp}", std::process::id()));
    let root = base.join("root");
    let sibling = base.join("root-sibling");
    fs::create_dir_all(root.join("child")).expect("create root");
    fs::create_dir_all(&sibling).expect("create sibling");
    fs::write(root.join("child/source.rs"), "pub fn item() {}\n").expect("write source");
    let guard = RootGuard::new([root.clone()], std::iter::empty()).expect("root guard");
    let authority = guard.configured_roots()[0].clone();
    let plain = PathBuf::from(
        root.to_str()
            .expect("UTF-8 root")
            .strip_prefix(r"\\?\")
            .expect("verbatim drive"),
    );
    assert!(authority.contains(&plain));
    assert_eq!(
        authority
            .authorize_dir(&plain.join("child"))
            .expect("ordinary authorized child")
            .path(),
        root.join("child")
    );
    let snapshot = guard
        .snapshot(ClientRoots::unsupported())
        .expect("snapshot");
    assert!(snapshot.select(Some(&plain)).is_ok());
    assert!(!authority.contains(&sibling));
    assert!(authority.authorize_dir(&sibling).is_err());
    let escape = PathBuf::from(format!("{}\\..\\root-sibling", plain.display()));
    assert!(matches!(
        snapshot.select(Some(&escape)),
        Err(RootError::ParentComponent)
    ));
    drop(snapshot);
    drop(authority);
    drop(guard);
    fs::remove_dir_all(base).expect("remove fixture");
}
