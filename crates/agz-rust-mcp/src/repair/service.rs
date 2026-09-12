//! Compiler-driven repair orchestration.
//!
//! `analyze` reads the current-revision fresh FAIL evidence of an existing
//! change. `try` recreates the base change for every candidate and validates
//! it through [`ChangeService`] on isolated scratch copies. `compare` adds
//! measured selection rules. No action writes the original workspace.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    change::{
        ChangeAction, ChangeData, ChangeEvidenceData, ChangeRequest, ChangeService, NewFileInput,
        PatchInput,
    },
    config::Config,
    gate::{GateDetail, GateTargetId, ValidationOptions},
    lsp::RustAnalyzerManager,
    tools::{symbol::request_until, with_rust_document},
    workspace::WorkspaceRoot,
};

use super::{
    analysis, guards, minimize,
    model::{
        MAX_ANALYZED_DIAGNOSTICS, MAX_CANDIDATE_DIAGNOSTICS, MAX_IMPACTS, MAX_LISTED_CANDIDATES,
        MAX_REMAINING_RISKS, RepairAnalysisData, RepairBudget, RepairBudgetData, RepairBudgetInput,
        RepairCandidateData, RepairCandidateInput, RepairCandidateSourceData,
        RepairConfigurationData, RepairData, RepairGateData, RepairOutcome, RepairPatchData,
        RepairRequest,
    },
};

const ANALYSIS_ASSIST_LIMIT: usize = 4;
const LSP_ASSIST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct RepairService {
    config: Config,
    change: Arc<ChangeService>,
}

impl std::fmt::Debug for RepairService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepairService")
            .field("budget", &self.config.repair)
            .finish_non_exhaustive()
    }
}

impl RepairService {
    pub fn new(config: Config, change: Arc<ChangeService>) -> Self {
        Self { config, change }
    }

    /// Effective budget: the request may only narrow the configured limits.
    pub fn effective_budget(&self, input: &RepairBudgetInput) -> RepairBudget {
        let config = &self.config.repair;
        RepairBudget {
            max_candidates: input.max_candidates.map_or(config.max_candidates, |value| {
                value.min(config.max_candidates)
            }),
            max_compiles: input
                .max_compiles
                .map_or(config.max_compiles, |value| value.min(config.max_compiles)),
            wall_time_ms: input
                .wall_time_ms
                .map_or(config.wall_time_ms, |value| value.min(config.wall_time_ms)),
        }
    }

    /// Effective minimization budget: a request may only narrow the dedicated
    /// `[repair]` minimization caps.
    pub fn effective_minimize_budget(&self, input: &RepairBudgetInput) -> RepairBudget {
        let config = &self.config.repair;
        RepairBudget {
            max_candidates: input
                .max_candidates
                .map_or(config.minimize_max_candidates, |value| {
                    value.min(config.minimize_max_candidates)
                }),
            max_compiles: input
                .max_compiles
                .map_or(config.minimize_max_compiles, |value| {
                    value.min(config.minimize_max_compiles)
                }),
            wall_time_ms: input
                .wall_time_ms
                .map_or(config.wall_time_ms, |value| value.min(config.wall_time_ms)),
        }
    }

    pub async fn execute(
        &self,
        request: RepairRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        lsp: Option<&Arc<RustAnalyzerManager>>,
    ) -> RepairOutcome {
        match request.action {
            super::model::RepairAction::Analyze => {
                self.analyze(request, workspace, cancellation, lsp).await
            }
            super::model::RepairAction::Try | super::model::RepairAction::Compare => {
                Box::pin(self.run_candidates(request, workspace, cancellation, lsp)).await
            }
            super::model::RepairAction::Minimize => {
                self.minimize(request, workspace, cancellation).await
            }
        }
    }

    async fn minimize(
        &self,
        request: RepairRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
    ) -> RepairOutcome {
        if cancellation.is_cancelled() {
            return self.cancelled(RepairActionKind::Minimize, &request.change_id);
        }
        let id = request.change_id.clone();
        let data = match self
            .inspect_change(RepairActionKind::Minimize, &id, workspace, &cancellation)
            .await
        {
            Ok(data) => data,
            Err(outcome) => return outcome,
        };
        let evidence = match base_evidence(&data) {
            BaseEvidence::Fail(evidence) => evidence.clone(),
            BaseEvidence::Clean => {
                return self.outcome(
                    "CLEAN",
                    "The current revision has no fresh compile failure to minimize.",
                    false,
                    RepairData {
                        action: "minimize".to_owned(),
                        usable: false,
                        change_id: Some(id),
                        base_identity: data.base_identity.clone(),
                        revision: Some(data.revision),
                        reason: "current revision already passes; nothing to minimize".to_owned(),
                        stop_reason: "noFailure".to_owned(),
                        ..RepairData::default()
                    },
                );
            }
            BaseEvidence::Missing(reason) => {
                return self.no_evidence("minimize".to_owned(), &id, &data, reason);
            }
        };
        let target = parse_gate_target(&evidence.target);
        let minimize_request = minimize::MinimizeRequest {
            change_id: id,
            diagnostic_ids: request.diagnostic_ids,
            failure_predicate: request.failure_predicate,
            reduction_scope: request.reduction_scope,
            budget: request.budget,
            base: data,
            evidence,
            target,
        };
        let outcome =
            minimize::minimize(&self.change, &self.config, minimize_request, cancellation).await;
        self.outcome(
            outcome.status,
            outcome.summary,
            outcome.is_error,
            outcome.data,
        )
    }

    async fn inspect_change(
        &self,
        action: RepairActionKind,
        id: &str,
        workspace: &WorkspaceRoot,
        cancellation: &CancellationToken,
    ) -> Result<ChangeData, RepairOutcome> {
        let request = ChangeRequest {
            action: ChangeAction::Inspect,
            change_id: Some(id.to_owned()),
            expected_revision: None,
            base_identity: None,
            patches: Vec::new(),
            new_files: Vec::new(),
            migration: None,
            target: GateTargetId::Check,
            options: ValidationOptions::default(),
            detail: GateDetail::Compact,
            timings: false,
        };
        let outcome = self
            .change
            .execute(request, workspace, cancellation.clone(), None)
            .await;
        if outcome.status == "INSPECTED" {
            return Ok(outcome.data);
        }
        Err(self.outcome(
            action.status(),
            "The change record could not be inspected.",
            true,
            RepairData {
                action: action.as_str().to_owned(),
                change_id: Some(id.to_owned()),
                reason: bounded(&outcome.data.reason, 512),
                stop_reason: "noUsableEvidence".to_owned(),
                ..RepairData::default()
            },
        ))
    }

