//! A sorted packed inventory retains identities and child metrics within the existing bound.

use super::{corrupt, inventory::Document, inventory::parse_document, workspace};
use crate::{
    artifacts::native::summaries::visit_documents,
    rocks::{GraphSnapshot, ReadBudget, RocksError, packed_uuid, unpacked_uuid},
};
use codevetra_domain::{DocumentId, DocumentKind, SymbolId};
use std::mem::size_of;

const MAXIMUM_BYTES: usize = 24 * 1_048_576;

pub(super) struct DocumentHandle {
    symbol: [u8; 16],
    id: [u8; 16],
    pub(super) code_lines: f64,
    pub(super) cyclomatic: f64,
    pub(super) unresolved: u64,
    pub(super) test: bool,
}

impl DocumentHandle {
    pub(super) fn symbol(&self) -> Result<SymbolId, RocksError> {
        SymbolId::parse(&unpacked_uuid(&self.symbol)?).map_err(|_| corrupt())
    }

    pub(super) fn load(
        &self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<Document, RocksError> {
        let id = DocumentId::parse(&unpacked_uuid(&self.id)?).map_err(|_| corrupt())?;
        let document = graph.document(&id, budget)?.ok_or(corrupt())?;
        if !matches!(document.kind, DocumentKind::Symbol | DocumentKind::Test)
            || self.test != (document.kind == DocumentKind::Test)
        {
            return Err(corrupt());
        }
        let value = serde_json::from_str(&document.metadata_json).map_err(|_| corrupt())?;
        parse_document(&value, self.test)
    }
}

pub(super) struct Documents(Vec<DocumentHandle>);

impl Documents {
    pub(super) fn read(
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<Self, RocksError> {
        let mut rows = Vec::new();
        visit_documents(graph, budget, |document, budget| {
            if !matches!(document.kind, DocumentKind::Symbol | DocumentKind::Test) {
                return Ok(());
            }
            let Some(symbol) = document.symbol_id else {
                return Ok(());
            };
            reserve_slot(&mut rows, budget)?;
            let value = serde_json::from_str(&document.metadata_json).map_err(|_| corrupt())?;
            let parsed = parse_document(&value, document.kind == DocumentKind::Test)?;
            rows.push(DocumentHandle {
                symbol: packed_uuid(symbol.as_str())?,
                id: packed_uuid(document.document_id.as_str())?,
                test: parsed.test,
                code_lines: parsed.health("code_lines"),
                cyclomatic: parsed.health("cyclomatic"),
                unresolved: 0,
            });
            Ok(())
        })?;
        compact(&mut rows, budget)?;
        Ok(Self(rows))
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &DocumentHandle> {
        self.0.iter()
    }

    // Only selected document owners can produce findings. Keep their exact
    // unresolved-site counts alongside the already admitted packed identity.
    pub(super) fn record_unresolved(
        &mut self,
        id: &SymbolId,
        count: u64,
    ) -> Result<(), RocksError> {
        let key = packed_uuid(id.as_str())?;
        if let Ok(index) = self.0.binary_search_by_key(&key, |row| row.symbol) {
            self.0[index].unresolved = self.0[index]
                .unresolved
                .checked_add(count)
                .ok_or(corrupt())?;
        }
        Ok(())
    }

    pub(super) fn get(&self, id: &SymbolId) -> Result<Option<&DocumentHandle>, RocksError> {
        let key = packed_uuid(id.as_str())?;
        Ok(self
            .0
            .binary_search_by_key(&key, |row| row.symbol)
            .ok()
            .map(|index| &self.0[index]))
    }
}

fn reserve_slot(rows: &mut Vec<DocumentHandle>, budget: &ReadBudget) -> Result<(), RocksError> {
    let maximum = MAXIMUM_BYTES / size_of::<DocumentHandle>();
    if rows.len() == maximum {
        compact(rows, budget)?;
    }
    if rows.len() < rows.capacity() {
        return Ok(());
    }
    let additional = maximum.saturating_sub(rows.capacity()).min(4096);
    if additional == 0 {
        return Err(workspace());
    }
    rows.try_reserve_exact(additional)
        .map_err(|_| workspace())?;
    if rows.capacity().saturating_mul(size_of::<DocumentHandle>()) > MAXIMUM_BYTES {
        return Err(workspace());
    }
    Ok(())
}

fn compact(rows: &mut Vec<DocumentHandle>, budget: &ReadBudget) -> Result<(), RocksError> {
    budget.checkpoint()?;
    // Canonical document order previously kept the first test, or the last
    // non-test document when a symbol had several documents. Preserve that
    // precedence without retaining a second map or sorting workspace.
    rows.sort_unstable_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then_with(|| right.test.cmp(&left.test))
            .then_with(|| {
                if left.test {
                    left.id.cmp(&right.id)
                } else {
                    right.id.cmp(&left.id)
                }
            })
    });
    rows.dedup_by_key(|row| row.symbol);
    budget.checkpoint()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn duplicate_documents_reuse_the_bounded_inventory_before_more_admission()
    -> Result<(), RocksError> {
        let maximum = MAXIMUM_BYTES / size_of::<DocumentHandle>();
        let mut rows = Vec::with_capacity(maximum);
        rows.resize_with(maximum, || DocumentHandle {
            symbol: [1; 16],
            id: [1; 16],
            code_lines: 0.0,
            cyclomatic: 0.0,
            unresolved: 0,
            test: false,
        });
        rows[maximum - 1].id = [2; 16];
        rows[maximum - 1].code_lines = 200.0;
        let budget = ReadBudget::new(1, 1, Duration::from_secs(10))?;
        reserve_slot(&mut rows, &budget)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, [2; 16]);
        assert_eq!(rows[0].code_lines, 200.0);
        assert!(rows.capacity() * size_of::<DocumentHandle>() <= MAXIMUM_BYTES);
        Ok(())
    }

    #[test]
    fn distinct_documents_cannot_exceed_the_inventory_bound() -> Result<(), RocksError> {
        let maximum = MAXIMUM_BYTES / size_of::<DocumentHandle>();
        let mut rows = Vec::with_capacity(maximum);
        for index in 0..maximum {
            let mut symbol = [1; 16];
            symbol[8..].copy_from_slice(&(index as u64).to_be_bytes());
            rows.push(DocumentHandle {
                symbol,
                id: symbol,
                code_lines: 0.0,
                cyclomatic: 0.0,
                unresolved: 0,
                test: false,
            });
        }
        let budget = ReadBudget::new(1, 1, Duration::from_secs(10))?;
        assert!(matches!(
            reserve_slot(&mut rows, &budget),
            Err(RocksError::Limit {
                resource: "insight-workspace"
            })
        ));
        assert_eq!(rows.len(), maximum);
        Ok(())
    }
}
