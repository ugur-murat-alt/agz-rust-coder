//! Impact mapping and structural edit planning for `change(action=migrate)`.
//!
//! Planning is a pure function over the advisory analysis: it classifies every
//! referenced site through the structural rewrite engine, separates same-named
//! unrelated symbols by definition identity, and records a typed obligation
//! for every site it refuses to rewrite. Budget cuts are always visible in the
//! report and always clear the `complete` flag.

use std::collections::{BTreeMap, BTreeSet};

use super::analyzer::{AnalyzedSite, AnchorAnalysis};
use super::rewrite::{
    CallShape, ParamList, apply_edit, bounded_chars, call_shape, insert_into_params, inside_macro,
    replace_in_params, signature_params, signature_text, split_top_level, token_at_offset,
    tokenize, use_is_public,
};
use crate::change::model::{
    MAX_MIGRATION_EDITS, MAX_MIGRATION_GROUPS, MAX_MIGRATION_IDENTITY_CHECKS,
    MAX_MIGRATION_MESSAGE_CHARS, MAX_MIGRATION_NOTES, MAX_MIGRATION_OBLIGATIONS,
    MAX_MIGRATION_REFERENCES, MAX_MIGRATION_SITES, MigrateRequest, MigrateTransformationKind,
    MigrationApiDiffData, MigrationBudgetData, MigrationEditGroupData, MigrationFlagsData,
    MigrationObligationData, MigrationReportData, MigrationSiteData, MigrationTransformationData,
};
use crate::lsp::Position;
use crate::tools::edits::{position_offset, unique_context};

