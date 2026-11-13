//! Rank arithmetic; model quality and relevance remain empirical properties.

pub(super) fn valid_rank(rank: u16, maximum: u16) -> bool {
    rank != 0 && rank <= maximum
}

pub(super) fn reciprocal_rank(rank: u16) -> f64 {
    1.0 / (60.0 + f64::from(rank))
}
