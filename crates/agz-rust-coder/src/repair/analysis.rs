//! Pure analysis helpers for the `repair` tool: diagnostic grouping,
//! reasoned-hypothesis root-cause relations, source-backed ownership
//! explanations, explicit mechanical transforms, measured deltas, and
//! comparison selection rules.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::change::{ChangeDiagnosticData, PatchInput};

use super::guards;
use super::model::{
    MAX_ANALYZED_DIAGNOSTICS, MAX_GROUPS, MAX_LISTED_CANDIDATES, MAX_MESSAGES_PER_GROUP,
    MAX_OWNERSHIP_EXPLANATIONS, MAX_RELATIONS, MAX_SOURCE_EXCERPTS, RepairCandidateData,
    RepairChangedData, RepairDeltaData, RepairDiagnosticGroupData, RepairEliminationData,
    RepairExcerptData, RepairOwnershipData, RepairPatchData, RepairRelationData,
    RepairSelectionData, RepairSpanData,
};

/// Stable diagnostic identity used by `diagnosticIds` filters and output.
pub(crate) fn diagnostic_id(diagnostic: &ChangeDiagnosticData) -> String {
    let code = diagnostic.code.as_deref().unwrap_or("diagnostic");
    match (diagnostic.file.as_deref(), diagnostic.line) {
        (Some(file), Some(line)) => format!("{code}@{file}:{line}"),
        (None, Some(line)) => format!("{code}@line:{line}"),
        (Some(file), None) => format!("{code}@{file}"),
        (None, None) => code.to_owned(),
    }
}

