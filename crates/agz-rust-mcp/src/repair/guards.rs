//! Bounded, lexical impact guards for repair candidates.
//!
//! Guards are heuristics over patch text, not a proof about behavior. They
//! distinguish impacts that are already present in the base change from impacts
//! a candidate adds, and they are surfaced as behavior/performance evidence.
//! A candidate-added impact is never auto-accepted by `compare`.

use std::collections::{BTreeMap, BTreeSet};

use crate::change::PatchInput;

use super::model::{MAX_IMPACTS, RepairImpactData};

/// One recognized guard category and the counters it is derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum GuardKind {
    UnnecessaryClone,
    UnsafeAdded,
    PanicPathAdded,
    AssertionRemoval,
    TestRemoval,
    TestIgnored,
    LintDisabled,
    ErrorSwallowed,
    CodeDeletion,
    PublicApiChange,
}

impl GuardKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::UnnecessaryClone => "unnecessaryClone",
            Self::UnsafeAdded => "unsafeAdded",
            Self::PanicPathAdded => "panicPathAdded",
            Self::AssertionRemoval => "assertionRemoval",
            Self::TestRemoval => "testRemoval",
            Self::TestIgnored => "testIgnored",
            Self::LintDisabled => "lintDisabled",
            Self::ErrorSwallowed => "errorSwallowed",
            Self::CodeDeletion => "codeDeletion",
            Self::PublicApiChange => "publicApiChange",
        }
    }

    pub(crate) const fn category(self) -> &'static str {
        match self {
            Self::UnnecessaryClone => "performance",
            Self::UnsafeAdded
            | Self::PanicPathAdded
            | Self::AssertionRemoval
            | Self::TestRemoval
            | Self::TestIgnored
            | Self::LintDisabled
            | Self::ErrorSwallowed
            | Self::CodeDeletion
            | Self::PublicApiChange => "behavior",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Signals {
    unnecessary_clone: u64,
    unsafe_added: u64,
    panic_path_added: u64,
    assertion_removal: u64,
    test_removal: u64,
    test_ignored: u64,
    lint_disabled: u64,
    error_swallowed: u64,
    code_deletion: u64,
    public_api_added: BTreeSet<String>,
    public_api_removed: BTreeSet<String>,
}

impl Signals {
    fn value(&self, kind: GuardKind) -> u64 {
        match kind {
            GuardKind::UnnecessaryClone => self.unnecessary_clone,
            GuardKind::UnsafeAdded => self.unsafe_added,
            GuardKind::PanicPathAdded => self.panic_path_added,
            GuardKind::AssertionRemoval => self.assertion_removal,
            GuardKind::TestRemoval => self.test_removal,
            GuardKind::TestIgnored => self.test_ignored,
            GuardKind::LintDisabled => self.lint_disabled,
            GuardKind::ErrorSwallowed => self.error_swallowed,
            GuardKind::CodeDeletion => self.code_deletion,
            GuardKind::PublicApiChange => {
                u64::try_from(self.public_api_added.len() + self.public_api_removed.len())
                    .unwrap_or(u64::MAX)
            }
        }
    }
}

/// One candidate's guard outcome: impacts it adds and impacts inherited from
/// the base change.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct GuardReport {
    pub added: Vec<RepairImpactData>,
    pub inherited: Vec<RepairImpactData>,
}

/// Scans the base patches and the combined (base + candidate) patches. Only
/// impacts absent from the base are reported as candidate-added.
pub(crate) fn scan(base: &[PatchInput], combined: &[PatchInput]) -> GuardReport {
    let base = signals(base);
    let combined = signals(combined);
    let mut added = Vec::new();
    let mut inherited = Vec::new();
    for kind in [
        GuardKind::UnnecessaryClone,
        GuardKind::UnsafeAdded,
        GuardKind::PanicPathAdded,
        GuardKind::AssertionRemoval,
        GuardKind::TestRemoval,
        GuardKind::TestIgnored,
        GuardKind::LintDisabled,
        GuardKind::ErrorSwallowed,
        GuardKind::CodeDeletion,
        GuardKind::PublicApiChange,
    ] {
        let before = base.value(kind);
        let after = combined.value(kind);
        if before > 0 && inherited.len() < MAX_IMPACTS {
            inherited.push(impact(kind, before, "inherited", &base));
        }
        if after > before && added.len() < MAX_IMPACTS {
            added.push(impact(kind, after - before, "candidateAdded", &combined));
        }
    }
    GuardReport { added, inherited }
}

