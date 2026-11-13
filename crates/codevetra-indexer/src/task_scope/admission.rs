//! Byte admission after the task owner has checked closure and task capacity.

pub(super) fn reserve(current: u64, additional: u64, maximum: u64) -> Option<u64> {
    if additional == 0 || current > maximum || additional > maximum - current {
        None
    } else {
        Some(current + additional)
    }
}
