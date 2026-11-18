//! Exact source compatibility for native summary and role workflows.

use super::{
    Kind, Record, Scope, ScopedArtifact, State, WRITE_BYTES, WRITE_ROWS, corrupt, workspace,
};
use crate::{
    CanonicalSearchDocument, NativeDatabase, NewAgentArtifact, StorageError, SymbolInput,
    artifacts::{
        PendingModelSummaryQuery, PendingSummarySymbol, StructuralSymbolSummarySaveInput,
        SummaryCandidatePolicy, SummaryCoverageStats, SymbolRoleSaveInput, SymbolSummarySaveInput,
        validate_body, validate_limit, validate_model,
    },
    retrieval::native::visit_symbols,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use codevetra_domain::{ContentDigest, DocumentKind, ProjectId, SymbolId, Visibility};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

struct SummarySelection<'a> {
    model: Option<&'a str>,
    limit: u16,
    policy: &'a SummaryCandidatePolicy,
}

#[derive(Clone, Copy)]
pub(crate) struct SummaryReader<'a, 'view, 'store> {
    transaction: &'a StateTransaction<'store>,
    graph: &'a GraphSnapshot<'view, 'store>,
}

impl<'a, 'view, 'store> SummaryReader<'a, 'view, 'store> {
    pub(crate) const fn new(
        transaction: &'a StateTransaction<'store>,
        graph: &'a GraphSnapshot<'view, 'store>,
    ) -> Self {
        Self { transaction, graph }
    }

    pub(crate) const fn transaction(&self) -> &'a StateTransaction<'store> {
        self.transaction
    }

    pub(crate) const fn graph(&self) -> &'a GraphSnapshot<'view, 'store> {
        self.graph
    }
}

impl NativeDatabase {
    /// Publish a summary only if its echoed structural digest remains current.
    /// # Errors
    /// Rejects invalid fields, stale source, or atomic native write failure.
    pub async fn save_symbol_summary(
        &self,
        input: SymbolSummarySaveInput<'_>,
    ) -> Result<Record, StorageError> {
        self.require_project(input.project_id)?;
        validate_body(input.summary, State::Complete)?;
        validate_model(input.model)?;
        let (id, digest, body, model) = (
            input.symbol_id.clone(),
            input.source_digest.clone(),
            input.summary.to_owned(),
            input.model.to_owned(),
        );
        self.state(
            crate::rocks::StateAccess::write(None, WRITE_BYTES, WRITE_ROWS),
            move |transaction, budget| {
                let input = SymbolArtifact {
                    id: &id,
                    expected: Some(&digest),
                    kind: Kind::Summary,
                    body,
                    metadata: serde_json::json!({"model": model}),
                }
                .prepare(transaction, budget)?
                .ok_or(RocksError::GenerationChanged)?;
                let generation = input.generation_id.clone().ok_or(corrupt())?;
                let record = input.write(transaction, budget, true)?;
                transaction.delete(
                    StateNamespace::Derived,
                    &crate::summary_priority::native::key(&generation, &id),
                )?;
                Ok(record)
            },
        )
        .await
    }

    /// Publish a structural fallback while preserving a current judged summary.
    /// # Errors
    /// Rejects invalid fields, malformed artifacts, or native transaction failure.
    pub async fn save_structural_symbol_summary(
        &self,
        input: StructuralSymbolSummarySaveInput<'_>,
    ) -> Result<Option<Record>, StorageError> {
        self.require_project(input.project_id)?;
        validate_body(input.summary, State::Complete)?;
        let (id, digest, body) = (
            input.symbol_id.clone(),
            input.source_digest.clone(),
            input.summary.to_owned(),
        );
        self.state(crate::rocks::StateAccess::write(None, WRITE_BYTES, WRITE_ROWS),  move |transaction, budget| {
            let Some(input) = SymbolArtifact { id: &id, expected: Some(&digest), kind: Kind::Summary, body, metadata: serde_json::json!({"model":"structural:v2", "generationMode":"structural_rule"}) }.prepare(transaction, budget)?
            else {
                return Ok(None);
            };
            if let Some(existing) = (ScopedArtifact { kind: Kind::Summary, scope: Scope::Symbol, key: id.as_str() }).read(transaction, budget)? && existing.source_digest.as_deref() == Some(digest.as_str())
                && !existing
                    .metadata
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .starts_with("structural:")
            {
                return Ok(None);
            }
            input.write(transaction, budget, true).map(Some)
        })
        .await
    }

    /// Save a role with the current symbol digest in the same transaction.
    /// # Errors
    /// Rejects invalid role/provenance, absent current source, or native write failure.
    pub async fn save_symbol_role(
        &self,
        input: SymbolRoleSaveInput<'_>,
    ) -> Result<Record, StorageError> {
        let generation = input.generation_id.cloned();
        self.save_role_batch(generation, [input])
            .await?
            .pop()
            .ok_or(StorageError::InvalidInput {
                field: "role_batch",
            })
    }