fn impact(kind: GuardKind, count: u64, origin: &str, signals: &Signals) -> RepairImpactData {
    let detail = match kind {
        GuardKind::UnnecessaryClone => {
            format!(
                "{count} added `.clone()` call(s): a clone is a performance cost and may hide an ownership redesign"
            )
        }
        GuardKind::UnsafeAdded => format!("{count} added `unsafe` token(s)"),
        GuardKind::PanicPathAdded => {
            format!("{count} added `todo!`, `unimplemented!`, `panic!`, or `.unwrap()` path(s)")
        }
        GuardKind::AssertionRemoval => format!("{count} removed assertion(s)"),
        GuardKind::TestRemoval => format!("{count} removed `#[test]` item(s)"),
        GuardKind::TestIgnored => format!("{count} added `#[ignore]` item(s)"),
        GuardKind::LintDisabled => format!("{count} added lint-silencing attribute(s)"),
        GuardKind::ErrorSwallowed => format!(
            "{count} added error-swallowing conversion(s) (`.ok()`, `unwrap_or*`, or discarded results)"
        ),
        GuardKind::CodeDeletion => format!("{count} block(s) deleted without a replacement"),
        GuardKind::PublicApiChange => format!(
            "{count} public API item(s) added or removed (added: {:?}, removed: {:?})",
            signals.public_api_added, signals.public_api_removed
        ),
    };
    RepairImpactData {
        kind: kind.as_str().to_owned(),
        category: kind.category().to_owned(),
        detail,
        origin: origin.to_owned(),
    }
}

fn signals(patches: &[PatchInput]) -> Signals {
    let mut signals = Signals::default();
    for patch in patches {
        let added = &patch.new_string;
        let removed = &patch.old_string;
        signals.unnecessary_clone =
            signals
                .unnecessary_clone
                .saturating_add(delta(removed, added, &[".clone()"]));
        signals.unsafe_added =
            signals
                .unsafe_added
                .saturating_add(delta(removed, added, &["unsafe "]));
        signals.panic_path_added = signals.panic_path_added.saturating_add(delta(
            removed,
            added,
            &["todo!", "unimplemented!", "panic!", ".unwrap()"],
        ));
        signals.assertion_removal = signals.assertion_removal.saturating_add(delta(
            added,
            removed,
            &[
                "assert!",
                "assert_eq!",
                "assert_ne!",
                "debug_assert!",
                "debug_assert_eq!",
                "debug_assert_ne!",
            ],
        ));
        signals.test_removal = signals.test_removal.saturating_add(delta(
            added,
            removed,
            &["#[test]", "#[tokio::test]", "#[async_std::test]"],
        ));
        signals.test_ignored = signals.test_ignored.saturating_add(delta(
            removed,
            added,
            &["#[ignore]", "#[ignore ="],
        ));
        signals.lint_disabled = signals.lint_disabled.saturating_add(delta(
            removed,
            added,
            &["#[allow(", "#![allow(", "#[expect(", "#![expect("],
        ));
        signals.error_swallowed = signals.error_swallowed.saturating_add(delta(
            removed,
            added,
            &[
                ".ok()",
                ".unwrap_or(",
                ".unwrap_or_default()",
                ".unwrap_or_else(",
                "let _ = ",
            ],
        ));
        if is_block_deletion(removed, added) {
            signals.code_deletion = signals.code_deletion.saturating_add(1);
        }
        let (added_items, removed_items) = public_api_items(removed, added);
        signals.public_api_added.extend(added_items);
        signals.public_api_removed.extend(removed_items);
    }
    signals
}

