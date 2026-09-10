//! Bounded compiler-verified minimization of a failing change candidate.
//!
//! The engine first reproduces the exact failure predicate (error code plus
//! normalized message structure, never the code alone) on the revision-bound
//! candidate snapshot, then tries permitted file/item/module reductions. A
//! reduction is only accepted when a real Cargo run still produces the target
//! diagnostic and introduces no new error signature. The result is a portable
//! proof package whose minimized source is re-verified in a clean temporary
//! directory before it can be called `REPRODUCED`.
//!
//! No global minimality is claimed: the search is bounded by compile,
//! candidate, wall-clock, and disk budgets. Exhausting a budget publishes
//! `BEST_KNOWN_REPRODUCER` plus the unsearched scope.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    change::{ChangeData, ChangeDiagnosticData, ChangeEvidenceData, ChangeService},
    config::{Config, GateCache},
    gate::{GateDetail, GateDiagnostic, GateEvidence, GateRequest, GateStatus, GateTargetId},
    tools::CheckService,
    workspace::RootGuard,
};

use super::{
    analysis,
    model::{
        MAX_PROOF_FILE_BYTES, MAX_PROOF_FILES, MAX_PROOF_TOTAL_BYTES, MAX_REDUCTIONS, RepairBudget,
        RepairBudgetData, RepairData, RepairFailurePredicateInput, RepairPredicateData,
        RepairProofData, RepairProofFileData, RepairReductionData, RepairReductionScope,
    },
};

static MINIMIZE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const MAX_PARSE_DEPTH: usize = 3;
const MAX_ITEMS_PER_FILE: usize = super::model::MAX_MINIMIZE_ITEMS_PER_FILE;
const MAX_INCLUDE_REFS_PER_FILE: usize = 64;
const MAX_TOUCHED_FILES_PER_REDUCTION: usize = 8;
const MAX_SIGNATURES_PER_RUN: usize = 512;
const MAX_REDUCTION_MESSAGE: usize = 320;

pub(crate) struct MinimizeRequest {
    pub change_id: String,
    pub diagnostic_ids: Vec<String>,
    pub failure_predicate: Option<RepairFailurePredicateInput>,
    pub reduction_scope: RepairReductionScope,
    pub budget: RepairBudget,
    pub base: ChangeData,
    pub evidence: ChangeEvidenceData,
    pub target: GateTargetId,
}

pub(crate) struct MinimizeOutcome {
    pub status: &'static str,
    pub summary: String,
    pub is_error: bool,
    pub data: RepairData,
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn minimize(
    change: &ChangeService,
    config: &Config,
    request: MinimizeRequest,
    cancellation: CancellationToken,
) -> MinimizeOutcome {
    if cancellation.is_cancelled() {
        return cancelled_outcome(&request.change_id);
    }
    let data = match change.read_candidate_tree(&request.change_id) {
        Ok(tree) => tree,
        Err(reason) => {
            return no_evidence_outcome(&request, reason);
        }
    };
    let (filtered, unmatched) =
        analysis::filter_diagnostics(&request.evidence.diagnostics, &request.diagnostic_ids);
    if filtered.is_empty() {
        let reason = if unmatched.is_empty() {
            "the fresh FAIL evidence carries no compiler diagnostics to minimize".to_owned()
        } else {
            format!("diagnosticIds did not match: {}", unmatched.join(", "))
        };
        return no_evidence_outcome(&request, reason);
    }
    let predicate = match FailurePredicate::build(&filtered, request.failure_predicate.as_ref()) {
        Ok(predicate) => predicate,
        Err(reason) => return no_evidence_outcome(&request, reason),
    };
    let target = TargetSite::locate(&data.files, &predicate);

    let scratch = change.scratch_root().to_path_buf();
    let mut session = match MinimizeSession::new(
        config.clone(),
        scratch,
        request.budget,
        request.target,
        cancellation.clone(),
    ) {
        Ok(session) => session,
        Err(reason) => {
            return MinimizeOutcome {
                status: "INCONCLUSIVE",
                summary: "The minimization scratch could not be prepared.".to_owned(),
                is_error: true,
                data: base_data(&request, "minimize", false, "scratchFailure", reason),
            };
        }
    };
    if data.truncated {
        session.risks.push(
            "The candidate snapshot omitted files at a capture bound; the package states this portability limit."
                .to_owned(),
        );
    }

    // 1. Reproduce the exact failure predicate on the unchanged candidate.
    let initial_run = match session
        .run_tree(&data.files, session.trial_root.clone())
        .await
    {
        Ok(run) => run,
        Err(reason) => return session.finish_unavailable(&request, reason),
    };
    let initial_signatures = initial_run.signatures();
    let initial = evaluate_run(&predicate, &initial_signatures, &initial_run);
    if !initial.matched {
        let stop = if initial.kind == MatchKind::Inconclusive {
            "initialInconclusive"
        } else {
            "initialMismatch"
        };
        return session.finish_not_reproduced(
            &request,
            stop,
            format!(
                "The unchanged candidate did not reproduce the target failure predicate ({}). No minimization was attempted.",
                initial.reason
            ),
            &initial_run,
            &predicate,
        );
    }
    let baseline = initial_run.signatures();

    // 2. A second fresh run must confirm stability; a flaky failure stops visibly.
    let mut confirmed = false;
    if session.can_compile() {
        match session
            .run_tree(&data.files, session.trial_root.clone())
            .await
        {
            Ok(confirm_run) => {
                let confirm = evaluate_run(&predicate, &baseline, &confirm_run);
                if !confirm.matched {
                    return session.finish_not_reproduced(
                        &request,
                        "unreproducible",
                        format!(
                            "The failure did not reproduce on a second fresh run of the unchanged candidate ({}); it is treated as flaky and no reduction was attempted.",
                            confirm.reason
                        ),
                        &confirm_run,
                        &predicate,
                    );
                }
                confirmed = true;
            }
            Err(reason) => {
                return session.finish_unavailable(
                    &request,
                    format!("the confirmation run could not start: {reason}"),
                );
            }
        }
    } else {
        session.risks.push(
            "The confirmation run was skipped because the compile budget was exhausted.".to_owned(),
        );
    }

    // 3. Bounded reduction search over permitted file/item/module reductions.
    let mut current = data.files.clone();
    let mut skipped_referenced = 0usize;
    let mut skipped_scope = 0usize;
    let mut stop_reason = "completed";
    let mut progressed = true;
    while progressed {
        if session.cancellation.is_cancelled() {
            return session.finish_cancelled(&request, &predicate, &current);
        }
        if let Some(stop) = session.budget_stop() {
            stop_reason = stop;
            break;
        }
        let plan = plan_units(
            &current,
            &target,
            request.reduction_scope,
            &mut skipped_referenced,
            &mut skipped_scope,
        );
        if plan.is_empty() {
            break;
        }
        progressed = false;
        let mut stack: Vec<Vec<Unit>> = vec![plan];
        while let Some(group) = stack.pop() {
            if session.cancellation.is_cancelled() {
                return session.finish_cancelled(&request, &predicate, &current);
            }
            if let Some(stop) = session.budget_stop() {
                stop_reason = stop;
                break;
            }
            let Some(trial) = apply_units(&current, &group) else {
                continue;
            };
            let run = match session.run_tree(&trial, session.trial_root.clone()).await {
                Ok(run) => run,
                Err(reason) => return session.finish_unavailable(&request, reason),
            };
            let evaluation = evaluate_run(&predicate, &baseline, &run);
            session.record_reduction(&group, &run, &evaluation, &current);
            if evaluation.matched {
                current = trial;
                progressed = true;
                break;
            }
            if group.len() > 1 {
                let middle = group.len() / 2;
                let (left, right) = group.split_at(middle);
                stack.push(right.to_vec());
                stack.push(left.to_vec());
            }
        }
    }
    if cancellation.is_cancelled() {
        return session.finish_cancelled(&request, &predicate, &current);
    }
    if skipped_referenced > 0 {
        session.risks.push(format!(
            "{skipped_referenced} item(s) were not offered as reductions because their names are still referenced elsewhere in the candidate."
        ));
    }
    if skipped_scope > 0 {
        session.risks.push(format!(
            "{skipped_scope} unit(s) could not be expressed under the requested reductionScope."
        ));
    }

    // 4. Export the minimized source to a clean directory and verify the exact
    //    same predicate there before claiming a reproducer.
    let mut export_verified = false;
    let export_verification;
    let mut status = "REPRODUCED";
    if let Some(stop) = session.budget_stop() {
        if stop_reason == "completed" {
            stop_reason = stop;
        }
        status = "BEST_KNOWN_REPRODUCER";
        export_verification = format!(
            "not run: the {stop} budget stop was reached before the clean-directory verification"
        );
        session.risks.push(
            "The clean-directory export verification did not run because a budget stop was reached; the minimized source is verified only in the trial directory."
                .to_owned(),
        );
    } else {
        match session
            .run_tree(&current, session.export_root.clone())
            .await
        {
            Ok(export_run) => {
                let export = evaluate_run(&predicate, &baseline, &export_run);
                if export.matched {
                    export_verified = true;
                    export_verification = format!(
                        "fresh {} in a clean directory reproduced the target predicate with no new error signatures",
                        export_run.status.as_str()
                    );
                } else {
                    status = "NOT_REPRODUCED";
                    export_verification = format!(
                        "the clean-directory verification did not reproduce the target predicate ({})",
                        export.reason
                    );
                    if stop_reason == "completed" {
                        stop_reason = "exportMismatch";
                    }
                }
            }
            Err(reason) => {
                status = "NOT_REPRODUCED";
                export_verification =
                    format!("the clean-directory verification could not start: {reason}");
                if stop_reason == "completed" {
                    stop_reason = "exportInconclusive";
                }
            }
        }
    }
    if !export_verified && status == "REPRODUCED" {
        status = "BEST_KNOWN_REPRODUCER";
    }
    if !confirmed {
        session.risks.push(
            "The failure was reproduced once but not confirmed by a second fresh unchanged run."
                .to_owned(),
        );
    }
    if !data.files.contains_key("Cargo.lock") {
        session.risks.push(
            "The candidate carries no Cargo.lock; the export preserves whatever lockfile state exists in the candidate copy."
                .to_owned(),
        );
    }
    if !current.values().any(|_| true) {
        session
            .risks
            .push("The minimized source is empty.".to_owned());
    }
    let proof = build_proof(
        &current,
        &session,
        &predicate,
        export_verified,
        &export_verification,
    );
    session.finish_reproducer(status, &request, &predicate, current, proof, stop_reason)
}

// ---------------------------------------------------------------------------
// Failure predicate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FailurePredicate {
    diagnostic_id: String,
    code: String,
    level: String,
    file: Option<String>,
    line: Option<u64>,
    normalized_message: String,
    message_fragments: Vec<String>,
    message_tokens: Vec<String>,
    source: String,
}