/// One byte-exact structural edit that will become a patch for the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedEdit {
    pub file: String,
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub group: String,
    pub line: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationPlan {
    pub report: MigrationReportData,
    pub edits: Vec<PlannedEdit>,
    /// `Some((status, reason))` when planning itself must be refused.
    pub refused: Option<(&'static str, String)>,
}

pub(crate) struct PlanBudgets {
    pub max_edits: u64,
}

impl PlanBudgets {
    pub(crate) fn from_request(request: &MigrateRequest) -> Self {
        Self {
            max_edits: request
                .constraints
                .max_edits
                .map_or(MAX_MIGRATION_EDITS, |value| {
                    u64::from(value).min(MAX_MIGRATION_EDITS)
                }),
        }
    }
}

/// Plans a migration from advisory analysis. `read_file` returns candidate
/// bytes for a relative path; it is the only source of source text.
pub(crate) fn plan_migration(
    request: &MigrateRequest,
    analysis: &AnchorAnalysis,
    read_file: &dyn Fn(&str) -> Option<String>,
    budgets: PlanBudgets,
) -> MigrationPlan {
    let transformation = &request.transformation;
    let mut plan = MigrationPlan {
        report: MigrationReportData {
            anchor_file: request.anchor.file.clone(),
            anchor_symbol: request.anchor.symbol.clone(),
            definition_file: analysis.definition.file.clone(),
            definition_line: u64::from(analysis.definition.start_line).saturating_add(1),
            transformation: MigrationTransformationData {
                kind: transformation.kind.as_str().to_owned(),
                parameter: transformation.parameter.clone(),
                argument: transformation.argument.clone(),
                resolved_position: transformation.position.map_or(0, u64::from),
                argument_source: "hostProvided".to_owned(),
            },
            compatibility_scope: "capturedWorkspaceOnly".to_owned(),
            ..MigrationReportData::default()
        },
        edits: Vec::new(),
        refused: None,
    };
    let mut obligations = Vec::new();
    let mut notes = analysis.notes.clone();
    let mut packages = BTreeSet::new();
    let mut feature_hints = BTreeSet::new();

    // The definition must be rewriteable or nothing can be migrated.
    let Some(definition_content) = read_file(&analysis.definition.file) else {
        return refuse(
            plan,
            format!(
                "the anchor definition file {} is not readable in the candidate",
                analysis.definition.file
            ),
        );
    };
    let parsed_before = tokenize(&definition_content);
    let Some((definition_token, definition_list)) = resolve_signature(
        &definition_content,
        &parsed_before,
        &analysis.definition,
        &request.anchor.symbol,
    ) else {
        return refuse(
            plan,
            "the anchor does not resolve to a function signature this engine can rewrite"
                .to_owned(),
        );
    };
    let declared = definition_list.explicit().len();
    let resolved_position = match transformation.kind {
        MigrateTransformationKind::AddParameter => transformation
            .position
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(declared),
        MigrateTransformationKind::ChangeParameter => {
            let Some(position) = transformation
                .position
                .and_then(|value| usize::try_from(value).ok())
            else {
                return refuse(plan, "changeParameter requires position".to_owned());
            };
            position
        }
    };
    if resolved_position > declared
        || (transformation.kind == MigrateTransformationKind::ChangeParameter
            && resolved_position >= declared)
    {
        return refuse(
            plan,
            format!(
                "position {resolved_position} is not valid for {declared} declared parameter(s)"
            ),
        );
    }
    plan.report.transformation.resolved_position =
        u64::try_from(resolved_position).unwrap_or(u64::MAX);
    let Some(definition_edit) = signature_edit(
        &parsed_before,
        &definition_list,
        transformation,
        resolved_position,
    ) else {
        return refuse(
            plan,
            "the anchor signature could not be rewritten structurally".to_owned(),
        );
    };
    let definition_name_offset = parsed_before
        .tokens
        .get(definition_token)
        .map_or(0, |token| token.start);
    let definition_after = apply_edit(&definition_content, &definition_edit).unwrap_or_default();
    let signature_before =
        signature_text(&definition_content, &parsed_before, &definition_list, 400)
            .or_else(|| analysis.signature.clone())
            .unwrap_or_default();
    let parsed_after = tokenize(&definition_after);
    let signature_after = token_at_offset(&parsed_after.tokens, definition_name_offset)
        .and_then(|token| signature_params(&definition_after, &parsed_after, token))
        .and_then(|list| signature_text(&definition_after, &parsed_after, &list, 400))
        .unwrap_or_default();
    let visibility = item_visibility(&definition_content, &parsed_before, definition_token);
    let public_api = match visibility.as_str() {
        "public" => true,
        "traitItem" => owner_public(&definition_content, &parsed_before, definition_token),
        _ => false,
    };
    plan.report.api_diff = MigrationApiDiffData {
        before: signature_before.clone(),
        after: signature_after.clone(),
        changed: signature_before != signature_after,
        visibility,
        public_api,
    };

    let definition_feature = feature_scope(&definition_content, analysis.definition.start_line);
    plan.report.impact.definitions.push(site(
        &analysis.definition.file,
        analysis.definition.start_line,
        analysis.definition.start_character,
        "definition",
        Some(bounded_chars(&signature_after, 200)),
        definition_feature.clone(),
    ));
    plan.report.impact.definitions_total = 1;
    packages.insert(package_of(&analysis.definition.file, read_file));
    if let Some(scope) = definition_feature {
        feature_hints.insert(format!("{}: {scope}", analysis.definition.file));
    }
    plan.edits.push(PlannedEdit {
        file: analysis.definition.file.clone(),
        start: definition_edit.start,
        end: definition_edit.end,
        text: definition_edit.text,
        group: "definition".to_owned(),
        line: u64::from(analysis.definition.start_line).saturating_add(1),
    });

    // Implementations receive the same positional rewrite.
    let mut omitted_edits = 0u64;
    let mut budget_obligation_added = false;
    for implementation in &analysis.implementations {
        if implementation.file == analysis.definition.file
            && implementation.start_line == analysis.definition.start_line
        {
            continue;
        }
        if plan.edits.len() >= budgets.max_edits as usize {
            omitted_edits = omitted_edits.saturating_add(1);
            if !budget_obligation_added {
                budget_obligation_added = true;
                obligations.push(obligation(
                    "budget",
                    None,
                    None,
                    "one or more implementation edits were omitted because the edit budget was reached",
                ));
            }
            continue;
        }
        let Some(content) = read_file(&implementation.file) else {
            obligations.push(obligation(
                "unreadableFile",
                Some(&implementation.file),
                Some(u64::from(implementation.start_line).saturating_add(1)),
                "implementation file is not readable in the candidate",
            ));
            continue;
        };
        packages.insert(package_of(&implementation.file, read_file));
        let parsed = tokenize(&content);
        let Some((token, list)) =
            resolve_signature(&content, &parsed, implementation, &request.anchor.symbol)
        else {
            obligations.push(obligation(
                "unparseableSignature",
                Some(&implementation.file),
                Some(u64::from(implementation.start_line).saturating_add(1)),
                "implementation does not resolve to a rewriteable fn signature",
            ));
            continue;
        };
        if list.explicit().len() != declared {
            obligations.push(obligation(
                "arityMismatch",
                Some(&implementation.file),
                Some(u64::from(implementation.start_line).saturating_add(1)),
                &format!(
                    "implementation declares {} parameter(s) but the anchor declares {declared}",
                    list.explicit().len()
                ),
            ));
            continue;
        }
        let Some(edit) = signature_edit(&parsed, &list, transformation, resolved_position) else {
            obligations.push(obligation(
                "unparseableSignature",
                Some(&implementation.file),
                Some(u64::from(implementation.start_line).saturating_add(1)),
                "implementation parameter list could not be rewritten structurally",
            ));
            continue;
        };
        let _ = token;
        let feature = feature_scope(&content, implementation.start_line);
        if let Some(scope) = &feature {
            feature_hints.insert(format!("{}: {scope}", implementation.file));
        }
        plan.report.impact.implementations.push(site(
            &implementation.file,
            implementation.start_line,
            implementation.start_character,
            "implementation",
            None,
            feature,
        ));
        plan.edits.push(PlannedEdit {
            file: implementation.file.clone(),
            start: edit.start,
            end: edit.end,
            text: edit.text,
            group: "implementation".to_owned(),
            line: u64::from(implementation.start_line).saturating_add(1),
        });
    }
    plan.report.impact.implementations_total =
        u64::try_from(plan.report.impact.implementations.len()).unwrap_or(u64::MAX);

    // Consumers, re-exports, and unrelated same-named symbols.
    let mut consumers_total = 0u64;
    let mut reexports_total = 0u64;
    let mut unrelated_total = 0u64;
    for reference in &analysis.references {
        let Some(content) = read_file(&reference.file) else {
            consumers_total = consumers_total.saturating_add(1);
            obligations.push(obligation(
                "unreadableFile",
                Some(&reference.file),
                Some(u64::from(reference.start_line).saturating_add(1)),
                "consumer file is not readable in the candidate",
            ));
            continue;
        };
        let parsed = tokenize(&content);
        let reference_offset = position_offset(
            &content,
            &Position {
                line: reference.start_line,
                character: reference.start_character,
            },
        );
        let use_context =
            reference_offset.and_then(|offset| use_context(&content, &parsed, offset));
        let feature = feature_scope(&content, reference.start_line);
        if let Some(scope) = &feature {
            feature_hints.insert(format!("{}: {scope}", reference.file));
        }
        match reference.identity {
            Some(false) => {
                unrelated_total = unrelated_total.saturating_add(1);
                plan.report.impact.unrelated.push(site(
                    &reference.file,
                    reference.start_line,
                    reference.start_character,
                    "unrelatedIdentity",
                    Some("reference resolves to a different definition".to_owned()),
                    feature,
                ));
                notes.push(format!(
                    "excluded same-named reference at {}:{} because its definition differs from the anchor",
                    reference.file,
                    reference.start_line.saturating_add(1)
                ));
                continue;
            }
            None => {
                consumers_total = consumers_total.saturating_add(1);
                obligations.push(obligation(
                    "identityUnverified",
                    Some(&reference.file),
                    Some(u64::from(reference.start_line).saturating_add(1)),
                    "definition identity check was inconclusive; the site was not rewritten",
                ));
                continue;
            }
            Some(true) => {}
        }
        let Some(token) = find_site_token(&content, &parsed, reference, &request.anchor.symbol)
        else {
            consumers_total = consumers_total.saturating_add(1);
            obligations.push(obligation(
                "unresolvedSite",
                Some(&reference.file),
                Some(u64::from(reference.start_line).saturating_add(1)),
                "the reference does not cover an identifier this engine can rewrite",
            ));
            continue;
        };
        if let Some(public) = use_context {
            reexports_total = reexports_total.saturating_add(1);
            packages.insert(package_of(&reference.file, read_file));
            plan.report.impact.reexports.push(site(
                &reference.file,
                reference.start_line,
                reference.start_character,
                if public { "reExport" } else { "import" },
                None,
                feature,
            ));
            continue;
        }
        consumers_total = consumers_total.saturating_add(1);
        if inside_macro(&parsed.tokens, parsed.tokens[token].start) {
            obligations.push(obligation(
                "macro",
                Some(&reference.file),
                Some(u64::from(reference.start_line).saturating_add(1)),
                "call site is inside a macro invocation; expansion cannot be rewritten mechanically",
            ));
            continue;
        }
        match call_shape(&parsed, token) {
            Some(CallShape::Macro) => {
                obligations.push(obligation(
                    "macro",
                    Some(&reference.file),
                    Some(u64::from(reference.start_line).saturating_add(1)),
                    "reference is a macro invocation and cannot be rewritten mechanically",
                ));
            }
            Some(CallShape::Value) | None => {
                obligations.push(obligation(
                    "callableValue",
                    Some(&reference.file),
                    Some(u64::from(reference.start_line).saturating_add(1)),
                    "reference is used as a value rather than a direct call; the host must update it",
                ));
            }
            Some(CallShape::Call { open, close }) => {
                let already_omitted = plan.edits.len() >= budgets.max_edits as usize;
                if already_omitted {
                    omitted_edits = omitted_edits.saturating_add(1);
                    if !budget_obligation_added {
                        budget_obligation_added = true;
                        obligations.push(obligation(
                            "budget",
                            None,
                            None,
                            "one or more call-site edits were omitted because the edit budget was reached",
                        ));
                    }
                    continue;
                }
                let Some(args) = split_top_level(&parsed, open, close) else {
                    obligations.push(obligation(
                        "unparseableCall",
                        Some(&reference.file),
                        Some(u64::from(reference.start_line).saturating_add(1)),
                        "call argument list could not be parsed structurally",
                    ));
                    continue;
                };
                let argument_matches = |args: &[(usize, usize)]| {
                    args.get(resolved_position).is_some_and(|(start, end)| {
                        content
                            .get(*start..*end)
                            .is_some_and(|text| text.trim() == transformation.argument.trim())
                    })
                };
                let edit = match transformation.kind {
                    MigrateTransformationKind::AddParameter => {
                        if args.len() == declared {
                            insert_into_params(
                                &parsed,
                                &args,
                                open,
                                close,
                                false,
                                resolved_position,
                                &transformation.argument,
                            )
                        } else if args.len() == declared.saturating_add(1)
                            && argument_matches(&args)
                        {
                            notes.push(format!(
                                "call at {}:{} already carries the host argument",
                                reference.file,
                                reference.start_line.saturating_add(1)
                            ));
                            None
                        } else {
                            obligations.push(obligation(
                                "arityMismatch",
                                Some(&reference.file),
                                Some(u64::from(reference.start_line).saturating_add(1)),
                                &format!(
                                    "call passes {} argument(s) but the anchor declares {declared}",
                                    args.len()
                                ),
                            ));
                            continue;
                        }
                    }
                    MigrateTransformationKind::ChangeParameter => {
                        if args.len() != declared {
                            obligations.push(obligation(
                                "arityMismatch",
                                Some(&reference.file),
                                Some(u64::from(reference.start_line).saturating_add(1)),
                                &format!(
                                    "call passes {} argument(s) but the anchor declares {declared}",
                                    args.len()
                                ),
                            ));
                            continue;
                        }
                        if argument_matches(&args) {
                            None
                        } else {
                            replace_in_params(
                                &args,
                                false,
                                resolved_position,
                                &transformation.argument,
                            )
                        }
                    }
                };
                let Some(edit) = edit else {
                    continue;
                };
                packages.insert(package_of(&reference.file, read_file));
                if let Some(scope) = &feature {
                    feature_hints.insert(format!("{}: {scope}", reference.file));
                }
                plan.report.impact.consumers.push(site(
                    &reference.file,
                    reference.start_line,
                    reference.start_character,
                    "call",
                    Some(bounded_chars(&transformation.argument, 200)),
                    feature,
                ));
                plan.edits.push(PlannedEdit {
                    file: reference.file.clone(),
                    start: edit.start,
                    end: edit.end,
                    text: edit.text,
                    group: "consumer".to_owned(),
                    line: u64::from(reference.start_line).saturating_add(1),
                });
            }
        }
    }

    plan.report.impact.consumers_total = consumers_total;
    plan.report.impact.reexports_total = reexports_total;
    plan.report.impact.unrelated_total = unrelated_total;
    // Site listings stay bounded; the `*Total` counters above retain the
    // pre-limit counts so a truncation is visible rather than silent.
    plan.report.impact.definitions.truncate(MAX_MIGRATION_SITES);
    plan.report
        .impact
        .implementations
        .truncate(MAX_MIGRATION_SITES);
    plan.report.impact.consumers.truncate(MAX_MIGRATION_SITES);
    plan.report.impact.reexports.truncate(MAX_MIGRATION_SITES);
    plan.report.impact.unrelated.truncate(MAX_MIGRATION_SITES);
    plan.report.impact.packages = packages.into_iter().collect();
    plan.report.impact.feature_hints = feature_hints.into_iter().take(16).collect();

    let omitted_references = analysis
        .references_total
        .saturating_sub(u64::try_from(analysis.references.len()).unwrap_or(u64::MAX));
    plan.report.budget = MigrationBudgetData {
        max_references: request
            .constraints
            .max_references
            .map_or(MAX_MIGRATION_REFERENCES, u64::from),
        references_total: analysis.references_total,
        max_identity_checks: request
            .constraints
            .max_identity_checks
            .map_or(MAX_MIGRATION_IDENTITY_CHECKS, u64::from),
        identity_checks: analysis.identity_checks,
        max_edits: budgets.max_edits,
        edits_planned: u64::try_from(plan.edits.len()).unwrap_or(u64::MAX),
        omitted_references,
        omitted_edits,
        truncated: omitted_references > 0 || omitted_edits > 0,
    };
    if omitted_references > 0 {
        notes.push(format!(
            "{omitted_references} reference(s) were omitted by the reference budget and are not migrated"
        ));
    }
    if omitted_edits > 0 {
        notes.push(format!(
            "{omitted_edits} edit(s) were omitted by the edit budget and are not migrated"
        ));
    }
    plan.report.flags = flags(transformation, resolved_position, declared);
    let obligation_total = u64::try_from(obligations.len()).unwrap_or(u64::MAX);
    plan.report.obligations = truncate_obligations(obligations);
    plan.report.obligations_total = obligation_total;
    plan.report.complete = plan.report.obligations_total == 0 && !plan.report.budget.truncated;
    plan.report.notes = notes.into_iter().take(MAX_MIGRATION_NOTES).collect();
    plan.report.edit_groups = edit_groups(&plan.edits);
    plan
}

fn refuse(mut plan: MigrationPlan, reason: String) -> MigrationPlan {
    plan.refused = Some(("MIGRATION_REFUSED", reason));
    plan.report.complete = false;
    plan
}

fn truncate_obligations(
    mut obligations: Vec<MigrationObligationData>,
) -> Vec<MigrationObligationData> {
    if obligations.len() > MAX_MIGRATION_OBLIGATIONS {
        obligations.truncate(MAX_MIGRATION_OBLIGATIONS.saturating_sub(1));
        obligations.push(obligation(
            "truncated",
            None,
            None,
            "additional obligations were truncated; see obligationsTotal",
        ));
    }
    obligations
}

fn edit_groups(edits: &[PlannedEdit]) -> Vec<MigrationEditGroupData> {
    let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    for edit in edits.iter().take(MAX_MIGRATION_EDITS as usize) {
        *counts
            .entry((edit.group.clone(), edit.file.clone()))
            .or_default() += 1;
    }
    counts
        .into_iter()
        .take(MAX_MIGRATION_GROUPS)
        .map(|((kind, label), sites)| MigrationEditGroupData { kind, label, sites })
        .collect()
}

fn flags(
    transformation: &crate::change::model::MigrateTransformationInput,
    position: usize,
    declared: usize,
) -> MigrationFlagsData {
    let mut reasons = vec![
        "the public signature changes; consumers are compiled against the new signature".to_owned(),
        "compile-pass does not prove semantic equivalence".to_owned(),
    ];
    let evaluation_order_risk =
        transformation.kind == MigrateTransformationKind::AddParameter && position < declared;
    let move_borrow_risk = argument_risk(&transformation.argument);
    if evaluation_order_risk {
        reasons.push(
            "the new argument is evaluated before existing arguments, which can change evaluation order and side effects"
                .to_owned(),
        );
    }
    if move_borrow_risk {
        reasons.push(
            "the host argument is not a literal; it can move or borrow values at the call position differently from the old call"
                .to_owned(),
        );
    }
    if transformation.kind == MigrateTransformationKind::ChangeParameter {
        reasons.push("an existing argument expression is replaced".to_owned());
    }
    MigrationFlagsData {
        behavior_change: true,
        behavior_change_reasons: reasons,
        evaluation_order_risk,
        move_borrow_risk,
        semantic_equivalence_claim: "notClaimed".to_owned(),
    }
}

/// Conservative move/borrow hint for a host-supplied expression.
fn argument_risk(argument: &str) -> bool {
    let trimmed = argument.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.parse::<f64>().is_ok() || matches!(trimmed, "true" | "false") {
        return false;
    }
    if trimmed.starts_with('"')
        || trimmed.starts_with("b\"")
        || trimmed.starts_with("r\"")
        || trimmed.starts_with("r#")
    {
        return false;
    }
    if let Some(rest) = trimmed.strip_prefix('&') {
        return rest.trim_start().starts_with("mut ");
    }
    true
}

fn resolve_signature(
    content: &str,
    parsed: &super::rewrite::ParsedFile,
    site: &AnalyzedSite,
    symbol: &str,
) -> Option<(usize, ParamList)> {
    let token = find_site_token(content, parsed, site, symbol)?;
    let list = signature_params(content, parsed, token)?;
    Some((token, list))
}

/// Locates the identifier the analysis reported. Exact offsets are preferred;
/// the reported range is used as a fallback so a range that covers a path
/// (`api::compute`) still resolves to the final segment.
fn find_site_token(
    content: &str,
    parsed: &super::rewrite::ParsedFile,
    site: &AnalyzedSite,
    symbol: &str,
) -> Option<usize> {
    let start = position_offset(
        content,
        &Position {
            line: site.start_line,
            character: site.start_character,
        },
    )?;
    let end = position_offset(
        content,
        &Position {
            line: site.end_line,
            character: site.end_character,
        },
    )
    .unwrap_or(start);
    if let Some(index) = token_at_offset(&parsed.tokens, start) {
        let token = &parsed.tokens[index];
        if token.is_ident() && content.get(token.start..token.end) == Some(symbol) {
            return Some(index);
        }
    }
    let range_end = end.max(start.saturating_add(1));
    parsed.tokens.iter().position(|token| {
        token.is_ident()
            && token.start >= start
            && token.start < range_end
            && content.get(token.start..token.end) == Some(symbol)
    })
}

fn signature_edit(
    parsed: &super::rewrite::ParsedFile,
    list: &ParamList,
    transformation: &crate::change::model::MigrateTransformationInput,
    position: usize,
) -> Option<super::rewrite::ListEdit> {
    match transformation.kind {
        MigrateTransformationKind::AddParameter => insert_into_params(
            parsed,
            &list.params,
            list.open,
            list.close,
            list.receiver,
            position,
            &transformation.parameter,
        ),
        MigrateTransformationKind::ChangeParameter => replace_in_params(
            &list.params,
            list.receiver,
            position,
            &transformation.parameter,
        ),
    }
}

/// True when the offset lies inside a `use` statement; the boolean is the
/// statement's `pub` visibility.
fn use_context(content: &str, parsed: &super::rewrite::ParsedFile, offset: usize) -> Option<bool> {
    for (index, token) in parsed.tokens.iter().enumerate() {
        if !token.is_ident() || content.get(token.start..token.end) != Some("use") {
            continue;
        }
        let mut cursor = index + 1;
        let mut end = token.end;
        while cursor < parsed.tokens.len() {
            let current = &parsed.tokens[cursor];
            if current.is_punct(b';') {
                end = current.end;
                break;
            }
            if current.is_open() {
                cursor = current.pair.unwrap_or(cursor);
            } else if matches!(current.kind, super::rewrite::TokenKind::Close(_)) {
                break;
            }
            end = current.end;
            cursor += 1;
        }
        if token.start <= offset && offset < end {
            return Some(use_is_public(content, &parsed.tokens, index));
        }
    }
    None
}

fn feature_scope(content: &str, line: u32) -> Option<String> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut index = usize::try_from(line).ok()?.min(lines.len());
    let mut steps = 0usize;
    while index > 0 && steps < 60 {
        index -= 1;
        steps += 1;
        let raw = lines.get(index).copied().unwrap_or_default();
        let trimmed = raw.trim_start();
        if trimmed.starts_with("#[cfg") {
            return Some(bounded_chars(trimmed, 120));
        }
        if raw.starts_with('}') {
            break;
        }
    }
    None
}

