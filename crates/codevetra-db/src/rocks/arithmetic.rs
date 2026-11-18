//! Checked resource formulas at the native storage boundary.

use super::StorePolicy;

// Resource-admission formulas share named production constants. Formal contracts
// retain independent numeric expectations and copy these declarations verbatim.
const WORKSPACE_RESERVATION_MULTIPLIER: usize = 2;
const WORKER_RESERVATION_OVERHEAD_BYTES: usize = 4096;
const CHANGE_KEY_MULTIPLIER: usize = 5;
const CHANGE_VALUE_MULTIPLIER: usize = 3;
const CHANGE_OVERHEAD_BYTES: usize = 128;
const RETENTION_COUNTER_BYTES: usize = 96;
const RETENTION_CURSOR_ALLOWANCE_BYTES: usize = 128 * 1024;
const RETENTION_KEY_MULTIPLIER: usize = 2;
const RETENTION_RECORD_OVERHEAD_BYTES: usize = 256;
const MINIMUM_CACHE_BYTES: u64 = 8 * 1_048_576;
const MAXIMUM_CACHE_BYTES: u64 = 4096 * 1_048_576;
const MINIMUM_MEMTABLE_BYTES: usize = 4 * 1_048_576;
const MINIMUM_BATCH_BYTES: usize = 1_048_576;
const MAXIMUM_BATCH_BYTES: usize = 64 * 1_048_576;
const MAXIMUM_BATCH_ROWS: usize = 100_000;

pub(super) fn worker_reservation(workspace: usize, record: usize) -> Option<u32> {
    let bytes = workspace
        .checked_mul(WORKSPACE_RESERVATION_MULTIPLIER)?
        .checked_add(record)?
        .checked_add(WORKER_RESERVATION_OVERHEAD_BYTES)?;
    u32::try_from(bytes).ok()
}

pub(super) fn change_reservation(key: usize, value: usize) -> Option<usize> {
    key.checked_mul(CHANGE_KEY_MULTIPLIER)?
        .checked_add(value.checked_mul(CHANGE_VALUE_MULTIPLIER)?)?
        .checked_add(CHANGE_OVERHEAD_BYTES)
}

pub(super) fn retention_workspace(batch: usize, counters: usize, key: usize) -> Option<usize> {
    let counter_bytes = counters.checked_mul(RETENTION_COUNTER_BYTES)?;
    let key_bytes = key.checked_mul(RETENTION_KEY_MULTIPLIER)?;
    batch
        .checked_add(counter_bytes)?
        .checked_add(RETENTION_CURSOR_ALLOWANCE_BYTES)?
        .checked_add(key_bytes)?
        .checked_add(RETENTION_RECORD_OVERHEAD_BYTES)
}

pub(super) fn valid_store_policy(policy: &StorePolicy) -> bool {
    let Ok(cache_bytes) = u64::try_from(policy.cache_bytes) else {
        return false;
    };
    (MINIMUM_CACHE_BYTES..=MAXIMUM_CACHE_BYTES).contains(&cache_bytes)
        && (MINIMUM_MEMTABLE_BYTES..=policy.cache_bytes).contains(&policy.memtable_bytes)
        && (MINIMUM_BATCH_BYTES..=MAXIMUM_BATCH_BYTES).contains(&policy.maximum_batch_bytes)
        && (1..=MAXIMUM_BATCH_ROWS).contains(&policy.maximum_batch_rows)
}
