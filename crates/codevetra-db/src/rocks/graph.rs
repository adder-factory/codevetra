//! Typed canonical graph records and radix-partitioned packed adjacency.

use std::{mem::size_of, sync::Arc, time::Duration};

use codevetra_domain::{ContentDigest, GenerationId};
use serde::{Deserialize, Serialize};

use crate::CanonicalGenerationFacts;

use super::{ProjectStore, PruneReport, Result, RocksError, StorageRequest, StoredRevision};

pub(super) mod codec;
mod ingest;
mod lexical;
mod names;
mod packed;
mod preparation;
mod read;

pub use preparation::GraphPreparation;

pub use lexical::{LexicalPosting, LexicalStatistics, TokenPostings};
pub use read::{
    FileReferences, GraphAdjacency, GraphCursor, GraphLink, GraphNeighborPage, GraphSnapshot,
    NamedSymbols,
};

const MIB: usize = 1_048_576;
const FILE: &[u8] = b"cg1/f/";
const SYMBOL: &[u8] = b"cg1/s/";
const EDGE: &[u8] = b"cg1/e/";
const REFERENCE: &[u8] = b"cg1/r/";
const NUMERICAL: &[u8] = b"cg1/n/";
const DOCUMENT: &[u8] = b"cg1/d/";
const OUTGOING: &[u8] = b"cg1/o/";
const INCOMING: &[u8] = b"cg1/i/";
const MANIFEST: &[u8] = b"cg1/m";

/// Explicit base revision; stale canonical output can never silently rebase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphBase {
    /// Initialize a project with no published revision.
    Empty,
    /// Replace exactly this complete published revision.
    Current(GenerationId),
}

/// Complete source identity and exact publication base supplied by the indexer.
#[derive(Clone, Debug)]
pub struct GraphRevision {
    /// New immutable generation identity.
    pub generation_id: GenerationId,
    /// Source and admission-policy digest captured by the native pipeline owner.
    pub source_revision: ContentDigest,
    /// Expected current graph before starting this publication.
    pub base: GraphBase,
}

/// Finite canonical-input, indexing-workspace, and durable staging bounds.
#[derive(Clone, Copy, Debug)]
pub struct GraphPolicy {
    /// Maximum Rust-owned bytes in the validated canonical input.
    pub maximum_input_bytes: u64,
    /// Temporary key indexes and adjacency entries, excluding bounded I/O buffers.
    pub maximum_working_bytes: usize,
    /// Maximum encoded bytes accumulated before a durable staging batch.
    pub maximum_batch_bytes: usize,
}

impl Default for GraphPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl GraphPolicy {
    const DEFAULT: Self = Self {
        maximum_input_bytes: 128 * 1_048_576,
        maximum_working_bytes: 32 * MIB,
        maximum_batch_bytes: 4 * MIB,
    };
    const MAXIMUM_INPUT_BYTES: u64 = 512 * 1_048_576;
    const MAXIMUM_WORKING_BYTES: usize = 512 * MIB;
    const MAXIMUM_BATCH_BYTES: usize = 16 * MIB;

    fn validate(self) -> Result<Self> {
        if self.maximum_input_bytes == 0
            || self.maximum_input_bytes > Self::MAXIMUM_INPUT_BYTES
            || !(MIB..=Self::MAXIMUM_WORKING_BYTES).contains(&self.maximum_working_bytes)
            || !(MIB..=Self::MAXIMUM_BATCH_BYTES).contains(&self.maximum_batch_bytes)
        {
            return Err(RocksError::Limit {
                resource: "graph_policy",
            });
        }
        Ok(self)
    }
}

/// Canonical facts with an immutable, fully measured admission reservation.
///
/// Construction checks the retained payload's complete size proof and workspace
/// policy. New payload measurement belongs on a supervised CPU worker. Neither
/// admission nor measurement performs database I/O.
pub struct GraphPublicationInput {
    identity: GraphRevision,
    payload: GraphPayload,
    changes: Option<ingest::FileChanges>,
    policy: GraphPolicy,
    retained_bytes: u64,
    run_excludes: Vec<String>,
    maximum_file_bytes: Option<usize>,
}

enum GraphPayload {
    Resident(Arc<CanonicalGenerationFacts>),
    Packed(packed::PackedGraph),
}

