//! Bounded source and topology evidence for the model-free and role passes.

use super::summaries::{
    current_symbol_summary, documentation_lengths, summary_kind, visit_documents,
};
use super::{Kind, Scope, ScopedArtifact, State, corrupt, workspace};
use crate::{
    GraphDirection, NativeDatabase, StorageError, SymbolInput,
    artifacts::{
        PendingRoleSymbol, PendingStructuralSummary, PendingStructuralSummaryQuery, RoleSweepModel,
        StructuralSummaryEdge, validate_limit, validate_model,
    },
    retrieval::native::visit_symbols,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction},
};
use codevetra_domain::{DocumentKind, SymbolId, Visibility};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

const MAXIMUM_EVIDENCE_BYTES: usize = 32 * 1_048_576;
const MAXIMUM_EDGE_BYTES: usize = 16 * 1_048_576;
const MAXIMUM_EVIDENCE_SYMBOLS: usize = 16_384;
const EVIDENCE_ROW_OVERHEAD: usize = 256;
const EDGE_PAGE_SIZE: u16 = 256;
const SUMMARY_EXCERPT_CHARS: usize = 600;
const SOURCE_EXCERPT_CHARS: usize = 2048;

struct Reader<'a, 'view, 'store> {
    transaction: &'a StateTransaction<'store>,
    graph: &'a GraphSnapshot<'view, 'store>,
}
#[derive(Clone, Copy)]
struct Selection<'a> {
    after: Option<&'a SymbolId>,
    limit: usize,
    threshold: usize,
}

#[derive(Default)]
pub(super) struct Evidence {
    pub(super) name: String,
    pub(super) code: String,
    pub(super) natural: String,
}

pub(super) fn evidence(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    ids: &BTreeSet<SymbolId>,
) -> Result<BTreeMap<SymbolId, Evidence>, RocksError> {
    let mut output = BTreeMap::new();
    if ids.is_empty() {
        return Ok(output);
    }
    let mut bytes = 0_usize;
    visit_documents(graph, budget, |document, _| {
        if document.kind != DocumentKind::Symbol {
            return Ok(());
        }
        let Some(id) = document.symbol_id else {
            return Ok(());
        };
        if !ids.contains(&id) {
            return Ok(());
        }
        let metadata: serde_json::Value =
            serde_json::from_str(&document.metadata_json).map_err(|_| corrupt())?;
        let item = Evidence {
            name: metadata
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(&document.qualified_name)
                .to_owned(),
            code: document.code.chars().take(SOURCE_EXCERPT_CHARS).collect(),
            natural: document
                .natural_text
                .chars()
                .take(SOURCE_EXCERPT_CHARS)
                .collect(),
        };
        bytes = bytes
            .checked_add(
                item.name.len() + item.code.len() + item.natural.len() + EVIDENCE_ROW_OVERHEAD,
            )
            .ok_or_else(workspace)?;
        if bytes > MAXIMUM_EVIDENCE_BYTES || output.len() >= MAXIMUM_EVIDENCE_SYMBOLS {
            return Err(workspace());
        }
        if output.insert(id, item).is_some() {
            return Err(corrupt());
        }
        Ok(())
    })?;
    Ok(output)
}