    /// Count default-policy eligible and digest-compatible current summaries.
    /// # Errors
    /// Rejects project mismatch, malformed artifacts, or exhausted bounded reads.
    pub async fn current_summary_coverage(
        &self,
        project: &ProjectId,
    ) -> Result<SummaryCoverageStats, StorageError> {
        self.current_summary_coverage_with_policy(project, &SummaryCandidatePolicy::default())
            .await
    }

    /// Count summaries under the exact configured body and documentation policy.
    /// # Errors
    /// Rejects project mismatch, corrupt source, or bounded workspace exhaustion.
    pub async fn current_summary_coverage_with_policy(
        &self,
        project: &ProjectId,
        policy: &SummaryCandidatePolicy,
    ) -> Result<SummaryCoverageStats, StorageError> {
        self.require_project(project)?;
        let policy = policy.clone();
        self.state(
            crate::rocks::StateAccess::read_only(None),
            move |transaction, budget| {
                let mut output = SummaryCoverageStats {
                    eligible_symbols: 0,
                    summarized_symbols: 0,
                    pending_symbols: 0,
                    by_model: BTreeMap::new(),
                };
                if transaction.current()?.is_none() {
                    return Ok(output);
                }
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let lengths = documentation_lengths(&graph, budget)?;
                visit_symbols(&graph, budget, |symbol, budget| {
                    if !eligible(&symbol, &policy, &lengths) {
                        return Ok(());
                    }
                    output.eligible_symbols += 1;
                    if let Some(summary) = current_symbol_summary(transaction, &symbol, budget)? {
                        output.summarized_symbols += 1;
                        let model = summary
                            .metadata
                            .get("model")
                            .and_then(serde_json::Value::as_str)
                            .filter(|value| !value.is_empty())
                            .unwrap_or("unknown")
                            .to_owned();
                        *output.by_model.entry(model).or_default() += 1;
                        if output.by_model.len() > 8192 {
                            return Err(workspace());
                        }
                    }
                    Ok(())
                })?;
                output.pending_symbols = output.eligible_symbols - output.summarized_symbols;
                Ok(output)
            },
        )
        .await
    }

    /// Select a bounded default-policy backlog before hydrating source text.
    /// # Errors
    /// Rejects invalid limits, project mismatch, or bounded native read failure.
    pub async fn pending_symbol_summaries(
        &self,
        project: &ProjectId,
        limit: u16,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        self.pending_symbol_summaries_with_policy(
            project,
            limit,
            &SummaryCandidatePolicy::default(),
        )
        .await
    }

    /// Select missing judged summaries under the configured candidate policy.
    /// # Errors
    /// Rejects invalid limits, project mismatch, or bounded native read failure.
    pub async fn pending_symbol_summaries_with_policy(
        &self,
        project: &ProjectId,
        limit: u16,
        policy: &SummaryCandidatePolicy,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        self.pending_summaries(
            project,
            SummarySelection {
                model: None,
                limit,
                policy,
            },
        )
        .await
    }

    /// Select candidates without this exact model and structural digest.
    /// # Errors
    /// Rejects invalid model/limits, project mismatch, or native read failure.
    pub async fn pending_symbol_summaries_for_model_with_policy(
        &self,
        request: PendingModelSummaryQuery<'_>,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        validate_model(request.model)?;
        self.pending_summaries(
            request.project_id,
            SummarySelection {
                model: Some(request.model),
                limit: request.limit,
                policy: request.policy,
            },
        )
        .await
    }

    async fn pending_summaries(
        &self,
        project: &ProjectId,
        selection: SummarySelection<'_>,
    ) -> Result<Vec<PendingSummarySymbol>, StorageError> {
        let SummarySelection {
            model,
            limit,
            policy,
        } = selection;
        self.require_project(project)?;
        validate_limit(limit, super::super::MAX_SUMMARY_BATCH)?;
        let (model, policy) = (model.map(str::to_owned), policy.clone());
        self.state(
            crate::rocks::StateAccess::read_only(None),
            move |transaction, budget| {
                let Some(current) = transaction.current()? else {
                    return Ok(Vec::new());
                };
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let lengths = documentation_lengths(&graph, budget)?;
                let mut selected =
                    crate::retrieval::native::RankedRows::new(usize::from(limit), candidate_order);
                visit_symbols(&graph, budget, |symbol, budget| {
                    if !eligible(&symbol, &policy, &lengths) {
                        return Ok(());
                    }
                    if let Some(record) = current_symbol_summary(transaction, &symbol, budget)? {
                        let producer = record
                            .metadata
                            .get("model")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        if model.as_ref().map_or_else(
                            || !producer.starts_with("structural:"),
                            |model| producer == model,
                        ) {
                            return Ok(());
                        }
                    }
                    let priority = crate::summary_priority::native::QueueReader::new(
                        transaction,
                        &current.generation_id,
                    )
                    .read(&symbol.symbol_id, budget)?;
                    let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                    selected.insert(Candidate {
                        symbol,
                        path: file.normalized_path,
                        language: file.language,
                        priority,
                    });
                    if selected.iter().map(candidate_bytes).sum::<usize>() > 32 * 1_048_576 {
                        return Err(workspace());
                    }
                    Ok(())
                })?;
                hydrate_pending(&graph, budget, selected.into_vec())
            },
        )
        .await
    }
}