    async fn analyze(
        &self,
        request: RepairRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        lsp: Option<&Arc<RustAnalyzerManager>>,
    ) -> RepairOutcome {
        if cancellation.is_cancelled() {
            return self.cancelled(RepairActionKind::Analyze, &request.change_id);
        }
        let id = request.change_id.clone();
        let data = match self
            .inspect_change(RepairActionKind::Analyze, &id, workspace, &cancellation)
            .await
        {
            Ok(data) => data,
            Err(outcome) => return outcome,
        };
        let evidence = match base_evidence(&data) {
            BaseEvidence::Fail(evidence) => evidence.clone(),
            BaseEvidence::Clean => {
                return self.outcome(
                    "CLEAN",
                    "The current revision has no fresh compile failure to repair.",
                    false,
                    RepairData {
                        action: "analyze".to_owned(),
                        usable: false,
                        change_id: Some(id),
                        base_identity: data.base_identity.clone(),
                        revision: Some(data.revision),
                        reason: "current revision already passes; no repair evidence is published"
                            .to_owned(),
                        stop_reason: "noFailure".to_owned(),
                        ..RepairData::default()
                    },
                );
            }
            BaseEvidence::Missing(reason) => {
                return self.no_evidence("analyze".to_owned(), &id, &data, reason);
            }
        };
        let (diagnostics, unmatched) =
            analysis::filter_diagnostics(&evidence.diagnostics, &request.diagnostic_ids);
        let files = diagnostic_files(&diagnostics);
        let sources = self.read_candidate_sources(&id, &files);
        let read = |file: &str| sources.get(file).cloned();
        let groups = analysis::group_diagnostics(&diagnostics);
        let relations = analysis::relations(&groups);
        let ownership = analysis::ownership(&diagnostics, &read);
        let (candidate_data, source_rows) = self
            .build_candidates(&diagnostics, &evidence, &read, lsp, workspace)
            .await;
        let usable = !diagnostics.is_empty();
        let mut risks = Vec::new();
        if !relations.is_empty() {
            risks.push(
                "Root-cause relations are reasoned hypotheses derived from bounded spans, not proven facts."
                    .to_owned(),
            );
        }
        if candidate_data
            .iter()
            .any(|candidate| candidate.source == "mechanical")
        {
            risks.push(
                "Mechanical transforms are explicit heuristics; every candidate is verified by a real compile before selection."
                    .to_owned(),
            );
        }
        if evidence.diagnostics_omitted > 0 {
            risks.push("Compiler diagnostics were truncated for this evidence row.".to_owned());
        }
        if evidence
            .suggestion_package
            .as_ref()
            .is_some_and(|package| package.truncated)
        {
            risks.push(
                "The machine-applicable suggestion package was truncated by the persistence bound."
                    .to_owned(),
            );
        }
        for row in &source_rows {
            if row.status != "available" && !row.reason.is_empty() {
                risks.push(format!("{} source: {}", row.kind, row.reason));
            }
        }
        risks.truncate(MAX_REMAINING_RISKS);
        let analysis = RepairAnalysisData {
            diagnostics: groups,
            relations,
            ownership,
            sources: source_rows,
            unmatched_diagnostic_ids: unmatched,
        };
        let summary = format!(
            "Analyzed {} diagnostic group(s) from the fresh {:?} evidence; {} candidate source(s) are available.",
            analysis.diagnostics.len(),
            evidence.target,
            analysis
                .sources
                .iter()
                .filter(|source| source.status == "available")
                .count(),
        );
        self.outcome(
            "ANALYZED",
            summary,
            false,
            RepairData {
                action: "analyze".to_owned(),
                usable,
                change_id: Some(id),
                base_identity: data.base_identity.clone(),
                revision: Some(data.revision),
                evidence_revision: Some(evidence.revision),
                analysis: Some(analysis),
                candidates: candidate_data,
                verification: "none".to_owned(),
                configuration: Some(self.configuration(
                    &data,
                    &evidence,
                    parse_gate_target(&evidence.target),
                    None,
                )),
                budget: Some(self.budget_data(&request.budget, 0, 0, 0)),
                stop_reason: "completed".to_owned(),
                remaining_risks: risks,
                reason: "bounded analysis from current-revision compiler evidence".to_owned(),
                ..RepairData::default()
            },
        )
    }

