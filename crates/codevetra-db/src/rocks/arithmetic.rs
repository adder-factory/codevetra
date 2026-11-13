//! Checked resource formulas at the native storage boundary.

pub(super) fn worker_reservation(workspace: usize, record: usize) -> Option<u32> {
    let bytes = workspace
        .checked_mul(2)?
        .checked_add(record)?
        .checked_add(4096)?;
    u32::try_from(bytes).ok()
}

pub(super) fn change_reservation(key: usize, value: usize) -> Option<usize> {
    key.checked_mul(5)?
        .checked_add(value.checked_mul(3)?)?
        .checked_add(128)
}

pub(super) fn retention_batch_fits(
    batch: usize,
    counters: usize,
    key: usize,
    maximum: usize,
) -> bool {
    let Some(counter_bytes) = counters.checked_mul(96) else {
        return false;
    };
    let Some(key_bytes) = key.checked_mul(2) else {
        return false;
    };
    let Some(total) = batch.checked_add(counter_bytes) else {
        return false;
    };
    let Some(total) = total.checked_add(128 * 1024) else {
        return false;
    };
    let Some(total) = total.checked_add(key_bytes) else {
        return false;
    };
    let Some(total) = total.checked_add(256) else {
        return false;
    };
    total <= maximum
}

pub(super) fn valid_store_policy(cache: usize, memtable: usize, batch: usize, rows: usize) -> bool {
    let Ok(cache_bytes) = u64::try_from(cache) else {
        return false;
    };
    (8 * 1_048_576..=4096_u64 * 1_048_576).contains(&cache_bytes)
        && (4 * 1_048_576..=cache).contains(&memtable)
        && (1_048_576..=64 * 1_048_576).contains(&batch)
        && (1..=100_000).contains(&rows)
}