impl NativeDatabase {
    /// Page unsummarized declarations with bounded source and exact outgoing evidence.
    /// # Errors
    /// Rejects invalid bounds, corrupt facts, or exhausted native read/workspace budgets.
    pub async fn pending_structural_summaries(
        &self,
        request: PendingStructuralSummaryQuery<'_>,
    ) -> Result<Vec<PendingStructuralSummary>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, super::super::MAX_STRUCTURAL_SUMMARY_BATCH)?;
        let (after, limit, threshold) = (
            request.after_symbol_id.cloned(),
            usize::from(request.limit),
            request.policy.existing_docstring_char_threshold as usize,
        );
        self.state(
            crate::rocks::StateAccess::read_only(None),
            move |transaction, budget| {
                let Some(current) = transaction.current()? else {
                    return Ok(Vec::new());
                };
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let reader = Reader {
                    transaction,
                    graph: &graph,
                };
                let selected = reader.select_structural_symbols(
                    budget,
                    Selection {
                        after: after.as_ref(),
                        limit,
                        threshold,
                    },
                )?;
                let (mut edges, wanted) = reader.structural_edges(budget, &selected)?;
                let mut documents = evidence(&graph, budget, &wanted)?;
                hydrate_structural_edges(&mut edges, &documents)?;
                let mut output = Vec::new();
                for symbol in selected {
                    let source = documents.remove(&symbol.symbol_id).unwrap_or_default();
                    let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                    output.push(PendingStructuralSummary {
                        generation_id: current.generation_id.to_string(),
                        symbol_id: symbol.symbol_id.to_string(),
                        path: file.normalized_path,
                        symbol_kind: symbol.symbol_kind,
                        name: if source.name.is_empty() {
                            symbol.qualified_name.clone()
                        } else {
                            source.name
                        },
                        qualified_name: symbol.qualified_name,
                        signature: symbol.signature.chars().take(1024).collect(),
                        code: source.code,
                        start_line: symbol.start_line,
                        end_line: symbol.end_line,
                        content_hash: symbol.structural_digest.to_string(),
                        declaration_only: symbol.declaration_only,
                        edges: edges.remove(&symbol.symbol_id).unwrap_or_default(),
                    });
                }
                Ok(output)
            },
        )
        .await
    }

    /// Page roles needing the exact configured producer without downgrading judged roles.
    /// # Errors
    /// Rejects invalid model/bounds, corrupt source, or exhausted native read budgets.
    pub async fn pending_symbol_roles(
        &self,
        project: &codevetra_domain::ProjectId,
        sweep: RoleSweepModel<'_>,
        limit: u16,
    ) -> Result<Vec<PendingRoleSymbol>, StorageError> {
        self.require_project(project)?;
        validate_limit(limit, super::super::MAX_ROLE_BATCH)?;
        let (model, keep_judged) = match sweep {
            RoleSweepModel::Structural(model) => (model, true),
            RoleSweepModel::Judge(model) => (model, false),
        };
        validate_model(model)?;
        let model = model.to_owned();
        self.state(
            crate::rocks::StateAccess::read_only(None),
            move |transaction, budget| {
                if transaction.current()?.is_none() {
                    return Ok(Vec::new());
                }
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let mut selected =
                    crate::retrieval::native::RankedRows::new(usize::from(limit), role_order);
                let sweep = if keep_judged {
                    RoleSweepModel::Structural(&model)
                } else {
                    RoleSweepModel::Judge(&model)
                };
                visit_symbols(&graph, budget, |symbol, budget| {
                    if matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter") {
                        return Ok(());
                    }
                    let role = (ScopedArtifact {
                        kind: Kind::Role,
                        scope: Scope::Symbol,
                        key: symbol.symbol_id.as_str(),
                    })
                    .read(transaction, budget)?;
                    if role.is_some_and(|record| sweep.satisfied(&record, &symbol)) {
                        return Ok(());
                    }
                    let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                    selected.insert((symbol, file));
                    if selected
                        .iter()
                        .map(|(symbol, file)| {
                            symbol.signature.len()
                                + symbol.qualified_name.len()
                                + file.normalized_path.len()
                                + 1024
                        })
                        .sum::<usize>()
                        > MAXIMUM_EDGE_BYTES
                    {
                        return Err(workspace());
                    }
                    Ok(())
                })?;
                let wanted = selected
                    .iter()
                    .map(|(symbol, _)| symbol.symbol_id.clone())
                    .collect();
                let mut documents = evidence(&graph, budget, &wanted)?;
                selected
                    .into_iter()
                    .map(|(symbol, file)| {
                        let source = documents.remove(&symbol.symbol_id).unwrap_or_default();
                        let summary = current_symbol_summary(transaction, &symbol, budget)?;
                        Ok(PendingRoleSymbol {
                            symbol_id: symbol.symbol_id.to_string(),
                            path: file.normalized_path,
                            language: file.language,
                            symbol_kind: symbol.symbol_kind,
                            qualified_name: symbol.qualified_name,
                            signature: symbol.signature.chars().take(1024).collect(),
                            description: summary
                                .filter(|summary| !summary.body.is_empty())
                                .map_or(source.natural, |summary| summary.body),
                            code: source.code,
                            exported: symbol.export.exported,
                        })
                    })
                    .collect()
            },
        )
        .await
    }
}

