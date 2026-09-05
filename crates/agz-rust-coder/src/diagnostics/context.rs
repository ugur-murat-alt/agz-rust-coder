//! Authorized, bounded context for a compiler diagnostic; never generated advice.
use super::Diagnostic;
use crate::workspace::{AuthorizedRoot, WorkspaceSnapshot};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedDependency {
    pub name: String,
    pub version: String,
    pub package_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticContext {
    pub diagnostic_index: usize,
    pub file: Option<String>,
    pub line_start: usize,
    pub excerpt: String,
    pub source_hash: Option<String>,
    /// `input-identity-matched` means pre/post identities agree, not an atomic filesystem snapshot.
    pub freshness: String,
    pub package_id: Option<String>,
    pub target_name: Option<String>,
    pub dependencies: Vec<ResolvedDependency>,
    pub dependencies_omitted: usize,
    pub reason: Option<String>,
    pub untrusted_data: bool,
}

pub(crate) fn diagnostic_contexts(
    root: &AuthorizedRoot,
    snapshot: &WorkspaceSnapshot,
    diagnostics: &[Diagnostic],
) -> Vec<DiagnosticContext> {
    const MAX_FILE: u64 = 1024 * 1024;
    const MAX_FILES: usize = 4;
    let mut sources = BTreeMap::<String, Result<String, String>>::new();
    diagnostics
        .iter()
        .take(24)
        .enumerate()
        .map(|(index, diagnostic)| {
            let mut context = DiagnosticContext {
                diagnostic_index: index,
                file: diagnostic.file.clone(),
                line_start: 0,
                excerpt: String::new(),
                source_hash: None,
                freshness: "unverified".into(),
                package_id: diagnostic.package_id.clone(),
                target_name: diagnostic.target_name.clone(),
                dependencies: Vec::new(),
                dependencies_omitted: 0,
                reason: None,
                untrusted_data: true,
            };
            if let Some(node) = snapshot.metadata.resolve.as_ref().and_then(|resolve| {
                resolve
                    .nodes
                    .iter()
                    .find(|node| Some(node.id.repr.as_str()) == diagnostic.package_id.as_deref())
            }) {
                let mut dependencies = node
                    .dependencies
                    .iter()
                    .filter_map(|id| {
                        snapshot
                            .metadata
                            .packages
                            .iter()
                            .find(|package| package.id == *id)
                    })
                    .collect::<Vec<_>>();
                dependencies.sort_by(|a, b| a.id.cmp(&b.id));
                context.dependencies_omitted = dependencies.len().saturating_sub(16);
                context.dependencies = dependencies
                    .into_iter()
                    .take(16)
                    .map(|package| ResolvedDependency {
                        name: package.name.to_string(),
                        version: package.version.to_string(),
                        package_id: package.id.to_string(),
                    })
                    .collect();
            }
            let Some(file) = diagnostic.file.as_deref() else {
                context.reason = Some("compiler diagnostic has no source file".into());
                return context;
            };
            if !sources.contains_key(file) && sources.len() < MAX_FILES {
                let path = if Path::new(file).is_absolute() {
                    Path::new(file).to_owned()
                } else {
                    root.path().join(file)
                };
                let source = root
                    .read_file(&path, MAX_FILE)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| {
                        String::from_utf8(bytes).map_err(|_| "source is not UTF-8".into())
                    });
                sources.insert(file.into(), source);
            }
            match sources.get(file) {
                Some(Ok(source)) => {
                    let Some(line) = diagnostic.line.filter(|line| *line > 0) else {
                        context.reason = Some("compiler diagnostic has no valid line".into());
                        return context;
                    };
                    context.source_hash = Some(format!("{:x}", Sha256::digest(source.as_bytes())));
                    let first = line.saturating_sub(3).max(1);
                    context.line_start = first;
                    context.excerpt = source
                        .lines()
                        .enumerate()
                        .skip(first - 1)
                        .take(7)
                        .map(|(index, text)| {
                            format!(
                                "{}: {}",
                                index + 1,
                                text.chars().take(240).collect::<String>()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if context.excerpt.is_empty() {
                        context.reason = Some("compiler range is outside source".into());
                    }
                }
                Some(Err(reason)) => context.reason = Some(reason.clone()),
                None => context.reason = Some("source context file budget reached".into()),
            }
            context
        })
        .collect()
}