    async fn run_candidates(
        &self,
        request: RepairRequest,
        workspace: &WorkspaceRoot,
        cancellation: CancellationToken,
        lsp: Option<&Arc<RustAnalyzerManager>>,
    ) -> RepairOutcome {
        let started = Instant::now();
        let action = match request.action {
            super::model::RepairAction::Try => RepairActionKind::Try,
            _ => RepairActionKind::Compare,
        };
        if cancellation.is_cancelled() {
            return self.cancelled(action, &request.change_id);
        }
        let compare = action == RepairActionKind::Compare;
        let id = request.change_id.clone();
        let data = match self
            .inspect_change(action, &id, workspace, &cancellation)
            .await
        {
            Ok(data) => data,
            Err(outcome) => return outcome,
        };
        let evidence = match base_evidence(&data) {
            BaseEvidence::Fail(evidence) => evidence.clone(),
            BaseEvidence::Clean => {
                return self.outcome(
                    "CLEAN",
                    "The current revision has no fresh compile failure to repair.",
                    false,
                    RepairData {
                        action: action.as_str().to_owned(),
                        usable: false,
                        change_id: Some(id),
                        base_identity: data.base_identity.clone(),
                        revision: Some(data.revision),
                        reason: "current revision already passes; no candidate was tried"
                            .to_owned(),
                        stop_reason: "noFailure".to_owned(),
                        ..RepairData::default()
                    },
                );
            }
            BaseEvidence::Missing(reason) => {
                return self.no_evidence(action.as_str().to_owned(), &id, &data, reason);
            }
        };
        if data.patches_total > u64::try_from(data.patches.len()).unwrap_or(u64::MAX) {
            return self.no_evidence(
                action.as_str().to_owned(),
                &id,
                &data,
                "the base change patch list was truncated and cannot be replayed".to_owned(),
            );
        }
        let base_patches: Vec<PatchInput> = analysis::to_patch_input(
            &data
                .patches
                .iter()
                .map(|patch| RepairPatchData {
                    file: patch.file.clone(),
                    old_string: patch.old_string.clone(),
                    new_string: patch.new_string.clone(),
                })
                .collect::<Vec<_>>(),
        );
        let base_new_files = match self
            .base_new_files(&id, &data, workspace, &cancellation)
            .await
        {
            Ok(files) => files,
            Err(reason) => {
                return self.no_evidence(action.as_str().to_owned(), &id, &data, reason);
            }
        };
        let (diagnostics, unmatched) =
            analysis::filter_diagnostics(&evidence.diagnostics, &request.diagnostic_ids);
        let files = diagnostic_files(&diagnostics);
        let sources = self.read_candidate_sources(&id, &files);
        let read = |file: &str| sources.get(file).cloned();
        let target = request
            .test_target
            .unwrap_or_else(|| parse_gate_target(&evidence.target));
        let (candidates, source_rows) = if request.candidates.is_empty() {
            self.build_candidates(&diagnostics, &evidence, &read, lsp, workspace)
                .await
        } else {
            host_candidates(&request.candidates)
        };

        let mut results: Vec<RepairCandidateData> = Vec::new();
        let mut seen_hashes = BTreeSet::new();
        let mut candidates_used = 0u32;
        let mut compiles_used = 0u32;
        let mut stop_reason = "completed";
        let elapsed_ms =
            |started: Instant| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        for (index, candidate) in candidates.iter().enumerate() {
            if !seen_hashes.insert(candidate.hash.clone()) {
                let mut result = candidate.clone();
                result.status = "duplicate".to_owned();
                result.reason =
                    "identical candidate hash was already tried in this action".to_owned();
                results.push(result);
                continue;
            }
            if cancellation.is_cancelled() {
                return self.abort(
                    action.as_str().to_owned(),
                    &id,
                    &data,
                    "CANCELLED",
                    "cancelled",
                    "repair was cancelled before the candidate could be tried".to_owned(),
                );
            }
            let budget_stop = if candidates_used >= request.budget.max_candidates {
                Some((
                    "maxCandidates",
                    "candidate budget (maxCandidates) was exhausted",
                ))
            } else if compiles_used >= request.budget.max_compiles {
                Some(("maxCompiles", "compile budget (maxCompiles) was exhausted"))
            } else if elapsed_ms(started) > request.budget.wall_time_ms {
                Some(("wallTimeMs", "wall-clock budget (wallTimeMs) was exhausted"))
            } else {
                None
            };
            if let Some((reason, detail)) = budget_stop {
                stop_reason = reason;
                for pending in &candidates[index..] {
                    let mut result = pending.clone();
                    if result.status == "candidate" {
                        result.status = "budgetExhausted".to_owned();
                        result.reason = detail.to_owned();
                    }
                    results.push(result);
                }
                break;
            }
            match self
                .run_candidate(
                    candidate,
                    &data,
                    &base_patches,
                    &base_new_files,
                    target,
                    workspace,
                    &cancellation,
                )
                .await
            {
                CandidateRun::Measured(result) => {
                    candidates_used = candidates_used.saturating_add(1);
                    compiles_used = compiles_used.saturating_add(1);
                    results.push(result);
                }
                CandidateRun::Rejected(result) => {
                    candidates_used = candidates_used.saturating_add(1);
                    results.push(result);
                }
                CandidateRun::Abort {
                    status,
                    stop_reason: abort_reason,
                    reason,
                } => {
                    return self.abort(
                        action.as_str().to_owned(),
                        &id,
                        &data,
                        status,
                        abort_reason,
                        reason,
                    );
                }
            }
        }

        let mut risks = source_rows
            .iter()
            .filter(|row| row.status != "available" && !row.reason.is_empty())
            .map(|row| format!("{} source: {}", row.kind, row.reason))
            .take(MAX_REMAINING_RISKS)
            .collect::<Vec<_>>();
        if !unmatched.is_empty() {
            risks.push(format!(
                "diagnosticIds did not match: {}",
                unmatched.join(", ")
            ));
        }
        if !results.is_empty()
            && results
                .iter()
                .all(|result| result.status == "budgetExhausted")
        {
            risks.push(
                "No candidate was measured; the budget stopped before the first compile."
                    .to_owned(),
            );
        }
        risks.truncate(MAX_REMAINING_RISKS);

        let (selected, eliminated, selection_risks) = if compare {
            analysis::select(&results, request.test_target.map(|target| target.as_str()))
        } else {
            (None, Vec::new(), Vec::new())
        };
        risks.extend(selection_risks);
        risks.truncate(MAX_REMAINING_RISKS);
        if stop_reason != "completed" {
            risks.push(format!(
                "Candidates remain unattempted because the {stop_reason} budget stop was reached."
            ));
            risks.truncate(MAX_REMAINING_RISKS);
        }
        if let Some(selected) = &selected {
            risks.extend(selected_impacts(&results, &selected.id));
            risks.truncate(MAX_REMAINING_RISKS);
        }

        let usable = results.iter().any(|result| result.status == "measured");
        let verification = selected.as_ref().map_or_else(
            || "none".to_owned(),
            |selection| selection.verification.clone(),
        );
        let measured = results
            .iter()
            .filter(|result| result.status == "measured")
            .count();
        let summary = if compare {
            format!(
                "Compared {} candidate(s); {} measured, selected {}.",
                results.len(),
                measured,
                selected
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |selection| selection.id.clone())
            )
        } else {
            format!(
                "Tried {} candidate(s); {measured} produced fresh measurement evidence.",
                results.len()
            )
        };
        self.outcome(
            action.status(),
            summary,
            false,
            RepairData {
                action: action.as_str().to_owned(),
                usable,
                change_id: Some(id),
                base_identity: data.base_identity.clone(),
                revision: Some(data.revision),
                evidence_revision: Some(evidence.revision),
                candidates: results,
                selected,
                eliminated,
                verification,
                configuration: Some(self.configuration(
                    &data,
                    &evidence,
                    target,
                    request.test_target,
                )),
                budget: Some(self.budget_data(
                    &request.budget,
                    candidates_used,
                    compiles_used,
                    elapsed_ms(started),
                )),
                stop_reason: stop_reason.to_owned(),
                remaining_risks: risks,
                reason: "measured against isolated candidate copies".to_owned(),
                ..RepairData::default()
            },
        )
    }

    fn configuration(
        &self,
        data: &ChangeData,
        evidence: &ChangeEvidenceData,
        target: GateTargetId,
        test_target: Option<GateTargetId>,
    ) -> RepairConfigurationData {
        RepairConfigurationData::from_base(target, test_target, data, evidence)
    }

    fn budget_data(
        &self,
        budget: &RepairBudget,
        candidates_used: u32,
        compiles_used: u32,
        elapsed_ms: u64,
    ) -> RepairBudgetData {
        RepairBudgetData {
            max_candidates: budget.max_candidates,
            max_compiles: budget.max_compiles,
            wall_time_ms: budget.wall_time_ms,
            candidates_used,
            compiles_used,
            elapsed_ms,
        }
    }

    fn outcome(
        &self,
        status: &'static str,
        summary: impl Into<String>,
        is_error: bool,
        data: RepairData,
    ) -> RepairOutcome {
        RepairOutcome {
            status,
            summary: summary.into(),
            is_error,
            data,
        }
    }

    fn cancelled(&self, action: RepairActionKind, id: &str) -> RepairOutcome {
        self.outcome(
            "CANCELLED",
            "No usable repair evidence was published because the action was cancelled before it started.",
            true,
            RepairData {
                action: action.as_str().to_owned(),
                usable: false,
                change_id: Some(id.to_owned()),
                reason: "repair was cancelled before it started".to_owned(),
                stop_reason: "cancelled".to_owned(),
                ..RepairData::default()
            },
        )
    }

    fn no_evidence(
        &self,
        action: String,
        id: &str,
        data: &ChangeData,
        reason: String,
    ) -> RepairOutcome {
        self.outcome(
            "NO_EVIDENCE",
            "No usable repair evidence was published.",
            true,
            RepairData {
                action,
                usable: false,
                change_id: Some(id.to_owned()),
                base_identity: data.base_identity.clone(),
                revision: Some(data.revision),
                reason: bounded(&reason, 512),
                stop_reason: "noUsableEvidence".to_owned(),
                ..RepairData::default()
            },
        )
    }

    fn abort(
        &self,
        action: String,
        id: &str,
        data: &ChangeData,
        status: &'static str,
        stop_reason: &'static str,
        reason: String,
    ) -> RepairOutcome {
        self.outcome(
            match status {
                "CANCELLED" => "cancelled",
                "TIMEOUT" => "timeout",
                "STALE" => "stale",
                _ => "inconclusive",
            },
            "No usable repair evidence was published because the action did not complete cleanly.",
            true,
            RepairData {
                action,
                usable: false,
                change_id: Some(id.to_owned()),
                base_identity: data.base_identity.clone(),
                revision: Some(data.revision),
                reason: bounded(&reason, 512),
                stop_reason: stop_reason.to_owned(),
                ..RepairData::default()
            },
        )
    }

    async fn base_new_files(
        &self,
        id: &str,
        data: &ChangeData,
        workspace: &WorkspaceRoot,
        cancellation: &CancellationToken,
    ) -> Result<Vec<NewFileInput>, String> {
        if data.new_files_total == 0 {
            return Ok(Vec::new());
        }
        if u64::try_from(data.new_files.len()).unwrap_or(u64::MAX) < data.new_files_total {
            return Err(
                "the base change new-file list was truncated and cannot be replayed".to_owned(),
            );
        }
        let request = ChangeRequest {
            action: ChangeAction::Export,
            change_id: Some(id.to_owned()),
            expected_revision: None,
            base_identity: None,
            patches: Vec::new(),
            new_files: Vec::new(),
            migration: None,
            target: GateTargetId::Check,
            options: ValidationOptions::default(),
            detail: GateDetail::Compact,
            timings: false,
        };
        let outcome = self
            .change
            .execute(request, workspace, cancellation.clone(), None)
            .await;
        if !matches!(outcome.status, "EXPORTED" | "EXPORTED_UNVERIFIED") {
            return Err(format!(
                "the base change new files could not be exported: {}",
                bounded(&outcome.data.reason, 256)
            ));
        }
        if outcome.data.new_files_content_omitted {
            return Err(
                "the base change contains new files whose content is not exportable; candidate replay is refused"
                    .to_owned(),
            );
        }
        let mut files = Vec::new();
        for file in &outcome.data.new_files {
            let content = file.content.clone().ok_or_else(|| {
                "the base change contains a new file without exportable content".to_owned()
            })?;
            files.push(NewFileInput {
                file: file.file.clone(),
                content,
            });
        }
        Ok(files)
    }

    fn read_candidate_sources(&self, id: &str, files: &[String]) -> BTreeMap<String, String> {
        let mut sources = BTreeMap::new();
        for file in files {
            if let Ok(source) = self.change.read_candidate_source(id, file) {
                sources.insert(file.clone(), source);
            }
        }
        sources
    }

    async fn build_candidates(
        &self,
        diagnostics: &[crate::change::ChangeDiagnosticData],
        evidence: &ChangeEvidenceData,
        read: &(dyn Fn(&str) -> Option<String> + Send + Sync),
        lsp: Option<&Arc<RustAnalyzerManager>>,
        workspace: &WorkspaceRoot,
    ) -> (Vec<RepairCandidateData>, Vec<RepairCandidateSourceData>) {
        let mut candidates: Vec<RepairCandidateData> = Vec::new();
        let mut sources = Vec::new();
        let mut seen = BTreeSet::new();
        let mut push = |candidate: RepairCandidateData,
                        candidates: &mut Vec<RepairCandidateData>| {
            if seen.insert(candidate.hash.clone()) {
                candidates.push(candidate);
            }
        };

        let package_count = evidence
            .suggestion_package
            .as_ref()
            .map_or(0, |package| package.patches.len());
        if let Some(package) = &evidence.suggestion_package {
            // The flattened package does not preserve individual suggestion
            // grouping, so it is replayed as one atomic candidate: applying a
            // single part of a multipart suggestion could produce broken text.
            if !package.patches.is_empty() && candidates.len() < MAX_LISTED_CANDIDATES {
                let patches: Vec<RepairPatchData> = package
                    .patches
                    .iter()
                    .map(|patch| RepairPatchData {
                        file: patch.file.clone(),
                        old_string: patch.old_string.clone(),
                        new_string: patch.new_string.clone(),
                    })
                    .collect();
                let patch_inputs = analysis::to_patch_input(&patches);
                let hash = analysis::candidate_hash(&patch_inputs);
                push(
                    RepairCandidateData::new(
                        "pkg-1".to_owned(),
                        "compilerSuggestion",
                        hash,
                        patches,
                    ),
                    &mut candidates,
                );
            }
            let reason = if package.truncated {
                "the suggestion package was truncated by the persistence bound".to_owned()
            } else {
                String::new()
            };
            sources.push(RepairCandidateSourceData {
                kind: "compilerSuggestion".to_owned(),
                status: if package_count == 0 {
                    "empty"
                } else {
                    "available"
                }
                .to_owned(),
                count: u64::try_from(package_count).unwrap_or(u64::MAX),
                reason,
            });
        } else {
            sources.push(RepairCandidateSourceData {
                kind: "compilerSuggestion".to_owned(),
                status: "empty".to_owned(),
                count: 0,
                reason: "the failure evidence carried no machine-applicable suggestion package"
                    .to_owned(),
            });
        }

        let mechanical = analysis::mechanical_candidates(diagnostics, read);
        for candidate in &mechanical {
            if candidates.len() >= MAX_LISTED_CANDIDATES {
                break;
            }
            let patches = to_repair_patches(&candidate.patches);
            let hash = analysis::candidate_hash(&candidate.patches);
            push(
                RepairCandidateData::new(
                    next_mechanical_id(&candidates, &candidate.id),
                    "mechanical",
                    hash,
                    patches,
                ),
                &mut candidates,
            );
        }
        sources.push(RepairCandidateSourceData {
            kind: "mechanical".to_owned(),
            status: if mechanical.is_empty() {
                "empty"
            } else {
                "available"
            }
            .to_owned(),
            count: u64::try_from(mechanical.len()).unwrap_or(u64::MAX),
            reason: String::new(),
        });

        match lsp {
            None => sources.push(RepairCandidateSourceData {
                kind: "raAssist".to_owned(),
                status: "unavailable".to_owned(),
                count: 0,
                reason: "rust-analyzer is not configured for this server".to_owned(),
            }),
            Some(manager) => {
                let mut assist_count = 0usize;
                let mut assist_failure = String::new();
                for diagnostic in diagnostics.iter().take(ANALYSIS_ASSIST_LIMIT) {
                    if candidates.len() >= MAX_LISTED_CANDIDATES {
                        break;
                    }
                    match ra_assist_patches(manager, workspace, diagnostic).await {
                        Ok(patches) => {
                            for (assist_index, patches) in patches.into_iter().enumerate() {
                                if candidates.len() >= MAX_LISTED_CANDIDATES {
                                    break;
                                }
                                if !verified_against_candidate(&patches, read) {
                                    continue;
                                }
                                let hash = analysis::candidate_hash(&patches);
                                push(
                                    RepairCandidateData::new(
                                        format!(
                                            "ra-{}-{}",
                                            diagnostic_digest(diagnostic),
                                            assist_index + 1
                                        ),
                                        "raAssist",
                                        hash,
                                        to_repair_patches(&patches),
                                    ),
                                    &mut candidates,
                                );
                                assist_count += 1;
                            }
                        }
                        Err(reason) => assist_failure = reason,
                    }
                }
                sources.push(RepairCandidateSourceData {
                    kind: "raAssist".to_owned(),
                    status: if assist_count > 0 {
                        "available"
                    } else {
                        "empty"
                    }
                    .to_owned(),
                    count: u64::try_from(assist_count).unwrap_or(u64::MAX),
                    reason: assist_failure,
                });
            }
        }
        (candidates, sources)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_candidate(
        &self,
        candidate: &RepairCandidateData,
        base: &ChangeData,
        base_patches: &[PatchInput],
        base_new_files: &[NewFileInput],
        target: GateTargetId,
        workspace: &WorkspaceRoot,
        cancellation: &CancellationToken,
    ) -> CandidateRun {
        let requested_target = target;
        let mut result = candidate.clone();
        let create = self
            .change
            .execute(create_request(), workspace, cancellation.clone(), None)
            .await;
        if !matches!(create.status, "CREATED") {
            if matches!(create.status, "CANCELLED" | "TIMEOUT") {
                return CandidateRun::Abort {
                    status: create.status,
                    stop_reason: if create.status == "CANCELLED" {
                        "cancelled"
                    } else {
                        "timeout"
                    },
                    reason: format!("candidate create did not complete: {}", create.data.reason),
                };
            }
            result.status = "unavailable".to_owned();
            result.reason = bounded(&create.data.reason, 320);
            return CandidateRun::Rejected(result);
        }
        let Some(candidate_id) = create.data.change_id.clone() else {
            result.status = "unavailable".to_owned();
            result.reason = "the candidate change did not publish an id".to_owned();
            return CandidateRun::Rejected(result);
        };
        let Some(candidate_base) = create.data.base_identity.clone() else {
            return CandidateRun::Abort {
                status: "INCONCLUSIVE",
                stop_reason: "inconclusive",
                reason: "the candidate change did not publish a base identity".to_owned(),
            };
        };
        if base.base_identity.as_deref() != Some(candidate_base.as_str()) {
            let _ = self.discard(&candidate_id, workspace, cancellation).await;
            return CandidateRun::Abort {
                status: "STALE",
                stop_reason: "baseMismatch",
                reason:
                    "the workspace base no longer matches the base the failure evidence was produced for; stale candidates were not tried"
                        .to_owned(),
            };
        }
        result.change_id = Some(candidate_id.clone());

        // Replay the base patches first so candidate patches that edit text
        // introduced by the original change apply to the updated candidate
        // bytes instead of overlapping the base replacement ranges.
        let mut current_revision = 0u64;
        if !base_patches.is_empty() || !base_new_files.is_empty() {
            let base_stage = self
                .change
                .execute(
                    stage_request(
                        &candidate_id,
                        &candidate_base,
                        current_revision,
                        base_patches.to_vec(),
                        base_new_files.to_vec(),
                    ),
                    workspace,
                    cancellation.clone(),
                    None,
                )
                .await;
            if !matches!(base_stage.status, "STAGED") {
                if matches!(base_stage.status, "CANCELLED" | "TIMEOUT") {
                    let _ = self.discard(&candidate_id, workspace, cancellation).await;
                    return CandidateRun::Abort {
                        status: base_stage.status,
                        stop_reason: if base_stage.status == "CANCELLED" {
                            "cancelled"
                        } else {
                            "timeout"
                        },
                        reason: format!(
                            "base patch replay did not complete: {}",
                            base_stage.data.reason
                        ),
                    };
                }
                if !base_stage.data.cleanup_warnings.is_empty() {
                    return CandidateRun::Abort {
                        status: "INCONCLUSIVE",
                        stop_reason: "cleanupFailure",
                        reason:
                            "base patch replay left cleanup warnings; no repair evidence is usable"
                                .to_owned(),
                    };
                }
                let _ = self.discard(&candidate_id, workspace, cancellation).await;
                return CandidateRun::Abort {
                    status: "STALE",
                    stop_reason: "baseMismatch",
                    reason: format!(
                        "the base change could not be replayed against the captured base: {}",
                        bounded(&base_stage.data.reason, 256)
                    ),
                };
            }
            current_revision = base_stage.data.revision;
        }

        let stage = self
            .change
            .execute(
                stage_request(
                    &candidate_id,
                    &candidate_base,
                    current_revision,
                    analysis::to_patch_input(&candidate.patches),
                    Vec::new(),
                ),
                workspace,
                cancellation.clone(),
                None,
            )
            .await;
        if !matches!(stage.status, "STAGED") {
            if matches!(stage.status, "CANCELLED" | "TIMEOUT") {
                let _ = self.discard(&candidate_id, workspace, cancellation).await;
                return CandidateRun::Abort {
                    status: stage.status,
                    stop_reason: if stage.status == "CANCELLED" {
                        "cancelled"
                    } else {
                        "timeout"
                    },
                    reason: format!("candidate stage did not complete: {}", stage.data.reason),
                };
            }
            if !stage.data.cleanup_warnings.is_empty() {
                return CandidateRun::Abort {
                    status: "INCONCLUSIVE",
                    stop_reason: "cleanupFailure",
                    reason: "candidate staging left cleanup warnings; no repair evidence is usable"
                        .to_owned(),
                };
            }
            result.status = "rejected".to_owned();
            result.reason = bounded(
                &format!(
                    "the candidate was not applied as a whole: {}",
                    stage.data.reason
                ),
                320,
            );
            let _ = self.discard(&candidate_id, workspace, cancellation).await;
            return CandidateRun::Rejected(result);
        }
        let candidate_revision = stage.data.revision;
        result.revision = Some(candidate_revision);

        let validate = self
            .change
            .execute(
                validate_request(
                    &candidate_id,
                    &candidate_base,
                    candidate_revision,
                    requested_target,
                ),
                workspace,
                cancellation.clone(),
                None,
            )
            .await;
        if !validate.data.cleanup_warnings.is_empty() {
            let _ = self.discard(&candidate_id, workspace, cancellation).await;
            return CandidateRun::Abort {
                status: "INCONCLUSIVE",
                stop_reason: "cleanupFailure",
                reason: "candidate validation left cleanup warnings; no repair evidence is usable"
                    .to_owned(),
            };
        }
        let fresh = validate
            .data
            .evidence
            .iter()
            .rev()
            .find(|row| row.revision == validate.data.revision && row.fresh && row.authoritative)
            .cloned();
        let discarded = self.discard(&candidate_id, workspace, cancellation).await;
        if !discarded {
            return CandidateRun::Abort {
                status: "INCONCLUSIVE",
                stop_reason: "cleanupFailure",
                reason: "candidate scratch cleanup did not complete; no repair evidence is usable"
                    .to_owned(),
            };
        }
        if cancellation.is_cancelled() {
            return CandidateRun::Abort {
                status: "CANCELLED",
                stop_reason: "cancelled",
                reason: "repair was cancelled during candidate validation".to_owned(),
            };
        }
        let Some(row) = fresh else {
            let status = match validate.status {
                "CANCELLED" => "CANCELLED",
                "TIMEOUT" => "TIMEOUT",
                _ => "INCONCLUSIVE",
            };
            return CandidateRun::Abort {
                status,
                stop_reason: if status == "CANCELLED" {
                    "cancelled"
                } else if status == "TIMEOUT" {
                    "timeout"
                } else {
                    "incompleteDiagnostics"
                },
                reason: format!(
                    "candidate validation did not produce clean fresh evidence: {}",
                    bounded(&validate.data.reason, 256)
                ),
            };
        };
        if row.diagnostics_omitted > 0 {
            return CandidateRun::Abort {
                status: "INCONCLUSIVE",
                stop_reason: "incompleteDiagnostics",
                reason: "candidate diagnostics were truncated; no usable repair evidence"
                    .to_owned(),
            };
        }

        result.status = "measured".to_owned();
        result.compile = row
            .stats
            .build_success
            .map(|success| if success { "compiled" } else { "failed" }.to_owned());
        result.gate = Some(RepairGateData {
            target: row.target.clone(),
            status: row.status.clone(),
            exit_code: row.exit_code,
            total_ms: row.total_ms,
            tests_executed: row.stats.tests_executed,
            build_success: row.stats.build_success,
        });
        result.diagnostics = row
            .diagnostics
            .iter()
            .take(MAX_CANDIDATE_DIAGNOSTICS)
            .cloned()
            .collect();
        result.diagnostics_total = row.diagnostics_total;
        result.diagnostics_omitted = row.diagnostics_omitted;
        result.delta = Some(analysis::diagnostic_delta(
            &base_fail_diagnostics(base),
            &row.diagnostics,
        ));
        result.changed = Some(analysis::changed_summary(&analysis::to_patch_input(
            &candidate.patches,
        )));
        let combined = {
            let mut combined = base_patches.to_vec();
            combined.extend(analysis::to_patch_input(&candidate.patches));
            combined
        };
        let report = guards::scan(base_patches, &combined);
        result.impacts = report.added;
        result.inherited_impacts = report.inherited;
        result.reason = format!(
            "measured {} at revision {} with a fresh {} gate result",
            row.status, row.revision, row.target
        );
        CandidateRun::Measured(result)
    }

    async fn discard(
        &self,
        id: &str,
        workspace: &WorkspaceRoot,
        cancellation: &CancellationToken,
    ) -> bool {
        let request = ChangeRequest {
            action: ChangeAction::Discard,
            change_id: Some(id.to_owned()),
            expected_revision: None,
            base_identity: None,
            patches: Vec::new(),
            new_files: Vec::new(),
            migration: None,
            target: GateTargetId::Check,
            options: ValidationOptions::default(),
            detail: GateDetail::Compact,
            timings: false,
        };
        let outcome = self
            .change
            .execute(request, workspace, cancellation.clone(), None)
            .await;
        outcome.data.cleanup_warnings.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairActionKind {
    Analyze,
    Try,
    Compare,
    Minimize,
}

impl RepairActionKind {
    const fn status(self) -> &'static str {
        match self {
            Self::Analyze => "ANALYZED",
            Self::Try => "TRIED",
            Self::Compare => "COMPARED",
            Self::Minimize => "INCONCLUSIVE",
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Analyze => "analyze",
            Self::Try => "try",
            Self::Compare => "compare",
            Self::Minimize => "minimize",
        }
    }
}

enum CandidateRun {
    Measured(RepairCandidateData),
    Rejected(RepairCandidateData),
    Abort {
        status: &'static str,
        stop_reason: &'static str,
        reason: String,
    },
}

enum BaseEvidence<'a> {
    Fail(&'a ChangeEvidenceData),
    Clean,
    Missing(String),
}

fn base_evidence(data: &ChangeData) -> BaseEvidence<'_> {
    if data.state != "ready" {
        return BaseEvidence::Missing(format!("the change is {}", data.state));
    }
    if data.discarded {
        return BaseEvidence::Missing("the change was discarded".to_owned());
    }
    if !data.cleanup_warnings.is_empty() {
        return BaseEvidence::Missing(
            "the change record has cleanup warnings; no usable repair evidence".to_owned(),
        );
    }
    for row in data.evidence.iter().rev() {
        if row.revision != data.revision || !row.fresh || !row.authoritative {
            continue;
        }
        if row.status == "FAIL" {
            if row.diagnostics_omitted > 0 {
                return BaseEvidence::Missing(
                    "compiler diagnostics were truncated (diagnosticsOmitted > 0); no usable repair evidence"
                        .to_owned(),
                );
            }
            return BaseEvidence::Fail(row);
        }
        if row.status == "PASS" {
            return BaseEvidence::Clean;
        }
    }
    BaseEvidence::Missing(
        "no fresh authoritative FAIL evidence exists for the current revision".to_owned(),
    )
}

