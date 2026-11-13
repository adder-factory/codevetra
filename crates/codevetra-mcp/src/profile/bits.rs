//! Algebra of explicit profile membership; profiles have no implicit hierarchy.

pub(super) const fn union(left: u8, right: u8) -> u8 {
    left | right
}

pub(super) const fn includes(membership: u8, profile: u8) -> bool {
    membership & profile != 0
}