fn bounded(message: &str, max: usize) -> String {
    if message.len() <= max {
        return message.to_owned();
    }
    let mut end = max;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

/// Filters diagnostics by reported ids, codes, or group ids. Returns the
/// filtered diagnostics and the requested ids that matched nothing.
pub(crate) fn filter_diagnostics(
    diagnostics: &[ChangeDiagnosticData],
    requested: &[String],
) -> (Vec<ChangeDiagnosticData>, Vec<String>) {
    if requested.is_empty() {
        return (diagnostics.to_vec(), Vec::new());
    }
    let groups = group_diagnostics(diagnostics);
    let mut matched = BTreeSet::new();
    let mut filtered = Vec::new();
    for diagnostic in diagnostics {
        let id = diagnostic_id(diagnostic);
        let code = diagnostic.code.as_deref().unwrap_or_default();
        let group_match = groups.iter().any(|group| {
            requested.iter().any(|request| request == &group.group_id)
                && group.diagnostic_ids.contains(&id)
        });
        let direct = requested
            .iter()
            .any(|request| request == &id || request == code);
        if direct || group_match {
            filtered.push(diagnostic.clone());
            for request in requested {
                if request == &id || request == code || {
                    groups.iter().any(|group| {
                        request == &group.group_id && group.diagnostic_ids.contains(&id)
                    })
                } {
                    matched.insert(request.clone());
                }
            }
        }
    }
    let unmatched = requested
        .iter()
        .filter(|request| !matched.contains(*request))
        .take(16)
        .cloned()
        .collect();
    (filtered, unmatched)
}

/// Groups bounded diagnostics by error code and file. The first diagnostic in
/// a group is the primary anchor; later lines become secondary spans.
pub(crate) fn group_diagnostics(
    diagnostics: &[ChangeDiagnosticData],
) -> Vec<RepairDiagnosticGroupData> {
    let mut order: Vec<(String, Option<String>)> = Vec::new();
    let mut buckets: BTreeMap<(String, Option<String>), Vec<&ChangeDiagnosticData>> =
        BTreeMap::new();
    for diagnostic in diagnostics.iter().take(MAX_ANALYZED_DIAGNOSTICS) {
        let key = (
            diagnostic
                .code
                .clone()
                .unwrap_or_else(|| "diagnostic".to_owned()),
            diagnostic.file.clone(),
        );
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_default().push(diagnostic);
    }
    order
        .into_iter()
        .take(MAX_GROUPS)
        .enumerate()
        .map(|(index, key)| {
            let entries = &buckets[&key];
            let first = entries[0];
            let mut secondary = Vec::new();
            let mut seen_lines = BTreeSet::new();
            for entry in entries.iter().skip(1) {
                if entry.line.is_some() && seen_lines.insert(entry.line) {
                    secondary.push(RepairSpanData {
                        file: entry.file.clone(),
                        line: entry.line,
                        label: None,
                    });
                }
                if secondary.len() >= 8 {
                    break;
                }
            }
            RepairDiagnosticGroupData {
                group_id: format!("dg-{}", index + 1),
                codes: vec![key.0.clone()],
                level: first.level.clone(),
                primary: RepairSpanData {
                    file: first.file.clone(),
                    line: first.line,
                    label: None,
                },
                secondary,
                count: u64::try_from(entries.len()).unwrap_or(u64::MAX),
                messages: entries
                    .iter()
                    .take(MAX_MESSAGES_PER_GROUP)
                    .map(|entry| bounded(&entry.message, 512))
                    .collect(),
                diagnostic_ids: entries
                    .iter()
                    .take(16)
                    .map(|entry| diagnostic_id(entry))
                    .collect(),
            }
        })
        .collect()
}

const UNRESOLVED_CODES: &[&str] = &["E0432", "E0433", "E0405", "E0412", "E0425"];

/// Derives root-cause links between groups. Every link is a reasoned
/// hypothesis: it is consistent with the anchored spans but is not proven.
pub(crate) fn relations(groups: &[RepairDiagnosticGroupData]) -> Vec<RepairRelationData> {
    let mut relations = Vec::new();
    for (index, left) in groups.iter().enumerate() {
        for right in groups.iter().skip(index + 1) {
            if relations.len() >= MAX_RELATIONS {
                return relations;
            }
            if left.primary.file.is_none() || left.primary.file != right.primary.file {
                continue;
            }
            let (Some(left_line), Some(right_line)) = (left.primary.line, right.primary.line)
            else {
                continue;
            };
            let file = left.primary.file.clone().unwrap_or_default();
            if left_line.abs_diff(right_line) <= 3 {
                relations.push(RepairRelationData {
                    from: left.group_id.clone(),
                    to: right.group_id.clone(),
                    kind: "adjacentRootCause".to_owned(),
                    hypothesis: true,
                    reasoning: format!(
                        "{} and {} are anchored within three lines in {file}; one local ownership or type decision may resolve both. This is a reasoned hypothesis, not a proven root cause.",
                        left.codes.join(","),
                        right.codes.join(",")
                    ),
                    evidence: vec![
                        format!("{}:{}", left.codes.join(","), left_line),
                        format!("{}:{}", right.codes.join(","), right_line),
                    ],
                });
                continue;
            }
            let left_unresolved = left
                .codes
                .iter()
                .any(|code| UNRESOLVED_CODES.contains(&code.as_str()));
            let right_unresolved = right
                .codes
                .iter()
                .any(|code| UNRESOLVED_CODES.contains(&code.as_str()));
            if left_unresolved && left_line < right_line {
                relations.push(RepairRelationData {
                    from: left.group_id.clone(),
                    to: right.group_id.clone(),
                    kind: "cascadeHypothesis".to_owned(),
                    hypothesis: true,
                    reasoning: format!(
                        "The unresolved name reported by {} at {file}:{left_line} may make the later {} diagnostic at line {right_line} secondary. Resolve the unresolved name first. This is a reasoned hypothesis.",
                        left.codes.join(","),
                        right.codes.join(",")
                    ),
                    evidence: vec![format!("{file}:{left_line}"), format!("{file}:{right_line}")],
                });
            } else if right_unresolved && right_line < left_line {
                relations.push(RepairRelationData {
                    from: right.group_id.clone(),
                    to: left.group_id.clone(),
                    kind: "cascadeHypothesis".to_owned(),
                    hypothesis: true,
                    reasoning: format!(
                        "The unresolved name reported by {} at {file}:{right_line} may make the later {} diagnostic at line {left_line} secondary. Resolve the unresolved name first. This is a reasoned hypothesis.",
                        right.codes.join(","),
                        left.codes.join(",")
                    ),
                    evidence: vec![format!("{file}:{right_line}"), format!("{file}:{left_line}")],
                });
            }
        }
    }
    relations
}

fn first_backticked(message: &str) -> Option<String> {
    let start = message.find('`')?;
    let rest = &message[start + 1..];
    let end = rest.find('`')?;
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn moved_symbol(message: &str) -> Option<String> {
    for prefix in [
        "borrow of moved value: ",
        "use of moved value: ",
        "value moved here: ",
        "cannot move out of ",
    ] {
        if let Some(rest) = message
            .find(prefix)
            .map(|index| &message[index + prefix.len()..])
        {
            if let Some(symbol) = first_backticked(rest) {
                return Some(symbol);
            }
        }
    }
    if message.contains("moved") {
        return first_backticked(message);
    }
    None
}

fn trait_obligation(message: &str) -> Option<(Option<String>, String)> {
    if let Some(index) = message.find("the trait bound ") {
        let rest = &message[index + "the trait bound ".len()..];
        let bound = first_backticked(rest)?;
        if let Some((type_name, trait_name)) = bound.split_once(':') {
            let trait_name = trait_name.trim().trim_start_matches('~').trim();
            if !trait_name.is_empty() {
                return Some((
                    (!type_name.trim().is_empty()).then(|| type_name.trim().to_owned()),
                    trait_name.to_owned(),
                ));
            }
        }
        return Some((None, bound));
    }
    if let Some(index) = message.find("doesn't implement ") {
        let trait_name = first_backticked(&message[index + "doesn't implement ".len()..])?;
        let type_name = first_backticked(message);
        return Some((type_name, trait_name));
    }
    if let Some(index) = message.find("no method named ") {
        let rest = &message[index + "no method named ".len()..];
        let method = first_backticked(rest);
        let type_name = rest
            .find(" for ")
            .and_then(|offset| first_backticked(&rest[offset..]));
        return Some((type_name.or(method), "method bound".to_owned()));
    }
    None
}

fn line_bounds(text: &str) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut start = 0usize;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            let end = if index > start && text.as_bytes()[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            bounds.push((start, end));
            start = index + 1;
        }
    }
    if start < text.len() {
        bounds.push((start, text.len()));
    }
    bounds
}