fn base_fail_diagnostics(data: &ChangeData) -> Vec<crate::change::ChangeDiagnosticData> {
    match base_evidence(data) {
        BaseEvidence::Fail(row) => row.diagnostics.clone(),
        _ => Vec::new(),
    }
}

fn create_request() -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Create,
        change_id: None,
        expected_revision: None,
        base_identity: None,
        patches: Vec::new(),
        new_files: Vec::new(),
        migration: None,
        target: GateTargetId::Check,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn stage_request(
    id: &str,
    base_identity: &str,
    expected_revision: u64,
    patches: Vec<PatchInput>,
    new_files: Vec<NewFileInput>,
) -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Stage,
        change_id: Some(id.to_owned()),
        expected_revision: Some(expected_revision),
        base_identity: Some(base_identity.to_owned()),
        patches,
        new_files,
        migration: None,
        target: GateTargetId::Check,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn validate_request(
    id: &str,
    base_identity: &str,
    expected_revision: u64,
    target: GateTargetId,
) -> ChangeRequest {
    ChangeRequest {
        action: ChangeAction::Validate,
        change_id: Some(id.to_owned()),
        expected_revision: Some(expected_revision),
        base_identity: Some(base_identity.to_owned()),
        patches: Vec::new(),
        new_files: Vec::new(),
        migration: None,
        target,
        options: ValidationOptions::default(),
        detail: GateDetail::Compact,
        timings: false,
    }
}