pub(super) struct SymbolArtifact<'a> {
    pub(super) id: &'a SymbolId,
    pub(super) expected: Option<&'a ContentDigest>,
    pub(super) kind: Kind,
    pub(super) body: String,
    pub(super) metadata: serde_json::Value,
}

impl SymbolArtifact<'_> {
    pub(super) fn prepare(
        self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
    ) -> Result<Option<NewAgentArtifact>, RocksError> {
        let Self {
            id,
            expected,
            kind,
            body,
            metadata,
        } = self;
        let Some(current) = transaction.current()? else {
            return Ok(None);
        };
        let snapshot = transaction.graph_snapshot()?;
        let graph = snapshot.graph(budget)?;
        let Some(symbol) = graph.symbol(id, budget)? else {
            return Ok(None);
        };
        if expected.is_some_and(|digest| digest != &symbol.structural_digest) {
            return Ok(None);
        }
        Ok(Some(NewAgentArtifact {
            kind,
            scope: Scope::Symbol,
            scope_key: id.to_string(),
            body,
            metadata,
            generation_id: Some(current.generation_id),
            source_digest: Some(symbol.structural_digest),
            state: State::Complete,
        }))
    }
}

pub(crate) fn current_symbol_summary(
    transaction: &StateTransaction<'_>,
    symbol: &SymbolInput,
    budget: &mut ReadBudget,
) -> Result<Option<Record>, RocksError> {
    Ok((ScopedArtifact {
        kind: Kind::Summary,
        scope: Scope::Symbol,
        key: symbol.symbol_id.as_str(),
    })
    .read(transaction, budget)?
    .filter(|record| {
        record.state == State::Complete
            && record.source_digest.as_deref() == Some(symbol.structural_digest.as_str())
    }))
}

impl SummaryReader<'_, '_, '_> {
    pub(crate) fn embedding_summary(
        &self,
        document: &CanonicalSearchDocument,
        budget: &mut ReadBudget,
    ) -> Result<String, RocksError> {
        let Self { transaction, graph } = *self;
        let generation = graph.generation_id();

        if let Some(id) = &document.symbol_id
            && !(ScopedArtifact {
                kind: Kind::Summary,
                scope: Scope::Symbol,
                key: id.as_str(),
            })
            .present(transaction, budget)?
            && !(ScopedArtifact {
                kind: Kind::Summary,
                scope: Scope::File,
                key: &document.path,
            })
            .present(transaction, budget)?
        {
            // Neither scope can contribute text in this exact state snapshot. An
            // unrelated symbol row need not be hydrated to prove an empty summary.
            return Ok(String::new());
        }
        if let Some(id) = &document.symbol_id
            && let Some(symbol) = graph.symbol(id, budget)?
            && symbol.symbol_kind != "file"
        {
            return Ok(current_symbol_summary(transaction, &symbol, budget)?
                .filter(|summary| {
                    !summary
                        .metadata
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .starts_with("neighbor:")
                })
                .map_or_else(String::new, |summary| summary.body));
        }
        Ok((ScopedArtifact {
            kind: Kind::Summary,
            scope: Scope::File,
            key: &document.path,
        })
        .read(transaction, budget)?
        .filter(|summary| {
            summary.state == State::Complete
                && summary.generation_id.as_deref() == Some(generation.as_str())
        })
        .map_or_else(String::new, |summary| summary.body))
    }
}

impl ScopedArtifact<'_> {
    fn present(
        &self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
    ) -> Result<bool, RocksError> {
        transaction
            .get(
                super::NAMESPACE,
                super::scope_key(self.kind, self.scope, self.key).as_bytes(),
                budget,
            )
            .map(|value| value.is_some())
    }
}