impl Reader<'_, '_, '_> {
    fn select_structural_symbols(
        &self,
        budget: &mut ReadBudget,
        selection: Selection<'_>,
    ) -> Result<Vec<SymbolInput>, RocksError> {
        let Self { transaction, graph } = *self;
        let Selection {
            after,
            limit,
            threshold,
        } = selection;

        let lengths = documentation_lengths(graph, budget)?;
        let mut selected =
            crate::retrieval::native::RankedRows::new(limit, |a: &SymbolInput, b| {
                a.symbol_id.cmp(&b.symbol_id)
            });
        visit_symbols(graph, budget, |symbol, budget| {
            if summary_kind(&symbol.symbol_kind)
                && after.is_none_or(|after| &symbol.symbol_id > after)
                && lengths.get(&symbol.symbol_id).copied().unwrap_or(0) < threshold
                && current_symbol_summary(transaction, &symbol, budget)?.is_none()
            {
                selected.insert(symbol);
                check_symbols(&selected)?;
            }
            Ok(())
        })?;
        Ok(selected.into_vec())
    }
}

type SummaryEdges = BTreeMap<SymbolId, Vec<StructuralSummaryEdge>>;

impl Reader<'_, '_, '_> {
    fn structural_edges(
        &self,
        budget: &mut ReadBudget,
        selected: &[SymbolInput],
    ) -> Result<(SummaryEdges, BTreeSet<SymbolId>), RocksError> {
        let mut wanted = selected
            .iter()
            .map(|symbol| symbol.symbol_id.clone())
            .collect::<BTreeSet<_>>();
        let mut edges = BTreeMap::new();
        let mut bytes = 0_usize;
        for symbol in selected {
            let links = self.summary_edges(budget, &symbol.symbol_id)?;
            for edge in &links {
                wanted.insert(SymbolId::parse(&edge.target_symbol_id).map_err(|_| corrupt())?);
                bytes = bytes
                    .checked_add(
                        edge.target_path.len()
                            + edge.target_name.len()
                            + edge.target_qualified_name.len()
                            + edge.target_summary.as_ref().map_or(0, String::len)
                            + EVIDENCE_ROW_OVERHEAD,
                    )
                    .ok_or_else(workspace)?;
                if bytes > MAXIMUM_EDGE_BYTES || wanted.len() > MAXIMUM_EVIDENCE_SYMBOLS {
                    return Err(workspace());
                }
            }
            edges.insert(symbol.symbol_id.clone(), links);
        }
        Ok((edges, wanted))
    }
}

fn hydrate_structural_edges(
    edges: &mut SummaryEdges,
    documents: &BTreeMap<SymbolId, Evidence>,
) -> Result<(), RocksError> {
    for links in edges.values_mut() {
        for edge in links {
            let id = SymbolId::parse(&edge.target_symbol_id).map_err(|_| corrupt())?;
            if let Some(source) = documents.get(&id) {
                edge.target_name.clone_from(&source.name);
                if edge.target_summary.is_none() && !source.natural.is_empty() {
                    edge.target_summary =
                        Some(source.natural.chars().take(SUMMARY_EXCERPT_CHARS).collect());
                }
            }
        }
    }
    Ok(())
}