fn excerpt(file: &str, text: &str, line: usize, role: &str) -> Option<RepairExcerptData> {
    let lines: Vec<&str> = text.lines().collect();
    let line_text = lines.get(line.saturating_sub(1))?;
    let text = bounded(line_text.trim(), 240);
    if text.is_empty() {
        return None;
    }
    Some(RepairExcerptData {
        file: file.to_owned(),
        line_start: u64::try_from(line).unwrap_or(u64::MAX),
        line_end: u64::try_from(line).unwrap_or(u64::MAX),
        role: role.to_owned(),
        text,
    })
}

fn symbol_excerpts(
    file: &str,
    line: u64,
    source: &str,
    needles: &[(&str, &str)],
) -> Vec<RepairExcerptData> {
    let mut selected: BTreeMap<u64, (&str, String)> = BTreeMap::new();
    let diagnostic_line = usize::try_from(line).unwrap_or(usize::MAX);
    if let Some(entry) = excerpt(file, source, diagnostic_line, "diagnosticSite") {
        selected.insert(line, ("diagnosticSite", entry.text));
    }
    for (needle, role) in needles {
        if needle.is_empty() {
            continue;
        }
        for (index, text) in source.lines().enumerate() {
            if selected.len() >= MAX_SOURCE_EXCERPTS {
                break;
            }
            if text.contains(needle) {
                let line_number = u64::try_from(index + 1).unwrap_or(u64::MAX);
                selected
                    .entry(line_number)
                    .or_insert_with(|| (*role, bounded(text.trim(), 240)));
            }
        }
    }
    selected
        .into_iter()
        .take(MAX_SOURCE_EXCERPTS)
        .filter(|(_, (_, text))| !text.is_empty())
        .map(|(line, (role, text))| RepairExcerptData {
            file: file.to_owned(),
            line_start: line,
            line_end: line,
            role: role.to_owned(),
            text,
        })
        .collect()
}