fn host_candidates(
    inputs: &[RepairCandidateInput],
) -> (Vec<RepairCandidateData>, Vec<RepairCandidateSourceData>) {
    let mut candidates = Vec::new();
    for (index, input) in inputs.iter().take(MAX_LISTED_CANDIDATES).enumerate() {
        let patches: Vec<RepairPatchData> = input
            .patches
            .iter()
            .map(|patch| RepairPatchData {
                file: patch.file.clone(),
                old_string: patch.old_string.clone(),
                new_string: patch.new_string.clone(),
            })
            .collect();
        let hash = analysis::candidate_hash(&input.patches);
        let id = input
            .id
            .clone()
            .unwrap_or_else(|| format!("host-{}", index + 1));
        let source = input.source.clone().unwrap_or_else(|| "host".to_owned());
        candidates.push(RepairCandidateData::new(id, &source, hash, patches));
    }
    let count = u64::try_from(inputs.len()).unwrap_or(u64::MAX);
    (
        candidates,
        vec![RepairCandidateSourceData {
            kind: "host".to_owned(),
            status: if inputs.is_empty() {
                "empty"
            } else {
                "available"
            }
            .to_owned(),
            count,
            reason: String::new(),
        }],
    )
}

fn to_repair_patches(patches: &[PatchInput]) -> Vec<RepairPatchData> {
    patches
        .iter()
        .map(|patch| RepairPatchData {
            file: patch.file.clone(),
            old_string: patch.old_string.clone(),
            new_string: patch.new_string.clone(),
        })
        .collect()
}

