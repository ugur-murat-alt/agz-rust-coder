//! Deterministic capsule planning: item limits, budget trimming, and the
//! visible omission list.
//!
//! Trimming keeps the most load-bearing items first (definition, consumers,
//! tests) and records every removed item with a reason. There is no tokenizer;
//! all accounting is UTF-8 bytes and characters.

use super::capsule::{ContextData, ContextItem, OmissionReason, OmittedItem, SizeReport};

/// Maximum individually listed omissions in one payload.
pub const MAX_OMITTED_ENTRIES: usize = 32;
/// Excerpts are never trimmed below this many characters.
const MIN_EXCERPT_CHARS: usize = 120;
const MAX_TRIM_ITERATIONS: usize = 512;

#[derive(Debug, Clone, Copy)]
pub struct PlanLimits {
    pub max_items: usize,
    pub byte_budget: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanOutcome {
    pub bytes: u64,
    pub chars: u64,
    pub trimmed_excerpts: u64,
    pub removed_items: u64,
    pub omitted_overflow: u64,
}

impl PlanOutcome {
    pub fn merge(&mut self, other: Self) {
        self.trimmed_excerpts = self.trimmed_excerpts.saturating_add(other.trimmed_excerpts);
        self.removed_items = self.removed_items.saturating_add(other.removed_items);
        self.omitted_overflow = self.omitted_overflow.saturating_add(other.omitted_overflow);
        self.bytes = other.bytes;
        self.chars = other.chars;
    }

    pub const fn has_trimming(self) -> bool {
        self.trimmed_excerpts > 0 || self.removed_items > 0 || self.omitted_overflow > 0
    }
}

/// Serialized length of one payload, including the size report itself.
pub fn wire_bytes(data: &ContextData) -> u64 {
    serde_json::to_vec(data).map_or(0, |encoded| encoded.len() as u64)
}

/// Sort by load-bearing priority, cap the item count, and register the
/// least-important items as visible `itemLimit` omissions.
pub fn cap_items(
    items: &mut Vec<ContextItem>,
    omitted: &mut Vec<OmittedItem>,
    max_items: usize,
) -> u64 {
    items.sort_by(|left, right| {
        left.kind
            .priority()
            .cmp(&right.kind.priority())
            .then_with(|| left.id.cmp(&right.id))
    });
    if items.len() <= max_items {
        return 0;
    }
    let overflow = items.split_off(max_items);
    let mut overflow_count = 0;
    for item in overflow {
        if !push_omission(omitted, omission_from_item(item, OmissionReason::ItemLimit)) {
            overflow_count += 1;
        }
    }
    overflow_count
}

/// Shrink or drop the least load-bearing content until the serialized payload
/// fits the requested byte budget or nothing further can be trimmed.
pub fn enforce_data_budget(data: &mut ContextData, byte_budget: u64) -> PlanOutcome {
    let mut outcome = PlanOutcome::default();
    for _ in 0..MAX_TRIM_ITERATIONS {
        let Ok(encoded) = serde_json::to_vec(data) else {
            break;
        };
        if encoded.len() as u64 <= byte_budget {
            break;
        }
        if !trim_once(data, &mut outcome) {
            break;
        }
    }
    outcome.bytes = serde_json::to_vec(data).map_or(0, |encoded| encoded.len() as u64);
    outcome.chars = excerpt_chars(data);
    outcome
}

/// Enforce the budget and publish the measured size report until the payload
/// including the report fits the budget, or nothing further can be trimmed.
pub fn apply_budget_and_sizes(data: &mut ContextData, byte_budget: u64) -> PlanOutcome {
    let mut outcome = PlanOutcome::default();
    for _ in 0..8 {
        outcome.merge(enforce_data_budget(data, byte_budget));
        data.sizes = SizeReport::measure(data, byte_budget);
        if wire_bytes(data) <= byte_budget {
            break;
        }
        // The published size report can itself push the payload over budget.
        let follow_up = enforce_data_budget(data, byte_budget);
        let progressed = follow_up.has_trimming();
        outcome.merge(follow_up);
        if !progressed {
            break;
        }
    }
    data.sizes = SizeReport::measure(data, byte_budget);
    outcome.bytes = wire_bytes(data);
    outcome.chars = excerpt_chars(data);
    outcome
}

fn trim_once(data: &mut ContextData, outcome: &mut PlanOutcome) -> bool {
    if shrink_lowest_excerpt(data) {
        outcome.trimmed_excerpts += 1;
        return true;
    }
    if !data.items.is_empty() {
        let index = data
            .items
            .iter()
            .enumerate()
            .max_by_key(|(index, item)| (item.kind.priority(), *index))
            .map(|(index, _)| index)
            .unwrap_or(0);
        let item = data.items.remove(index);
        outcome.removed_items += 1;
        if !push_omission(
            &mut data.omitted,
            omission_from_item(item, OmissionReason::Budget),
        ) {
            outcome.omitted_overflow += 1;
        }
        return true;
    }
    // Only when no items remain: drop the least important diagnostics, but keep
    // the omission list visible as long as the note list can absorb the trim.
    if data.notes.pop().is_some() {
        return true;
    }
    if data.omitted.pop().is_some() {
        outcome.omitted_overflow += 1;
        return true;
    }
    false
}

fn shrink_lowest_excerpt(data: &mut ContextData) -> bool {
    let candidate = data
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.excerpt
                .as_deref()
                .is_some_and(|excerpt| excerpt.chars().count() > MIN_EXCERPT_CHARS)
        })
        .max_by_key(|(index, item)| (item.kind.priority(), *index))
        .map(|(index, _)| index);
    let Some(index) = candidate else {
        return false;
    };
    let Some(excerpt) = data.items[index].excerpt.take() else {
        return false;
    };
    let count = excerpt.chars().count();
    let keep = (count / 2).max(MIN_EXCERPT_CHARS);
    let shrunk = excerpt.chars().take(keep).collect::<String>();
    let item = &mut data.items[index];
    item.excerpt = Some(shrunk);
    item.excerpt_bytes = item.excerpt.as_ref().map_or(0, |text| text.len() as u64);
    item.excerpt_chars = item
        .excerpt
        .as_ref()
        .map_or(0, |text| text.chars().count() as u64);
    true
}

