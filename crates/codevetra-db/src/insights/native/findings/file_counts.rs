//! Per-name file counts retain packed rows and charge their exact vector capacity.

use super::{corrupt, workspace};
use crate::rocks::RocksError;
use std::mem::size_of;

type Row = ([u8; 16], u64);
const MAXIMUM_BYTES: usize = 8 * 1_048_576;

#[derive(Default)]
pub(super) struct FileCounts(Vec<Row>);

impl FileCounts {
    pub(super) fn add(
        &mut self,
        file: [u8; 16],
        count: u64,
        bytes: &mut usize,
    ) -> Result<(), RocksError> {
        match self.0.binary_search_by_key(&file, |row| row.0) {
            Ok(index) => self.0[index].1 = self.0[index].1.checked_add(count).ok_or(corrupt())?,
            Err(index) => {
                if self.0.len() == self.0.capacity() {
                    let spare =
                        MAXIMUM_BYTES.checked_sub(*bytes).ok_or_else(workspace)? / size_of::<Row>();
                    let additional = self.0.capacity().max(1).min(spare);
                    if additional == 0 {
                        return Err(workspace());
                    }
                    let before = self.0.capacity();
                    self.0
                        .try_reserve_exact(additional)
                        .map_err(|_| workspace())?;
                    reserve(bytes, (self.0.capacity() - before) * size_of::<Row>())?;
                }
                self.0.insert(index, (file, count));
            }
        }
        Ok(())
    }

    pub(super) fn external(&self, file: [u8; 16]) -> Result<u64, RocksError> {
        self.0
            .iter()
            .filter(|(id, _)| *id != file)
            .try_fold(0_u64, |sum, (_, count)| {
                sum.checked_add(*count).ok_or(corrupt())
            })
    }
}

pub(super) fn reserve(bytes: &mut usize, additional: usize) -> Result<(), RocksError> {
    let next = bytes.checked_add(additional).ok_or_else(workspace)?;
    if next > MAXIMUM_BYTES {
        return Err(workspace());
    }
    *bytes = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_keep_distinct_files_and_exact_allocated_capacity() -> Result<(), RocksError> {
        let mut files = FileCounts::default();
        let mut bytes = 0;
        files.add([2; 16], 3, &mut bytes)?;
        assert_eq!(bytes, 24);
        files.add([1; 16], 5, &mut bytes)?;
        files.add([2; 16], 4, &mut bytes)?;
        assert_eq!(bytes, 48);
        assert_eq!(files.external([1; 16])?, 7);
        assert_eq!(files.external([2; 16])?, 5);
        assert_eq!(files.external([3; 16])?, 12);
        Ok(())
    }

    #[test]
    fn a_new_file_is_rejected_before_exceeding_the_aggregate_bound() -> Result<(), RocksError> {
        let mut files = FileCounts::default();
        let mut bytes = MAXIMUM_BYTES - 24;
        files.add([1; 16], 3, &mut bytes)?;
        assert_eq!(bytes, MAXIMUM_BYTES);
        assert!(matches!(
            files.add([2; 16], 5, &mut bytes),
            Err(RocksError::Limit {
                resource: "insight-workspace"
            })
        ));
        assert_eq!(files.external([3; 16])?, 3);
        Ok(())
    }
}