fn next_mechanical_id(candidates: &[RepairCandidateData], requested: &str) -> String {
    let base = requested.trim_start_matches("mechanical-");
    let mut index = 1usize;
    loop {
        let id = format!("{base}-{index}");
        if !candidates.iter().any(|candidate| candidate.id == id) {
            return id;
        }
        index += 1;
    }
}

fn selected_impacts(candidates: &[RepairCandidateData], id: &str) -> Vec<String> {
    candidates
        .iter()
        .find(|candidate| candidate.id == id)
        .filter(|candidate| !candidate.impacts.is_empty())
        .map(|candidate| {
            candidate
                .impacts
                .iter()
                .take(MAX_IMPACTS)
                .map(|impact| format!("selected candidate impact: {}", impact.detail))
                .collect()
        })
        .unwrap_or_default()
}

fn diagnostic_files(diagnostics: &[crate::change::ChangeDiagnosticData]) -> Vec<String> {
    let mut files = BTreeSet::new();
    for diagnostic in diagnostics.iter().take(MAX_ANALYZED_DIAGNOSTICS) {
        if let Some(file) = diagnostic.file.as_deref() {
            if is_rust_file(file) {
                files.insert(file.to_owned());
            }
        }
    }
    files.into_iter().take(16).collect()
}

fn diagnostic_digest(diagnostic: &crate::change::ChangeDiagnosticData) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in analysis::diagnostic_id(diagnostic).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:08x}")
}