impl GraphPublicationInput {
    /// Actual retained input reservation, including source-admission allocations.
    #[must_use]
    pub fn retained_input_bytes(&self) -> u64 {
        self.retained_bytes
    }

    pub(in crate::rocks) fn reclaimable_bytes(&self) -> Result<u32> {
        match &self.payload {
            GraphPayload::Resident(_) => Ok(0),
            GraphPayload::Packed(graph) => {
                u32::try_from(graph.reclaimable_bytes()?).map_err(|_| limit())
            }
        }
    }

    /// Retain the already admitted canonical payload in the same project owner.
    /// No second copy of the file, reference, or document tables is allocated.
    #[must_use]
    pub fn shared_facts(&self) -> Option<Arc<CanonicalGenerationFacts>> {
        match &self.payload {
            GraphPayload::Resident(facts) => Some(facts.clone()),
            GraphPayload::Packed(_) => None,
        }
    }

    /// Exact canonical counts before the input moves to a storage worker.
    ///
    /// # Errors
    /// Rejects a count that cannot be represented in the durable manifest.
    pub fn info(&self) -> Result<GraphInfo> {
        let facts = match &self.payload {
            GraphPayload::Resident(facts) => facts,
            GraphPayload::Packed(graph) => return Ok(graph.info.clone()),
        };
        Ok(GraphInfo {
            format_version: 1,
            name_index_version: names::version(facts, self.policy),
            content_digest: facts.digest().clone(),
            files: count(facts.files().len())?,
            symbols: count(facts.symbols().len())?,
            edges: count(facts.edges().len())?,
            references: count(facts.references().len())?,
            numerical_sites: count(facts.numerical_sites().len())?,
            documents: count(facts.documents().len())?,
        })
    }

    /// Canonical source manifest for the owner's pre-publication source fence.
    #[must_use]
    pub fn files(&self) -> &[crate::FileInput] {
        match &self.payload {
            GraphPayload::Resident(facts) => facts.files(),
            GraphPayload::Packed(graph) => &graph.files,
        }
    }

    /// Exact run policy retained by this input for publication and source verification.
    #[must_use]
    pub fn source_admission(&self) -> (&[String], Option<usize>) {
        (&self.run_excludes, self.maximum_file_bytes)
    }

    /// Retain the exact run admission so later freshness checks use the same source set.
    /// # Errors
    /// Rejects excessive patterns, invalid text, or unsupported source-file bounds.
    pub fn with_source_admission(
        mut self,
        run_excludes: Vec<String>,
        maximum_file_bytes: Option<usize>,
    ) -> Result<Self> {
        let retained = |patterns: &Vec<String>| -> Option<usize> {
            patterns.iter().try_fold(
                patterns.capacity().checked_mul(size_of::<String>())?,
                |sum, pattern| sum.checked_add(pattern.capacity()),
            )
        };
        let bytes = retained(&run_excludes);
        if run_excludes.len() > 4096
            || bytes.is_none_or(|bytes| bytes > 4 * MIB)
            || run_excludes
                .iter()
                .any(|pattern| !(1..=4096).contains(&pattern.len()) || pattern.contains('\0'))
            || maximum_file_bytes.is_some_and(|bytes| !(1..=32 * MIB).contains(&bytes))
        {
            return Err(RocksError::InvalidInput {
                field: "source_admission",
            });
        }
        let old =
            u64::try_from(retained(&self.run_excludes).ok_or_else(limit)?).map_err(|_| limit())?;
        let added = u64::try_from(bytes.ok_or_else(limit)?).map_err(|_| limit())?;
        self.retained_bytes = self
            .retained_bytes
            .checked_sub(old)
            .and_then(|bytes| bytes.checked_add(added))
            .ok_or_else(limit)?;
        if self.retained_bytes > self.policy.maximum_input_bytes {
            return Err(limit());
        }
        self.run_excludes = run_excludes;
        self.maximum_file_bytes = maximum_file_bytes;
        Ok(self)
    }

    pub(super) fn reservation_bytes(&self) -> Result<u32> {
        // Includes old pages/records, new encoding, native batch copies, and
        // the retained canonical payload. No producer may enqueue without its
        // enclosing pipeline reservation while waiting for this admission.
        self.retained_bytes
            .checked_add(u64::try_from(self.policy.maximum_working_bytes).map_err(|_| limit())?)
            .and_then(|bytes| bytes.checked_add(96 * 1_048_576))
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(self.policy.maximum_batch_bytes)
                        .ok()?
                        .checked_mul(4)?,
                )
            })
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(limit)
    }
}

