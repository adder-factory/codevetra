//! Sparse publication requires the exact validated base; topology stays shared.

use std::collections::{BTreeMap, BTreeSet};

use codevetra_domain::{ContentDigest, FileId};

use super::super::{CanonicalGenerationFacts, GraphPolicy, Result, RocksError, StorageRequest};
use super::super::{DOCUMENT, FILE, NUMERICAL, REFERENCE, SYMBOL};
use super::{Collection, Delta, Row, codec};

type Change = (Collection, RecordKey, Option<Vec<u8>>);
use crate::rocks::RecordKey;

type Rows = BTreeMap<(Collection, RecordKey), Vec<u8>>;

pub(in crate::rocks::graph) struct FileChanges {
    base_digest: ContentDigest,
    affected: BTreeSet<FileId>,
    old: Rows,
    changes: Vec<Change>,
    candidates: u64,
    reused: u64,
}

impl FileChanges {
    pub(in crate::rocks::graph) fn finish(
        &mut self,
        facts: &CanonicalGenerationFacts,
        policy: GraphPolicy,
        request: &StorageRequest,
    ) -> Result<()> {
        let occupied = measured(&self.old)?
            .checked_add(retained_overhead(&self.affected, &self.base_digest)?)
            .ok_or_else(super::super::limit)?;
        let remaining = policy
            .maximum_working_bytes
            .checked_sub(occupied)
            .ok_or_else(super::super::limit)?;
        let next = RowCollector::new(facts, request, remaining).collect(&self.affected)?;
        let descriptors = self
            .old
            .len()
            .checked_add(next.len())
            .ok_or_else(super::super::limit)?;
        let descriptor_bytes = descriptors
            .checked_mul(std::mem::size_of::<Change>())
            .ok_or_else(super::super::limit)?;
        if measured(&next)?
            .checked_add(descriptor_bytes)
            .is_none_or(|bytes| bytes > remaining)
        {
            return Err(super::super::limit());
        }
        self.candidates = super::super::count(next.len())?;
        self.changes
            .try_reserve_exact(descriptors)
            .map_err(|_| super::super::limit())?;
        for ((collection, key), value) in next {
            request.check()?;
            if self
                .old
                .remove(&(collection, key.clone()))
                .is_some_and(|old| old == value)
            {
                self.reused += 1;
            } else {
                self.changes.push((collection, key, Some(value)));
            }
        }
        for ((collection, key), _) in std::mem::take(&mut self.old) {
            request.check()?;
            self.changes.push((collection, key, None));
        }
        Ok(())
    }

    pub(in crate::rocks::graph) fn retained_bytes(&self) -> Result<u64> {
        let bytes = self
            .changes
            .iter()
            .try_fold(
                self.changes
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Change>()),
                |sum, (_, key, value)| {
                    sum.checked_add(key.as_bytes().len())?
                        .checked_add(value.as_ref().map_or(0, Vec::capacity))
                },
            )
            .ok_or_else(super::super::limit)?;
        let bytes = bytes
            .checked_add(retained_overhead(&self.affected, &self.base_digest)?)
            .ok_or_else(super::super::limit)?;
        super::super::count(bytes)
    }

    pub(super) fn apply(&self, delta: &mut Delta<'_, '_>) -> Result<()> {
        let previous = delta
            .publication
            .previous
            .ok_or(RocksError::GenerationChanged)?;
        if previous.revision().content_digest.as_ref() != Some(&self.base_digest) {
            return Err(RocksError::GenerationChanged);
        }
        delta.report.candidate_records += self.candidates;
        delta.report.reused_records += self.reused;
        for (collection, key, value) in &self.changes {
            delta.publication.request.check()?;
            if let Some(value) = value {
                delta.push(*collection, key.clone(), Some(value.clone()))?;
                delta.report.written_records += 1;
            } else {
                delta.remove(*collection, key.clone())?;
            }
        }
        Ok(())
    }
}

/// One sparse capture owns its retained rows, cancellation scope, and accounting ceiling.
pub(in crate::rocks::graph) struct RowCollector<'a> {
    facts: &'a CanonicalGenerationFacts,
    request: &'a StorageRequest,
    maximum_bytes: usize,
    bytes: usize,
    rows: Rows,
}

impl<'a> RowCollector<'a> {
    pub(in crate::rocks::graph) fn new(
        facts: &'a CanonicalGenerationFacts,
        request: &'a StorageRequest,
        maximum_bytes: usize,
    ) -> Self {
        Self {
            facts,
            request,
            maximum_bytes,
            bytes: 0,
            rows: Rows::new(),
        }
    }