impl FailurePredicate {
    fn build(
        diagnostics: &[ChangeDiagnosticData],
        input: Option<&RepairFailurePredicateInput>,
    ) -> Result<Self, String> {
        let code_filter = input
            .and_then(|input| input.code.as_deref())
            .filter(|code| !code.trim().is_empty());
        let file_filter = input
            .and_then(|input| input.file.as_deref())
            .filter(|file| !file.trim().is_empty());
        let selected = if let Some(code) = code_filter {
            diagnostics
                .iter()
                .find(|diagnostic| {
                    diagnostic.level == "error"
                        && diagnostic.code.as_deref() == Some(code)
                        && file_filter.is_none_or(|file| diagnostic.file.as_deref() == Some(file))
                })
                .ok_or_else(|| {
                    "the failurePredicate code does not match any fresh error diagnostic".to_owned()
                })?
        } else {
            diagnostics
                .iter()
                .find(|diagnostic| {
                    diagnostic.level == "error"
                        && file_filter.is_none_or(|file| diagnostic.file.as_deref() == Some(file))
                        && diagnostic
                            .code
                            .as_deref()
                            .is_some_and(|code| !code.trim().is_empty())
                })
                .or_else(|| {
                    diagnostics.iter().find(|diagnostic| {
                        diagnostic.level == "error"
                            && file_filter
                                .is_none_or(|file| diagnostic.file.as_deref() == Some(file))
                    })
                })
                .ok_or_else(|| {
                    "the failurePredicate does not match any fresh error diagnostic".to_owned()
                })?
        };
        let code = selected
            .code
            .clone()
            .filter(|code| !code.trim().is_empty())
            .ok_or_else(|| {
                "the selected diagnostic has no error code; an explicit failurePredicate.code is required"
                    .to_owned()
            })?;
        let fragments: Vec<String> = input
            .map(|input| {
                input
                    .message_contains
                    .iter()
                    .map(|fragment| normalize_message(fragment))
                    .filter(|fragment| !fragment.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let normalized_message = if fragments.is_empty() {
            normalize_message(&selected.message)
        } else {
            String::new()
        };
        if normalized_message.is_empty() && fragments.is_empty() {
            return Err(
                "the selected diagnostic carries no message structure to anchor a predicate"
                    .to_owned(),
            );
        }
        let source = match input {
            Some(input)
                if input.code.is_some()
                    || input.file.is_some()
                    || !input.message_contains.is_empty() =>
            {
                "input+evidence"
            }
            _ => "evidence",
        };
        Ok(Self {
            diagnostic_id: analysis::diagnostic_id(selected),
            code,
            level: "error".to_owned(),
            file: input
                .and_then(|input| input.file.clone())
                .or_else(|| selected.file.clone()),
            line: selected.line,
            normalized_message,
            message_fragments: fragments,
            message_tokens: backticked_tokens(&selected.message),
            source: source.to_owned(),
        })
    }

    fn matches(&self, diagnostic: &ObservedDiagnostic) -> bool {
        if diagnostic.level != self.level {
            return false;
        }
        if diagnostic.code.as_deref() != Some(self.code.as_str()) {
            return false;
        }
        if let Some(file) = &self.file {
            if diagnostic.file.as_deref() != Some(file.as_str()) {
                return false;
            }
        }
        let normalized = normalize_message(&diagnostic.message);
        if !self.normalized_message.is_empty() && normalized != self.normalized_message {
            return false;
        }
        self.message_fragments
            .iter()
            .all(|fragment| normalized.contains(fragment))
    }

    fn code_matches(&self, diagnostic: &ObservedDiagnostic) -> bool {
        diagnostic.level == self.level
            && diagnostic.code.as_deref() == Some(self.code.as_str())
            && self
                .file
                .as_ref()
                .is_none_or(|file| diagnostic.file.as_deref() == Some(file.as_str()))
    }

    fn data(&self) -> RepairPredicateData {
        RepairPredicateData {
            diagnostic_id: self.diagnostic_id.clone(),
            code: self.code.clone(),
            level: self.level.clone(),
            file: self.file.clone(),
            message: if self.normalized_message.is_empty() {
                self.message_fragments.join(" & ")
            } else {
                self.normalized_message.clone()
            },
            message_tokens: self.message_tokens.clone(),
            source: self.source.clone(),
        }
    }
}

fn normalize_message(message: &str) -> String {
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn backticked_tokens(message: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = message;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        let token = after[..end].trim();
        if !token.is_empty() && !tokens.iter().any(|existing| existing == token) {
            tokens.push(token.to_owned());
            if tokens.len() >= 16 {
                break;
            }
        }
        rest = &after[end + 1..];
    }
    tokens
}

// ---------------------------------------------------------------------------
// Observed run classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ObservedDiagnostic {
    code: Option<String>,
    level: String,
    file: Option<String>,
    message: String,
}

#[derive(Debug, Clone)]
struct TrialRun {
    status: GateStatus,
    total_ms: u64,
    diagnostics: Vec<ObservedDiagnostic>,
    diagnostics_omitted: u64,
    command: String,
    message: String,
}

impl TrialRun {
    fn signatures(&self) -> BTreeSet<String> {
        let mut signatures = BTreeSet::new();
        for diagnostic in &self.diagnostics {
            if diagnostic.level != "error" {
                continue;
            }
            signatures.insert(signature_key(diagnostic));
            if signatures.len() >= MAX_SIGNATURES_PER_RUN {
                break;
            }
        }
        signatures
    }

    fn error_count(&self) -> u64 {
        u64::try_from(
            self.diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.level == "error")
                .count(),
        )
        .unwrap_or(u64::MAX)
    }
}

fn signature_key(diagnostic: &ObservedDiagnostic) -> String {
    format!(
        "{}|{}",
        diagnostic.code.as_deref().unwrap_or("uncoded"),
        normalize_message(&diagnostic.message)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    Matched,
    NewSignatures,
    MessageMismatch,
    CodeAbsent,
    Inconclusive,
}

impl MatchKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::NewSignatures => "newSignatures",
            Self::MessageMismatch => "messageMismatch",
            Self::CodeAbsent => "codeAbsent",
            Self::Inconclusive => "inconclusive",
        }
    }
}

#[derive(Debug, Clone)]
struct Evaluation {
    kind: MatchKind,
    matched: bool,
    reason: String,
}

// ---------------------------------------------------------------------------
// Session and Cargo runner
// ---------------------------------------------------------------------------

struct MinimizeSession {
    config: Config,
    budget: RepairBudget,
    target: GateTargetId,
    started: Instant,
    phase_root: PathBuf,
    trial_root: PathBuf,
    export_root: PathBuf,
    cache_root: PathBuf,
    cancellation: CancellationToken,
    compiles: u32,
    attempts: u32,
    last_command: String,
    reductions: Vec<RepairReductionData>,
    reductions_total: u64,
    risks: Vec<String>,
}

impl Drop for MinimizeSession {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.phase_root);
    }
}

