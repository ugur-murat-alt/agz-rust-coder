//! Bounded line-oriented Cargo evidence, independent of the human log tail.
use super::{CargoBuildTelemetry, CargoOutput, Diagnostic, DiagnosticLevel, parse_cargo_output};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceStats {
    /// Parsed diagnostic records, including repetitions (not a unique-error count).
    pub diagnostic_records: u64,
    pub duplicates: u64,
    pub omitted_records: u64,
    pub malformed_lines: u64,
    pub oversized_lines: u64,
    pub build_finished: bool,
    pub build_success: Option<bool>,
    /// Executed libtest cases when its standard summary is observed.
    pub tests_executed: Option<u64>,
}

#[derive(Debug)]
pub struct CargoStream {
    line: Vec<u8>,
    discard_line: bool,
    max_line: usize,
    max_diagnostics: usize,
    max_evidence_bytes: usize,
    evidence_bytes: usize,
    diagnostics: Vec<Diagnostic>,
    keys: BTreeSet<String>,
    build: CargoBuildTelemetry,
    stats: EvidenceStats,
}

impl Default for CargoStream {
    fn default() -> Self {
        Self::new(256 * 1024, 128, 1024 * 1024)
    }
}

impl CargoStream {
    pub fn new(max_line: usize, max_diagnostics: usize, max_evidence_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            discard_line: false,
            max_line,
            max_diagnostics,
            max_evidence_bytes,
            evidence_bytes: 0,
            diagnostics: Vec::new(),
            keys: BTreeSet::new(),
            build: CargoBuildTelemetry {
                total_units: 0,
                fresh_units: 0,
                rebuilt_units: 0,
                build_scripts: 0,
                linked_units: 0,
            },
            stats: EvidenceStats::default(),
        }
    }

    /// Returns true once a complete diagnostic has been observed in this chunk.
    pub fn push(&mut self, mut bytes: &[u8]) -> bool {
        let before = self.stats.diagnostic_records;
        while !bytes.is_empty() {
            let end = bytes.iter().position(|byte| *byte == b'\n');
            let segment = &bytes[..end.unwrap_or(bytes.len())];
            if !self.discard_line {
                if self.line.len().saturating_add(segment.len()) > self.max_line {
                    self.line.clear();
                    self.discard_line = true;
                    self.stats.oversized_lines = self.stats.oversized_lines.saturating_add(1);
                } else {
                    self.line.extend_from_slice(segment);
                }
            }
            if let Some(end) = end {
                self.consume_line();
                self.discard_line = false;
                bytes = &bytes[end + 1..];
            } else {
                break;
            }
        }
        self.stats.diagnostic_records > before
    }

    /// Provisional, bounded text only. Never a completed validation result.
    pub fn first_summary(&self) -> Option<String> {
        self.diagnostics.first().map(|d| {
            format!(
                "UNTRUSTED provisional compiler evidence: {}: {}",
                d.code.as_deref().unwrap_or(d.level.as_str()),
                d.message.chars().take(256).collect::<String>()
            )
        })
    }

    pub fn finish(mut self) -> (CargoOutput, EvidenceStats) {
        self.consume_line();
        let truncated = self.stats.omitted_records > 0
            || self.stats.malformed_lines > 0
            || self.stats.oversized_lines > 0;
        (
            CargoOutput {
                diagnostics: self.diagnostics,
                build: (!self.build.is_empty()).then_some(self.build),
                untrusted_data: true,
                truncated,
            },
            self.stats,
        )
    }

    fn consume_line(&mut self) {
        if self.discard_line {
            return;
        }
        let bytes = std::mem::take(&mut self.line);
        let Ok(line) = std::str::from_utf8(&bytes) else {
            self.stats.malformed_lines = self.stats.malformed_lines.saturating_add(1);
            return;
        };
        if let Some(summary) = line
            .strip_prefix("test result: ")
            .and_then(|s| s.split_once(". ").map(|(_, s)| s))
        {
            let mut fields = summary.split(';');
            let passed = fields
                .next()
                .and_then(|s| s.trim().strip_suffix(" passed"))
                .and_then(|s| s.parse::<u64>().ok());
            let failed = fields
                .next()
                .and_then(|s| s.trim().strip_suffix(" failed"))
                .and_then(|s| s.parse::<u64>().ok());
            if let (Some(passed), Some(failed)) = (passed, failed) {
                self.stats.tests_executed = Some(
                    self.stats
                        .tests_executed
                        .unwrap_or(0)
                        .saturating_add(passed)
                        .saturating_add(failed),
                );
            }
        }
        if line.trim_start().starts_with('{') {
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(message) => {
                    if message["reason"] == "build-finished" {
                        self.stats.build_finished = message["success"].is_boolean();
                        self.stats.build_success = message["success"].as_bool();
                    }
                    if !super::parser::valid_known_message(line, &message) {
                        self.stats.malformed_lines = self.stats.malformed_lines.saturating_add(1);
                        return;
                    }
                }
                Err(_) => {
                    self.stats.malformed_lines = self.stats.malformed_lines.saturating_add(1);
                    return;
                }
            }
        }
        let parsed = parse_cargo_output(line);
        if let Some(build) = parsed.build {
            self.build.total_units = self.build.total_units.saturating_add(build.total_units);
            self.build.fresh_units = self.build.fresh_units.saturating_add(build.fresh_units);
            self.build.rebuilt_units = self.build.rebuilt_units.saturating_add(build.rebuilt_units);
            self.build.build_scripts = self.build.build_scripts.saturating_add(build.build_scripts);
            self.build.linked_units = self.build.linked_units.saturating_add(build.linked_units);
        }
        for diagnostic in parsed.diagnostics {
            self.retain(diagnostic);
        }
    }

    fn key(diagnostic: &Diagnostic) -> String {
        // Include compilation-unit provenance: equal spans in different feature/target
        // compilations must not be silently collapsed.
        format!(
            "{:?}:{:?}:{}",
            diagnostic.package_id,
            diagnostic.target_name,
            super::root_key(diagnostic)
        )
    }

    fn retain(&mut self, diagnostic: Diagnostic) {
        self.stats.diagnostic_records = self.stats.diagnostic_records.saturating_add(1);
        let key = Self::key(&diagnostic);
        if self.keys.contains(&key) {
            self.stats.duplicates = self.stats.duplicates.saturating_add(1);
            return;
        }
        let size = serde_json::to_vec(&diagnostic).map_or(usize::MAX, |bytes| bytes.len());
        if size > self.max_evidence_bytes || self.max_diagnostics == 0 {
            self.stats.omitted_records = self.stats.omitted_records.saturating_add(1);
            return;
        }
        while self.diagnostics.len() >= self.max_diagnostics
            || self.evidence_bytes.saturating_add(size) > self.max_evidence_bytes
        {
            if diagnostic.level == DiagnosticLevel::Error
                && let Some(index) = self
                    .diagnostics
                    .iter()
                    .rposition(|d| d.level == DiagnosticLevel::Warning)
            {
                let removed = self.diagnostics.remove(index);
                self.keys.remove(&Self::key(&removed));
                self.evidence_bytes = self
                    .evidence_bytes
                    .saturating_sub(serde_json::to_vec(&removed).map_or(0, |bytes| bytes.len()));
                self.stats.omitted_records = self.stats.omitted_records.saturating_add(1);
            } else {
                self.stats.omitted_records = self.stats.omitted_records.saturating_add(1);
                return;
            }
        }
        self.evidence_bytes += size;
        self.keys.insert(key);
        self.diagnostics.push(diagnostic);
    }
}