/// Builds source-backed ownership explanations. When the candidate source is
/// unavailable the explanation still states the compiler evidence, but the
/// source list stays empty and the reason is explicit.
pub(crate) fn ownership(
    diagnostics: &[ChangeDiagnosticData],
    read: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<RepairOwnershipData> {
    let mut explanations = Vec::new();
    for diagnostic in diagnostics {
        if explanations.len() >= MAX_OWNERSHIP_EXPLANATIONS {
            break;
        }
        let code = diagnostic.code.as_deref().unwrap_or_default();
        let message = diagnostic.message.as_str();
        let (kind, symbol, trait_name, explanation) = if matches!(code, "E0382" | "E0505" | "E0507")
            || message.contains("moved value")
            || message.contains("use of moved value")
        {
            let symbol = moved_symbol(message);
            (
                "movedValue",
                symbol.clone(),
                None,
                format!(
                    "The compiler reports {} moved before this use. Restructuring ownership (borrow from the caller, take a reference parameter, or return the value) preserves behavior; cloning is a candidate but is surfaced as a performance impact, not accepted automatically.",
                    symbol
                        .as_deref()
                        .map_or_else(|| "a value".to_owned(), |name| format!("`{name}`"))
                ),
            )
        } else if matches!(code, "E0502" | "E0499" | "E0596" | "E0503" | "E0506")
            || message.contains("cannot borrow")
        {
            let symbol = first_backticked(message);
            (
                "borrowedValue",
                symbol.clone(),
                None,
                format!(
                    "An aliasing or mutability conflict is reported for {}. Narrow or reorder the borrows so only one mutable access is live at a time; do not silence the borrow checker.",
                    symbol
                        .as_deref()
                        .map_or_else(|| "the value".to_owned(), |name| format!("`{name}`"))
                ),
            )
        } else if matches!(code, "E0277" | "E0599")
            || message.contains("trait bound")
            || message.contains("doesn't implement")
            || message.contains("no method named")
        {
            let (type_name, trait_name) = trait_obligation(message)
                .unwrap_or_else(|| (None, "the required trait".to_owned()));
            (
                "missingTraitBound",
                type_name.clone(),
                Some(trait_name.clone()),
                format!(
                    "The trait obligation `{}: {trait_name}` is not satisfied at this use. Satisfy it by adding the bound, deriving the local type, or changing the generic constraint; the compiler suggestion package and the mechanical derive candidate are advisory until a compile confirms them.",
                    type_name.as_deref().unwrap_or("value")
                ),
            )
        } else {
            continue;
        };
        let source_evidence = match (diagnostic.file.as_deref(), diagnostic.line) {
            (Some(file), Some(line)) => read(file)
                .map(|source| {
                    let mut needles: Vec<(&str, &str)> = Vec::new();
                    if let Some(symbol) = symbol.as_deref() {
                        needles.push((symbol, "symbolUse"));
                    }
                    if let Some(trait_name) = trait_name.as_deref() {
                        needles.push((trait_name, "obligationSite"));
                    }
                    symbol_excerpts(file, line, &source, &needles)
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        explanations.push(RepairOwnershipData {
            diagnostic_id: diagnostic_id(diagnostic),
            kind: kind.to_owned(),
            symbol,
            trait_name,
            explanation,
            source_evidence,
        });
    }
    explanations
}

/// One generated mechanical candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MechanicalCandidate {
    pub id: String,
    pub patches: Vec<PatchInput>,
}

/// A few explicit mechanical transforms. They are heuristics, are always
/// verified by a real compile before selection, and never write the workspace.
pub(crate) fn mechanical_candidates(
    diagnostics: &[ChangeDiagnosticData],
    read: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<MechanicalCandidate> {
    let mut candidates = Vec::new();
    let mut seen_hashes = BTreeSet::new();
    for diagnostic in diagnostics.iter().take(MAX_ANALYZED_DIAGNOSTICS) {
        if candidates.len() >= MAX_LISTED_CANDIDATES {
            break;
        }
        let Some(file) = diagnostic.file.as_deref() else {
            continue;
        };
        let Some(source) = read(file) else {
            continue;
        };
        let code = diagnostic.code.as_deref().unwrap_or_default();
        let generated = if code == "E0382" {
            clone_at_move(diagnostic, file, &source)
                .map(|patch| ("mechanical-clone-at-move".to_owned(), vec![patch]))
        } else if code == "E0277" {
            derive_trait(diagnostic, file, &source)
                .map(|patch| ("mechanical-derive-trait".to_owned(), vec![patch]))
        } else if UNRESOLVED_CODES.contains(&code) {
            known_import(diagnostic, file, &source)
                .map(|patch| ("mechanical-known-import".to_owned(), vec![patch]))
        } else {
            None
        };
        let Some((id, patches)) = generated else {
            continue;
        };
        let hash = candidate_hash(&patches);
        if seen_hashes.insert(hash) {
            candidates.push(MechanicalCandidate { id, patches });
        }
    }
    candidates
}

fn candidate_line(diagnostic: &ChangeDiagnosticData) -> Option<usize> {
    diagnostic
        .line
        .and_then(|line| usize::try_from(line).ok())
        .filter(|line| *line > 0)
}

fn unique_window(
    source: &str,
    anchor: usize,
    replacement: Option<(&str, &str)>,
) -> Option<(String, String)> {
    let bounds = line_bounds(source);
    let anchor = anchor.checked_sub(1)?;
    if anchor >= bounds.len() {
        return None;
    }
    for radius in 0..=8usize {
        let start = anchor.saturating_sub(radius);
        let end = (anchor + radius).min(bounds.len().saturating_sub(1));
        let excerpt = source.get(bounds[start].0..bounds[end].1)?.to_owned();
        if excerpt.is_empty() || count_occurrences(source, &excerpt) != 1 {
            continue;
        }
        if let Some((needle, replacement)) = replacement {
            let Some(offset) = excerpt.find(needle) else {
                continue;
            };
            let new_excerpt = format!(
                "{}{}{}",
                &excerpt[..offset],
                replacement,
                &excerpt[offset + needle.len()..]
            );
            return Some((excerpt, new_excerpt));
        }
        return Some((excerpt.clone(), excerpt));
    }
    None
}

fn count_occurrences(source: &str, needle: &str) -> usize {
    if needle.is_empty() {
        usize::MAX
    } else {
        source.match_indices(needle).count()
    }
}

fn clone_at_move(
    diagnostic: &ChangeDiagnosticData,
    file: &str,
    source: &str,
) -> Option<PatchInput> {
    let symbol = moved_symbol(&diagnostic.message)?;
    let line = candidate_line(diagnostic)?;
    let lines: Vec<&str> = source.lines().collect();
    let text = lines.get(line - 1)?;
    let offset = text.find(&symbol)?;
    if text[offset + symbol.len()..].starts_with(".clone()") {
        return None;
    }
    let replacement = format!("{symbol}.clone()");
    let (old_string, new_string) = unique_window(source, line, Some((&symbol, &replacement)))?;
    Some(PatchInput {
        file: file.to_owned(),
        old_string,
        new_string,
    })
}

const DERIVABLE_TRAITS: &[&str] = &[
    "Clone",
    "Copy",
    "Debug",
    "Default",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
];

fn derive_trait(diagnostic: &ChangeDiagnosticData, file: &str, source: &str) -> Option<PatchInput> {
    let (type_name, trait_name) = trait_obligation(&diagnostic.message)?;
    let type_name = type_name?;
    let trait_name = trait_name.trim();
    if !DERIVABLE_TRAITS.contains(&trait_name) {
        return None;
    }
    let lines: Vec<&str> = source.lines().collect();
    let definition = lines.iter().position(|line| {
        let line = line.trim_start_matches("pub ");
        line.starts_with(&format!("struct {type_name}"))
            || line.starts_with(&format!("enum {type_name}"))
    })?;
    let definition_line = definition + 1;
    let previous = definition.checked_sub(1).map(|index| lines[index]);
    if let Some(previous) = previous {
        let previous_trimmed = previous.trim();
        if previous_trimmed.starts_with("#[derive(") && previous_trimmed.ends_with(")]") {
            if previous_trimmed.contains(trait_name) {
                return None;
            }
            let inner = &previous_trimmed["#[derive(".len()..previous_trimmed.len() - 2];
            let updated = format!("#[derive({inner}, {trait_name})]");
            let anchor = definition_line - 1;
            let (old_string, new_string) =
                unique_window(source, anchor, Some((previous_trimmed, &updated)))?;
            return Some(PatchInput {
                file: file.to_owned(),
                old_string,
                new_string,
            });
        }
    }
    let definition_text = lines[definition].to_owned();
    let (old_string, new_string) = unique_window(
        source,
        definition_line,
        Some((
            &definition_text,
            &format!("#[derive({trait_name})]\n{definition_text}"),
        )),
    )?;
    Some(PatchInput {
        file: file.to_owned(),
        old_string,
        new_string,
    })
}

const KNOWN_IMPORTS: &[(&str, &str)] = &[
    ("HashMap", "std::collections::HashMap"),
    ("HashSet", "std::collections::HashSet"),
    ("BTreeMap", "std::collections::BTreeMap"),
    ("BTreeSet", "std::collections::BTreeSet"),
    ("VecDeque", "std::collections::VecDeque"),
    ("BinaryHeap", "std::collections::BinaryHeap"),
    ("Arc", "std::sync::Arc"),
    ("Rc", "std::rc::Rc"),
    ("Mutex", "std::sync::Mutex"),
    ("RwLock", "std::sync::RwLock"),
    ("PathBuf", "std::path::PathBuf"),
    ("Duration", "std::time::Duration"),
    ("Instant", "std::time::Instant"),
    ("IpAddr", "std::net::IpAddr"),
    ("SocketAddr", "std::net::SocketAddr"),
];

fn known_import(diagnostic: &ChangeDiagnosticData, file: &str, source: &str) -> Option<PatchInput> {
    let message = &diagnostic.message;
    let name = [
        "use of undeclared type `",
        "cannot find type `",
        "cannot find struct, variant or union type `",
        "use of undeclared crate or module `",
        "failed to resolve: use of undeclared type `",
    ]
    .iter()
    .find_map(|prefix| {
        let index = message.find(prefix)?;
        let rest = &message[index + prefix.len()..];
        Some(rest[..rest.find('`')?].to_owned())
    })?;
    let path = KNOWN_IMPORTS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, path)| *path)?;
    if source.contains(&format!("use {path};")) {
        return None;
    }
    let lines: Vec<&str> = source.lines().collect();
    let anchor = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("use "))
        .or_else(|| {
            let mut last = None;
            for (index, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("#![") || trimmed.starts_with("//!") {
                    last = Some(index);
                } else if !trimmed.is_empty() {
                    break;
                }
            }
            last
        })
        .or_else(|| (!lines.is_empty()).then_some(0))?;
    let anchor_line = anchor + 1;
    let anchor_text = lines[anchor].to_owned();
    let (old_string, new_string) = unique_window(
        source,
        anchor_line,
        Some((&anchor_text, &format!("{anchor_text}\nuse {path};"))),
    )?;
    Some(PatchInput {
        file: file.to_owned(),
        old_string,
        new_string,
    })
}

/// Measured diagnostic delta between the base failure and a candidate result.
pub(crate) fn diagnostic_delta(
    before: &[ChangeDiagnosticData],
    after: &[ChangeDiagnosticData],
) -> RepairDeltaData {
    let before_counts = error_counts(before);
    let after_counts = error_counts(after);
    let mut fixed_codes = Vec::new();
    let mut remaining_codes = Vec::new();
    let mut new_codes = Vec::new();
    for (code, count) in &before_counts {
        match after_counts.get(code) {
            Some(after_count) if after_count >= count => remaining_codes.push(code.clone()),
            Some(_) => fixed_codes.push(code.clone()),
            None => fixed_codes.push(code.clone()),
        }
    }
    for code in after_counts.keys() {
        if !before_counts.contains_key(code) {
            new_codes.push(code.clone());
        }
    }
    RepairDeltaData {
        before_errors: before_counts.values().sum(),
        after_errors: after_counts.values().sum(),
        fixed_codes,
        remaining_codes,
        new_codes,
    }
}

fn error_counts(diagnostics: &[ChangeDiagnosticData]) -> BTreeMap<String, u64> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for diagnostic in diagnostics {
        if diagnostic.level != "error" {
            continue;
        }
        let code = diagnostic
            .code
            .clone()
            .unwrap_or_else(|| "uncoded".to_owned());
        *counts.entry(code).or_default() += 1;
    }
    counts
}