/// Exact canonical row counts and physical graph-format identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphInfo {
    /// Version of the typed records and compact adjacency representation.
    pub format_version: u8,
    /// Exact-name postings format; zero requires bounded scan fallback.
    /// Older manifests omit this optional derived capability.
    #[serde(default)]
    pub name_index_version: u8,
    /// Complete canonical digest, independent of physical adjacency encoding.
    pub content_digest: ContentDigest,
    /// Source files in this complete revision.
    pub files: u64,
    /// Declared symbols in this complete revision.
    pub symbols: u64,
    /// Canonical relationships, preserving distinct provenance.
    pub edges: u64,
    /// Resolved and unresolved source-reference rows.
    pub references: u64,
    /// Static numerical evidence rows.
    pub numerical_sites: u64,
    /// Durable canonical documents; a native lexical index is a separate capability.
    pub documents: u64,
}

/// Fixed-size accounting for one complete incremental replacement.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct GraphWriteReport {
    /// Candidate canonical records and packed adjacency blocks compared.
    pub candidate_records: u64,
    /// Existing equal records requiring no write or staging claim.
    pub reused_records: u64,
    /// New or changed logical values staged durably.
    pub written_records: u64,
    /// Removed records and obsolete adjacency blocks tombstoned.
    pub deleted_records: u64,
    /// Packed adjacency blocks in both directions.
    pub adjacency_blocks: u64,
    /// Largest number of canonical relationships adjacent to a symbol.
    pub maximum_degree: u64,
}

/// Retention is reported independently from an already committed publication.
pub enum GraphRetention {
    /// One bounded resumable cleanup batch completed.
    Completed(PruneReport),
    /// The graph committed; maintenance needs a later bounded attempt.
    Deferred(RocksError),
}

/// A complete durable graph revision and its incremental-write accounting.
pub struct GraphPublication {
    /// Actual committed current revision.
    pub revision: StoredRevision,
    /// Comparison and adjacency accounting.
    pub report: GraphWriteReport,
    /// Bounded maintenance outcome; never changes publication success.
    pub retention: GraphRetention,
}

/// Primary publication failure with independently reported cleanup diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{primary}")]
pub struct GraphPublicationError {
    /// The original request, fence, validation, or storage failure.
    #[source]
    pub primary: RocksError,
    /// Cleanup could not finish; this never replaces the primary failure.
    pub cleanup_failure: Option<RocksError>,
}

impl From<RocksError> for GraphPublicationError {
    fn from(primary: RocksError) -> Self {
        Self {
            primary,
            cleanup_failure: None,
        }
    }
}

fn limit() -> RocksError {
    RocksError::Limit {
        resource: "graph_working_bytes",
    }
}

fn count(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| limit())
}

fn check_workspace<T>(items: usize, per_item_bytes: usize, policy: GraphPolicy) -> Result<()> {
    let bytes = items
        .checked_mul(size_of::<T>().saturating_add(per_item_bytes))
        .ok_or_else(limit)?;
    if bytes > policy.maximum_working_bytes {
        return Err(limit());
    }
    Ok(())
}

impl ProjectStore {
    /// Replace a complete typed graph using only changed records and blocks.
    ///
    /// The caller supplies complete canonical facts from the existing native
    /// resolver, including cross-file changes. This synchronous method belongs
    /// on a storage worker. Readers continue using the old complete snapshot
    /// until all records and both adjacency directions are durably published.
    ///
    /// # Errors
    /// Rejects a stale base, format mismatch, bound, request, or storage failure.
    /// Incomplete staged work stays invisible. The primary failure and any
    /// secondary cleanup failure are reported independently.
    pub fn publish_graph(
        &self,
        input: GraphPublicationInput,
        request: &StorageRequest,
    ) -> std::result::Result<GraphPublication, GraphPublicationError> {
        self.publish_graph_reclaiming(input, request, |_| Ok(()))
    }

    pub(in crate::rocks) fn publish_graph_reclaiming(
        &self,
        input: GraphPublicationInput,
        request: &StorageRequest,
        reclaim: impl FnMut(usize) -> Result<()>,
    ) -> std::result::Result<GraphPublication, GraphPublicationError> {
        input.publish(self, request, reclaim)
    }
}