impl MinimizeSession {
    fn new(
        config: Config,
        scratch: PathBuf,
        budget: RepairBudget,
        target: GateTargetId,
        cancellation: CancellationToken,
    ) -> Result<Self, String> {
        if fs::symlink_metadata(&scratch)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("the minimize scratch root is a symlink".to_owned());
        }
        let root = scratch.join("minimize");
        if fs::symlink_metadata(&root)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("the minimize scratch directory is a symlink".to_owned());
        }
        let sequence = MINIMIZE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let phase_root = root.join(format!(
            "run-{}-{}-{sequence}",
            std::process::id(),
            now_millis()
        ));
        if fs::symlink_metadata(&phase_root).is_ok() {
            return Err("the minimize scratch entry already exists".to_owned());
        }
        let trial_root = phase_root.join("trial");
        let export_root = phase_root.join("export");
        let cache_root = phase_root.join("cache");
        fs::create_dir_all(&trial_root)
            .map_err(|error| format!("minimize scratch could not be created: {error}"))?;
        fs::create_dir_all(&export_root)
            .map_err(|error| format!("minimize scratch could not be created: {error}"))?;
        fs::create_dir_all(&cache_root)
            .map_err(|error| format!("minimize cache could not be created: {error}"))?;
        Ok(Self {
            config,
            budget,
            target,
            started: Instant::now(),
            phase_root,
            trial_root,
            export_root,
            cache_root,
            cancellation,
            compiles: 0,
            attempts: 0,
            last_command: String::new(),
            reductions: Vec::new(),
            reductions_total: 0,
            risks: Vec::new(),
        })
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn can_compile(&self) -> bool {
        self.compiles < self.budget.max_compiles
    }

    fn budget_stop(&self) -> Option<&'static str> {
        if self.compiles >= self.budget.max_compiles {
            Some("maxCompiles")
        } else if self.attempts >= self.budget.max_candidates {
            Some("maxCandidates")
        } else if self.elapsed_ms() > self.budget.wall_time_ms {
            Some("wallTimeMs")
        } else {
            None
        }
    }

    async fn run_tree(
        &mut self,
        tree: &BTreeMap<String, String>,
        root: PathBuf,
    ) -> Result<TrialRun, String> {
        materialize(&root, tree)?;
        let guard = RootGuard::new([root.clone()], std::iter::empty())
            .map_err(|error| format!("trial root guard failed: {error}"))?;
        let mut config = self.config.clone();
        config.gate.cache = GateCache::Isolated;
        config.gate.cache_dir = self.cache_root.clone();
        let service = CheckService::new(config, Arc::new(guard));
        let request = GateRequest::new(root.clone(), self.target).with_detail(GateDetail::Compact);
        self.compiles = self.compiles.saturating_add(1);
        let evidence = service
            .run(request, None, Some(self.cancellation.clone()))
            .await;
        service.close().await;
        let run = convert_evidence(&evidence, &root);
        if !run.command.is_empty() {
            self.last_command.clone_from(&run.command);
        }
        Ok(run)
    }

    fn record_reduction(
        &mut self,
        group: &[Unit],
        run: &TrialRun,
        evaluation: &Evaluation,
        before: &BTreeMap<String, String>,
    ) {
        self.reductions_total = self.reductions_total.saturating_add(1);
        self.attempts = self.attempts.saturating_add(1);
        if self.reductions.len() >= MAX_REDUCTIONS {
            return;
        }
        let kind = group_kind(group);
        let mut files = BTreeSet::new();
        let mut lines = 0u64;
        let mut items = 0u64;
        for unit in group {
            files.extend(unit.files());
            lines = lines.saturating_add(unit.lines(before));
            items = items.saturating_add(unit.item_count());
        }
        let status = if evaluation.matched {
            "accepted"
        } else if evaluation.kind == MatchKind::Inconclusive {
            "inconclusive"
        } else {
            "rejected"
        };
        let reason = if evaluation.matched {
            format!(
                "selected: {} still reproduces the target predicate and added no new error signatures",
                describe_group(group, before)
            )
        } else {
            format!("not selected: {}", evaluation.reason)
        };
        self.reductions.push(RepairReductionData {
            id: format!("r-{}", self.reductions_total),
            kind,
            files: files
                .into_iter()
                .take(MAX_TOUCHED_FILES_PER_REDUCTION)
                .collect(),
            items,
            lines_removed: lines,
            status: status.to_owned(),
            failure_match: evaluation.kind.as_str().to_owned(),
            reason: bounded(&reason, MAX_REDUCTION_MESSAGE),
            total_ms: run.total_ms,
        });
    }

    fn finish_reproducer(
        mut self,
        status: &'static str,
        request: &MinimizeRequest,
        predicate: &FailurePredicate,
        current: BTreeMap<String, String>,
        proof: RepairProofData,
        stop_reason: &'static str,
    ) -> MinimizeOutcome {
        let usable = status == "REPRODUCED" || status == "BEST_KNOWN_REPRODUCER";
        let reproduced = status == "REPRODUCED";
        if stop_reason != "completed" {
            self.risks.push(format!(
                "The reduction search stopped at the {stop_reason} budget; the reported reproducer is only small relative to the tried transformation set."
            ));
        }
        if !reproduced && usable {
            self.risks.push(
                "A mathematically minimal reproducer is not claimed; remaining unsearched scope stays visible."
                    .to_owned(),
            );
        }
        let reduction_files = current.len();
        let summary = format!(
            "{}: minimized to {} file(s) ({} byte(s)); {} reduction(s) were compile-evaluated.",
            status, reduction_files, proof.total_bytes, self.reductions_total
        );
        let omitted = u64::try_from(self.reductions.len()).unwrap_or(u64::MAX);
        let total = self.reductions_total;
        MinimizeOutcome {
            status,
            summary,
            is_error: !usable,
            data: RepairData {
                action: "minimize".to_owned(),
                usable,
                change_id: Some(request.change_id.clone()),
                base_identity: request.base.base_identity.clone(),
                revision: Some(request.base.revision),
                evidence_revision: Some(request.evidence.revision),
                predicate: Some(predicate.data()),
                proof: Some(proof),
                reductions: std::mem::take(&mut self.reductions),
                reductions_total: total,
                reductions_omitted: total.saturating_sub(omitted),
                reproduced,
                verification: if reproduced {
                    "exportVerified".to_owned()
                } else if usable {
                    "trialVerified".to_owned()
                } else {
                    "none".to_owned()
                },
                configuration: Some(super::model::RepairConfigurationData::from_base(
                    request.target,
                    None,
                    &request.base,
                    &request.evidence,
                )),
                budget: Some(RepairBudgetData {
                    max_candidates: self.budget.max_candidates,
                    max_compiles: self.budget.max_compiles,
                    wall_time_ms: self.budget.wall_time_ms,
                    candidates_used: self.attempts,
                    compiles_used: self.compiles,
                    elapsed_ms: self.elapsed_ms(),
                }),
                stop_reason: stop_reason.to_owned(),
                remaining_risks: std::mem::take(&mut self.risks),
                reason: if reproduced {
                    "the minimized source reproduced the target predicate in a clean directory"
                        .to_owned()
                } else {
                    "a budget or portability limit stopped the bounded search before a clean-directory REPRODUCED result"
                        .to_owned()
                },
                ..RepairData::default()
            },
        }
    }

    fn finish_not_reproduced(
        mut self,
        request: &MinimizeRequest,
        stop_reason: &'static str,
        reason: String,
        run: &TrialRun,
        predicate: &FailurePredicate,
    ) -> MinimizeOutcome {
        self.risks.push(format!(
            "The observed run reported {}.",
            run.status.as_str()
        ));
        if run.diagnostics.is_empty() {
            self.risks
                .push("The run carried no bounded compiler diagnostic.".to_owned());
        }
        MinimizeOutcome {
            status: "NOT_REPRODUCED",
            summary: "The candidate failure could not be reproduced as a stable, exact predicate."
                .to_owned(),
            is_error: false,
            data: RepairData {
                action: "minimize".to_owned(),
                usable: false,
                change_id: Some(request.change_id.clone()),
                base_identity: request.base.base_identity.clone(),
                revision: Some(request.base.revision),
                evidence_revision: Some(request.evidence.revision),
                predicate: Some(predicate.data()),
                reproduced: false,
                verification: "none".to_owned(),
                configuration: Some(super::model::RepairConfigurationData::from_base(
                    request.target,
                    None,
                    &request.base,
                    &request.evidence,
                )),
                budget: Some(RepairBudgetData {
                    max_candidates: self.budget.max_candidates,
                    max_compiles: self.budget.max_compiles,
                    wall_time_ms: self.budget.wall_time_ms,
                    candidates_used: self.attempts,
                    compiles_used: self.compiles,
                    elapsed_ms: self.elapsed_ms(),
                }),
                stop_reason: stop_reason.to_owned(),
                remaining_risks: std::mem::take(&mut self.risks),
                reason: bounded(&reason, 512),
                ..RepairData::default()
            },
        }
    }

    fn finish_unavailable(self, request: &MinimizeRequest, reason: String) -> MinimizeOutcome {
        let mut data = base_data(request, "minimize", false, "inconclusive", reason);
        data.reproduced = false;
        data.verification = "none".to_owned();
        MinimizeOutcome {
            status: "NOT_REPRODUCED",
            summary: "The minimization could not run to a verified reproducer.".to_owned(),
            is_error: false,
            data,
        }
    }

    fn finish_cancelled(
        mut self,
        request: &MinimizeRequest,
        predicate: &FailurePredicate,
        _current: &BTreeMap<String, String>,
    ) -> MinimizeOutcome {
        let mut data = base_data(
            request,
            "minimize",
            false,
            "cancelled",
            "the minimization was cancelled before a verified reproducer was exported",
        );
        data.predicate = Some(predicate.data());
        data.reductions = std::mem::take(&mut self.reductions);
        data.reductions_total = self.reductions_total;
        data.reductions_omitted = self
            .reductions_total
            .saturating_sub(u64::try_from(data.reductions.len()).unwrap_or(u64::MAX));
        data.budget = Some(RepairBudgetData {
            max_candidates: self.budget.max_candidates,
            max_compiles: self.budget.max_compiles,
            wall_time_ms: self.budget.wall_time_ms,
            candidates_used: self.attempts,
            compiles_used: self.compiles,
            elapsed_ms: self.elapsed_ms(),
        });
        data.remaining_risks = std::mem::take(&mut self.risks);
        MinimizeOutcome {
            status: "CANCELLED",
            summary: "No usable reproducer was published because the action was cancelled."
                .to_owned(),
            is_error: true,
            data,
        }
    }
}

fn base_data(
    request: &MinimizeRequest,
    action: &str,
    usable: bool,
    stop_reason: &str,
    reason: impl Into<String>,
) -> RepairData {
    RepairData {
        action: action.to_owned(),
        usable,
        change_id: Some(request.change_id.clone()),
        base_identity: request.base.base_identity.clone(),
        revision: Some(request.base.revision),
        evidence_revision: Some(request.evidence.revision),
        stop_reason: stop_reason.to_owned(),
        reason: bounded(&reason.into(), 512),
        ..RepairData::default()
    }
}

fn no_evidence_outcome(request: &MinimizeRequest, reason: String) -> MinimizeOutcome {
    MinimizeOutcome {
        status: "NO_EVIDENCE",
        summary: "No usable minimization evidence was published.".to_owned(),
        is_error: true,
        data: base_data(request, "minimize", false, "noUsableEvidence", reason),
    }
}