fn is_rust_file(file: &str) -> bool {
    Path::new(file)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
}

fn parse_gate_target(target: &str) -> GateTargetId {
    match target {
        "check" => GateTargetId::Check,
        "build" => GateTargetId::Build,
        "clippy" => GateTargetId::Clippy,
        "test" => GateTargetId::Test,
        "doc" => GateTargetId::Doc,
        "fmt" => GateTargetId::Fmt,
        "all" => GateTargetId::All,
        _ => GateTargetId::Check,
    }
}

fn bounded(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn verified_against_candidate(
    patches: &[PatchInput],
    read: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> bool {
    patches.iter().all(|patch| {
        read(&patch.file).is_some_and(|source| source.match_indices(&patch.old_string).count() == 1)
    })
}

#[allow(clippy::too_many_lines)]
async fn ra_assist_patches(
    manager: &Arc<RustAnalyzerManager>,
    workspace: &WorkspaceRoot,
    diagnostic: &crate::change::ChangeDiagnosticData,
) -> Result<Vec<Vec<PatchInput>>, String> {
    let file = diagnostic
        .file
        .clone()
        .filter(|file| is_rust_file(file))
        .ok_or_else(|| "the diagnostic has no Rust source path".to_owned())?;
    let line = diagnostic
        .line
        .ok_or_else(|| "the diagnostic has no source line".to_owned())?;
    let relative = PathBuf::from(&file);
    let root = workspace.authority_path().to_owned();
    let operation_line = line;
    let operation_file = file.clone();
    let patches = with_rust_document(manager, &root, &relative, move |client, uri, text| {
        Box::pin(async move {
            let line = u32::try_from(operation_line.saturating_sub(1)).unwrap_or(u32::MAX);
            let raw = request_until(
                client.as_ref(),
                "textDocument/codeAction",
                serde_json::json!({
                    "textDocument": {"uri": uri},
                    "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 0}},
                    "context": {"diagnostics": [], "only": ["quickfix"]}
                }),
                LSP_ASSIST_TIMEOUT,
                Value::is_array,
            )
            .await?;
            Ok(actions_to_patches(&raw, &uri, &text, &operation_file))
        })
    })
    .await
    .map_err(|error| error.to_string())?;
    if patches.is_empty() {
        return Err("rust-analyzer returned no quick-fix edit for this diagnostic".to_owned());
    }
    Ok(patches)
}

fn actions_to_patches(raw: &Value, uri: &str, text: &str, file: &str) -> Vec<Vec<PatchInput>> {
    let Some(actions) = raw.as_array() else {
        return Vec::new();
    };
    let mut patches = Vec::new();
    for action in actions.iter().take(8) {
        let Some(action) = action.as_object() else {
            continue;
        };
        let Some(edit) = action.get("edit") else {
            continue;
        };
        let mut edits = Vec::new();
        if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
            if let Some(list) = changes.get(uri).and_then(Value::as_array) {
                edits.extend(
                    list.iter()
                        .filter_map(|edit| lsp_edit_to_patch(edit, text, file)),
                );
            }
        }
        if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
            for change in document_changes {
                let change_uri = change
                    .get("textDocument")
                    .and_then(|document| document.get("uri"))
                    .and_then(Value::as_str);
                if change_uri != Some(uri) {
                    continue;
                }
                if let Some(list) = change.get("edits").and_then(Value::as_array) {
                    edits.extend(
                        list.iter()
                            .filter_map(|edit| lsp_edit_to_patch(edit, text, file)),
                    );
                }
            }
        }
        if !edits.is_empty() {
            patches.push(edits);
        }
    }
    patches
}