impl RoleSweepModel<'_> {
    pub(super) fn satisfied(self, record: &super::Record, symbol: &SymbolInput) -> bool {
        let (model, keep_judged) = match self {
            Self::Structural(model) => (model, true),
            Self::Judge(model) => (model, false),
        };

        let via = record
            .metadata
            .get("via")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let producer = record
            .metadata
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        record.state == State::Complete
            && record.source_digest.as_deref() == Some(symbol.structural_digest.as_str())
            && (matches!(via, "structural_rule" | "rule")
                || (matches!(via, "structural_fallback" | "llm" | "jev") && producer == model)
                || (keep_judged && matches!(via, "llm" | "jev")))
    }
}

fn role_order(
    a: &(SymbolInput, crate::FileInput),
    b: &(SymbolInput, crate::FileInput),
) -> Ordering {
    b.0.export
        .exported
        .cmp(&a.0.export.exported)
        .then(
            (b.0.visibility == Some(Visibility::Public))
                .cmp(&(a.0.visibility == Some(Visibility::Public))),
        )
        .then(a.1.normalized_path.cmp(&b.1.normalized_path))
        .then(a.0.start_line.cmp(&b.0.start_line))
        .then(a.0.symbol_id.cmp(&b.0.symbol_id))
}
fn check_symbols(symbols: &[SymbolInput]) -> Result<(), RocksError> {
    if symbols
        .iter()
        .map(|symbol| symbol.signature.len() + symbol.qualified_name.len() + 1024)
        .sum::<usize>()
        > MAXIMUM_EDGE_BYTES
    {
        return Err(workspace());
    }
    Ok(())
}
impl Reader<'_, '_, '_> {
    fn summary_edges(
        &self,
        budget: &mut ReadBudget,
        id: &SymbolId,
    ) -> Result<Vec<StructuralSummaryEdge>, RocksError> {
        let Self { transaction, graph } = *self;

        let mut output = Vec::new();
        let mut cursor = None;
        let mut bytes = 0_usize;
        loop {
            let page = graph.adjacency(id, GraphDirection::Outgoing).page(
                cursor.as_ref(),
                EDGE_PAGE_SIZE,
                budget,
            )?;
            for link in page.links {
                budget.checkpoint()?;
                let symbol = graph
                    .symbol(link.target_symbol_id(), budget)?
                    .ok_or(corrupt())?;
                let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                let summary =
                    current_symbol_summary(transaction, &symbol, budget)?.map(|summary| {
                        summary
                            .body
                            .chars()
                            .take(SUMMARY_EXCERPT_CHARS)
                            .collect::<String>()
                    });
                bytes = bytes
                    .checked_add(
                        symbol.qualified_name.len() * 2
                            + file.normalized_path.len()
                            + summary.as_ref().map_or(0, String::len)
                            + EVIDENCE_ROW_OVERHEAD,
                    )
                    .ok_or_else(workspace)?;
                if bytes > MAXIMUM_EDGE_BYTES || output.len() >= MAXIMUM_EVIDENCE_SYMBOLS {
                    return Err(workspace());
                }
                output.push(StructuralSummaryEdge {
                    edge_kind: link.kind().as_str().to_owned(),
                    target_symbol_id: symbol.symbol_id.to_string(),
                    target_kind: symbol.symbol_kind,
                    target_name: symbol.qualified_name.clone(),
                    target_qualified_name: symbol.qualified_name,
                    target_path: file.normalized_path,
                    target_summary: summary,
                });
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        output.sort_by(|a, b| {
            a.edge_kind
                .cmp(&b.edge_kind)
                .then(a.target_qualified_name.cmp(&b.target_qualified_name))
                .then(a.target_symbol_id.cmp(&b.target_symbol_id))
        });
        Ok(output)
    }
}
