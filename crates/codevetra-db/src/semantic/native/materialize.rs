//! Bounded exact similarity builds publish only after vector and summary fences pass.

use super::{
    NAMESPACE,
    vectors::{SourceReader, cosine_distance, current, documents_scan},
};
use crate::{
    NativeDatabase,
    native_database::{
        dataset::{self, CommitGuard},
        visit_state,
    },
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateNamespace, StateTransaction},
    semantic::{
        EmbeddingModelState, RegisteredEmbeddingModel, SemanticStorageError as Error,
        SimilarityMaterializationPolicy, SimilarityMaterializationReport,
    },
};
use codevetra_domain::{DocumentKind, GenerationId, ModelId, ProjectId, SymbolId};
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, ops::ControlFlow};

pub(crate) const INPUT_KEYS: &[&[u8]] = &[b"vector_epoch", b"summary_epoch"];

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SimilarityEdge {
    pub(crate) model: ModelId,
    pub(crate) source: SymbolId,
    pub(crate) target: SymbolId,
    pub(crate) score: f64,
    rank: u16,
}
#[derive(Serialize, Deserialize)]
struct Metadata {
    report: SimilarityMaterializationReport,
    guard: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}
struct Source {
    symbol: SymbolId,
    vector: Vec<f32>,
}
struct Build {
    generation: GenerationId,
    guard: CommitGuard,
    report: SimilarityMaterializationReport,
    rows: Vec<SimilarityEdge>,
}

impl NativeDatabase {
    /// Build exact current model neighbors and atomically replace the derived cache.
    /// # Errors
    /// Preserves the previous complete build on changed inputs, deadlines, or workspace limits.
    pub async fn rebuild_current_similarity_edges(
        &self,
        project: &ProjectId,
        policy: SimilarityMaterializationPolicy,
    ) -> Result<SimilarityMaterializationReport, Error> {
        self.require_project(project)?;
        let build = self
            .transaction(
                crate::rocks::StateAccess::read_only(None),
                policy.statement_timeout,
                move |transaction, budget| build(transaction, budget, policy),
            )
            .await?;
        let metadata = Metadata {
            report: build.report,
            guard: build
                .guard
                .0
                .iter()
                .map(|(key, value)| (key.to_vec(), value.clone()))
                .collect(),
        };
        self.replace_dataset(
            crate::native_database::dataset::DatasetReplacement {
                namespace: StateNamespace::Derived,
                name: "similarity".to_owned(),
                expected: Some(build.generation),
                metadata: &metadata,
                guard: build.guard,
            },
            build.rows.into_iter(),
        )
        .await?;
        Ok(build.report)
    }
}

fn build(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    policy: SimilarityMaterializationPolicy,
) -> Result<Build, Error> {
    let generation = current(transaction)?;
    let guard = CommitGuard::capture(transaction, INPUT_KEYS, budget)?;
    let mut models = Vec::new();
    visit_state(
        &transaction.scan(NAMESPACE, b"m/"),
        budget,
        |_, model: RegisteredEmbeddingModel, _| {
            if model.state == EmbeddingModelState::Active {
                models.push(model);
            }
            if models.len() > 256 {
                return Err(RocksError::Limit {
                    resource: "embedding_models",
                });
            }
            Ok(())
        },
    )?;
    let snapshot = transaction.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let mut report = SimilarityMaterializationReport {
        models: 0,
        source_symbols: 0,
        model_symbol_pairs: 0,
        edges_written: 0,
        neighbors_per_symbol: policy.neighbors,
        minimum_score_millionths: (policy.minimum_score * 1_000_000.0)
            .round()
            .to_u32()
            .ok_or(Error::InvalidInput {
                field: "similarity_score",
            })?,
    };
    let mut distinct = BTreeSet::new();
    let mut neighbors = Neighbors {
        rows: Vec::new(),
        policy,
    };
    for model in models {
        let sources = model_sources(
            SourceReader::new(transaction, &graph).model(&model.selector),
            &graph,
            budget,
        )?;
        if sources.is_empty() {
            continue;
        }
        report.models += 1;
        report.model_symbol_pairs = report
            .model_symbol_pairs
            .checked_add(sources.len() as u64)
            .ok_or(workspace())?;
        if report.model_symbol_pairs > 100_000 {
            return Err(Error::InvalidInput {
                field: "similarity_source_symbols",
            });
        }
        distinct.extend(sources.iter().map(|source| source.symbol.clone()));
        neighbors.append(&sources, &model.selector.model_id, budget)?;
    }
    report.source_symbols = distinct.len() as u64;
    report.edges_written = neighbors.rows.len() as u64;
    Ok(Build {
        generation,
        guard,
        report,
        rows: neighbors.rows,
    })
}