/// Changed-file, line, and public API summary for one candidate's patches.
pub(crate) fn changed_summary(patches: &[PatchInput]) -> RepairChangedData {
    let mut files = BTreeSet::new();
    for patch in patches {
        files.insert(patch.file.clone());
    }
    let (lines_added, lines_removed) = guards::line_delta(patches);
    let (public_api_added, public_api_removed) = guards::public_api_delta(patches);
    RepairChangedData {
        files: files.into_iter().collect(),
        lines_added,
        lines_removed,
        public_api_added,
        public_api_removed,
    }
}

/// Stable content hash for one candidate's patch set. Patches are canonically
/// ordered so reordered equivalents share one hash and are never retried.
pub(crate) fn candidate_hash(patches: &[PatchInput]) -> String {
    let mut canonical = patches
        .iter()
        .map(|patch| {
            (
                patch.file.clone(),
                patch.old_string.clone(),
                patch.new_string.clone(),
            )
        })
        .collect::<Vec<_>>();
    canonical.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-coder-repair-candidate\0");
    for (file, old_string, new_string) in canonical {
        hasher.update(file.as_bytes());
        hasher.update(b"\0");
        hasher.update(old_string.as_bytes());
        hasher.update(b"\0");
        hasher.update(new_string.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

/// Converts wire patches into domain patch inputs.
pub(crate) fn to_patch_input(patches: &[RepairPatchData]) -> Vec<PatchInput> {
    patches
        .iter()
        .map(|patch| PatchInput {
            file: patch.file.clone(),
            old_string: patch.old_string.clone(),
            new_string: patch.new_string.clone(),
        })
        .collect()
}

/// Comparison selection. A candidate can never be selected when it failed to
/// compile, when it adds a behavior/performance impact, or when the requested
/// test gate failed. This keeps a compile-pass-but-test-breaking candidate
/// below any behavior-preserving candidate.
pub(crate) fn select(
    candidates: &[RepairCandidateData],
    test_target: Option<&str>,
) -> (
    Option<RepairSelectionData>,
    Vec<RepairEliminationData>,
    Vec<String>,
) {
    let mut selectable: Vec<&RepairCandidateData> = candidates
        .iter()
        .filter(|candidate| {
            candidate.status == "measured"
                && candidate.compiled()
                && candidate.behavior_preserving()
                && test_target.is_none_or(|_| candidate.test_passed())
        })
        .collect();
    selectable.sort_by(|left, right| {
        left.error_count()
            .cmp(&right.error_count())
            .then_with(|| changed_lines(left).cmp(&changed_lines(right)))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut eliminated = Vec::new();
    for candidate in candidates {
        if selectable
            .first()
            .is_some_and(|selected| selected.id == candidate.id)
        {
            continue;
        }
        eliminated.push(RepairEliminationData {
            id: candidate.id.clone(),
            reason: elimination_reason(candidate, test_target),
        });
    }

    let mut risks = Vec::new();
    if test_target.is_none() {
        risks.push(
            "No test gate was requested; the comparison is compile-verified only.".to_owned(),
        );
    }
    let Some(selected) = selectable.first() else {
        risks.push(
            "No candidate satisfied every selection rule (compile pass, no candidate-added impact, requested test gate when present)."
                .to_owned(),
        );
        return (None, eliminated, risks);
    };
    let verification = if test_target.is_some() {
        "testVerified".to_owned()
    } else {
        "compileVerified".to_owned()
    };
    if !selected.inherited_impacts.is_empty() {
        risks.push(format!(
            "Selected candidate inherits pre-existing impacts from the base change: {}.",
            selected
                .inherited_impacts
                .iter()
                .map(|impact| impact.kind.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    (
        Some(RepairSelectionData {
            id: selected.id.clone(),
            reason: format!(
                "Selected {} after real compile measurement ({} error diagnostic(s), {} changed line(s)); verification is {verification}.",
                selected.id,
                selected.error_count(),
                changed_lines(selected),
            ),
            verification,
            patches: selected.patches.clone(),
        }),
        eliminated,
        risks,
    )
}

fn changed_lines(candidate: &RepairCandidateData) -> u64 {
    candidate.changed.as_ref().map_or(0, |changed| {
        changed.lines_added.saturating_add(changed.lines_removed)
    })
}

fn elimination_reason(candidate: &RepairCandidateData, test_target: Option<&str>) -> String {
    match candidate.status.as_str() {
        "duplicate" | "budgetExhausted" | "rejected" | "unsupported" | "unavailable"
        | "staleBase" | "cleanupFailure" => bounded(&candidate.reason, 320),
        "measured" => {
            if !candidate.compiled() {
                "candidate did not compile".to_owned()
            } else if !candidate.behavior_preserving() {
                format!(
                    "candidate adds behavior/performance impacts: {}",
                    candidate
                        .impacts
                        .iter()
                        .map(|impact| impact.kind.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else if let Some(target) = test_target {
                if candidate.test_passed() {
                    "another candidate ranked higher".to_owned()
                } else {
                    format!("requested test gate `{target}` did not pass")
                }
            } else {
                "another candidate ranked higher".to_owned()
            }
        }
        _ => "candidate was not measured".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::PatchInput;

    fn diagnostic(code: &str, file: &str, line: u64, message: &str) -> ChangeDiagnosticData {
        ChangeDiagnosticData {
            code: Some(code.to_owned()),
            level: "error".to_owned(),
            file: Some(file.to_owned()),
            line: Some(line),
            message: message.to_owned(),
        }
    }

    fn patch(file: &str, old: &str, new: &str) -> PatchInput {
        PatchInput {
            file: file.to_owned(),
            old_string: old.to_owned(),
            new_string: new.to_owned(),
        }
    }

    fn candidate(id: &str, compile: bool, impacts: Vec<&str>) -> RepairCandidateData {
        let mut candidate = RepairCandidateData::new(
            id.to_owned(),
            "host",
            format!("hash-{id}"),
            vec![RepairPatchData {
                file: "src/lib.rs".to_owned(),
                old_string: "a".to_owned(),
                new_string: "b".to_owned(),
            }],
        );
        candidate.status = "measured".to_owned();
        candidate.compile = Some(if compile { "compiled" } else { "failed" }.to_owned());
        candidate.impacts = impacts
            .into_iter()
            .map(|kind| crate::repair::model::RepairImpactData {
                kind: kind.to_owned(),
                category: "behavior".to_owned(),
                detail: kind.to_owned(),
                origin: "candidateAdded".to_owned(),
            })
            .collect();
        candidate.diagnostics = vec![diagnostic("E0308", "src/lib.rs", 1, "mismatched types")];
        candidate.changed = Some(RepairChangedData {
            files: vec!["src/lib.rs".to_owned()],
            lines_added: 1,
            lines_removed: 1,
            public_api_added: Vec::new(),
            public_api_removed: Vec::new(),
        });
        candidate
    }

    #[test]
    fn diagnostics_group_by_code_and_file_with_secondary_spans() {
        let diagnostics = vec![
            diagnostic("E0382", "src/lib.rs", 4, "borrow of moved value: `text`"),
            diagnostic("E0382", "src/lib.rs", 9, "borrow of moved value: `other`"),
            diagnostic("E0308", "src/lib.rs", 4, "mismatched types"),
            diagnostic("E0433", "src/main.rs", 1, "failed to resolve"),
        ];
        let groups = group_diagnostics(&diagnostics);
        assert_eq!(groups.len(), 3);
        let first = &groups[0];
        assert_eq!(first.codes, vec!["E0382"]);
        assert_eq!(first.count, 2);
        assert_eq!(first.primary.line, Some(4));
        assert_eq!(first.secondary.len(), 1);
        assert_eq!(first.secondary[0].line, Some(9));
        assert!(first.diagnostic_ids[0].contains("E0382@src/lib.rs:4"));
    }

    #[test]
    fn relations_are_labelled_hypotheses() {
        let groups = group_diagnostics(&[
            diagnostic(
                "E0433",
                "src/lib.rs",
                2,
                "failed to resolve: use of undeclared type `HashMap`",
            ),
            diagnostic("E0308", "src/lib.rs", 9, "mismatched types"),
            diagnostic("E0599", "src/lib.rs", 3, "no method named `finish` found"),
        ]);
        let relations = relations(&groups);
        assert!(!relations.is_empty());
        assert!(relations.iter().all(|relation| relation.hypothesis));
        assert!(
            relations
                .iter()
                .any(|relation| relation.kind == "cascadeHypothesis"),
            "{relations:?}"
        );
        assert!(
            relations
                .iter()
                .any(|relation| relation.kind == "adjacentRootCause"),
            "{relations:?}"
        );
    }

    #[test]
    fn ownership_uses_real_source_excerpts_when_available() {
        let diagnostics = vec![
            diagnostic("E0382", "src/lib.rs", 5, "borrow of moved value: `text`"),
            diagnostic(
                "E0277",
                "src/lib.rs",
                8,
                "the trait bound `Config: Debug` is not satisfied",
            ),
        ];
        let source = "pub struct Config { value: u8 }\nfn run() {\n let text = String::new();\n consume(text);\n println!(\"{}\", text);\n}\n";
        let explanations = ownership(&diagnostics, &|file| {
            (file == "src/lib.rs").then(|| source.to_owned())
        });
        assert_eq!(explanations.len(), 2);
        let moved = &explanations[0];
        assert_eq!(moved.kind, "movedValue");
        assert_eq!(moved.symbol.as_deref(), Some("text"));
        assert!(
            moved
                .source_evidence
                .iter()
                .any(|excerpt| excerpt.role == "diagnosticSite" && excerpt.text.contains("text")),
            "{moved:?}"
        );
        let obligation = &explanations[1];
        assert_eq!(obligation.kind, "missingTraitBound");
        assert_eq!(obligation.trait_name.as_deref(), Some("Debug"));
        assert_eq!(obligation.symbol.as_deref(), Some("Config"));
    }

    #[test]
    fn mechanical_transforms_generate_verified_shapes() {
        let diagnostics = vec![
            diagnostic("E0382", "src/lib.rs", 5, "borrow of moved value: `text`"),
            diagnostic(
                "E0277",
                "src/lib.rs",
                1,
                "the trait bound `Config: Debug` is not satisfied",
            ),
            diagnostic(
                "E0412",
                "src/main.rs",
                2,
                "use of undeclared type `HashMap`",
            ),
        ];
        let lib = "pub struct Config { value: u8 }\nfn run() -> usize {\n    let text = String::new();\n    consume(text);\n    text.len()\n}\n";
        let main = "use std::io;\nfn run() -> usize { HashMap::new().len() }\n";
        let candidates = mechanical_candidates(&diagnostics, &|file| match file {
            "src/lib.rs" => Some(lib.to_owned()),
            "src/main.rs" => Some(main.to_owned()),
            _ => None,
        });
        assert_eq!(candidates.len(), 3, "{candidates:?}");
        let clone = &candidates[0].patches[0];
        assert!(clone.old_string.contains("text") && !clone.old_string.contains(".clone()"));
        assert!(clone.new_string.contains("text.clone()"));
        let derive = &candidates[1].patches[0];
        assert!(derive.new_string.contains("#[derive(Debug)]"));
        let import = &candidates[2].patches[0];
        assert!(import.new_string.contains("use std::collections::HashMap;"));
    }

    #[test]
    fn delta_counts_fixed_remaining_and_new_codes() {
        let before = vec![
            diagnostic("E0382", "src/lib.rs", 4, "borrow of moved value"),
            diagnostic("E0308", "src/lib.rs", 8, "mismatched types"),
        ];
        let after = vec![diagnostic("E0308", "src/lib.rs", 8, "mismatched types")];
        let delta = diagnostic_delta(&before, &after);
        assert_eq!(delta.before_errors, 2);
        assert_eq!(delta.after_errors, 1);
        assert_eq!(delta.fixed_codes, vec!["E0382"]);
        assert_eq!(delta.remaining_codes, vec!["E0308"]);
        assert!(delta.new_codes.is_empty());
    }

    #[test]
    fn selection_rejects_guarded_and_non_compiling_candidates() {
        let candidates = vec![
            candidate("clone", true, vec!["unnecessaryClone"]),
            candidate("broken", false, vec![]),
            candidate("clean", true, vec![]),
        ];
        let (selected, eliminated, _) = select(&candidates, None);
        let selected = selected.expect("clean candidate must be selectable");
        assert_eq!(selected.id, "clean");
        assert_eq!(selected.verification, "compileVerified");
        assert_eq!(eliminated.len(), 2);
        assert!(eliminated.iter().any(|entry| entry.id == "clone"));
    }

    #[test]
    fn test_breaking_candidate_never_outranks_behavior_preserving() {
        let mut test_breaker = candidate("breaker", true, vec![]);
        test_breaker.gate = Some(super::super::model::RepairGateData {
            target: "test".to_owned(),
            status: "FAIL".to_owned(),
            exit_code: Some(101),
            total_ms: 4,
            tests_executed: Some(1),
            build_success: Some(true),
        });
        let mut behavior = candidate("behavior", true, vec![]);
        behavior.gate = Some(super::super::model::RepairGateData {
            target: "test".to_owned(),
            status: "PASS".to_owned(),
            exit_code: Some(0),
            total_ms: 4,
            tests_executed: Some(1),
            build_success: Some(true),
        });
        let (selected, eliminated, risks) = select(&[test_breaker, behavior], Some("test"));
        let selected = selected.expect("behavior-preserving candidate must win");
        assert_eq!(selected.id, "behavior");
        assert_eq!(selected.verification, "testVerified");
        assert!(
            eliminated
                .iter()
                .any(|entry| entry.id == "breaker" && entry.reason.contains("test gate")),
            "{eliminated:?}"
        );
        assert!(
            !risks
                .iter()
                .any(|risk| risk.contains("compile-verified only"))
        );
    }

    #[test]
    fn identical_patch_sets_share_one_canonical_hash() {
        let first = vec![
            patch("src/lib.rs", "a", "b"),
            patch("src/other.rs", "c", "d"),
        ];
        let second = vec![
            patch("src/other.rs", "c", "d"),
            patch("src/lib.rs", "a", "b"),
        ];
        assert_eq!(candidate_hash(&first), candidate_hash(&second));
        assert_ne!(
            candidate_hash(&first),
            candidate_hash(&[patch("src/lib.rs", "a", "b")])
        );
    }
}
