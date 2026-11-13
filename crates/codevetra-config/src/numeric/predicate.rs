//! The numeric contract after JSON has established an unsigned integer.

pub(super) fn positive_bounded(value: u64, maximum: u64) -> bool {
    value != 0 && value <= maximum
}
