//! Interchange exports are sorted and read from one immutable native snapshot.

use super::{
    InterchangeEdge, InterchangeFile, InterchangeReference, InterchangeSnapshot,
    InterchangeSnapshotError as Error, InterchangeSnapshotRequest, InterchangeSymbol,
    MAXIMUM_INTERCHANGE_ROWS,
};
use crate::{
    GraphDirection, NativeDatabase, StorageError,
    artifacts::native::summaries::visit_documents,
    retrieval::native::{visit_files, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError},
};
use codevetra_domain::{DocumentKind, SymbolId};
use std::collections::BTreeMap;

struct Export {
    snapshot: InterchangeSnapshot,
    bytes: usize,
}
impl Export {
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(Error::ByteBoundExceeded)?;
        if self.bytes > 64 * 1_048_576 {
            return Err(Error::ByteBoundExceeded);
        }
        Ok(())
    }
}

impl NativeDatabase {
    /// Export one exact current graph with explicit row, memory, and deadline bounds.
    /// # Errors
    /// Rejects absent current data, exceeded limits, corrupt facts, and native read failure.
    pub async fn current_interchange_snapshot(
        &self,
        request: InterchangeSnapshotRequest<'_>,
    ) -> Result<InterchangeSnapshot, Error> {
        self.require_project(request.project_id)?;
        if request.maximum_rows == 0
            || request.maximum_rows > MAXIMUM_INTERCHANGE_ROWS
            || request.statement_timeout.is_zero()
            || request.statement_timeout > std::time::Duration::from_secs(300)
        {
            return Err(Error::InvalidBounds);
        }
        let maximum = request.maximum_rows;
        self.transaction(
            crate::rocks::StateAccess::read_only(None),
            request.statement_timeout,
            move |transaction, budget| {
                let generation = transaction
                    .current()?
                    .ok_or(Error::CurrentGenerationUnavailable)?
                    .generation_id;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let info = graph.info();
                let total = info
                    .files
                    .checked_add(info.symbols)
                    .and_then(|n| n.checked_add(info.edges))
                    .and_then(|n| n.checked_add(info.references))
                    .ok_or(Error::RowBoundExceeded)?;
                if total > maximum {
                    return Err(Error::RowBoundExceeded);
                }
                let mut output = Export {
                    snapshot: InterchangeSnapshot {
                        generation_id: generation,
                        files: Vec::new(),
                        symbols: Vec::new(),
                        edges: Vec::new(),
                        references: Vec::new(),
                    },
                    bytes: 0,
                };
                output.files(&graph, budget)?;
                output.symbols(&graph, budget)?;
                output.references(&graph, budget)?;
                sort(&mut output.snapshot);
                Ok(output.snapshot)
            },
        )
        .await
    }
}