fn cancelled_outcome(change_id: &str) -> MinimizeOutcome {
    MinimizeOutcome {
        status: "CANCELLED",
        summary:
            "No usable reproducer was published because the action was cancelled before it started."
                .to_owned(),
        is_error: true,
        data: RepairData {
            action: "minimize".to_owned(),
            usable: false,
            change_id: Some(change_id.to_owned()),
            stop_reason: "cancelled".to_owned(),
            reason: "repair was cancelled before it started".to_owned(),
            ..RepairData::default()
        },
    }
}

fn evaluate_run(
    predicate: &FailurePredicate,
    baseline: &BTreeSet<String>,
    run: &TrialRun,
) -> Evaluation {
    if !matches!(
        run.status,
        GateStatus::Fail | GateStatus::FastPass | GateStatus::FullPass
    ) || run.diagnostics_omitted > 0
    {
        return Evaluation {
            kind: MatchKind::Inconclusive,
            matched: false,
            reason: inconclusive_reason(run),
        };
    }
    let matched = run
        .diagnostics
        .iter()
        .any(|diagnostic| predicate.matches(diagnostic));
    if !matched {
        let same_code = run
            .diagnostics
            .iter()
            .any(|diagnostic| predicate.code_matches(diagnostic));
        return Evaluation {
            kind: if same_code {
                MatchKind::MessageMismatch
            } else {
                MatchKind::CodeAbsent
            },
            matched: false,
            reason: if same_code {
                format!(
                    "{} appears but its message no longer matches the target predicate",
                    predicate.code
                )
            } else {
                format!("{} is no longer produced by the trial", predicate.code)
            },
        };
    }
    let signatures = run.signatures();
    let new_signatures: Vec<String> = signatures.difference(baseline).take(8).cloned().collect();
    if !new_signatures.is_empty() {
        return Evaluation {
            kind: MatchKind::NewSignatures,
            matched: false,
            reason: format!(
                "the trial introduced new error signature(s): {}",
                new_signatures.join("; ")
            ),
        };
    }
    Evaluation {
        kind: MatchKind::Matched,
        matched: true,
        reason: format!(
            "the target predicate matched with {} error diagnostic(s) and no new signatures",
            run.error_count()
        ),
    }
}

fn inconclusive_reason(run: &TrialRun) -> String {
    match run.status {
        GateStatus::Timeout => {
            "the Cargo run timed out; a timeout never counts as the original failure".to_owned()
        }
        GateStatus::Cancelled => "the Cargo run was cancelled".to_owned(),
        GateStatus::Unavailable | GateStatus::Inconclusive | GateStatus::ResourceBlocked => {
            format!(
                "the Cargo run was unavailable or inconclusive: {}",
                bounded(&run.message, 160)
            )
        }
        GateStatus::FastPass | GateStatus::FullPass => {
            "the trial compiled successfully, so the target failure is gone".to_owned()
        }
        _ => {
            if run.diagnostics_omitted > 0 {
                "the Cargo diagnostics were truncated; no unambiguous match is possible".to_owned()
            } else {
                format!(
                    "the Cargo run was not a clean terminal result: {}",
                    run.status.as_str()
                )
            }
        }
    }
}

fn convert_evidence(evidence: &GateEvidence, root: &Path) -> TrialRun {
    let workspace_root = evidence
        .workspace_root
        .clone()
        .unwrap_or_else(|| root.to_path_buf());
    let mut diagnostics = Vec::new();
    let mut diagnostics_omitted = 0u64;
    let mut total_ms = 0u64;
    let mut command = String::new();
    for step in &evidence.steps {
        total_ms = total_ms.saturating_add(step.duration_ms);
        diagnostics_omitted = diagnostics_omitted.saturating_add(step.diagnostics_omitted);
        if command.is_empty() {
            command.clone_from(&step.command);
        }
        for diagnostic in &step.diagnostics {
            diagnostics.push(observed(diagnostic, &workspace_root));
        }
    }
    TrialRun {
        status: evidence.status,
        total_ms,
        diagnostics,
        diagnostics_omitted,
        command,
        message: evidence.message.clone().unwrap_or_default(),
    }
}

fn observed(diagnostic: &GateDiagnostic, root: &Path) -> ObservedDiagnostic {
    ObservedDiagnostic {
        code: diagnostic.code.clone(),
        level: diagnostic.level.clone(),
        file: relative_file(diagnostic.file.as_deref(), root),
        message: bounded(&diagnostic.message, 2_048),
    }
}

fn is_rust_source(file: &str) -> bool {
    Path::new(file)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
}

fn relative_file(file: Option<&str>, root: &Path) -> Option<String> {
    let file = file?;
    let normalized = file.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(root) {
            return Some(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Some(normalized)
}

fn materialize(root: &Path, tree: &BTreeMap<String, String>) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(root) {
        if metadata.file_type().is_symlink() {
            return Err("the trial root was replaced by a symlink".to_owned());
        }
        if metadata.is_dir() {
            fs::remove_dir_all(root)
                .map_err(|error| format!("the trial root could not be cleared: {error}"))?;
        } else {
            fs::remove_file(root)
                .map_err(|error| format!("the trial root could not be cleared: {error}"))?;
        }
    }
    fs::create_dir_all(root)
        .map_err(|error| format!("the trial root could not be created: {error}"))?;
    for (relative, content) in tree {
        let path = safe_relative(root, relative)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("a trial directory could not be created: {error}"))?;
        }
        fs::write(&path, content)
            .map_err(|error| format!("a trial source file could not be written: {error}"))?;
    }
    Ok(())
}

fn safe_relative(root: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty() || relative.len() > 1_024 {
        return Err("a candidate path is empty or too long".to_owned());
    }
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err("a candidate path is absolute".to_owned());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err("a candidate path escapes the trial root".to_owned());
            }
        }
    }
    Ok(root.join(path))
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

// ---------------------------------------------------------------------------
// Reduction planning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Use,
    ModDecl,
    ModBlock,
    Fn,
    Struct,
    Enum,
    Impl,
    Trait,
    Type,
    Const,
    Static,
    Macro,
    Extern,
    Other,
}

#[derive(Debug, Clone)]
struct ParsedItem {
    kind: ItemKind,
    start: usize,
    end: usize,
    body: Option<(usize, usize)>,
    name: Option<String>,
    path_attr: Option<String>,
    test: bool,
    children: Vec<ParsedItem>,
}

impl ParsedItem {
    fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

#[derive(Debug, Clone)]
struct TargetSite {
    file: String,
    anchor: Option<String>,
}

impl TargetSite {
    fn locate(tree: &BTreeMap<String, String>, predicate: &FailurePredicate) -> Self {
        let file = predicate.file.clone().unwrap_or_default();
        let anchor = tree
            .get(&file)
            .and_then(|content| {
                let line = predicate.line?;
                let index = usize::try_from(line).ok()?.checked_sub(1)?;
                content.lines().nth(index)
            })
            .map(str::trim)
            .filter(|line| !line.is_empty() && line.len() <= 240)
            .map(str::to_owned);
        Self { file, anchor }
    }
}

#[derive(Debug, Clone)]
enum Unit {
    Span {
        file: String,
        start: usize,
        end: usize,
        kind: String,
        label: String,
    },
    ModuleDecl {
        file: String,
        start: usize,
        end: usize,
        module_file: Option<String>,
        label: String,
    },
    File {
        file: String,
        label: String,
    },
}

impl Unit {
    fn files(&self) -> Vec<String> {
        match self {
            Self::Span { file, .. } => vec![file.clone()],
            Self::ModuleDecl {
                file, module_file, ..
            } => {
                let mut files = vec![file.clone()];
                if let Some(module_file) = module_file {
                    files.push(module_file.clone());
                }
                files
            }
            Self::File { file, .. } => vec![file.clone()],
        }
    }

    fn lines(&self, tree: &BTreeMap<String, String>) -> u64 {
        match self {
            Self::Span {
                file, start, end, ..
            }
            | Self::ModuleDecl {
                file, start, end, ..
            } => tree.get(file).map_or(0, |content| {
                let range = content.get(*start..*end).unwrap_or_default();
                u64::try_from(range.lines().count()).unwrap_or(u64::MAX)
            }),
            Self::File { file, .. } => tree.get(file).map_or(0, |content| {
                u64::try_from(content.lines().count()).unwrap_or(u64::MAX)
            }),
        }
    }

