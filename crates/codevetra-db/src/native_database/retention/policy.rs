//! Time arithmetic for unchanged orphan records, evaluated inside their transaction.

pub(super) fn grace_elapsed(timestamp: i64, first: i64) -> bool {
    i128::from(timestamp) - i128::from(first) >= 600
}
