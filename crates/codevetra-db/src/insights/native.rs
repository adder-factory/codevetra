//! Relationship-aware feature queries over the complete native graph snapshot.

mod findings;
mod impact;
mod references;
mod risk;

use crate::{
    ReferenceInput,
    rocks::{GraphSnapshot, ReadBudget, RocksError},
};

fn visit_references(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(ReferenceInput, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let mut cursor = None;
    loop {
        let (references, next) = graph.reference_records(cursor.as_ref(), 256, budget)?;
        for reference in references {
            budget.checkpoint()?;
            visit(reference, budget)?;
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "native-insight",
    }
}
fn workspace() -> RocksError {
    RocksError::Limit {
        resource: "insight-workspace",
    }
}