    fn item_count(&self) -> u64 {
        match self {
            Self::File { .. } => 1,
            Self::Span { .. } | Self::ModuleDecl { .. } => 1,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Span { label, .. }
            | Self::ModuleDecl { label, .. }
            | Self::File { label, .. } => label.clone(),
        }
    }
}

const MAX_UNITS: usize = 512;

fn plan_units(
    tree: &BTreeMap<String, String>,
    target: &TargetSite,
    scope: RepairReductionScope,
    skipped_referenced: &mut usize,
    skipped_scope: &mut usize,
) -> Vec<Unit> {
    let mut units = Vec::new();
    if scope.includes_items() || scope.includes_modules() {
        let names = candidate_names(tree);
        let counts = count_names(tree, &names);
        for (file, content) in tree {
            if !is_rust_source(file) || units.len() >= MAX_UNITS {
                continue;
            }
            let anchor = if file == &target.file {
                target
                    .anchor
                    .as_deref()
                    .and_then(|anchor| content.find(anchor))
            } else {
                None
            };
            let items = parse_items(content);
            plan_items(
                file,
                &items,
                anchor,
                scope,
                tree,
                &counts,
                &mut units,
                skipped_referenced,
                skipped_scope,
            );
        }
    }
    if scope.includes_files() {
        plan_files(tree, target, &mut units);
    }
    units.sort_by(|left, right| {
        right
            .lines(tree)
            .cmp(&left.lines(tree))
            .then_with(|| left.label().cmp(&right.label()))
    });
    units.truncate(MAX_UNITS);
    units
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn plan_items(
    file: &str,
    items: &[ParsedItem],
    anchor: Option<usize>,
    scope: RepairReductionScope,
    tree: &BTreeMap<String, String>,
    counts: &BTreeMap<String, u32>,
    units: &mut Vec<Unit>,
    skipped_referenced: &mut usize,
    skipped_scope: &mut usize,
) {
    for item in items {
        if units.len() >= MAX_UNITS {
            return;
        }
        let is_target = anchor.is_some_and(|offset| item.contains(offset));
        if is_target {
            if item.kind == ItemKind::ModBlock
                && scope.includes_modules()
                && !item.children.is_empty()
            {
                plan_items(
                    file,
                    &item.children,
                    anchor,
                    scope,
                    tree,
                    counts,
                    units,
                    skipped_referenced,
                    skipped_scope,
                );
            }
            continue;
        }
        match item.kind {
            ItemKind::ModDecl => {
                if !scope.includes_modules() {
                    *skipped_scope += 1;
                    continue;
                }
                let name = item.name.clone().unwrap_or_else(|| "?".to_owned());
                if module_name_used_elsewhere(tree, &name) {
                    *skipped_referenced += 1;
                    continue;
                }
                let module_file = resolve_module_file(file, &name, item.path_attr.as_deref(), tree);
                units.push(Unit::ModuleDecl {
                    file: file.to_owned(),
                    start: item.start,
                    end: item.end,
                    module_file,
                    label: format!("module `{name}` declaration in {file}"),
                });
            }
            ItemKind::ModBlock => {
                if !scope.includes_modules() {
                    *skipped_scope += 1;
                    continue;
                }
                let name = item
                    .name
                    .clone()
                    .map_or_else(|| "inline".to_owned(), |name| format!("`{name}`"));
                units.push(Unit::Span {
                    file: file.to_owned(),
                    start: item.start,
                    end: item.end,
                    kind: if item.test { "testModule" } else { "module" }.to_owned(),
                    label: format!("inline module {name} in {file}"),
                });
            }
            ItemKind::Use => {
                if !scope.includes_items() {
                    *skipped_scope += 1;
                    continue;
                }
                if name_referenced_elsewhere(item, counts) {
                    *skipped_referenced += 1;
                    continue;
                }
                units.push(Unit::Span {
                    file: file.to_owned(),
                    start: item.start,
                    end: item.end,
                    kind: "use".to_owned(),
                    label: format!("use item in {file}"),
                });
            }
            _ => {
                if !scope.includes_items() {
                    *skipped_scope += 1;
                    continue;
                }
                if !item.test && name_referenced_elsewhere(item, counts) {
                    *skipped_referenced += 1;
                    continue;
                }
                units.push(Unit::Span {
                    file: file.to_owned(),
                    start: item.start,
                    end: item.end,
                    kind: if item.test { "testItem" } else { "item" }.to_owned(),
                    label: format!(
                        "{} in {file}",
                        item.name
                            .as_deref()
                            .map_or_else(|| "item".to_owned(), |name| format!("item `{name}`"))
                    ),
                });
            }
        }
    }
}

fn plan_files(tree: &BTreeMap<String, String>, target: &TargetSite, units: &mut Vec<Unit>) {
    let referenced = referenced_files(tree);
    for file in tree.keys() {
        if units.len() >= MAX_UNITS {
            return;
        }
        if !is_rust_source(file) || file == &target.file || referenced.contains(file) {
            continue;
        }
        units.push(Unit::File {
            file: file.clone(),
            label: format!("file {file}"),
        });
    }
}

fn name_referenced_elsewhere(item: &ParsedItem, counts: &BTreeMap<String, u32>) -> bool {
    let Some(name) = item.name.as_deref() else {
        return false;
    };
    let Some(total) = counts.get(name) else {
        return false;
    };
    *total > 1
}

/// A module declaration is only offered when no remaining code references the
/// module by path (`name::`); plain identifier matches are ignored so an
/// unrelated item with the same name cannot block the reduction.
fn module_name_used_elsewhere(tree: &BTreeMap<String, String>, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let qualified = format!("{name}::");
    let prefix = format!("::{name}");
    for content in tree.values() {
        if content.contains(&qualified) || content.contains(&prefix) {
            return true;
        }
    }
    false
}

fn candidate_names(tree: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (file, content) in tree {
        if !is_rust_source(file) {
            continue;
        }
        for item in parse_items(content) {
            collect_names(&item, &mut names);
        }
    }
    names
}

fn collect_names(item: &ParsedItem, names: &mut BTreeSet<String>) {
    if let Some(name) = &item.name
        && !name.is_empty()
    {
        names.insert(name.clone());
    }
    for child in &item.children {
        collect_names(child, names);
    }
}

fn count_names(tree: &BTreeMap<String, String>, names: &BTreeSet<String>) -> BTreeMap<String, u32> {
    let mut counts: BTreeMap<String, u32> = names.iter().map(|name| (name.clone(), 0u32)).collect();
    if names.is_empty() {
        return counts;
    }
    for content in tree.values() {
        scan_identifiers(content, &mut counts);
    }
    counts
}

fn scan_identifiers(content: &str, counts: &mut BTreeMap<String, u32>) {
    let bytes = content.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i - start <= 128 {
                if let Some(count) = counts.get_mut(&content[start..i]) {
                    *count = count.saturating_add(1);
                }
            }
            continue;
        }
        i += 1;
    }
}

fn group_kind(group: &[Unit]) -> String {
    let mut kinds = BTreeSet::new();
    for unit in group {
        let kind = match unit {
            Unit::Span { kind, .. } => kind.clone(),
            Unit::ModuleDecl { .. } => "module".to_owned(),
            Unit::File { .. } => "file".to_owned(),
        };
        kinds.insert(kind);
    }
    if group.len() > 1 {
        return "group".to_owned();
    }
    kinds
        .into_iter()
        .next()
        .unwrap_or_else(|| "group".to_owned())
}

fn describe_group(group: &[Unit], before: &BTreeMap<String, String>) -> String {
    let mut parts = Vec::new();
    let mut lines = 0u64;
    for unit in group.iter().take(3) {
        lines = lines.saturating_add(unit.lines(before));
        parts.push(unit.label());
    }
    if group.len() > 3 {
        parts.push(format!("and {} more", group.len() - 3));
    }
    format!("{} ({} line(s))", parts.join(", "), lines)
}

fn apply_units(
    tree: &BTreeMap<String, String>,
    units: &[Unit],
) -> Option<BTreeMap<String, String>> {
    let mut next = tree.clone();
    let mut removals: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let mut deletions: BTreeSet<String> = BTreeSet::new();
    for unit in units {
        match unit {
            Unit::Span {
                file, start, end, ..
            } => removals
                .entry(file.clone())
                .or_default()
                .push((*start, *end)),
            Unit::ModuleDecl {
                file,
                start,
                end,
                module_file,
                ..
            } => {
                removals
                    .entry(file.clone())
                    .or_default()
                    .push((*start, *end));
                if let Some(module_file) = module_file {
                    deletions.insert(module_file.clone());
                }
            }
            Unit::File { file, .. } => {
                deletions.insert(file.clone());
            }
        }
    }
    for (file, mut ranges) in removals {
        let content = next.get_mut(&file)?;
        ranges.sort_unstable();
        let mut last_end = 0usize;
        for (start, end) in &ranges {
            if *end <= *start || *end > content.len() || *start < last_end {
                return None;
            }
            if !content.is_char_boundary(*start) || !content.is_char_boundary(*end) {
                return None;
            }
            last_end = *end;
        }
        for (start, end) in ranges.into_iter().rev() {
            content.replace_range(start..end, "");
        }
    }
    for file in deletions {
        if is_rust_source(&file) {
            next.remove(&file);
        }
    }
    Some(next)
}

// ---------------------------------------------------------------------------
// Reference resolution
// ---------------------------------------------------------------------------

fn referenced_files(tree: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut referenced = crate_roots(tree);
    for (file, content) in tree {
        if !is_rust_source(file) {
            continue;
        }
        let items = parse_items(content);
        collect_mod_references(&items, file, tree, &mut referenced);
        collect_include_references(content, file, &mut referenced);
    }
    referenced
}

fn collect_mod_references(
    items: &[ParsedItem],
    file: &str,
    tree: &BTreeMap<String, String>,
    referenced: &mut BTreeSet<String>,
) {
    for item in items {
        if item.kind == ItemKind::ModDecl
            && let Some(name) = item.name.as_deref()
            && let Some(resolved) = resolve_module_file(file, name, item.path_attr.as_deref(), tree)
        {
            referenced.insert(resolved);
        }
        if !item.children.is_empty() {
            collect_mod_references(&item.children, file, tree, referenced);
        }
    }
}

fn collect_include_references(content: &str, file: &str, referenced: &mut BTreeSet<String>) {
    let mut found = 0usize;
    for macro_name in ["include_str!", "include_bytes!", "include!"] {
        let mut search = 0usize;
        while let Some(index) = content[search..].find(macro_name) {
            let at = search + index + macro_name.len();
            if found >= MAX_INCLUDE_REFS_PER_FILE {
                return;
            }
            found += 1;
            if let Some(path) = paren_string(&content[at..]) {
                if let Some(resolved) = resolve_relative(file, &path) {
                    referenced.insert(resolved);
                }
            }
            search = at;
        }
    }
}

fn paren_string(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('(')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    (!value.is_empty() && !value.contains('\\')).then(|| value.to_owned())
}

