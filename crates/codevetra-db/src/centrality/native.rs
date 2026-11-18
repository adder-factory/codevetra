//! Read canonical fixed-point centrality without recomputation or SQL.

use crate::{
    NativeDatabase, StorageError, SymbolBetweennessScore, SymbolInput, SymbolPageRankScore,
    rocks::RocksError,
};
use codevetra_domain::{GenerationId, ProjectId, SymbolId};
use std::{collections::BTreeSet, time::Duration};

impl NativeDatabase {
    /// Read sampled Brandes evidence from the expected complete generation.
    /// # Errors
    /// Rejects oversized symbol sets, missing symbols, and publication changes.
    pub async fn current_symbol_betweenness(
        &self,
        project: &ProjectId,
        expected: &GenerationId,
        symbols: &[SymbolId],
    ) -> Result<Vec<SymbolBetweennessScore>, StorageError> {
        self.native_centrality(project, expected, symbols).await
    }

    /// Read directed `PageRank` evidence from the expected complete generation.
    /// # Errors
    /// Rejects oversized symbol sets, missing symbols, and publication changes.
    pub async fn current_symbol_pagerank(
        &self,
        project: &ProjectId,
        expected: &GenerationId,
        symbols: &[SymbolId],
    ) -> Result<Vec<SymbolPageRankScore>, StorageError> {
        self.native_centrality(project, expected, symbols).await
    }

    async fn native_centrality<T: CentralityScore>(
        &self,
        project: &ProjectId,
        expected: &GenerationId,
        symbols: &[SymbolId],
    ) -> Result<Vec<T>, StorageError> {
        self.require_project(project)?;
        if symbols.len() > 500 {
            return Err(StorageError::InvalidInput {
                field: "symbol_ids",
            });
        }
        let symbols = symbols.iter().cloned().collect::<BTreeSet<_>>();
        self.graph(expected, Duration::from_secs(30), move |graph, budget| {
            symbols
                .into_iter()
                .map(|id| {
                    let symbol = graph
                        .symbol(&id, budget)?
                        .ok_or(RocksError::GenerationChanged)?;
                    Ok(T::from_symbol(id, &symbol))
                })
                .collect()
        })
        .await
    }
}

// The result type selects its own stored score, so the two centrality measures
// cannot be paired with the wrong record constructor by a boolean argument.
trait CentralityScore: Send + 'static {
    fn from_symbol(id: SymbolId, symbol: &SymbolInput) -> Self;
}

impl CentralityScore for SymbolBetweennessScore {
    fn from_symbol(symbol_id: SymbolId, symbol: &SymbolInput) -> Self {
        Self {
            symbol_id,
            score: normalized_score(symbol.betweenness_ppb),
        }
    }
}

impl CentralityScore for SymbolPageRankScore {
    fn from_symbol(symbol_id: SymbolId, symbol: &SymbolInput) -> Self {
        Self {
            symbol_id,
            score: normalized_score(symbol.pagerank_ppb),
        }
    }
}

fn normalized_score(parts_per_billion: Option<u32>) -> Option<f64> {
    parts_per_billion.map(|value| f64::from(value) / super::SCORE_PARTS_PER_BILLION)
}
