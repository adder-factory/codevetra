//! Packed sortable clone keys avoid a tree node and UUID string for every singleton.

use super::{PackedSymbol, ReadBudget, RocksError, workspace};
use std::mem::size_of;

type Row = ([u8; 32], PackedSymbol);
const MAXIMUM_BYTES: usize = 16 * 1_048_576;

#[derive(Default)]
pub(super) struct Groups(Vec<Row>);

impl Groups {
    pub(super) fn push(
        &mut self,
        hash: [u8; 32],
        id: PackedSymbol,
        bytes: &mut usize,
    ) -> Result<(), RocksError> {
        if self.0.len() == self.0.capacity() {
            let spare = MAXIMUM_BYTES.checked_sub(*bytes).ok_or_else(workspace)? / size_of::<Row>();
            let additional = spare.min(4096);
            if additional == 0 {
                return Err(workspace());
            }
            let before = self.0.capacity();
            self.0
                .try_reserve_exact(additional)
                .map_err(|_| workspace())?;
            *bytes = bytes
                .checked_add((self.0.capacity() - before) * size_of::<Row>())
                .ok_or_else(workspace)?;
            if *bytes > MAXIMUM_BYTES {
                return Err(workspace());
            }
        }
        self.0.push((hash, id));
        Ok(())
    }

    pub(super) fn sorted(
        mut self,
        budget: &ReadBudget,
    ) -> Result<std::vec::IntoIter<Row>, RocksError> {
        budget.checkpoint()?;
        self.0.sort_unstable();
        budget.checkpoint()?;
        Ok(self.0.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn packed_groups_preserve_hash_and_member_order() -> Result<(), RocksError> {
        let mut groups = Groups::default();
        let mut bytes = 0;
        for (hash, id) in [(2, 9), (1, 8), (1, 3)] {
            groups.push([hash; 32], [id; 16], &mut bytes)?;
        }
        let rows = groups
            .sorted(&ReadBudget::new(1, 1, Duration::from_secs(5))?)?
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![([1; 32], [3; 16]), ([1; 32], [8; 16]), ([2; 32], [9; 16])]
        );
        Ok(())
    }

    #[test]
    fn packed_group_growth_accounts_for_allocated_capacity() -> Result<(), RocksError> {
        let mut groups = Groups::default();
        let mut bytes = MAXIMUM_BYTES - size_of::<Row>();
        groups.push([1; 32], [1; 16], &mut bytes)?;
        assert_eq!(bytes, MAXIMUM_BYTES);
        assert!(matches!(
            groups.push([2; 32], [2; 16], &mut bytes),
            Err(RocksError::Limit {
                resource: "insight-workspace"
            })
        ));
        assert_eq!(groups.0.len(), 1);
        Ok(())
    }
}
