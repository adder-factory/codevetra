//! Complete refreshes use separately admitted stages and bounded symbol batches.
//! Every stage keeps the original generation fence and one shared request deadline.

#[cfg(test)]
mod tests;

use super::*;
use crate::native_database::{request_for, storage_error};

pub(super) async fn prepare(
    database: &NativeDatabase,
    timeout: Duration,
) -> Result<Option<Prepared>, StorageError> {
    let request = request_for(timeout).map_err(storage_error)?;
    // Retained stage inputs share one admission through publication, so
    // concurrent refreshes cannot accumulate inventories between worker jobs.
    let admission = database
        .finding_computation
        .clone()
        .try_lock_owned()
        .map_err(|_| StorageError::NativeCapacityExceeded)?;
    let initial = database
        .state_with_request(None, 0, 0, &request, |transaction, budget| {
            fingerprint(transaction, budget)?
                .map(|fingerprint| {
                    let inputs = inventory::Inputs::read(transaction, budget)?;
                    Ok((fingerprint, inputs))
                })
                .transpose()
        })
        .await?;
    let Some((fingerprint, inputs)) = initial else {
        return Ok(None);
    };
    let generation = fingerprint.generation.clone();
    let referenced = database
        .state_with_request(
            Some(generation.clone()),
            0,
            0,
            &request,
            move |transaction, budget| inputs.references(transaction, budget),
        )
        .await?;
    let mut inventory = database
        .state_with_request(
            Some(generation.clone()),
            0,
            0,
            &request,
            move |transaction, budget| referenced.clones(transaction, budget),
        )
        .await?;
    loop {
        let previous = fingerprint.previous.clone();
        let (next, done) = database
            .state_with_request(
                Some(generation.clone()),
                0,
                0,
                &request,
                move |transaction, budget| {
                    let done = inventory.batch(transaction, budget, previous.as_ref())?;
                    Ok((inventory, done))
                },
            )
            .await?;
        inventory = next;
        if done {
            break;
        }
    }
    database
        .state_with_request(
            Some(generation),
            0,
            0,
            &request,
            move |transaction, budget| {
                // No stage can publish a partial relation. Auxiliary changes, retention
                // of prior evidence, or a day-boundary change invalidate the whole run.
                let current = super::fingerprint(transaction, budget)?
                    .ok_or(RocksError::GenerationChanged)?;
                if current.digest != fingerprint.digest {
                    return Err(RocksError::GenerationChanged);
                }
                finish(fingerprint, inventory.finish(), budget, admission).map(Some)
            },
        )
        .await
}

fn finish(
    fingerprint: Fingerprint,
    inventory: inventory::ComputedInventory,
    budget: &mut ReadBudget,
    admission: tokio::sync::OwnedMutexGuard<()>,
) -> Result<Prepared, RocksError> {
    let inventory::ComputedInventory {
        rows,
        analyzed,
        coverage,
    } = inventory;
    let mut stats = StructuralFindingStats {
        state: Computation::Ready,
        analyzed_symbols: analyzed,
        detector_coverage: coverage,
        ..StructuralFindingStats::default()
    };
    for row in &rows {
        budget.checkpoint()?;
        stats.total_findings += 1;
        match row.severity.as_str() {
            "error" => stats.error_findings += 1,
            "warning" => stats.warning_findings += 1,
            "info" => stats.info_findings += 1,
            _ => return Err(corrupt()),
        }
        *stats.by_finding.entry(row.finding.clone()).or_default() += 1;
        stats.very_long_symbols += u64::from(row.finding == "large_method");
        stats.high_fan_out_symbols += u64::from(row.finding == "high_fan_out");
        stats.unresolved_reference_pressure +=
            u64::from(row.finding == "unresolved_reference_pressure");
    }
    let weighted = stats.error_findings * 4 + stats.warning_findings * 2 + stats.info_findings;
    stats.code_health_score = (100.0
        - weighted.to_f64().ok_or(corrupt())? * 100.0
            / analyzed.max(1).to_f64().ok_or(corrupt())?)
    .max(0.0)
    .to_f32()
    .ok_or(corrupt())?;
    Ok(Prepared {
        fingerprint,
        rows,
        stats,
        _admission: admission,
    })
}