impl Export {
    fn files(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<(), Error> {
        visit_files(graph, budget, |file, _| {
            self.charge(file.normalized_path.len() + file.language.len() + 256)
                .map_err(bound)?;
            self.snapshot.files.push(InterchangeFile {
                file_id: file.file_id,
                path: file.normalized_path,
                language: file.language,
                content_hash: file.content_hash.to_string(),
                byte_size: file.byte_size,
            });
            Ok(())
        })
        .map_err(Error::from)
    }
    fn symbols(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<(), Error> {
        let mut ids = BTreeMap::<SymbolId, usize>::new();
        visit_symbols(graph, budget, |symbol, budget| {
            self.charge(
                symbol.qualified_name.len()
                    + symbol.signature.len()
                    + symbol.symbol_kind.len()
                    + 768,
            )
            .map_err(bound)?;
            ids.insert(symbol.symbol_id.clone(), self.snapshot.symbols.len());
            self.edges(graph, budget, &symbol.symbol_id)?;
            self.snapshot.symbols.push(InterchangeSymbol {
                symbol_id: symbol.symbol_id,
                file_id: symbol.file_id,
                symbol_kind: symbol.symbol_kind,
                qualified_name: symbol.qualified_name,
                signature: symbol.signature,
                code: String::new(),
                natural_text: String::new(),
                start_byte: symbol.start_byte,
                end_byte: symbol.end_byte,
                start_line: symbol.start_line,
                end_line: symbol.end_line,
            });
            Ok(())
        })?;
        visit_documents(graph, budget, |document, _| {
            if document.kind == DocumentKind::Symbol
                && let Some(index) = document
                    .symbol_id
                    .as_ref()
                    .and_then(|id| ids.get(id))
                    .copied()
            {
                self.charge(document.code.len() + document.natural_text.len())
                    .map_err(bound)?;
                let row = &mut self.snapshot.symbols[index];
                row.code = document.code;
                row.natural_text = document.natural_text;
            }
            Ok(())
        })
        .map_err(Error::from)
    }
    fn edges(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
        id: &SymbolId,
    ) -> Result<(), RocksError> {
        let mut cursor = None;
        loop {
            let page =
                graph
                    .adjacency(id, GraphDirection::Outgoing)
                    .page(cursor.as_ref(), 256, budget)?;
            for link in page.links {
                budget.checkpoint()?;
                let edge = graph.edge(&link, budget)?;
                self.charge(edge.provenance.len() + 256).map_err(bound)?;
                self.snapshot.edges.push(InterchangeEdge {
                    source_symbol_id: edge.source_symbol_id,
                    target_symbol_id: edge.target_symbol_id,
                    edge_kind: edge.kind.as_str().to_owned(),
                    confidence: edge.confidence,
                    provenance: edge.provenance,
                    site_count: edge.site_count,
                });
            }
            cursor = page.next;
            if cursor.is_none() {
                return Ok(());
            }
        }
    }
    fn references(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<(), Error> {
        let mut cursor = None;
        loop {
            let (references, next) = graph.reference_records(cursor.as_ref(), 256, budget)?;
            for reference in references {
                budget.checkpoint()?;
                self.charge(reference.reference_name.len() + reference.reference_kind.len() + 320)?;
                self.snapshot.references.push(InterchangeReference {
                    file_id: reference.file_id,
                    owner_symbol_id: reference.owner_symbol_id,
                    target_symbol_id: reference.target_symbol_id,
                    reference_name: reference.reference_name,
                    reference_kind: reference.reference_kind,
                    start_byte: reference.start_byte,
                    end_byte: reference.end_byte,
                    site_count: reference.site_count,
                });
            }
            cursor = next;
            if cursor.is_none() {
                return Ok(());
            }
        }
    }
}

fn sort(snapshot: &mut InterchangeSnapshot) {
    snapshot
        .files
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.file_id.cmp(&b.file_id)));
    snapshot.symbols.sort_by(|a, b| {
        a.file_id
            .cmp(&b.file_id)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.start_byte.cmp(&b.start_byte))
            .then(a.symbol_id.cmp(&b.symbol_id))
    });
    snapshot.edges.sort_by(|a, b| {
        a.source_symbol_id
            .cmp(&b.source_symbol_id)
            .then(a.target_symbol_id.cmp(&b.target_symbol_id))
            .then(a.edge_kind.cmp(&b.edge_kind))
            .then(a.provenance.cmp(&b.provenance))
    });
    snapshot.references.sort_by(|a, b| {
        a.file_id
            .cmp(&b.file_id)
            .then(a.start_byte.cmp(&b.start_byte))
            .then(a.end_byte.cmp(&b.end_byte))
            .then(a.reference_kind.cmp(&b.reference_kind))
            .then(a.reference_name.cmp(&b.reference_name))
            .then(a.owner_symbol_id.cmp(&b.owner_symbol_id))
            .then(a.target_symbol_id.cmp(&b.target_symbol_id))
    });
}
fn bound(error: Error) -> RocksError {
    match error {
        Error::ByteBoundExceeded => RocksError::Limit {
            resource: "interchange_bytes",
        },
        _ => RocksError::Corrupt {
            field: "interchange_export",
        },
    }
}
impl From<RocksError> for Error {
    fn from(error: RocksError) -> Self {
        match error {
            RocksError::Limit {
                resource: "interchange_bytes",
            } => Self::ByteBoundExceeded,
            _ => Self::from(crate::native_database::storage_error(error)),
        }
    }
}
impl From<StorageError> for Error {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::InvalidInput { .. } => Self::InvalidBounds,
            StorageError::GenerationNotFound => Self::CurrentGenerationUnavailable,
            StorageError::CorruptStoredValue { .. } => Self::CorruptStoredValue,
            _ => Self::DatabaseOperation,
        }
    }
}