fn resolve_relative(file: &str, path: &str) -> Option<String> {
    let directory = Path::new(file).parent()?;
    let joined = directory.join(path);
    let mut parts = Vec::new();
    for component in joined.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

fn resolve_module_file(
    file: &str,
    name: &str,
    path_attr: Option<&str>,
    tree: &BTreeMap<String, String>,
) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Some(path) = path_attr {
        let resolved = resolve_relative(file, path)?;
        return tree.contains_key(&resolved).then_some(resolved);
    }
    let path = Path::new(file);
    let directory = path.parent()?;
    let stem = path.file_stem().map(|stem| stem.to_string_lossy())?;
    let base = if matches!(stem.as_ref(), "lib" | "main" | "mod") {
        directory.to_path_buf()
    } else {
        directory.join(stem.as_ref())
    };
    let plain = base.join(format!("{name}.rs"));
    let nested = base.join(name).join("mod.rs");
    for candidate in [plain, nested] {
        if let Some(relative) = path_to_slash(&candidate)
            && tree.contains_key(&relative)
        {
            return Some(relative);
        }
    }
    None
}

fn path_to_slash(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn crate_roots(tree: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    let manifest = tree
        .get("Cargo.toml")
        .map(String::as_str)
        .unwrap_or_default();
    let mut in_lib = false;
    let mut in_bin = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib = trimmed == "[lib]";
            in_bin = trimmed == "[[bin]]";
            continue;
        }
        if (in_lib || in_bin)
            && let Some(value) = quoted_after_eq(trimmed)
        {
            roots.insert(value);
        }
    }
    if tree.contains_key("src/lib.rs") {
        roots.insert("src/lib.rs".to_owned());
    }
    if tree.contains_key("src/main.rs") {
        roots.insert("src/main.rs".to_owned());
    }
    roots
}

fn quoted_after_eq(line: &str) -> Option<String> {
    let (_, value) = line.split_once('=')?;
    let value = value.trim();
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    let value = &value[..end];
    (!value.is_empty()).then(|| value.to_owned())
}

fn manifest_edition(tree: &BTreeMap<String, String>) -> String {
    let manifest = tree
        .get("Cargo.toml")
        .map(String::as_str)
        .unwrap_or_default();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("edition")
            && let Some(value) = quoted_after_eq(trimmed)
        {
            return value;
        }
    }
    "unset".to_owned()
}

// ---------------------------------------------------------------------------
// Rust item scanner
// ---------------------------------------------------------------------------

fn parse_items(text: &str) -> Vec<ParsedItem> {
    let mut items = Vec::new();
    scan_items(text, 0, text.len(), 0, &mut items);
    items
}

#[allow(clippy::too_many_lines)]
fn scan_items(text: &str, start: usize, end: usize, depth: usize, out: &mut Vec<ParsedItem>) {
    let bytes = text.as_bytes();
    let mut i = start;
    let mut pending_start: Option<usize> = None;
    let mut test = false;
    let mut path_attr: Option<String> = None;
    while i < end && out.len() < MAX_ITEMS_PER_FILE {
        let byte = bytes[i];
        if byte.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if byte == b'/' && i + 1 < end {
            if bytes[i + 1] == b'/' {
                let doc = i + 2 < end && (bytes[i + 2] == b'/' || bytes[i + 2] == b'!');
                if doc {
                    pending_start.get_or_insert(i);
                }
                i = skip_line_comment(text, i, end);
                continue;
            }
            if bytes[i + 1] == b'*' {
                let doc = i + 2 < end && (bytes[i + 2] == b'*' || bytes[i + 2] == b'!');
                if doc {
                    pending_start.get_or_insert(i);
                }
                i = skip_block_comment(text, i, end);
                continue;
            }
        }
        if byte == b'#'
            && i + 1 < end
            && (bytes[i + 1] == b'['
                || (bytes[i + 1] == b'!' && i + 2 < end && bytes[i + 2] == b'['))
        {
            let attr_end = if bytes[i + 1] == b'!' {
                skip_delimited(text, i + 2, end, b'[', b']')
            } else {
                skip_delimited(text, i + 1, end, b'[', b']')
            };
            if attr_end <= i {
                break;
            }
            let attr_text = &text[i..attr_end];
            if is_test_cfg(attr_text) {
                test = true;
            }
            if let Some(path) = parse_path_attribute(attr_text) {
                path_attr = Some(path);
            }
            pending_start.get_or_insert(i);
            i = attr_end;
            continue;
        }
        let item_start = pending_start.take().unwrap_or(i);
        let keyword_at = skip_modifiers(text, i, end);
        let kind = classify_kind(text, keyword_at, end);
        let scan = find_item_end(text, i, end);
        let item_end = extend_to_line_end(text, scan.end, end);
        let name = item_name(text, kind, keyword_at, item_end);
        let mut item = ParsedItem {
            kind,
            start: item_start,
            end: item_end,
            body: scan.body,
            name,
            path_attr: path_attr.take(),
            test,
            children: Vec::new(),
        };
        if item.kind == ItemKind::ModBlock
            && depth < MAX_PARSE_DEPTH
            && let Some((open, close)) = item.body
            && close > open + 1
        {
            let mut children = Vec::new();
            scan_items(text, open + 1, close, depth + 1, &mut children);
            item.children = children;
        }
        out.push(item);
        i = item_end;
        test = false;
        path_attr = None;
    }
}

#[derive(Debug, Clone, Copy)]
struct ItemScan {
    end: usize,
    body: Option<(usize, usize)>,
}

#[allow(clippy::too_many_lines)]
fn find_item_end(text: &str, start: usize, end: usize) -> ItemScan {
    let bytes = text.as_bytes();
    let mut i = start;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut body_open = None;
    while i < end {
        let byte = bytes[i];
        match byte {
            b'"' => {
                i = skip_quoted(text, i, end);
                continue;
            }
            b'\'' => {
                i = skip_char_or_lifetime(text, i, end);
                continue;
            }
            b'r' | b'b' | b'c' => {
                if let Some(next) = skip_prefixed_string(text, i, end) {
                    i = next;
                    continue;
                }
            }
            b'/' if i + 1 < end && bytes[i + 1] == b'/' => {
                i = skip_line_comment(text, i, end);
                continue;
            }
            b'/' if i + 1 < end && bytes[i + 1] == b'*' => {
                i = skip_block_comment(text, i, end);
                continue;
            }
            b'{' => {
                brace += 1;
                if brace == 1 && paren == 0 && bracket == 0 {
                    body_open = Some(i);
                }
            }
            b'}' => {
                brace -= 1;
                if brace <= 0 {
                    let mut cursor = i + 1;
                    while cursor < end && matches!(bytes[cursor], b' ' | b'\t') {
                        cursor += 1;
                    }
                    if cursor < end && bytes[cursor] == b';' {
                        cursor += 1;
                    }
                    return ItemScan {
                        end: cursor,
                        body: body_open.map(|open| (open, i)),
                    };
                }
            }
            b'(' => paren += 1,
            b')' => paren -= 1,
            b'[' => bracket += 1,
            b']' => bracket -= 1,
            b';' if brace == 0 && paren == 0 && bracket == 0 => {
                return ItemScan {
                    end: i + 1,
                    body: None,
                };
            }
            _ => {}
        }
        i += 1;
    }
    ItemScan {
        end,
        body: body_open.map(|open| (open, end)),
    }
}

fn extend_to_line_end(text: &str, end: usize, limit: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = end;
    while i < limit {
        match bytes[i] {
            b'\n' => return i + 1,
            b' ' | b'\t' | b'\r' => i += 1,
            _ => return i,
        }
    }
    i
}

fn skip_line_comment(text: &str, start: usize, end: usize) -> usize {
    match text[start..end].find('\n') {
        Some(offset) => start + offset + 1,
        None => end,
    }
}

fn skip_block_comment(text: &str, start: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = start;
    while i + 1 < end {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth += 1;
            i += 2;
            continue;
        }
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth <= 0 {
                return i;
            }
            continue;
        }
        i += 1;
    }
    end
}