fn item_visibility(content: &str, parsed: &super::rewrite::ParsedFile, token: usize) -> String {
    let default = enclosing_owner(content, parsed, token)
        .map_or_else(|| "freeFunction".to_owned(), |(kind, _)| kind);
    let mut cursor = token;
    while let Some(previous) = super::rewrite::prev_significant(&parsed.tokens, cursor) {
        let current = &parsed.tokens[previous];
        if current.is_punct(b';') || matches!(current.kind, super::rewrite::TokenKind::Close(_)) {
            break;
        }
        if current.is_open() {
            break;
        }
        if current.is_ident() {
            let text = content.get(current.start..current.end).unwrap_or_default();
            if text == "pub" {
                return "public".to_owned();
            }
            if matches!(text, "fn" | "async" | "unsafe" | "const" | "extern") {
                cursor = previous;
                continue;
            }
            break;
        }
        cursor = previous;
    }
    default
}

/// Returns `(ownerKind, ownerToken)` for the item that lexically contains the
/// `fn` token.
fn enclosing_owner(
    content: &str,
    parsed: &super::rewrite::ParsedFile,
    token: usize,
) -> Option<(String, usize)> {
    let offset = parsed.tokens.get(token)?.start;
    let ancestors = super::rewrite::ancestors(&parsed.tokens, offset);
    let owner_open = *ancestors.last()?;
    let mut cursor = owner_open;
    for _ in 0..6 {
        let previous = super::rewrite::prev_significant(&parsed.tokens, cursor)?;
        let current = &parsed.tokens[previous];
        if current.is_ident() {
            match content.get(current.start..current.end) {
                Some("trait") => return Some(("traitItem".to_owned(), previous)),
                Some("impl") => return Some(("implItem".to_owned(), previous)),
                _ => {}
            }
        }
        cursor = previous;
    }
    None
}

