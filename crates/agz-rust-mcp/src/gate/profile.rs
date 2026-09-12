//! Typed validation choices. No free-form Cargo flags or shell fragments.
use super::{GateTarget, GateTargetId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TestRunner {
    #[default]
    Cargo,
    Nextest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationOptions {
    /// Exact workspace member names. Full/fmt validation cannot be narrowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub packages: Vec<String>,
    /// One exact target in each selected package; requires packages and a
    /// check/clippy/test stage. This is separate from the platform targetTriple.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_target: Option<super::CargoTargetSelection>,
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub features: Vec<String>,
    #[serde(default)]
    pub all_features: bool,
    #[serde(default)]
    pub no_default_features: bool,
    #[serde(default)]
    pub target_triple: Option<String>,
    /// A test-name substring, never a flag or an arbitrary filter expression.
    #[serde(default)]
    pub test_filter: Option<String>,
    #[serde(default)]
    pub runner: TestRunner,
    /// Use an already configured, trusted sccache wrapper. Never replaces a wrapper.
    #[serde(default)]
    pub sccache: bool,
    /// Add bounded source excerpts and resolved dependency versions to diagnostics.
    #[serde(default)]
    pub context: bool,
}

impl ValidationOptions {
    pub fn validate(&self, target: GateTargetId) -> Result<(), String> {
        super::selection::validate(self, target)?;
        if self.features.len() > 64
            || self.features.iter().any(|feature| {
                feature.is_empty()
                    || feature.len() > 128
                    || feature.starts_with(['-', '.'])
                    || !feature
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-/+.".contains(&b))
            })
        {
            return Err("features must contain at most 64 bounded Cargo feature names".into());
        }
        if self.all_features && !self.features.is_empty() {
            return Err("allFeatures and features cannot be combined".into());
        }
        if self.target_triple.as_ref().is_some_and(|triple| {
            triple.is_empty()
                || triple.len() > 128
                || triple.starts_with('-')
                || !triple
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        }) {
            return Err(
                "targetTriple must be a built-in target triple, not a JSON path or flag".into(),
            );
        }
        if let Some(filter) = &self.test_filter {
            if target != GateTargetId::Test
                || filter.is_empty()
                || filter.len() > 256
                || filter.starts_with('-')
                || filter.chars().any(char::is_control)
            {
                return Err(
                    "testFilter is a bounded test-name substring for target=test only".into(),
                );
            }
        }
        if self.runner == TestRunner::Nextest
            && !matches!(target, GateTargetId::Test | GateTargetId::All)
        {
            return Err("runner=nextest requires target=test or target=all".into());
        }
        if target == GateTargetId::Fmt && (self.has_build_selection() || self.sccache) {
            return Err("feature, target and compiler-cache options do not apply to fmt".into());
        }
        Ok(())
    }

    pub fn has_build_selection(&self) -> bool {
        !self.packages.is_empty()
            || self.cargo_target.is_some()
            || self.all_features
            || self.no_default_features
            || !self.features.is_empty()
            || self.target_triple.is_some()
    }

    pub fn apply(&self, target: &mut GateTarget) {
        if target.id == GateTargetId::Fmt {
            return;
        }
        if let Some(selected) = &self.cargo_target {
            selected.apply(target);
        }
        let mut flags = Vec::<OsString>::new();
        if self.all_features {
            flags.push("--all-features".into());
        }
        if self.no_default_features {
            flags.push("--no-default-features".into());
        }
        if !self.features.is_empty() {
            let mut features = self.features.clone();
            features.sort();
            features.dedup();
            flags.extend(["--features".into(), features.join(",").into()]);
        }
        if let Some(triple) = &self.target_triple {
            flags.extend(["--target".into(), triple.into()]);
        }
        let separator = target
            .args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(target.args.len());
        target.args.splice(separator..separator, flags);
        if target.id == GateTargetId::Test {
            if self.runner == TestRunner::Nextest {
                // Cargo build JSON is separate from nextest's test-result format.
                target.args[0] = "nextest".into();
                target.args.insert(1, "run".into());
                for arg in &mut target.args {
                    if arg == "--message-format=json" {
                        *arg = "--cargo-message-format=json".into();
                    }
                }
                target.label = "cargo nextest run";
                target.args.push("--no-tests=fail".into());
            }
            if let Some(filter) = &self.test_filter {
                target.args.push(filter.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::targets::target_for;
    use std::path::Path;
    #[test]
    fn rejects_ambiguous_or_injected_options() {
        for feature in ["--config", "", "../other", "a b", "a\n"] {
            let p = ValidationOptions {
                features: vec![feature.into()],
                ..Default::default()
            };
            assert!(p.validate(GateTargetId::Check).is_err());
        }
        let p = ValidationOptions {
            test_filter: Some("one_test".into()),
            ..Default::default()
        };
        assert!(p.validate(GateTargetId::All).is_err());
        assert!(p.validate(GateTargetId::Test).is_ok());
        for package in ["--workspace", "*", "../other", "one@1.0.0", "one two", ""] {
            let p = ValidationOptions {
                packages: vec![package.into()],
                ..Default::default()
            };
            assert!(p.validate(GateTargetId::Check).is_err());
        }
        let p = ValidationOptions {
            cargo_target: Some(super::super::CargoTargetSelection::Lib {}),
            ..Default::default()
        };
        assert!(p.validate(GateTargetId::Check).is_err());
        assert!(
            serde_json::from_value::<ValidationOptions>(serde_json::json!({
                "packages": ["app"], "cargoTarget": {"kind": "lib", "name": "unused"}
            }))
            .is_err()
        );
    }

    #[test]
    fn selected_target_replaces_all_targets_before_clippy_arguments() {
        use super::super::CargoTargetSelection;
        let options = ValidationOptions {
            packages: vec!["app".into()],
            cargo_target: Some(CargoTargetSelection::Test {
                name: "focused".into(),
            }),
            features: vec!["extra".into()],
            ..Default::default()
        };
        assert!(options.validate(GateTargetId::Clippy).is_ok());
        let mut target = target_for(
            GateTargetId::Clippy,
            Path::new("/tmp/Cargo.toml"),
            false,
            false,
            &[],
        )
        .unwrap();
        options.apply(&mut target);
        let split = target.args.iter().position(|arg| arg == "--").unwrap();
        assert!(!target.args.iter().any(|arg| arg == "--all-targets"));
        assert!(
            target.args[..split]
                .windows(2)
                .any(|args| args == ["--test", "focused"])
        );
        assert_eq!(&target.args[split + 1..], ["-D", "warnings"]);
    }
    #[test]
    fn cargo_options_precede_clippy_separator_and_nextest_preserves_doc() {
        let p = ValidationOptions {
            features: vec!["serde".into()],
            no_default_features: true,
            target_triple: Some("x86_64-unknown-linux-gnu".into()),
            runner: TestRunner::Nextest,
            ..Default::default()
        };
        let mut clippy = target_for(
            GateTargetId::Clippy,
            Path::new("/tmp/Cargo.toml"),
            true,
            false,
            &[],
        )
        .unwrap();
        p.apply(&mut clippy);
        let split = clippy.args.iter().position(|a| a == "--").unwrap();
        assert!(clippy.args[..split].iter().any(|a| a == "--features"));
        let mut doc = target_for(
            GateTargetId::Doc,
            Path::new("/tmp/Cargo.toml"),
            true,
            false,
            &[],
        )
        .unwrap();
        p.apply(&mut doc);
        assert_eq!(doc.args[0], "test");
        assert!(doc.args.iter().any(|a| a == "--doc"));
        let mut test = target_for(
            GateTargetId::Test,
            Path::new("/tmp/Cargo.toml"),
            true,
            false,
            &[],
        )
        .unwrap();
        p.apply(&mut test);
        assert_eq!(
            &test.args[..2],
            &[OsString::from("nextest"), OsString::from("run")]
        );
        assert!(test.args.iter().any(|a| a == "--cargo-message-format=json"));
    }
}
