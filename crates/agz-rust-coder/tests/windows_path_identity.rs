#![cfg(windows)]
use agz_rust_coder::workspace::{ClientRoots, RootError, RootGuard};
use std::{fs, path::PathBuf, time::SystemTime};

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