fn owner_public(content: &str, parsed: &super::rewrite::ParsedFile, token: usize) -> bool {
    let Some((_, owner)) = enclosing_owner(content, parsed, token) else {
        return false;
    };
    let mut cursor = owner;
    while let Some(previous) = super::rewrite::prev_significant(&parsed.tokens, cursor) {
        let current = &parsed.tokens[previous];
        if current.is_ident() {
            match content.get(current.start..current.end) {
                Some("pub") => return true,
                Some("unsafe" | "default") => {
                    cursor = previous;
                    continue;
                }
                _ => return false,
            }
        }
        break;
    }
    false
}

fn package_of(file: &str, read_file: &dyn Fn(&str) -> Option<String>) -> String {
    let mut parts = file.split('/').collect::<Vec<_>>();
    parts.pop();
    while !parts.is_empty() {
        let manifest = format!("{}/Cargo.toml", parts.join("/"));
        if let Some(text) = read_file(&manifest)
            && let Some(name) = package_name(&text)
        {
            return name;
        }
        parts.pop();
    }
    read_file("Cargo.toml")
        .and_then(|text| package_name(&text))
        .unwrap_or_else(|| "(unknown)".to_owned())
}

fn package_name(manifest: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(manifest).ok()?;
    value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

fn site(
    file: &str,
    line: u32,
    column: u32,
    kind: &str,
    detail: Option<String>,
    feature_scope: Option<String>,
) -> MigrationSiteData {
    MigrationSiteData {
        file: file.to_owned(),
        line: u64::from(line).saturating_add(1),
        column: u64::from(column).saturating_add(1),
        kind: kind.to_owned(),
        detail,
        feature_scope,
    }
}

fn obligation(
    kind: &str,
    file: Option<&str>,
    line: Option<u64>,
    message: &str,
) -> MigrationObligationData {
    MigrationObligationData {
        kind: kind.to_owned(),
        file: file.map(str::to_owned),
        line,
        message: bounded_chars(message, MAX_MIGRATION_MESSAGE_CHARS),
    }
}

/// Converts byte-exact planned edits into candidate patches. Each patch's
/// `oldString` is a unique window of the *original* candidate content, and
/// windows that would overlap are merged into one patch, so sequential patch
/// application can never see a stale or overlapping context. Fails closed per
/// group with a typed obligation.
pub(crate) fn compose_patches(
    edits: &[PlannedEdit],
    read_file: &dyn Fn(&str) -> Option<String>,
) -> (
    Vec<crate::change::model::PatchInput>,
    Vec<MigrationObligationData>,
) {
    struct Cluster<'a> {
        start: usize,
        end: usize,
        edits: Vec<&'a PlannedEdit>,
    }

    let mut obligations = Vec::new();
    let mut by_file: BTreeMap<&str, Vec<&PlannedEdit>> = BTreeMap::new();
    for edit in edits.iter().take(MAX_MIGRATION_EDITS as usize) {
        by_file.entry(edit.file.as_str()).or_default().push(edit);
    }
    let mut patches = Vec::new();
    for (file, mut file_edits) in by_file {
        file_edits
            .sort_by(|left, right| left.start.cmp(&right.start).then(left.end.cmp(&right.end)));
        let Some(original) = read_file(file) else {
            obligations.push(obligation(
                "composeFailed",
                Some(file),
                None,
                "candidate file became unreadable during patch composition",
            ));
            continue;
        };
        let mut clusters: Vec<Cluster<'_>> = Vec::new();
        let mut previous_end: Option<usize> = None;
        let mut failed = false;
        for edit in file_edits {
            let valid = edit.start <= edit.end
                && edit.end <= original.len()
                && original.is_char_boundary(edit.start)
                && original.is_char_boundary(edit.end);
            if !valid || previous_end.is_some_and(|previous| edit.start < previous) {
                obligations.push(obligation(
                    "overlappingEdit",
                    Some(file),
                    Some(edit.line),
                    "planned edits are invalid or overlap in the candidate and were not applied",
                ));
                failed = true;
                continue;
            }
            previous_end = Some(edit.end);
            let Some(context) = unique_context(&original, edit.start, edit.end) else {
                obligations.push(obligation(
                    "nonUniqueContext",
                    Some(file),
                    Some(edit.line),
                    "a unique candidate oldString could not be derived for the edit",
                ));
                failed = true;
                continue;
            };
            clusters.push(Cluster {
                start: context.start,
                end: context.end,
                edits: vec![edit],
            });
        }
        if failed && clusters.is_empty() {
            continue;
        }
        // Merge clusters whose unique windows overlap; expanding a merged
        // window for uniqueness can overlap its neighbour, so iterate.
        for _ in 0..clusters.len().max(1).saturating_add(1) {
            clusters.sort_by_key(|cluster| cluster.start);
            let mut merged_any = false;
            let mut merged: Vec<Cluster<'_>> = Vec::new();
            for cluster in clusters.drain(..) {
                if let Some(last) = merged.last_mut()
                    && cluster.start <= last.end
                {
                    last.end = last.end.max(cluster.end);
                    last.edits.extend(cluster.edits);
                    merged_any = true;
                    continue;
                }
                merged.push(cluster);
            }
            clusters = merged;
            let mut expanded = false;
            for cluster in &mut clusters {
                if let Some(context) = unique_context(&original, cluster.start, cluster.end) {
                    if context.start < cluster.start || context.end > cluster.end {
                        expanded = true;
                    }
                    cluster.start = context.start;
                    cluster.end = context.end;
                }
            }
            let overlap = clusters.windows(2).any(|pair| pair[1].start < pair[0].end);
            if !overlap && !expanded {
                break;
            }
            if !merged_any && !overlap {
                break;
            }
        }
        for mut cluster in clusters {
            cluster
                .edits
                .sort_by(|left, right| left.start.cmp(&right.start));
            let mut updated = String::with_capacity(cluster.end.saturating_sub(cluster.start));
            let mut cursor = cluster.start;
            let mut valid = true;
            for edit in &cluster.edits {
                if edit.start < cursor || edit.end > cluster.end {
                    valid = false;
                    break;
                }
                updated.push_str(original.get(cursor..edit.start).unwrap_or_default());
                updated.push_str(&edit.text);
                cursor = edit.end;
            }
            if !valid {
                obligations.push(obligation(
                    "overlappingEdit",
                    Some(file),
                    Some(cluster.edits[0].line),
                    "merged candidate edit window was inconsistent and was not applied",
                ));
                continue;
            }
            updated.push_str(original.get(cursor..cluster.end).unwrap_or_default());
            let Some(old_string) = original.get(cluster.start..cluster.end) else {
                obligations.push(obligation(
                    "composeFailed",
                    Some(file),
                    Some(cluster.edits[0].line),
                    "candidate context does not cover the planned edit",
                ));
                continue;
            };
            patches.push(crate::change::model::PatchInput {
                file: file.to_owned(),
                old_string: old_string.to_owned(),
                new_string: updated,
            });
        }
    }
    (patches, obligations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::model::{
        MigrateAnchorInput, MigrateConstraintsInput, MigrateTransformationInput,
    };
    use std::collections::HashMap;

    fn request(
        kind: MigrateTransformationKind,
        parameter: &str,
        argument: &str,
        position: Option<u32>,
    ) -> MigrateRequest {
        MigrateRequest {
            anchor: MigrateAnchorInput {
                file: "api/src/lib.rs".to_owned(),
                symbol: "compute".to_owned(),
                line: None,
            },
            transformation: MigrateTransformationInput {
                kind,
                parameter: parameter.to_owned(),
                argument: argument.to_owned(),
                position,
            },
            consumer_scope: Some("workspace".to_owned()),
            constraints: MigrateConstraintsInput::default(),
        }
    }

    fn site_at(
        files: &HashMap<String, String>,
        file: &str,
        line: u32,
        needle: &str,
        identity: Option<bool>,
    ) -> AnalyzedSite {
        let content = files.get(file).expect("fixture file");
        let text = content.lines().nth(line as usize).expect("fixture line");
        let character = u32::try_from(text.find(needle).expect("needle column")).unwrap_or(0);
        AnalyzedSite {
            file: file.to_owned(),
            start_line: line,
            start_character: character,
            end_line: line,
            end_character: character.saturating_add(u32::try_from(needle.len()).unwrap_or(0)),
            identity,
        }
    }

    fn analysis(
        definition: AnalyzedSite,
        implementations: Vec<AnalyzedSite>,
        references: Vec<AnalyzedSite>,
    ) -> AnchorAnalysis {
        let references_total = u64::try_from(references.len()).unwrap_or(u64::MAX);
        let implementations_total = u64::try_from(implementations.len()).unwrap_or(u64::MAX);
        AnchorAnalysis {
            definition,
            implementations,
            references,
            references_total,
            implementations_total,
            identity_checks: 0,
            signature: None,
            notes: Vec::new(),
        }
    }

    fn plan_with(
        files: &HashMap<String, String>,
        request: &MigrateRequest,
        analysis: &AnchorAnalysis,
    ) -> MigrationPlan {
        let read = |file: &str| files.get(file).cloned();
        plan_migration(request, analysis, &read, PlanBudgets::from_request(request))
    }

    fn manifest(name: &str) -> String {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
    }

    #[test]
    fn adds_parameter_to_definition_and_consumer_calls() {
        let mut files = HashMap::new();
        files.insert("Cargo.toml".to_owned(), manifest("fixture"));
        files.insert("api/Cargo.toml".to_owned(), manifest("api"));
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32, base: u32) -> u32 { value + base }\n".to_owned(),
        );
        files.insert(
            "consumer/src/lib.rs".to_owned(),
            "use api::compute;\npub fn run() -> u32 { compute(1, 2) }\n".to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let reference = site_at(&files, "consumer/src/lib.rs", 1, "compute", Some(true));
        let analysis = analysis(definition, Vec::new(), vec![reference]);
        let request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "FACTOR",
            None,
        );
        let plan = plan_with(&files, &request, &analysis);
        assert!(plan.refused.is_none(), "{:?}", plan.refused);
        assert!(plan.report.complete, "{:#?}", plan.report.obligations);
        assert_eq!(plan.report.transformation.resolved_position, 2);
        assert_eq!(plan.report.impact.consumers.len(), 1);
        assert!(plan.report.flags.behavior_change);
        assert_eq!(plan.report.flags.semantic_equivalence_claim, "notClaimed");
        let patches = compose_patches(&plan.edits, &|file: &str| files.get(file).cloned());
        assert_eq!(patches.0.len(), 2, "{:#?}", patches.1);
        assert!(patches.1.is_empty());
    }

    #[test]
    fn unrelated_same_named_sites_are_excluded_by_identity() {
        let mut files = HashMap::new();
        files.insert("Cargo.toml".to_owned(), manifest("fixture"));
        files.insert("api/Cargo.toml".to_owned(), manifest("api"));
        files.insert("other/Cargo.toml".to_owned(), manifest("other"));
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32) -> u32 { value }\n".to_owned(),
        );
        files.insert(
            "other/src/lib.rs".to_owned(),
            "fn compute(value: u32) -> u32 { value }\npub fn run() -> u32 { compute(1) }\n"
                .to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let unrelated = site_at(&files, "other/src/lib.rs", 1, "compute", Some(false));
        let analysis = analysis(definition, Vec::new(), vec![unrelated]);
        let request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "2",
            None,
        );
        let plan = plan_with(&files, &request, &analysis);
        assert!(plan.refused.is_none(), "{:?}", plan.refused);
        assert_eq!(plan.report.impact.unrelated_total, 1);
        assert_eq!(plan.report.impact.unrelated[0].file, "other/src/lib.rs");
        assert!(
            plan.report
                .impact
                .consumers
                .iter()
                .all(|site| site.file != "other/src/lib.rs")
        );
        assert!(plan.report.complete);
        assert!(plan.report.impact.packages.contains(&"api".to_owned()));
    }

    #[test]
    fn macro_and_value_sites_become_typed_obligations() {
        let mut files = HashMap::new();
        files.insert("Cargo.toml".to_owned(), manifest("fixture"));
        files.insert("api/Cargo.toml".to_owned(), manifest("api"));
        files.insert("consumer/Cargo.toml".to_owned(), manifest("consumer"));
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32) -> u32 { value }\n".to_owned(),
        );
        files.insert(
            "consumer/src/lib.rs".to_owned(),
            "use api::compute;\npub fn run() -> u32 { assert_eq!(compute(1), 1); let f = compute; f(1) }\n"
                .to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let macro_site = site_at(&files, "consumer/src/lib.rs", 1, "compute", Some(true));
        let value_site = {
            let line = files["consumer/src/lib.rs"].lines().nth(1).expect("line");
            let column = u32::try_from(line.rfind("compute").expect("second compute")).unwrap_or(0);
            let mut site = site_at(&files, "consumer/src/lib.rs", 1, "compute", Some(true));
            site.start_character = column;
            site.end_character = column.saturating_add(7);
            site
        };
        let mut analysis = analysis(definition, Vec::new(), vec![macro_site, value_site]);
        analysis.references_total = 5;
        let request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "2",
            None,
        );
        let plan = plan_with(&files, &request, &analysis);
        let kinds = plan
            .report
            .obligations
            .iter()
            .map(|obligation| obligation.kind.as_str())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"macro"), "{kinds:?}");
        assert!(kinds.contains(&"callableValue"), "{kinds:?}");
        assert!(!plan.report.complete);
        assert_eq!(plan.report.budget.omitted_references, 3);
    }

    #[test]
    fn change_parameter_replaces_argument_and_signature() {
        let mut files = HashMap::new();
        files.insert("Cargo.toml".to_owned(), manifest("fixture"));
        files.insert("api/Cargo.toml".to_owned(), manifest("api"));
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32) -> u32 { value }\n".to_owned(),
        );
        files.insert(
            "consumer/src/lib.rs".to_owned(),
            "pub fn run() -> u32 { api::compute(1) }\n".to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let reference = site_at(&files, "consumer/src/lib.rs", 0, "compute", Some(true));
        let analysis = analysis(definition, Vec::new(), vec![reference]);
        let request = request(
            MigrateTransformationKind::ChangeParameter,
            "value: u64",
            "1u64",
            Some(0),
        );
        let plan = plan_with(&files, &request, &analysis);
        assert!(plan.refused.is_none(), "{:?}", plan.refused);
        assert!(plan.report.complete, "{:#?}", plan.report.obligations);
        assert!(plan.report.api_diff.after.contains("value: u64"));
    }

    #[test]
    fn budget_omitted_edits_are_visible_and_incomplete() {
        let mut files = HashMap::new();
        files.insert("Cargo.toml".to_owned(), manifest("fixture"));
        files.insert("api/Cargo.toml".to_owned(), manifest("api"));
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32) -> u32 { value }\n".to_owned(),
        );
        files.insert(
            "consumer/src/lib.rs".to_owned(),
            "pub fn run() -> u32 { compute(1) + compute(2) }\n".to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let first = site_at(&files, "consumer/src/lib.rs", 0, "compute(1)", Some(true));
        let second = {
            let mut site = site_at(&files, "consumer/src/lib.rs", 0, "compute(1)", Some(true));
            site.start_character = site.start_character.saturating_add(13);
            site.end_character = site.start_character.saturating_add(7);
            site
        };
        let analysis = analysis(definition, Vec::new(), vec![first, second]);
        let mut request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "2",
            None,
        );
        request.constraints.max_edits = Some(2);
        let plan = plan_with(&files, &request, &analysis);
        assert_eq!(
            plan.report.budget.omitted_edits, 1,
            "{:#?}",
            plan.report.budget
        );
        assert!(plan.report.budget.truncated);
        assert!(!plan.report.complete);
        assert!(
            plan.report
                .obligations
                .iter()
                .any(|obligation| obligation.kind == "budget")
        );
    }

    #[test]
    fn trait_impls_get_the_same_positional_rewrite() {
        let mut files = HashMap::new();
        files.insert("Cargo.toml".to_owned(), manifest("fixture"));
        files.insert("api/Cargo.toml".to_owned(), manifest("api"));
        files.insert("imp/Cargo.toml".to_owned(), manifest("imp"));
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub trait Scale {\n    fn scale(&self, base: u32) -> u32;\n}\n".to_owned(),
        );
        files.insert(
            "imp/src/lib.rs".to_owned(),
            "pub struct Gauge(pub u32);\nimpl api::Scale for Gauge {\n    fn scale(&self, base: u32) -> u32 { self.0 * base }\n}\n"
                .to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 1, "scale", Some(true));
        let implementation = site_at(&files, "imp/src/lib.rs", 2, "scale", Some(true));
        let analysis = analysis(definition, vec![implementation], Vec::new());
        let mut request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "2",
            None,
        );
        request.anchor.symbol = "scale".to_owned();
        let plan = plan_with(&files, &request, &analysis);
        assert!(plan.refused.is_none(), "{:?}", plan.refused);
        assert!(plan.report.complete, "{:#?}", plan.report.obligations);
        assert_eq!(plan.report.impact.implementations_total, 1);
        assert!(plan.edits.iter().any(|edit| edit.group == "implementation"));
        assert_eq!(plan.report.api_diff.visibility, "traitItem");
        assert!(plan.report.api_diff.public_api);
    }

    #[test]
    fn utf8_signature_offsets_are_not_corrupted() {
        let mut files = HashMap::new();
        files.insert(
            "api/src/lib.rs".to_owned(),
            "/// ünïcödé açıklama\npub fn render(prefix: &str, label: &str) {}\n".to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 1, "render", Some(true));
        let analysis = analysis(definition, Vec::new(), Vec::new());
        let mut request = request(
            MigrateTransformationKind::AddParameter,
            "suffix: &str",
            "\"x\"",
            Some(1),
        );
        request.anchor.symbol = "render".to_owned();
        let plan = plan_with(&files, &request, &analysis);
        assert!(plan.refused.is_none(), "{:?}", plan.refused);
        assert!(plan.report.complete);
        assert!(
            plan.report
                .api_diff
                .after
                .contains("prefix: &str, suffix: &str, label: &str"),
            "{}",
            plan.report.api_diff.after
        );
    }

    #[test]
    fn shadowed_local_bindings_with_the_same_name_are_excluded() {
        let mut files = HashMap::new();
        files.insert("api/Cargo.toml".to_owned(), manifest("api"));
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32) -> u32 { value + 1 }\n".to_owned(),
        );
        files.insert(
            "consumer/src/lib.rs".to_owned(),
            "pub fn run() -> u32 {\n    let compute = |value: u32| value;\n    compute(1)\n}\n"
                .to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let binding = site_at(&files, "consumer/src/lib.rs", 1, "compute", Some(false));
        let call = {
            let line = files["consumer/src/lib.rs"].lines().nth(2).expect("line");
            let column = u32::try_from(line.find("compute").expect("call")).unwrap_or(0);
            let mut site = site_at(&files, "consumer/src/lib.rs", 2, "compute", Some(false));
            site.start_character = column;
            site.end_character = column.saturating_add(7);
            site
        };
        let analysis = analysis(definition, Vec::new(), vec![binding, call]);
        let request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "2",
            None,
        );
        let plan = plan_with(&files, &request, &analysis);
        assert!(plan.refused.is_none(), "{:?}", plan.refused);
        assert_eq!(plan.report.impact.unrelated_total, 2);
        assert_eq!(plan.report.impact.consumers_total, 0);
        assert_eq!(plan.edits.len(), 1, "only the definition is edited");
        assert!(plan.report.complete);
        assert_eq!(plan.report.compatibility_scope, "capturedWorkspaceOnly");
    }

    #[test]
    fn evaluation_order_and_move_flags_are_reported() {
        let mut files = HashMap::new();
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32, base: u32) -> u32 { value + base }\n".to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let analysis = analysis(definition, Vec::new(), Vec::new());
        let request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "value",
            Some(1),
        );
        let plan = plan_with(&files, &request, &analysis);
        assert!(plan.report.flags.evaluation_order_risk);
        assert!(plan.report.flags.move_borrow_risk);
    }

    #[test]
    fn position_beyond_declared_parameters_is_refused() {
        let mut files = HashMap::new();
        files.insert(
            "api/src/lib.rs".to_owned(),
            "pub fn compute(value: u32) -> u32 { value }\n".to_owned(),
        );
        let definition = site_at(&files, "api/src/lib.rs", 0, "compute", Some(true));
        let analysis = analysis(definition, Vec::new(), Vec::new());
        let request = request(
            MigrateTransformationKind::AddParameter,
            "factor: u32",
            "2",
            Some(4),
        );
        let plan = plan_with(&files, &request, &analysis);
        assert_eq!(
            plan.refused.map(|(status, _)| status),
            Some("MIGRATION_REFUSED")
        );
    }
}