    pub(in crate::rocks::graph) fn capture(
        mut self,
        affected: BTreeSet<FileId>,
    ) -> Result<FileChanges> {
        let facts = self.facts;
        if affected.is_empty() || affected.len() > 16 {
            return Err(super::super::limit());
        }
        let overhead = retained_overhead(&affected, facts.digest())?;
        self.maximum_bytes = self
            .maximum_bytes
            .checked_sub(overhead)
            .ok_or_else(super::super::limit)?;
        let old = self.collect(&affected)?;
        Ok(FileChanges {
            base_digest: facts.digest().clone(),
            affected,
            old,
            changes: Vec::new(),
            candidates: 0,
            reused: 0,
        })
    }

    fn collect(mut self, affected: &BTreeSet<FileId>) -> Result<Rows> {
        let tables = [
            (FILE, self.facts.files().len()),
            (SYMBOL, self.facts.symbols().len()),
            (NUMERICAL, self.facts.numerical_sites().len()),
            (DOCUMENT, self.facts.documents().len()),
        ];
        for (prefix, length) in tables {
            for index in 0..length {
                self.request.check()?;
                let row = super::row_at(self.facts, prefix, index)?;
                let file = match row {
                    Row::File(row) => Some(&row.file_id),
                    Row::Symbol(row) => Some(&row.file_id),
                    Row::Reference(row) => Some(&row.file_id),
                    Row::Numerical(row) => Some(&row.file_id),
                    Row::Document(row) => row.file_id(),
                    Row::Edge(_) => None,
                };
                if file.is_none_or(|id| !affected.contains(id)) {
                    continue;
                }
                self.append(prefix, index)?;
            }
        }
        // Canonical reference order starts with file_id. Sparse edits can seek
        // directly to each complete file range without visiting other references.
        for file in affected {
            self.request.check()?;
            let start = self
                .facts
                .references()
                .partition_point(|row| row.file_id < *file);
            let end = self
                .facts
                .references()
                .partition_point(|row| row.file_id <= *file);
            for index in start..end {
                self.append(REFERENCE, index)?;
            }
        }
        Ok(self.rows)
    }

    fn append(&mut self, prefix: &[u8], index: usize) -> Result<()> {
        self.request.check()?;
        let (key, row) = super::CanonicalTable::new(self.facts, prefix)?.row(index)?;
        let value = row.encode()?;
        self.bytes = self
            .bytes
            .checked_add(key.as_bytes().len())
            .and_then(|n| n.checked_add(value.capacity()))
            .and_then(|n| n.checked_add(256))
            .ok_or_else(super::super::limit)?;
        if self.bytes > self.maximum_bytes {
            return Err(super::super::limit());
        }
        self.rows.insert(
            (
                if prefix == DOCUMENT {
                    Collection::Search
                } else {
                    Collection::Graph
                },
                key,
            ),
            value,
        );
        Ok(())
    }
}

fn measured(rows: &Rows) -> Result<usize> {
    rows.iter()
        .try_fold(0_usize, |sum, ((_, key), value)| {
            sum.checked_add(256)?
                .checked_add(key.as_bytes().len())?
                .checked_add(value.capacity())
        })
        .ok_or_else(codec::corrupt)
}

fn retained_overhead(affected: &BTreeSet<FileId>, digest: &ContentDigest) -> Result<usize> {
    affected
        .len()
        .checked_mul(256)
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<FileChanges>()))
        .and_then(|bytes| bytes.checked_add(digest.as_str().len()))
        .ok_or_else(super::super::limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GenerationFacts, GenerationValidationLimits, validate_generation_facts};
    use std::{error::Error, time::Duration};

    #[test]
    fn deletion_descriptors_are_admitted_before_reserving_the_vector()
    -> std::result::Result<(), Box<dyn Error>> {
        let (facts, _) = validate_generation_facts(
            GenerationFacts::default(),
            GenerationValidationLimits::new(1_048_576, 2_097_152)?,
            || false,
        )?;
        let mut changes = FileChanges {
            base_digest: facts.digest().clone(),
            affected: BTreeSet::from([FileId::from_uuid_v8([1; 16])]),
            old: BTreeMap::from([(
                (Collection::Graph, RecordKey::new(b"old".to_vec())?),
                vec![0; 100],
            )]),
            changes: Vec::new(),
            candidates: 0,
            reused: 0,
        };
        let occupied =
            measured(&changes.old)? + retained_overhead(&changes.affected, &changes.base_digest)?;
        let result = changes.finish(
            &facts,
            GraphPolicy {
                maximum_working_bytes: occupied,
                ..GraphPolicy::default()
            },
            &StorageRequest::new(Duration::from_secs(10))?,
        );
        assert!(matches!(result, Err(RocksError::Limit { .. })));
        assert_eq!(changes.changes.capacity(), 0);
        assert_eq!(changes.old.len(), 1);
        Ok(())
    }
}
