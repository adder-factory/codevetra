//! A bounded sorted prefix with one ordering for its entire lifetime.

use std::{cmp::Ordering, ops::Deref};

/// Retain the first `limit` rows under a stable ordering.
/// The comparator must not change its ordering while rows are retained.
/// Equal rows preserve insertion order, including at the capacity boundary.
pub(crate) struct RankedRows<T, F> {
    rows: Vec<T>,
    limit: usize,
    order: F,
}

impl<T, F: Fn(&T, &T) -> Ordering> RankedRows<T, F> {
    pub(crate) const fn new(limit: usize, order: F) -> Self {
        Self {
            rows: Vec::new(),
            limit,
            order,
        }
    }

    pub(crate) fn preallocated(limit: usize, order: F) -> Self {
        Self {
            rows: Vec::with_capacity(limit),
            limit,
            order,
        }
    }

    pub(crate) fn insert(&mut self, row: T) {
        let position = self
            .rows
            .partition_point(|existing| (self.order)(existing, &row) != Ordering::Greater);
        if position < self.limit {
            self.rows.insert(position, row);
            self.rows.truncate(self.limit);
        }
    }

    pub(crate) fn into_vec(self) -> Vec<T> {
        self.rows
    }

    /// Discard a suffix without changing the ordering or admission bound.
    pub(crate) fn truncate(&mut self, length: usize) {
        self.rows.truncate(length);
    }

    pub(crate) fn would_retain(&self, row: &T) -> bool {
        self.rows
            .partition_point(|existing| (self.order)(existing, row) != Ordering::Greater)
            < self.limit
    }

    pub(crate) fn is_full(&self) -> bool {
        self.rows.len() == self.limit
    }
}

impl<T, F> Deref for RankedRows<T, F> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl<T, F> IntoIterator for RankedRows<T, F> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

impl<'a, T, F> IntoIterator for &'a RankedRows<T, F> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::RankedRows;

    #[test]
    fn every_prefix_matches_stable_sort_and_take_for_all_five_row_permutations() {
        let source = [(1, 'a'), (0, 'b'), (1, 'c'), (0, 'd'), (2, 'e')];
        for permutation in 0..120 {
            let mut remaining = source.to_vec();
            let mut rank = permutation;
            let mut input = Vec::new();
            while !remaining.is_empty() {
                let index = rank % remaining.len();
                rank /= remaining.len();
                input.push(remaining.remove(index));
            }
            for limit in 0..=6 {
                let mut rows = RankedRows::new(limit, |a: &(i32, char), b| a.0.cmp(&b.0));
                let mut expected = Vec::new();
                for row in &input {
                    rows.insert(*row);
                    expected.push(*row);
                    expected.sort_by_key(|row| row.0);
                    assert_eq!(&*rows, &expected[..expected.len().min(limit)]);
                    assert!(rows.len() <= limit);
                }
                assert_eq!(
                    rows.into_vec(),
                    expected.into_iter().take(limit).collect::<Vec<_>>()
                );
            }
        }
    }
}
