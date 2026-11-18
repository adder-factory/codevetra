//! Reuse native index blocks within one finite batch or immutable graph view.

use rocksdb::DBRawIterator;
use std::{collections::BTreeMap, sync::Mutex};

use super::{
    Arc, BoundColumnFamily, Collection, ReadBudget, ReadOptions, RecordKey, Result,
    RevisionSnapshot, RocksError, Snapshot, decode_value, is_published, key, operation,
};

#[cfg(test)]
mod tests;

/// At most four native readers borrow the exact graph view that owns them.
pub(in crate::rocks) struct PointReaders<'view, 'store> {
    snapshot: &'view RevisionSnapshot<'store>,
    readers: Mutex<BTreeMap<Collection, DBRawIterator<'view>>>,
}

impl<'store> RevisionSnapshot<'store> {
    pub(in crate::rocks) fn point_readers(&self) -> PointReaders<'_, 'store> {
        PointReaders {
            snapshot: self,
            readers: Mutex::new(BTreeMap::new()),
        }
    }
}

impl PointReaders<'_, '_> {
    pub(in crate::rocks) fn get(
        &self,
        collection: Collection,
        key: &RecordKey,
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<u8>>> {
        budget.checkpoint()?;
        let snapshot = self.snapshot;
        let family = snapshot.store.family(collection)?;
        let meta = snapshot.store.meta()?;
        let mut readers = self
            .readers
            .lock()
            .map_err(|_| operation("point_reader_lock"))?;
        // No per-key upper bound survives a seek to a different key. The shared
        // reader still checks the exact encoded prefix and charges every native
        // record it visits, including an adjacent record on a missing-key seek.
        let reader = readers
            .entry(collection)
            .or_insert_with(|| snapshot.snapshot.raw_iterator_cf(&family));
        PublishedRead::new(&snapshot.snapshot, &meta, snapshot.revision.sequence)
            .read_from(reader, key, budget)
    }
}

/// Publication checks and sequence selection share the same immutable native view.
pub(super) struct PublishedRead<'view, 'store> {
    snapshot: &'view Snapshot<'store>,
    meta: &'view Arc<BoundColumnFamily<'store>>,
    sequence: u64,
}

impl<'view, 'store> PublishedRead<'view, 'store> {
    pub(super) const fn new(
        snapshot: &'view Snapshot<'store>,
        meta: &'view Arc<BoundColumnFamily<'store>>,
        sequence: u64,
    ) -> Self {
        Self {
            snapshot,
            meta,
            sequence,
        }
    }

    /// A replay must match the encoded row already present in this same view.
    pub(super) fn has_staged_replay(
        &self,
        family: &Arc<BoundColumnFamily<'_>>,
        (key, value): (&[u8], &[u8]),
        budget: &mut ReadBudget,
    ) -> Result<bool> {
        let Some(existing) = self.read_staged_row(family, key, budget)? else {
            return Ok(false);
        };
        if existing != value {
            return Err(RocksError::ReplayConflict);
        }
        Ok(true)
    }

    /// No-op claims are read from the same immutable view as their prior values.
    /// A zero base still needs this check: staged claims can precede publication.
    pub(super) fn has_unchanged_claim(
        &self,
        claim_key: &[u8],
        budget: &mut ReadBudget,
    ) -> Result<bool> {
        let Some(claim) = self.read_staged_row(self.meta, claim_key, budget)? else {
            return Ok(false);
        };
        if !claim.is_empty() {
            return Err(RocksError::Corrupt {
                field: "staging_claim",
            });
        }
        Ok(true)
    }

    fn read_staged_row(
        &self,
        family: &Arc<BoundColumnFamily<'_>>,
        key: &[u8],
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<u8>>> {
        let value = self
            .snapshot
            .get_cf(family, key)
            .map_err(|_| operation("stage_replay"))?;
        if let Some(value) = &value {
            budget.charge(key.len().saturating_add(value.len()))?;
        }
        Ok(value)
    }

    pub(super) fn read(
        &self,
        family: &Arc<BoundColumnFamily<'_>>,
        key: &RecordKey,
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<u8>>> {
        // Zero is the empty published base, including after failed staging work.
        if self.sequence == 0 {
            return Ok(None);
        }
        let mut options = ReadOptions::default();
        if let Some(upper) = key::successor(key.prefix()) {
            options.set_iterate_upper_bound(upper);
        }
        let mut iterator = self.snapshot.raw_iterator_cf_opt(family, options);
        self.read_from(&mut iterator, key, budget)
    }

    pub(super) fn read_from(
        &self,
        iterator: &mut DBRawIterator<'_>,
        key: &RecordKey,
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<u8>>> {
        budget.checkpoint()?;
        if self.sequence == 0 {
            return Ok(None);
        }
        let prefix = key.prefix();
        iterator.seek(key.at_sequence(self.sequence));
        while let Some((encoded_key, value)) = iterator.item() {
            budget.charge(encoded_key.len().saturating_add(value.len()))?;
            if !encoded_key.starts_with(&prefix) {
                break;
            }
            let (_, candidate) = key::decode(encoded_key)?;
            if is_published(self.snapshot, self.meta, candidate)? {
                return Ok(decode_value(value)?.map(<[u8]>::to_vec));
            }
            iterator.next();
        }
        iterator.status().map_err(|_| operation("read_record"))?;
        Ok(None)
    }
}