fn skip_quoted(text: &str, start: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    while i < end {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    end
}

fn skip_char_or_lifetime(text: &str, start: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start + 1;
    if i >= end {
        return end;
    }
    if bytes[i] == b'\\' {
        i += 1;
        if i < end {
            i += 1;
        }
    }
    let mut cursor = i;
    while cursor < end && cursor < i + 8 {
        if bytes[cursor] == b'\'' {
            return cursor + 1;
        }
        if bytes[cursor] == b'\n' {
            break;
        }
        cursor += 1;
    }
    start + 1
}

fn skip_prefixed_string(text: &str, start: usize, end: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = start;
    match bytes.get(i)? {
        b'r' => i += 1,
        b'b' => {
            i += 1;
            if bytes.get(i) == Some(&b'r') {
                i += 1;
            } else if bytes.get(i) == Some(&b'"') {
                return Some(skip_quoted(text, i, end));
            } else {
                return None;
            }
        }
        b'c' => {
            i += 1;
            if bytes.get(i) != Some(&b'"') {
                return None;
            }
            return Some(skip_quoted(text, i, end));
        }
        _ => return None,
    }
    let mut hashes = 0usize;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    while i < end {
        if bytes[i] == b'"' {
            let mut cursor = i + 1;
            let mut count = 0usize;
            while count < hashes && cursor < end && bytes[cursor] == b'#' {
                count += 1;
                cursor += 1;
            }
            if count == hashes {
                return Some(cursor);
            }
        }
        i += 1;
    }
    Some(end)
}

fn skip_delimited(text: &str, start: usize, end: usize, open: u8, close: u8) -> usize {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&open) {
        return start;
    }
    let mut depth = 0i32;
    let mut i = start;
    while i < end {
        let byte = bytes[i];
        match byte {
            b'"' => {
                i = skip_quoted(text, i, end);
                continue;
            }
            b'\'' => {
                i = skip_char_or_lifetime(text, i, end);
                continue;
            }
            b'r' | b'b' | b'c' => {
                if let Some(next) = skip_prefixed_string(text, i, end) {
                    i = next;
                    continue;
                }
            }
            b'/' if i + 1 < end && bytes[i + 1] == b'/' => {
                i = skip_line_comment(text, i, end);
                continue;
            }
            b'/' if i + 1 < end && bytes[i + 1] == b'*' => {
                i = skip_block_comment(text, i, end);
                continue;
            }
            _ if byte == open => depth += 1,
            _ if byte == close => {
                depth -= 1;
                if depth <= 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    end
}

fn skip_modifiers(text: &str, start: usize, end: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start;
    loop {
        while i < end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(word) = word_at(text, i, end) else {
            return i;
        };
        match word {
            "pub" => {
                i += 3;
                if bytes.get(i) == Some(&b'(') {
                    i = skip_delimited(text, i, end, b'(', b')');
                }
            }
            "async" | "unsafe" | "const" | "default" | "auto" => i += word.len(),
            "extern" => {
                i += 6;
                while i < end && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if bytes.get(i) == Some(&b'"') {
                    i = skip_quoted(text, i, end);
                }
            }
            _ => return i,
        }
    }
}

fn classify_kind(text: &str, start: usize, end: usize) -> ItemKind {
    let Some(word) = word_at(text, start, end) else {
        return ItemKind::Other;
    };
    match word {
        "use" => ItemKind::Use,
        "mod" => {
            let mut i = start + 3;
            while i < end && text.as_bytes()[i].is_ascii_whitespace() {
                i += 1;
            }
            while i < end
                && (text.as_bytes()[i].is_ascii_alphanumeric() || text.as_bytes()[i] == b'_')
            {
                i += 1;
            }
            while i < end && text.as_bytes()[i].is_ascii_whitespace() {
                i += 1;
            }
            if text.as_bytes().get(i) == Some(&b'{') {
                ItemKind::ModBlock
            } else {
                ItemKind::ModDecl
            }
        }
        "fn" => ItemKind::Fn,
        "struct" => ItemKind::Struct,
        "enum" => ItemKind::Enum,
        "impl" => ItemKind::Impl,
        "trait" => ItemKind::Trait,
        "type" => ItemKind::Type,
        "const" => ItemKind::Const,
        "static" => ItemKind::Static,
        "macro_rules" | "macro" => ItemKind::Macro,
        "crate" => ItemKind::Extern,
        _ => ItemKind::Other,
    }
}

fn item_name(text: &str, kind: ItemKind, keyword_at: usize, end: usize) -> Option<String> {
    match kind {
        ItemKind::ModDecl | ItemKind::ModBlock => read_ident(text, keyword_at + 3, end),
        ItemKind::Fn
        | ItemKind::Struct
        | ItemKind::Enum
        | ItemKind::Trait
        | ItemKind::Type
        | ItemKind::Const
        | ItemKind::Static => read_ident(text, keyword_at + word_len(text, keyword_at, end), end),
        ItemKind::Macro => {
            let after = keyword_at + word_len(text, keyword_at, end);
            let mut i = after;
            while i < end && text.as_bytes()[i].is_ascii_whitespace() {
                i += 1;
            }
            if text.as_bytes().get(i) == Some(&b'!') {
                i += 1;
            }
            read_ident(text, i, end)
        }
        ItemKind::Use => use_name(text.get(keyword_at..end).unwrap_or_default()),
        _ => None,
    }
}

fn word_len(text: &str, start: usize, end: usize) -> usize {
    word_at(text, start, end).map_or(0, str::len)
}

fn word_at(text: &str, start: usize, end: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if start >= end {
        return None;
    }
    let first = bytes[start];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut i = start;
    while i < end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    text.get(start..i)
}

fn read_ident(text: &str, start: usize, end: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = start;
    while i < end && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let ident_start = i;
    while i < end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == ident_start {
        return None;
    }
    Some(text[ident_start..i].to_owned())
}

fn use_name(text: &str) -> Option<String> {
    let trimmed = text
        .trim()
        .trim_start_matches("use")
        .trim()
        .trim_end_matches(';')
        .trim();
    if trimmed.contains('{') || trimmed.contains('\n') {
        return None;
    }
    let (path, alias) = match trimmed.rsplit_once(" as ") {
        Some((path, alias)) => (path, Some(alias.trim())),
        None => (trimmed, None),
    };
    let name = alias
        .unwrap_or_else(|| path.rsplit("::").next().unwrap_or(path))
        .trim();
    (!name.is_empty() && !name.contains(' ')).then(|| name.to_owned())
}

fn is_test_cfg(attr: &str) -> bool {
    attr.contains("cfg(test)") || attr.contains("cfg(any(test") || attr.contains("cfg(all(test")
}

fn parse_path_attribute(attr: &str) -> Option<String> {
    let index = attr.find("path")?;
    let rest = attr[index + 4..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    (!value.is_empty() && !value.contains('\\')).then(|| value.to_owned())
}

// ---------------------------------------------------------------------------
// Proof package
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn build_proof(
    tree: &BTreeMap<String, String>,
    session: &MinimizeSession,
    predicate: &FailurePredicate,
    export_verified: bool,
    export_verification: &str,
) -> RepairProofData {
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    let mut package_complete = true;
    let mut omitted = Vec::new();
    for (file, content) in tree {
        let bytes = u64::try_from(content.len()).unwrap_or(u64::MAX);
        let include = files.len() < MAX_PROOF_FILES
            && bytes <= MAX_PROOF_FILE_BYTES
            && total_bytes.saturating_add(bytes) <= MAX_PROOF_TOTAL_BYTES;
        if include {
            total_bytes = total_bytes.saturating_add(bytes);
            files.push(RepairProofFileData {
                file: file.clone(),
                sha256: sha256_hex(content.as_bytes()),
                bytes,
                content: Some(content.clone()),
                content_omitted: false,
            });
        } else {
            package_complete = false;
            if omitted.len() < 16 {
                omitted.push(format!("{file}: content omitted at the proof bound"));
            }
            files.push(RepairProofFileData {
                file: file.clone(),
                sha256: sha256_hex(content.as_bytes()),
                bytes,
                content: None,
                content_omitted: true,
            });
        }
    }
    let source_sha256 = tree_sha256(tree);
    let toolchain = tree
        .get("rust-toolchain.toml")
        .or_else(|| tree.get("rust-toolchain"))
        .map_or_else(|| "hostDefault".to_owned(), |text| bounded(text, 4_096));
    let edition = manifest_edition(tree);
    let features: Vec<String> = Vec::new();
    let configuration_sha256 = sha256_hex(
        format!(
            "target={}\nedition={}\nfeatures={}\ntoolchain={}\n",
            session.target.as_str(),
            edition,
            features.join(","),
            toolchain
        )
        .as_bytes(),
    );
    let evidence_sha256 = sha256_hex(
        format!(
            "{source_sha256}\n{configuration_sha256}\n{}|{}|{}\n{export_verified}",
            predicate.code,
            predicate.file.as_deref().unwrap_or(""),
            predicate.normalized_message,
        )
        .as_bytes(),
    );
    RepairProofData {
        files,
        files_total: u64::try_from(tree.len()).unwrap_or(u64::MAX),
        total_bytes,
        package_complete,
        omitted,
        source_sha256,
        configuration_sha256,
        evidence_sha256,
        reproduction_command: reproduction_command(&session.last_command),
        toolchain,
        edition,
        target: session.target.as_str().to_owned(),
        features,
        cargo_toml_sha256: tree
            .get("Cargo.toml")
            .map(|text| sha256_hex(text.as_bytes())),
        lockfile_sha256: tree
            .get("Cargo.lock")
            .map(|text| sha256_hex(text.as_bytes())),
        lockfile_preserved: tree.contains_key("Cargo.lock"),
        export_verified,
        initial_reproduction: format!(
            "the unchanged candidate reproduced {} at the first fresh run",
            predicate.diagnostic_id
        ),
        export_verification: bounded(export_verification, 512),
    }
}

fn reproduction_command(command: &str) -> String {
    let mut parts = command.split_whitespace();
    let _executable = parts.next();
    let rest = parts.collect::<Vec<_>>();
    if rest.is_empty() {
        "cargo check".to_owned()
    } else {
        bounded(&format!("cargo {}", rest.join(" ")), 512)
    }
}

fn tree_sha256(tree: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agz-rust-mcp-minimized-tree\0");
    for (file, content) in tree {
        hasher.update(file.as_bytes());
        hasher.update(b"\0");
        hasher.update(content.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn bounded(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(
        code: Option<&str>,
        file: Option<&str>,
        line: u64,
        message: &str,
    ) -> ChangeDiagnosticData {
        ChangeDiagnosticData {
            code: code.map(str::to_owned),
            level: "error".to_owned(),
            file: file.map(str::to_owned),
            line: Some(line),
            message: message.to_owned(),
        }
    }

    fn predicate(code: &str, message: &str) -> FailurePredicate {
        let diagnostics = vec![evidence(Some(code), Some("src/lib.rs"), 1, message)];
        FailurePredicate::build(&diagnostics, None).expect("predicate")
    }

    fn observed(
        code: Option<&str>,
        level: &str,
        file: Option<&str>,
        message: &str,
    ) -> ObservedDiagnostic {
        ObservedDiagnostic {
            code: code.map(str::to_owned),
            level: level.to_owned(),
            file: file.map(str::to_owned),
            message: message.to_owned(),
        }
    }

    fn trial(status: GateStatus, diagnostics: Vec<ObservedDiagnostic>) -> TrialRun {
        TrialRun {
            status,
            total_ms: 1,
            diagnostics,
            diagnostics_omitted: 0,
            command: "cargo check".to_owned(),
            message: String::new(),
        }
    }

    #[test]
    fn predicate_requires_a_code_and_message_structure() {
        let uncoded = evidence(None, Some("src/lib.rs"), 1, "expected `;`");
        assert!(FailurePredicate::build(&[uncoded], None).is_err());

        let coded = evidence(
            Some("E0382"),
            Some("src/lib.rs"),
            4,
            "borrow of moved value: `text`",
        );
        let predicate = FailurePredicate::build(&[coded], None).expect("predicate");
        assert_eq!(predicate.code, "E0382");
        assert_eq!(predicate.message_tokens, vec!["text"]);

        let wrong_code = RepairFailurePredicateInput {
            code: Some("E0308".to_owned()),
            message_contains: Vec::new(),
            file: None,
        };
        let coded = evidence(
            Some("E0382"),
            Some("src/lib.rs"),
            4,
            "borrow of moved value: `text`",
        );
        assert!(FailurePredicate::build(&[coded], Some(&wrong_code)).is_err());

        let wrong_file = RepairFailurePredicateInput {
            code: None,
            message_contains: Vec::new(),
            file: Some("src/other.rs".to_owned()),
        };
        let coded = evidence(
            Some("E0382"),
            Some("src/lib.rs"),
            4,
            "borrow of moved value: `text`",
        );
        assert!(FailurePredicate::build(&[coded], Some(&wrong_file)).is_err());
    }

    #[test]
    fn evaluation_rejects_irrelevant_same_code_and_uncoded_syntax_errors() {
        let predicate = predicate("E0382", "borrow of moved value: `text`");
        let baseline: BTreeSet<String> = trial(
            GateStatus::Fail,
            vec![observed(
                Some("E0382"),
                "error",
                Some("src/lib.rs"),
                "borrow of moved value: `text`",
            )],
        )
        .signatures();

        // Same error code, different trait/type/message structure.
        let same_code = trial(
            GateStatus::Fail,
            vec![observed(
                Some("E0382"),
                "error",
                Some("src/lib.rs"),
                "borrow of moved value: `other`",
            )],
        );
        let evaluation = evaluate_run(&predicate, &baseline, &same_code);
        assert_eq!(evaluation.kind, MatchKind::MessageMismatch);
        assert!(!evaluation.matched);

        // A wrong syntax error has no code and is never the original failure.
        let syntax = trial(
            GateStatus::Fail,
            vec![observed(
                None,
                "error",
                Some("src/lib.rs"),
                "expected one of `.`, `?`, `{`",
            )],
        );
        let evaluation = evaluate_run(&predicate, &baseline, &syntax);
        assert_eq!(evaluation.kind, MatchKind::CodeAbsent);
        assert!(!evaluation.matched);
    }

    #[test]
    fn evaluation_rejects_new_signatures_including_missing_dependencies() {
        let predicate = predicate("E0382", "borrow of moved value: `text`");
        let baseline: BTreeSet<String> = trial(
            GateStatus::Fail,
            vec![observed(
                Some("E0382"),
                "error",
                Some("src/lib.rs"),
                "borrow of moved value: `text`",
            )],
        )
        .signatures();
        let run = trial(
            GateStatus::Fail,
            vec![
                observed(
                    Some("E0382"),
                    "error",
                    Some("src/lib.rs"),
                    "borrow of moved value: `text`",
                ),
                observed(
                    Some("E0463"),
                    "error",
                    Some("src/lib.rs"),
                    "can't find crate for `external_crate`",
                ),
            ],
        );
        let evaluation = evaluate_run(&predicate, &baseline, &run);
        assert_eq!(evaluation.kind, MatchKind::NewSignatures);
        assert!(!evaluation.matched);
        assert!(evaluation.reason.contains("E0463"), "{}", evaluation.reason);
    }

    #[test]
    fn evaluation_rejects_timeouts_and_matches_the_exact_predicate() {
        let predicate = predicate("E0382", "borrow of moved value: `text`");
        let baseline: BTreeSet<String> = BTreeSet::new();
        let timeout = trial(
            GateStatus::Timeout,
            vec![observed(
                Some("E0382"),
                "error",
                Some("src/lib.rs"),
                "borrow of moved value: `text`",
            )],
        );
        assert_eq!(
            evaluate_run(&predicate, &baseline, &timeout).kind,
            MatchKind::Inconclusive
        );
        let matched = trial(
            GateStatus::Fail,
            vec![observed(
                Some("E0382"),
                "error",
                Some("src/lib.rs"),
                "borrow of moved value: `text`",
            )],
        );
        let own_baseline = matched.signatures();
        let evaluation = evaluate_run(&predicate, &own_baseline, &matched);
        assert!(evaluation.matched);
        assert_eq!(evaluation.kind, MatchKind::Matched);
    }

    #[test]
    fn planner_offers_unused_modules_and_protects_referenced_items() {
        let mut tree = BTreeMap::new();
        tree.insert(
            "Cargo.toml".to_owned(),
            "[package]\nname = \"fixture\"\n[lib]\npath = \"src/lib.rs\"\n".to_owned(),
        );
        tree.insert(
            "src/lib.rs".to_owned(),
            "mod used;\nmod unused;\nuse std::fmt;\npub fn consume(value: String) -> usize {\n    value.len()\n}\npub fn run() -> usize {\n    let text = String::from(\"x\");\n    used::f();\n    consume(text);\n    text.len()\n}\n"
                .to_owned(),
        );
        tree.insert("src/used.rs".to_owned(), "pub fn f() {}\n".to_owned());
        tree.insert("src/unused.rs".to_owned(), "pub fn g() {}\n".to_owned());
        let diagnostics = vec![evidence(
            Some("E0382"),
            Some("src/lib.rs"),
            11,
            "borrow of moved value: `text`",
        )];
        let predicate = FailurePredicate::build(&diagnostics, None).expect("predicate");
        let target = TargetSite::locate(&tree, &predicate);
        assert!(target.anchor.is_some());
        let mut skipped_referenced = 0usize;
        let mut skipped_scope = 0usize;
        let units = plan_units(
            &tree,
            &target,
            RepairReductionScope::default(),
            &mut skipped_referenced,
            &mut skipped_scope,
        );
        let labels = units.iter().map(Unit::label).collect::<Vec<_>>();
        assert!(
            labels.iter().any(|label| label.contains("module `unused`")),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label.contains("module `used`")),
            "a referenced module must not be offered: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("use item")),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label.contains("`consume`")),
            "a referenced helper must not be offered: {labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label.contains("`run`")),
            "the target item must never be offered: {labels:?}"
        );
        let module = units
            .iter()
            .find(|unit| unit.label().contains("module `unused`"))
            .expect("unused module unit");
        let removed = apply_units(&tree, std::slice::from_ref(module)).expect("apply reduction");
        assert!(!removed.contains_key("src/unused.rs"));
        assert!(!removed["src/lib.rs"].contains("mod unused;"));
    }

    #[test]
    fn item_scope_never_plans_module_units() {
        let mut tree = BTreeMap::new();
        tree.insert(
            "src/lib.rs".to_owned(),
            "mod unused;\npub fn run() {}\n".to_owned(),
        );
        tree.insert("src/unused.rs".to_owned(), "pub fn g() {}\n".to_owned());
        let target = TargetSite {
            file: "src/lib.rs".to_owned(),
            anchor: Some("pub fn run() {}".to_owned()),
        };
        let mut skipped_referenced = 0usize;
        let mut skipped_scope = 0usize;
        let units = plan_units(
            &tree,
            &target,
            RepairReductionScope::Items,
            &mut skipped_referenced,
            &mut skipped_scope,
        );
        assert!(
            !units
                .iter()
                .any(|unit| matches!(unit, Unit::ModuleDecl { .. })),
            "items scope must never plan a module declaration: {units:?}"
        );
        assert!(
            !units.iter().any(|unit| matches!(
                unit,
                Unit::Span { kind, .. } if kind == "module" || kind == "testModule"
            )),
            "items scope must never plan an inline module: {units:?}"
        );
        assert!(skipped_scope > 0);
    }

    #[test]
    fn parser_finds_top_level_items_and_inline_module_children() {
        let text = "//! crate docs\nuse std::fmt;\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn works() {}\n}\n\npub struct Config { pub value: u8 }\n\nimpl Config {\n    pub fn new() -> Self { Self { value: 0 } }\n}\n";
        let items = parse_items(text);
        let kinds = items.iter().map(|item| item.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                ItemKind::Use,
                ItemKind::ModBlock,
                ItemKind::Struct,
                ItemKind::Impl
            ],
            "{items:#?}"
        );
        let tests = &items[1];
        assert!(tests.test);
        assert_eq!(tests.name.as_deref(), Some("tests"));
        assert_eq!(tests.children.len(), 1);
        assert_eq!(tests.children[0].kind, ItemKind::Fn);
        let config = &items[2];
        assert_eq!(config.name.as_deref(), Some("Config"));
    }

    #[test]
    fn manifests_are_read_for_roots_edition_and_lockfile() {
        let mut tree = BTreeMap::new();
        tree.insert(
            "Cargo.toml".to_owned(),
            "[package]\nname = \"fixture\"\nedition = \"2024\"\n[lib]\npath = \"src/lib.rs\"\n"
                .to_owned(),
        );
        assert_eq!(manifest_edition(&tree), "2024");
        assert!(crate_roots(&tree).contains("src/lib.rs"));
        let proof_command = reproduction_command("/usr/bin/cargo check --message-format=json");
        assert_eq!(proof_command, "cargo check --message-format=json");
        assert_eq!(reproduction_command(""), "cargo check");
    }
}
