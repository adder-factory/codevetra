//! Decisions evaluated while the existing storage owner holds its mutation guards.

pub(super) fn owns_writer(
    active_sequence: u64,
    active_fence: &[u8; 32],
    sequence: u64,
    fence: &[u8; 32],
) -> bool {
    active_sequence == sequence && active_fence == fence
}

pub(super) fn can_mutate_writer(owned: bool, unexpired: bool) -> bool {
    owned && unexpired
}

pub(super) fn same_base(current: Option<u64>, base: Option<u64>) -> bool {
    current == base
}

pub(super) fn keep_superseded(sequence: u64, previous_floor: u64, retained: u8, keep: u8) -> bool {
    sequence >= previous_floor && retained < keep
}

pub(super) fn remove_version(
    published: bool,
    sequence: u64,
    floor: u64,
    kept_anchor: bool,
) -> bool {
    !published || (sequence < floor && kept_anchor)
}