pub(crate) fn documentation_lengths(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<SymbolId, usize>, RocksError> {
    let mut output = BTreeMap::new();
    visit_documents(graph, budget, |document, _| {
        record_documentation_length(&mut output, document)
    })?;
    Ok(output)
}

fn record_documentation_length(
    output: &mut BTreeMap<SymbolId, usize>,
    document: CanonicalSearchDocument,
) -> Result<(), RocksError> {
    if document.kind == DocumentKind::Symbol
        && let Some(id) = document.symbol_id
    {
        // Absent documentation is already interpreted as zero by eligibility.
        // Later documents for the same symbol still replace earlier lengths.
        if document.natural_text.is_empty() {
            output.remove(&id);
        } else {
            output.insert(id, document.natural_text.chars().count());
        }
        if output.len() > 131_072 {
            return Err(workspace());
        }
    }
    Ok(())
}

pub(crate) fn eligible(
    symbol: &SymbolInput,
    policy: &SummaryCandidatePolicy,
    lengths: &BTreeMap<SymbolId, usize>,
) -> bool {
    summary_kind(&symbol.symbol_kind)
        && symbol
            .end_line
            .saturating_sub(symbol.start_line)
            .saturating_add(1)
            >= policy
                .minimum_body_lines_by_kind
                .get(&symbol.symbol_kind)
                .copied()
                .unwrap_or(policy.minimum_body_lines)
        && lengths.get(&symbol.symbol_id).copied().unwrap_or(0)
            < policy.existing_docstring_char_threshold as usize
}
pub(crate) fn summary_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class"
            | "function"
            | "method"
            | "interface"
            | "struct"
            | "trait"
            | "protocol"
            | "enum"
            | "type_alias"
            | "component"
            | "route"
    )
}

pub(crate) fn visit_documents(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(CanonicalSearchDocument, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let mut cursor = None;
    loop {
        let (documents, next) = graph.documents(cursor.as_ref(), 64, budget)?;
        for document in documents {
            budget.checkpoint()?;
            visit(document, budget)?;
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

struct Candidate {
    symbol: SymbolInput,
    path: String,
    language: String,
    priority: Option<crate::summary_priority::native::Entry>,
}
fn candidate_order(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.priority
        .is_some()
        .cmp(&a.priority.is_some())
        .then_with(|| match (&a.priority, &b.priority) {
            (Some(a), Some(b)) => crate::native_database::timestamp_order(&b.enqueued, &a.enqueued)
                .then(b.requests.cmp(&a.requests)),
            _ => std::cmp::Ordering::Equal,
        })
        .then(b.symbol.export.exported.cmp(&a.symbol.export.exported))
        .then(
            (b.symbol.visibility == Some(Visibility::Public))
                .cmp(&(a.symbol.visibility == Some(Visibility::Public))),
        )
        .then(b.symbol.pagerank_ppb.cmp(&a.symbol.pagerank_ppb))
        .then(a.path.cmp(&b.path))
        .then(a.symbol.start_line.cmp(&b.symbol.start_line))
        .then(a.symbol.symbol_id.cmp(&b.symbol.symbol_id))
}
fn candidate_bytes(candidate: &Candidate) -> usize {
    candidate.symbol.signature.len()
        + candidate.symbol.qualified_name.len()
        + candidate.path.len()
        + candidate.language.len()
        + 1024
}

fn hydrate_pending(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    selected: Vec<Candidate>,
) -> Result<Vec<PendingSummarySymbol>, RocksError> {
    let wanted = selected
        .iter()
        .map(|candidate| candidate.symbol.symbol_id.clone())
        .collect::<BTreeSet<_>>();
    let mut sources = BTreeMap::new();
    visit_documents(graph, budget, |document, _| {
        if document.kind == DocumentKind::Symbol
            && let Some(id) = document.symbol_id
            && wanted.contains(&id)
        {
            sources.insert(
                id,
                (
                    document.code.chars().take(2048).collect::<String>(),
                    document.code.chars().count() > 2048,
                ),
            );
        }
        Ok(())
    })?;
    Ok(selected
        .into_iter()
        .map(|candidate| {
            let (code, code_truncated) = sources
                .remove(&candidate.symbol.symbol_id)
                .unwrap_or_default();
            PendingSummarySymbol {
                generation_id: graph.generation_id().to_string(),
                symbol_id: candidate.symbol.symbol_id.to_string(),
                path: candidate.path,
                language: candidate.language,
                symbol_kind: candidate.symbol.symbol_kind,
                qualified_name: candidate.symbol.qualified_name,
                signature: candidate.symbol.signature.chars().take(1024).collect(),
                start_line: candidate.symbol.start_line,
                end_line: candidate.symbol.end_line,
                content_hash: candidate.symbol.structural_digest.to_string(),
                code,
                code_truncated,
                priority: candidate.priority.is_some(),
            }
        })
        .collect())
}