/// Positive when `needles` occur more often in `added` than in `removed`.
fn delta(removed: &str, added: &str, needles: &[&str]) -> u64 {
    let mut before = 0u64;
    let mut after = 0u64;
    for needle in needles {
        before = before.saturating_add(count(removed, needle));
        after = after.saturating_add(count(added, needle));
    }
    after.saturating_sub(before)
}

fn count(text: &str, needle: &str) -> u64 {
    u64::try_from(text.match_indices(needle).count()).unwrap_or(u64::MAX)
}

fn is_block_deletion(removed: &str, added: &str) -> bool {
    let removed_lines = substantive_lines(removed);
    let added_lines = substantive_lines(added);
    if removed_lines == 0 {
        return false;
    }
    added_lines == 0 && removed_lines >= 3
}

fn substantive_lines(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with("//")
        })
        .count()
}

/// Extracts public API item names introduced and removed by one patch.
pub(crate) fn public_api_items(removed: &str, added: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let before = api_items(removed);
    let after = api_items(added);
    let added_items = after.difference(&before).cloned().collect();
    let removed_items = before.difference(&after).cloned().collect();
    (added_items, removed_items)
}

fn api_items(text: &str) -> BTreeSet<String> {
    let mut items = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("pub ")
            || line.starts_with("pub(crate)")
            || line.starts_with("pub(super)")
        {
            continue;
        }
        let rest = line.trim_start_matches("pub ");
        let rest = rest
            .strip_prefix("async ")
            .or_else(|| rest.strip_prefix("unsafe "))
            .or_else(|| rest.strip_prefix("const "))
            .unwrap_or(rest);
        if let Some(name) = rest
            .strip_prefix("use ")
            .map(|path| {
                path.trim_end_matches(';')
                    .rsplit("::")
                    .next()
                    .unwrap_or(path)
                    .trim()
                    .to_owned()
            })
            .or_else(|| keyword_name(rest))
        {
            if !name.is_empty() {
                items.insert(name);
            }
        }
    }
    items
}

fn keyword_name(rest: &str) -> Option<String> {
    for keyword in [
        "fn ", "struct ", "enum ", "trait ", "type ", "mod ", "const ", "static ", "union ",
        "macro ",
    ] {
        if let Some(name) = rest.strip_prefix(keyword) {
            let name = name
                .split(['<', '(', ' ', ';', '{', ':', '='])
                .next()
                .unwrap_or(name)
                .trim();
            return Some(name.to_owned());
        }
    }
    None
}

/// Aggregated public API delta for a patch list, used by comparison output.
pub(crate) fn public_api_delta(patches: &[PatchInput]) -> (Vec<String>, Vec<String>) {
    let mut added = BTreeSet::new();
    let mut removed = BTreeSet::new();
    for patch in patches {
        let (patch_added, patch_removed) = public_api_items(&patch.old_string, &patch.new_string);
        added.extend(patch_added);
        removed.extend(patch_removed);
    }
    (added.into_iter().collect(), removed.into_iter().collect())
}

/// Line multiset delta for a patch list.
pub(crate) fn line_delta(patches: &[PatchInput]) -> (u64, u64) {
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for patch in patches {
        for line in patch.old_string.lines() {
            *counts.entry(truncate(line)).or_default() -= 1;
        }
        for line in patch.new_string.lines() {
            *counts.entry(truncate(line)).or_default() += 1;
        }
    }
    let added = counts.values().filter(|count| **count > 0).sum::<i64>();
    let removed = counts
        .values()
        .filter(|count| **count < 0)
        .map(|count| count.saturating_abs())
        .sum::<i64>();
    (
        u64::try_from(added).unwrap_or(u64::MAX),
        u64::try_from(removed).unwrap_or(u64::MAX),
    )
}