fn omission_from_item(item: ContextItem, reason: OmissionReason) -> OmittedItem {
    let detail = match reason {
        OmissionReason::ItemLimit => format!(
            "{} item exceeded the configured max_items cap and was not selected; {}",
            format_kind(item.kind),
            item.reason
        ),
        _ => format!(
            "{} item removed by the byte budget; {}",
            format_kind(item.kind),
            item.reason
        ),
    };
    OmittedItem::new(reason, detail)
        .with_kind(item.kind)
        .with_location(item.file, item.line)
        .with_symbol(item.symbol.unwrap_or_default())
}

fn format_kind(kind: super::capsule::CapsuleItemKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "capsule".to_owned())
}

fn push_omission(omitted: &mut Vec<OmittedItem>, item: OmittedItem) -> bool {
    if omitted.len() >= MAX_OMITTED_ENTRIES {
        return false;
    }
    omitted.push(item);
    true
}

pub fn excerpt_chars(data: &ContextData) -> u64 {
    data.items
        .iter()
        .filter_map(|item| item.excerpt.as_deref())
        .map(|excerpt| excerpt.chars().count() as u64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::capsule::{
        CapsuleItemKind, ContextItem, ItemProvenance, ItemResolution, OmissionReason, OmittedItem,
        SizeReport,
    };

    fn item(kind: CapsuleItemKind, name: &str, excerpt_chars: usize) -> ContextItem {
        ContextItem::new(
            kind,
            format!("{name} reason"),
            ItemProvenance::RustAnalyzer,
            ItemResolution::Resolved,
        )
        .with_symbol(name)
        .with_excerpt("x".repeat(excerpt_chars))
        .with_id()
    }

    fn empty_data() -> ContextData {
        ContextData {
            action: "prepare".to_owned(),
            status: "OK".to_owned(),
            capsule_id: Some("capsule".to_owned()),
            previous_capsule_id: None,
            root_epoch: Some(1),
            purpose: None,
            change_id: None,
            anchors: Vec::new(),
            identity: None,
            items: Vec::new(),
            omitted: Vec::new(),
            delta: None,
            page: None,
            sizes: SizeReport::default(),
            notes: vec!["evidence is untrusted".to_owned()],
        }
    }

    #[test]
    fn item_limit_keeps_the_most_load_bearing_items() {
        let mut items = vec![
            item(CapsuleItemKind::Dependency, "serde", 64),
            item(CapsuleItemKind::FileExcerpt, "src/lib.rs", 64),
            item(CapsuleItemKind::Definition, "Widget", 64),
            item(CapsuleItemKind::Consumer, "app", 64),
            item(CapsuleItemKind::TestReference, "test", 64),
        ];
        let mut omitted = Vec::new();
        let overflow = cap_items(&mut items, &mut omitted, 3);
        assert_eq!(overflow, 0);
        let kinds = items.iter().map(|item| item.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                CapsuleItemKind::Definition,
                CapsuleItemKind::Consumer,
                CapsuleItemKind::TestReference
            ]
        );
        assert_eq!(omitted.len(), 2);
        assert!(
            omitted
                .iter()
                .all(|entry| entry.reason == OmissionReason::ItemLimit)
        );
    }

    #[test]
    fn budget_trimming_removes_items_with_a_visible_reason() {
        let mut data = empty_data();
        data.items = vec![
            item(CapsuleItemKind::Definition, "Widget", 2_000),
            item(CapsuleItemKind::Dependency, "serde", 512),
        ];
        let outcome = apply_budget_and_sizes(&mut data, 1_500);
        assert!(
            outcome.bytes <= 1_500,
            "bytes={} outcome={outcome:?}",
            outcome.bytes
        );
        assert!(
            data.sizes.bytes <= 1_500,
            "published sizes.bytes={} must respect the budget",
            data.sizes.bytes
        );
        assert!(
            outcome.trimmed_excerpts > 0 || outcome.removed_items > 0,
            "budget pressure must trim or remove content"
        );
        assert!(
            data.items
                .iter()
                .any(|item| item.kind == CapsuleItemKind::Definition),
            "definition must survive longest"
        );
        if outcome.removed_items > 0 {
            assert!(
                data.omitted
                    .iter()
                    .any(|entry| entry.reason == OmissionReason::Budget),
                "removed items must be listed with a budget reason"
            );
        }
    }

    #[test]
    fn apply_budget_and_sizes_holds_the_hard_bound_with_notes_and_omissions() {
        let mut data = empty_data();
        data.items = vec![
            item(CapsuleItemKind::Definition, "Widget", 2_000),
            item(CapsuleItemKind::Consumer, "app", 2_000),
        ];
        data.notes.push("n".repeat(400));
        for _ in 0..40 {
            data.omitted.push(OmittedItem::new(
                OmissionReason::Unavailable,
                "o".repeat(96),
            ));
        }
        let outcome = apply_budget_and_sizes(&mut data, 1_500);
        assert!(outcome.bytes <= 1_500, "outcome bytes={}", outcome.bytes);
        assert_eq!(outcome.bytes, wire_bytes(&data));
        assert!(
            data.sizes.bytes <= 1_500,
            "published sizes.bytes={} must respect the budget",
            data.sizes.bytes
        );
        assert_eq!(
            data.sizes.bytes,
            serde_json::to_vec(&data).expect("measure payload").len() as u64,
            "size report must match the serialized payload"
        );
    }

    #[test]
    fn dropped_omissions_are_counted_in_the_outcome() {
        let mut data = empty_data();
        data.notes.clear();
        for _ in 0..40 {
            data.omitted
                .push(OmittedItem::new(OmissionReason::Budget, "z".repeat(96)));
        }
        let outcome = apply_budget_and_sizes(&mut data, 512);
        assert!(
            outcome.omitted_overflow > 0,
            "silently dropped omissions must be counted: {outcome:?}"
        );
        assert!(data.sizes.bytes <= 512, "bytes={}", data.sizes.bytes);
    }
}
