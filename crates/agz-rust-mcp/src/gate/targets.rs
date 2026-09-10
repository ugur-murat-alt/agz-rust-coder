use std::{ffi::OsString, fs, path::Path, time::Duration};

use crate::workspace::WorkspaceSnapshot;

use super::types::{GateTarget, GateTargetId};

const CHECK_TIMEOUT: Duration = Duration::from_secs(180);
const CLIPPY_TIMEOUT: Duration = Duration::from_secs(240);
const TEST_TIMEOUT: Duration = Duration::from_secs(300);
const DOC_TIMEOUT: Duration = Duration::from_secs(300);
const FMT_TIMEOUT: Duration = Duration::from_secs(60);

/// The fixed order is part of the full-authority contract.
pub const FULL_ORDER: [GateTargetId; 4] = [
    GateTargetId::Fmt,
    GateTargetId::Clippy,
    GateTargetId::Test,
    GateTargetId::Doc,
];

pub fn target_for(
    id: GateTargetId,
    manifest: &Path,
    full_workspace: bool,
    timings: bool,
    scope_args: &[OsString],
) -> Option<GateTarget> {
    if id == GateTargetId::All {
        return None;
    }

    let manifest = manifest.as_os_str().to_owned();
    let mut args = match id {
        GateTargetId::Check => vec![
            OsString::from("check"),
            OsString::from("--manifest-path"),
            manifest.clone(),
            OsString::from("--locked"),
            OsString::from("--message-format=json"),
        ],
        GateTargetId::Clippy => vec![
            OsString::from("clippy"),
            OsString::from("--manifest-path"),
            manifest.clone(),
            OsString::from("--locked"),
            OsString::from("--all-targets"),
            OsString::from("--message-format=json"),
            OsString::from("--"),
            OsString::from("-D"),
            OsString::from("warnings"),
        ],
        GateTargetId::Test => vec![
            OsString::from("test"),
            OsString::from("--manifest-path"),
            manifest.clone(),
            OsString::from("--locked"),
            OsString::from("--all-targets"),
            OsString::from("--message-format=json"),
        ],
        GateTargetId::Doc => vec![
            OsString::from("test"),
            OsString::from("--manifest-path"),
            manifest.clone(),
            OsString::from("--doc"),
            OsString::from("--locked"),
            OsString::from("--message-format=json"),
        ],
        GateTargetId::Fmt => {
            let mut args = vec![
                OsString::from("fmt"),
                OsString::from("--manifest-path"),
                manifest.clone(),
            ];
            if full_workspace || is_virtual_workspace(manifest_path_from_args(&args)) {
                args.push(OsString::from("--all"));
            }
            args.push(OsString::from("--check"));
            args
        }
        GateTargetId::All => unreachable!(),
    };

    if full_workspace
        && matches!(
            id,
            GateTargetId::Clippy | GateTargetId::Test | GateTargetId::Doc
        )
    {
        let separator = args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(args.len());
        args.insert(separator, OsString::from("--workspace"));
    }

    if !full_workspace && id != GateTargetId::Fmt && !scope_args.is_empty() {
        let separator = args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(args.len());
        args.splice(separator..separator, scope_args.iter().cloned());
    }

    if timings && id != GateTargetId::Fmt {
        let separator = args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(args.len());
        args.insert(separator, OsString::from("--timings"));
    }

    let (label, timeout) = match id {
        GateTargetId::Check => ("cargo check", CHECK_TIMEOUT),
        GateTargetId::Clippy => ("cargo clippy (warnings as errors)", CLIPPY_TIMEOUT),
        GateTargetId::Test => ("cargo test --all-targets", TEST_TIMEOUT),
        GateTargetId::Doc => ("cargo test --doc", DOC_TIMEOUT),
        GateTargetId::Fmt => ("cargo fmt --check", FMT_TIMEOUT),
        GateTargetId::All => unreachable!(),
    };
    Some(GateTarget {
        id,
        label,
        args,
        timeout,
    })
}

pub fn targets_for(
    snapshot: &WorkspaceSnapshot,
    target: GateTargetId,
    manifest: &Path,
    timings: bool,
    scope_args: &[OsString],
) -> Vec<GateTarget> {
    if target == GateTargetId::All {
        FULL_ORDER
            .into_iter()
            .filter_map(|id| {
                if id == GateTargetId::Doc && !has_doctestable_target(snapshot) {
                    return None;
                }
                target_for(id, manifest, true, timings, &[])
            })
            .collect()
    } else {
        target_for(target, manifest, false, timings, scope_args)
            .into_iter()
            .collect()
    }
}

pub fn target_by_id(id: GateTargetId, manifest: &Path) -> Option<GateTarget> {
    target_for(id, manifest, false, false, &[])
}

pub fn has_doctestable_target(snapshot: &WorkspaceSnapshot) -> bool {
    snapshot
        .metadata
        .packages
        .iter()
        .filter(|package| snapshot.metadata.workspace_members.contains(&package.id))
        .any(|package| {
            package.targets.iter().any(|target| {
                target.doctest
                    && target.kind.iter().any(|kind| {
                        matches!(
                            kind,
                            cargo_metadata::TargetKind::Lib
                                | cargo_metadata::TargetKind::RLib
                                | cargo_metadata::TargetKind::ProcMacro
                        )
                    })
            })
        })
}

fn manifest_path_from_args(args: &[OsString]) -> &Path {
    args.windows(2)
        .find(|window| window[0] == "--manifest-path")
        .and_then(|window| window.get(1))
        .map(Path::new)
        .unwrap_or_else(|| Path::new("Cargo.toml"))
}

fn is_virtual_workspace(path: &Path) -> bool {
    let Ok(manifest) = fs::read_to_string(path) else {
        return false;
    };
    let has_workspace = manifest
        .lines()
        .any(|line| line.trim_start().starts_with("[workspace]"));
    let has_package = manifest
        .lines()
        .any(|line| line.trim_start().starts_with("[package]"));
    has_workspace && !has_package
}
