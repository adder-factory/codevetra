//! Overflow-free diagnostic token estimation.

pub(super) const fn estimate_tokens(chars: usize) -> usize {
    let remainder = match chars % 4 {
        0 => 0,
        _ => 1,
    };
    chars / 4 + remainder
}
