//! Exact package and Cargo-target selection for focused validation.
use std::ffi::OsString;

use cargo_metadata::{Target, TargetKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{GateScope, GateScopeStrategy, GateTarget, GateTargetId, ValidationOptions};
use crate::workspace::WorkspaceSnapshot;

/// One Cargo target kind/name within the explicitly selected packages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum CargoTargetSelection {
    Lib {},
    Bin { name: String },
    Test { name: String },
    Example { name: String },
    Bench { name: String },
}

impl CargoTargetSelection {
    fn flag_and_name(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Lib {} => ("--lib", None),
            Self::Bin { name } => ("--bin", Some(name)),
            Self::Test { name } => ("--test", Some(name)),
            Self::Example { name } => ("--example", Some(name)),
            Self::Bench { name } => ("--bench", Some(name)),
        }
    }

    pub(super) fn apply(&self, target: &mut GateTarget) {
        target.args.retain(|arg| arg != "--all-targets");
        let (flag, name) = self.flag_and_name();
        let separator = target
            .args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(target.args.len());
        target.args.splice(
            separator..separator,
            std::iter::once(OsString::from(flag)).chain(name.map(OsString::from)),
        );
        if target.id == GateTargetId::Test {
            target.label = "cargo test (selected target)";
        }
    }

    pub(crate) fn matches(&self, target: &Target) -> bool {
        let kind = match self {
            Self::Lib {} => {
                return target.kind.iter().any(|kind| {
                    matches!(
                        kind,
                        TargetKind::Lib
                            | TargetKind::RLib
                            | TargetKind::DyLib
                            | TargetKind::CDyLib
                            | TargetKind::StaticLib
                            | TargetKind::ProcMacro
                    )
                });
            }
            Self::Bin { .. } => TargetKind::Bin,
            Self::Test { .. } => TargetKind::Test,
            Self::Example { .. } => TargetKind::Example,
            Self::Bench { .. } => TargetKind::Bench,
        };
        self.flag_and_name().1 == Some(target.name.as_str()) && target.kind.contains(&kind)
    }
}

fn exact_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "_-".contains(c))
}

pub(super) fn validate(options: &ValidationOptions, target: GateTargetId) -> Result<(), String> {
    if options.packages.len() > 64 || options.packages.iter().any(|name| !exact_name(name)) {
        return Err("packages must contain at most 64 exact workspace package names; paths, IDs, flags and globs are not supported".into());
    }
    if (!options.packages.is_empty() || options.cargo_target.is_some())
        && matches!(target, GateTargetId::All | GateTargetId::Fmt)
    {
        return Err("package/target selection cannot narrow target=all or fmt; use a focused check, build, clippy, test or doc stage".into());
    }
    if let Some(selected) = &options.cargo_target {
        if options.packages.is_empty() {
            return Err("cargoTarget requires explicit workspace packages".into());
        }
        if !matches!(
            target,
            GateTargetId::Check | GateTargetId::Build | GateTargetId::Clippy | GateTargetId::Test
        ) {
            return Err("cargoTarget applies only to check, build, clippy or test; doc keeps its doctest selection".into());
        }
        if selected
            .flag_and_name()
            .1
            .is_some_and(|name| !exact_name(name))
        {
            return Err(
                "cargoTarget.name must be a bounded exact name, not a path, flag or glob".into(),
            );
        }
    }
    Ok(())
}

/// Resolve names before launching compilation. Dependencies are still selected
/// by Cargo; only workspace members may be requested as the validation scope.
pub(crate) fn resolve(
    snapshot: &WorkspaceSnapshot,
    options: &ValidationOptions,
) -> Result<Option<(GateScope, Vec<OsString>)>, String> {
    if options.packages.is_empty() {
        return Ok(None);
    }
    let mut packages = options.packages.clone();
    packages.sort();
    packages.dedup();
    let mut package_ids = Vec::new();
    for name in &packages {
        let package = snapshot
            .metadata
            .packages
            .iter()
            .find(|package| {
                package.name.as_str() == name
                    && snapshot.metadata.workspace_members.contains(&package.id)
            })
            .ok_or_else(|| format!("requested package {name} is not a workspace member"))?;
        if let Some(target) = &options.cargo_target
            && !package
                .targets
                .iter()
                .any(|candidate| target.matches(candidate))
        {
            return Err(format!(
                "requested Cargo target does not exist in workspace package {name}"
            ));
        }
        package_ids.push(package.id.repr.clone());
    }
    let args = packages
        .iter()
        .flat_map(|name| [OsString::from("-p"), OsString::from(name)])
        .collect();
    Ok(Some((
        GateScope {
            strategy: GateScopeStrategy::Explicit,
            packages,
            package_ids,
            changed_paths: Vec::new(),
            widened_because: Vec::new(),
        },
        args,
    )))
}