fn model_sources(
    reader: super::vectors::VectorReader<'_, '_, '_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<Vec<Source>, Error> {
    let mut sources = Vec::new();
    let mut bytes = 0_usize;
    documents_scan(graph, budget, |_, document, budget| {
        if document.kind == DocumentKind::Symbol
            && let Some(symbol) = document.symbol_id.clone()
        {
            let id = document.document_id.clone();
            let (_, digest) = reader.render(document, budget)?;
            if let Some(vector) = reader.read(&id, &digest, budget)? {
                bytes = bytes
                    .checked_add(vector.len() * 4 + symbol.as_str().len() + 128)
                    .ok_or(workspace())?;
                if bytes > 32 * 1_048_576 {
                    return Err(workspace());
                }
                sources.push(Source { symbol, vector });
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    sources.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    if sources
        .windows(2)
        .any(|pair| pair[0].symbol == pair[1].symbol)
    {
        return Err(Error::CorruptStoredValue {
            field: "similarity_source",
        });
    }
    Ok(sources)
}

const MAXIMUM_SIMILARITY_ROWS: usize = 16 * 1_048_576 / 256;

struct Neighbors {
    rows: Vec<SimilarityEdge>,
    policy: SimilarityMaterializationPolicy,
}

impl Neighbors {
    fn append(
        &mut self,
        sources: &[Source],
        model: &ModelId,
        budget: &mut ReadBudget,
    ) -> Result<(), Error> {
        for source in sources {
            let mut neighbors = crate::retrieval::native::RankedRows::new(
                usize::from(self.policy.neighbors),
                |a: &(&SymbolId, f64), b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)),
            );
            for target in sources {
                budget.checkpoint()?;
                if source.symbol == target.symbol {
                    continue;
                }
                let score = 1.0 - cosine_distance(&source.vector, &target.vector, budget)?;
                if score >= self.policy.minimum_score {
                    neighbors.insert((&target.symbol, score.clamp(0.0, 1.0)));
                }
            }
            for (index, (target, score)) in neighbors.into_iter().enumerate() {
                if self.rows.len() >= MAXIMUM_SIMILARITY_ROWS {
                    return Err(workspace());
                }
                self.rows.push(SimilarityEdge {
                    model: model.clone(),
                    source: source.symbol.clone(),
                    target: target.clone(),
                    score,
                    rank: u16::try_from(index + 1).map_err(|_| workspace())?,
                });
            }
        }
        Ok(())
    }
}

/// Visit only a current generation build whose exact vector/summary epochs still match.
pub(crate) fn visit_current(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(SimilarityEdge, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let Some(current) = transaction.current()? else {
        return Ok(());
    };
    let Some(manifest) = dataset::Reader::new(transaction, StateNamespace::Derived)
        .at("similarity")
        .manifest(budget)?
    else {
        return Ok(());
    };
    if manifest.generation.as_ref() != Some(&current.generation_id) {
        return Ok(());
    }
    let metadata: Metadata =
        serde_json::from_value(manifest.metadata.clone()).map_err(|_| RocksError::Corrupt {
            field: "similarity_metadata",
        })?;
    let guard = CommitGuard::capture(transaction, INPUT_KEYS, budget)?;
    let actual = guard
        .0
        .iter()
        .map(|(key, value)| (key.to_vec(), value.clone()))
        .collect::<Vec<_>>();
    if metadata.guard != actual {
        return Ok(());
    }
    dataset::Reader::new(transaction, StateNamespace::Derived)
        .at("similarity")
        .visit(&manifest, budget, |row: SimilarityEdge, budget| {
            if !row.score.is_finite()
                || !(0.0..=1.0).contains(&row.score)
                || row.source == row.target
                || row.rank == 0
                || row.rank > 50
            {
                return Err(RocksError::Corrupt {
                    field: "similarity_edge",
                });
            }
            visit(row, budget)
        })
}
fn workspace() -> Error {
    Error::InvalidInput {
        field: "similarity_materialization_workspace",
    }
}