fn truncate(line: &str) -> String {
    line.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(old: &str, new: &str) -> PatchInput {
        PatchInput {
            file: "src/lib.rs".to_owned(),
            old_string: old.to_owned(),
            new_string: new.to_owned(),
        }
    }

    #[test]
    fn clone_guard_distinguishes_inherited_from_candidate_added() {
        let base = vec![patch("let a = v;", "let a = v.clone();")];
        let combined = vec![
            patch("let a = v;", "let a = v.clone();"),
            patch("consume(a);", "consume(a.clone());"),
        ];
        let report = scan(&base, &combined);
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.added[0].kind, "unnecessaryClone");
        assert_eq!(report.added[0].origin, "candidateAdded");
        assert_eq!(report.inherited.len(), 1);
        assert_eq!(report.inherited[0].kind, "unnecessaryClone");
    }

    #[test]
    fn negative_controls_are_detected() {
        let reports = scan(
            &[],
            &[
                patch(
                    "fn safe(value: String) -> usize { value.len() }",
                    "fn safe(value: String) -> usize { value.clone().len() }",
                ),
                patch(
                    "fn a() { do_it(); }\nfn b() { do_it(); }\nfn c() { do_it(); }",
                    "",
                ),
                patch("assert_eq!(value(), 2);", ""),
                patch("let value = fallible()?;", "let value = fallible().ok();"),
                patch(
                    "fn value() -> Result<u8, Error> { Ok(1) }",
                    "fn value() -> Result<u8, Error> { Ok(1) }\n#[test]\nfn removed() {}",
                ),
                patch("#[test]\nfn keeps_behavior() { assert!(true); }", ""),
                patch("fn run() { safe(); }", "fn run() { unsafe { danger(); } }"),
                patch(
                    "fn pending() -> u8 { 1 }\nfn live() -> u8 { 2 }",
                    "fn pending() -> u8 { todo!() }\nfn live() -> u8 { 2 }",
                ),
                patch(
                    "#[deny(warnings)]\nfn strict() {}",
                    "#[allow(dead_code)]\nfn strict() {}",
                ),
                patch(
                    "pub fn kept() -> u8 { 1 }",
                    "pub fn kept() -> u8 { 1 }\npub fn added() -> u8 { 2 }",
                ),
            ],
        )
        .added;
        let kinds = reports
            .iter()
            .map(|impact| impact.kind.as_str())
            .collect::<BTreeSet<_>>();
        for expected in [
            "unnecessaryClone",
            "unsafeAdded",
            "panicPathAdded",
            "assertionRemoval",
            "testRemoval",
            "lintDisabled",
            "errorSwallowed",
            "codeDeletion",
            "publicApiChange",
        ] {
            assert!(
                kinds.contains(expected),
                "missing guard {expected}: {kinds:?}"
            );
        }
        assert!(
            reports
                .iter()
                .any(|impact| impact.kind == "publicApiChange" && impact.detail.contains("added")),
            "public API addition must be visible"
        );
    }

    #[test]
    fn test_ignore_addition_is_detected() {
        let report = scan(
            &[],
            &[patch(
                "#[test]\nfn keeps_behavior() { assert!(true); }",
                "#[ignore]\n#[test]\nfn keeps_behavior() { assert!(true); }",
            )],
        );
        assert!(
            report
                .added
                .iter()
                .any(|impact| impact.kind == "testIgnored"),
            "{report:?}"
        );
    }

    #[test]
    fn public_api_delta_and_line_delta_are_bounded() {
        let patches = vec![
            patch("pub fn old() {}", "pub fn new_name() {}"),
            patch("let a = 1;\nlet b = 2;", "let a = 1;"),
        ];
        let (added, removed) = public_api_delta(&patches);
        assert_eq!(added, vec!["new_name".to_owned()]);
        assert_eq!(removed, vec!["old".to_owned()]);
        let (lines_added, lines_removed) = line_delta(&patches);
        assert_eq!(lines_added, 1);
        assert_eq!(lines_removed, 2);
    }
}
