//! Overflow-free admission for bounded MCP input frames.

/// Admit an additional payload only when the complete size fits the bound.
pub(crate) fn admit_bytes(current: usize, additional: usize, maximum: usize) -> Option<usize> {
    if current > maximum || additional > maximum - current {
        None
    } else {
        Some(current + additional)
    }
}

#[cfg(test)]
mod tests {
    use super::admit_bytes;

    #[test]
    fn rejects_overflow_and_already_exhausted_budgets() {
        assert_eq!(admit_bytes(usize::MAX, 1, usize::MAX), None);
        assert_eq!(admit_bytes(2, 0, 1), None);
        assert_eq!(admit_bytes(0, 0, 0), Some(0));
        assert_eq!(admit_bytes(3, 5, 8), Some(8));
        assert_eq!(admit_bytes(usize::MAX - 1, 1, usize::MAX), Some(usize::MAX));
    }
}