fn lsp_edit_to_patch(edit: &Value, text: &str, file: &str) -> Option<PatchInput> {
    let range = edit.get("range")?;
    let start = position_offset(
        text,
        range.get("start")?.get("line")?.as_u64()?,
        range.get("start")?.get("character")?.as_u64()?,
    )?;
    let end = position_offset(
        text,
        range.get("end")?.get("line")?.as_u64()?,
        range.get("end")?.get("character")?.as_u64()?,
    )?;
    let replacement = edit.get("newText")?.as_str()?;
    if start >= end && replacement.is_empty() {
        return None;
    }
    // Expand to whole lines so the resulting patch is stable against byte
    // coordinate conventions and can be verified against the candidate bytes.
    let lines = line_offsets(text);
    let start_line = lines
        .partition_point(|offset| *offset <= start)
        .saturating_sub(1);
    let end_line = lines
        .partition_point(|offset| *offset <= end)
        .saturating_sub(1);
    let excerpt_start = *lines.get(start_line)?;
    let excerpt_end = lines
        .get(end_line.saturating_add(1))
        .copied()
        .unwrap_or(text.len());
    let old_string = text.get(excerpt_start..excerpt_end)?.to_owned();
    if old_string.is_empty() {
        return None;
    }
    let local_start = start - excerpt_start;
    let local_end = end - excerpt_start;
    if local_start > old_string.len() || local_end > old_string.len() {
        return None;
    }
    let new_string = format!(
        "{}{}{}",
        &old_string[..local_start],
        replacement,
        &old_string[local_end..]
    );
    if new_string == old_string {
        return None;
    }
    Some(PatchInput {
        file: file.to_owned(),
        old_string,
        new_string,
    })
}

fn position_offset(content: &str, line: u64, character: u64) -> Option<usize> {
    let starts = line_offsets(content);
    let index = usize::try_from(line).ok()?;
    let line_start = *starts.get(index)?;
    let line_end = starts.get(index + 1).copied().unwrap_or(content.len());
    let line_text = content
        .get(line_start..line_end)?
        .trim_end_matches('\n')
        .trim_end_matches('\r');
    let mut utf16 = 0u64;
    for (offset, value) in line_text.char_indices() {
        if utf16 == character {
            return Some(line_start + offset);
        }
        utf16 = utf16.saturating_add(u64::try_from(value.len_utf16()).unwrap_or(1));
        if utf16 > character {
            return None;
        }
    }
    (utf16 == character).then_some(line_start + line_text.len())
}

fn line_offsets(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (offset, character) in content.char_indices() {
        if character == '\n' {
            starts.push(offset.saturating_add(1));
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{
        ChangeDiagnosticData, ChangeSuggestionPackageData, ChangeSuggestionPatchData,
    };
    use crate::diagnostics::EvidenceStats;

    fn evidence_row(
        revision: u64,
        status: &str,
        fresh: bool,
        diagnostics_omitted: u64,
    ) -> ChangeEvidenceData {
        ChangeEvidenceData {
            revision,
            target: "check".to_owned(),
            command: "cargo check".to_owned(),
            status: status.to_owned(),
            exit_code: Some(1),
            first_diagnostic_ms: None,
            total_ms: 1,
            fresh,
            authoritative: fresh,
            diagnostics: vec![ChangeDiagnosticData {
                code: Some("E0382".to_owned()),
                level: "error".to_owned(),
                file: Some("src/lib.rs".to_owned()),
                line: Some(4),
                message: "borrow of moved value: `text`".to_owned(),
            }],
            diagnostics_total: 1,
            diagnostics_omitted,
            suggestion_package: Some(ChangeSuggestionPackageData {
                patches: vec![ChangeSuggestionPatchData {
                    file: "src/lib.rs".to_owned(),
                    old_string: "text".to_owned(),
                    new_string: "text.clone()".to_owned(),
                }],
                skipped: Vec::new(),
                unsupported: 0,
                patches_total: 1,
                skipped_total: 0,
                truncated: false,
            }),
            stats: EvidenceStats::default(),
        }
    }

    fn change_data(revision: u64, rows: Vec<ChangeEvidenceData>) -> ChangeData {
        ChangeData {
            action: "validate".to_owned(),
            state: "ready".to_owned(),
            revision,
            evidence: rows,
            ..ChangeData::default()
        }
    }

    #[test]
    fn base_evidence_requires_a_fresh_authoritative_complete_fail_row() {
        let fail = change_data(1, vec![evidence_row(1, "FAIL", true, 0)]);
        assert!(matches!(base_evidence(&fail), BaseEvidence::Fail(_)));

        let stale = change_data(1, vec![evidence_row(1, "FAIL", false, 0)]);
        assert!(matches!(base_evidence(&stale), BaseEvidence::Missing(_)));

        let incomplete = change_data(1, vec![evidence_row(1, "FAIL", true, 2)]);
        assert!(matches!(
            base_evidence(&incomplete),
            BaseEvidence::Missing(reason) if reason.contains("truncated")
        ));

        let clean = change_data(1, vec![evidence_row(1, "PASS", true, 0)]);
        assert!(matches!(base_evidence(&clean), BaseEvidence::Clean));

        let historical = change_data(2, vec![evidence_row(1, "FAIL", true, 0)]);
        assert!(matches!(
            base_evidence(&historical),
            BaseEvidence::Missing(_)
        ));

        let mut cleanup = change_data(1, vec![evidence_row(1, "FAIL", true, 0)]);
        cleanup.cleanup_warnings = vec!["warn".to_owned()];
        assert!(matches!(base_evidence(&cleanup), BaseEvidence::Missing(_)));
    }

    #[test]
    fn gate_target_parsing_is_fail_closed() {
        assert_eq!(parse_gate_target("test"), GateTargetId::Test);
        assert_eq!(parse_gate_target("all"), GateTargetId::All);
        assert_eq!(parse_gate_target("unexpected"), GateTargetId::Check);
    }
}
