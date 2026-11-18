//! Bounded cold-orphan collection in the same snapshot as the current graph.

mod policy;

use super::{NativeDatabase, decode, storage_error};
use crate::{
    AgentArtifactKind, AgentArtifactScope, RegisteredEmbeddingModel, StorageError,
    artifacts::native::{decode_primary, remove},
    retrieval::native::visit_files,
    rocks::{
        GraphSnapshot, ReadBudget, RocksError, StateNamespace as Ns, StateTransaction,
        StorageRequest,
    },
    semantic::native::{feature_error, vectors},
};
use codevetra_domain::{DocumentId, GenerationId, ModelId};
use serde::Serialize;
use std::collections::BTreeSet;

const PAGE: u16 = 64;
const MAXIMUM_WRITE_BYTES: u32 = 4 * 1_048_576;
const MAXIMUM_WRITE_ROWS: u32 = 1024;

/// Committed, bounded progress; continuation requires another maintenance call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeDerivedPruneReport {
    /// Physical queue/vector/artifact records inspected in this batch.
    pub scanned: u32,
    /// Obsolete generation demand removed immediately.
    pub queue_rows_removed: u32,
    /// Cold source-incompatible vectors removed after their grace period.
    pub vectors_removed: u32,
    /// Cold role/summary records removed with their UUID and scope indexes.
    pub artifacts_removed: u32,
    /// Newly observed orphans retained for their ten minute grace period.
    pub grace_started: u32,
    /// At least one namespace has an unvisited continuation page.
    pub incomplete: bool,
}

impl NativeDatabase {
    /// Collect at most 64 records per derived family under the caller's control.
    /// Notes and session records are never automatic deletion candidates.
    /// # Errors
    /// Fails closed on stopped requests, malformed state, or atomic write failure.
    pub async fn prune_derived_state_with_request(
        &self,
        request: &StorageRequest,
    ) -> Result<NativeDerivedPruneReport, StorageError> {
        request.check().map_err(storage_error)?;
        self.state_with_request(
            crate::rocks::StateAccess::write(None, MAXIMUM_WRITE_BYTES, MAXIMUM_WRITE_ROWS),
            request,
            |tx, budget| {
                let timestamp = jiff::Timestamp::now().as_second();
                collect(tx, budget, timestamp)
            },
        )
        .await
    }
}

fn collect(
    tx: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    timestamp: i64,
) -> Result<NativeDerivedPruneReport, RocksError> {
    let Some(current) = tx.current()? else {
        return Ok(NativeDerivedPruneReport::default());
    };
    let snapshot = tx.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let mut collector = Collector {
        tx,
        budget,
        timestamp,
        report: NativeDerivedPruneReport::default(),
        cursors: Vec::new(),
    };
    let queue = collector.page(Ns::Derived, b"q/", b"gc/queue_cursor")?;
    let vectors = collector.page(Ns::Embeddings, b"v/", b"gc/vector_cursor")?;
    let artifacts = collector.page(Ns::Artifacts, b"a/", b"gc/artifact_cursor")?;
    collector.collect_queue(&current.generation_id, queue)?;
    collector.collect_vectors(&graph, vectors)?;
    collector.collect_artifacts(&graph, &current.generation_id, artifacts)?;
    for (key, next) in collector.cursors {
        if let Some(next) = next {
            collector.tx.put(Ns::Metadata, &key, next)?;
        } else {
            collector.tx.delete(Ns::Metadata, &key)?;
        }
    }
    Ok(collector.report)
}

type Rows = Vec<(crate::rocks::RecordKey, Vec<u8>)>;
type CursorUpdates = Vec<(Vec<u8>, Option<Vec<u8>>)>;

#[derive(Clone, Copy)]
struct OrphanCandidate<'a> {
    family: &'a str,
    key: &'a [u8],
    bytes: &'a [u8],
    incompatible: bool,
}

pub(crate) fn forget_orphan(
    tx: &mut StateTransaction<'_>,
    family: &str,
    key: &[u8],
) -> Result<(), RocksError> {
    let text = std::str::from_utf8(key).map_err(|_| corrupt())?;
    tx.delete(
        Ns::Metadata,
        format!("gc/orphan/{family}/{text}").as_bytes(),
    )
}

struct Collector<'request, 'store> {
    tx: &'request mut StateTransaction<'store>,
    budget: &'request mut ReadBudget,
    timestamp: i64,
    report: NativeDerivedPruneReport,
    cursors: CursorUpdates,
}

impl<'store> Collector<'_, 'store> {
    fn page(&mut self, namespace: Ns, prefix: &[u8], cursor: &[u8]) -> Result<Rows, RocksError> {
        let after = self.tx.get(Ns::Metadata, cursor, self.budget)?;
        let page = self
            .tx
            .scan(namespace, prefix)
            .page(after.as_deref(), PAGE, self.budget)?;
        self.report.scanned += u32::try_from(page.rows.len()).map_err(|_| corrupt())?;
        self.report.incomplete |= page.next_after.is_some();
        self.cursors.push((
            cursor.to_vec(),
            page.next_after.map(|key| key.as_bytes().to_vec()),
        ));
        Ok(page.rows)
    }
    fn collect_queue(&mut self, generation: &GenerationId, rows: Rows) -> Result<(), RocksError> {
        let current = format!("q/{generation}/");
        for (key, _) in rows {
            if !key.as_bytes().starts_with(current.as_bytes()) {
                self.tx.delete(Ns::Derived, key.as_bytes())?;
                self.report.queue_rows_removed += 1;
            }
        }
        Ok(())
    }
    fn cold(&mut self, candidate: OrphanCandidate<'_>) -> Result<bool, RocksError> {
        let OrphanCandidate {
            family,
            key,
            bytes,
            incompatible,
        } = candidate;
        let text = std::str::from_utf8(key).map_err(|_| corrupt())?;
        let marker = format!("gc/orphan/{family}/{text}");
        let first = self.tx.get(Ns::Metadata, marker.as_bytes(), self.budget)?;
        if !incompatible {
            if first.is_some() {
                self.tx.delete(Ns::Metadata, marker.as_bytes())?;
            }
            return Ok(false);
        }
        let fingerprint = blake3::hash(bytes);
        let unchanged = first
            .as_deref()
            .filter(|stored| stored.len() == 40 && stored[8..] == fingerprint.as_bytes()[..]);
        let Some(first) = unchanged else {
            let mut marker_value = self.timestamp.to_be_bytes().to_vec();
            marker_value.extend_from_slice(fingerprint.as_bytes());
            self.tx.put(Ns::Metadata, marker.as_bytes(), marker_value)?;
            self.report.grace_started += 1;
            return Ok(false);
        };
        let first = i64::from_be_bytes(first[..8].try_into().map_err(|_| corrupt())?);
        if !policy::grace_elapsed(self.timestamp, first) {
            return Ok(false);
        }
        self.tx.delete(Ns::Metadata, marker.as_bytes())?;
        Ok(true)
    }
    fn collect_vectors(
        &mut self,
        graph: &GraphSnapshot<'_, 'store>,
        rows: Rows,
    ) -> Result<(), RocksError> {
        for (key, bytes) in rows {
            self.budget.checkpoint()?;
            let key_text = std::str::from_utf8(key.as_bytes()).map_err(|_| corrupt())?;
            let mut parts = key_text.split('/');
            if parts.next() != Some("v") {
                return Err(corrupt());
            }
            let model_id = ModelId::parse(parts.next().ok_or(corrupt())?).map_err(|_| corrupt())?;
            let document_id =
                DocumentId::parse(parts.next().ok_or(corrupt())?).map_err(|_| corrupt())?;
            if parts.next().is_some() {
                return Err(corrupt());
            }
            let model: RegisteredEmbeddingModel = self
                .tx
                .get(
                    Ns::Embeddings,
                    format!("m/{model_id}").as_bytes(),
                    self.budget,
                )?
                .map(|bytes| decode(&bytes))
                .transpose()?
                .ok_or(corrupt())?;
            if model.selector.model_id != model_id {
                return Err(corrupt());
            }
            let (digest, _) =
                vectors::decode_vector(&bytes, model.selector.dimension).map_err(semantic_error)?;
            let incompatible = if let Some(document) = graph.document(&document_id, self.budget)? {
                let (_, expected) = vectors::SourceReader::new(self.tx, graph)
                    .render(document, self.budget)
                    .map_err(semantic_error)?;
                digest != expected
            } else {
                true
            };
            if self.cold(OrphanCandidate {
                family: "vector",
                key: key.as_bytes(),
                bytes: &bytes,
                incompatible,
            })? {
                self.tx.delete(Ns::Embeddings, key.as_bytes())?;
                self.report.vectors_removed += 1;
            }
        }
        // Every removed vector was already excluded by the current source digest.
        // Reclaiming those bytes does not change current similarity or biomarker inputs.
        Ok(())
    }
    fn collect_artifacts(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        generation: &GenerationId,
        rows: Rows,
    ) -> Result<(), RocksError> {
        let (missing_files, missing_modules) = self.missing_scopes(graph, &rows)?;
        for (key, bytes) in rows {
            let record = decode_primary(key.as_bytes(), &bytes)?;
            if !matches!(
                record.kind(),
                AgentArtifactKind::Role | AgentArtifactKind::Summary
            ) {
                continue;
            }
            // A live file/module scope has only one indexed role/summary, which can
            // be refreshed in place. Keep it until the scope itself disappears.
            let compatible = match record.scope() {
                AgentArtifactScope::Symbol => {
                    record.is_current(Some(generation), Some(graph), self.budget)?
                }
                AgentArtifactScope::File => !missing_files.contains(record.scope_key()),
                AgentArtifactScope::Module => !missing_modules.contains(record.scope_key()),
                AgentArtifactScope::Project | AgentArtifactScope::Session => true,
            };
            if self.cold(OrphanCandidate {
                family: "artifact",
                key: key.as_bytes(),
                bytes: &bytes,
                incompatible: !compatible,
            })? {
                remove(self.tx, self.budget, record.id())?;
                self.report.artifacts_removed += 1;
            }
        }
        Ok(())
    }
    fn missing_scopes(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        rows: &Rows,
    ) -> Result<(BTreeSet<String>, BTreeSet<String>), RocksError> {
        // Retain only the scopes in this bounded page, never the project's full
        // path inventory. Notes and sessions cannot trigger a source scan.
        let mut missing_files = BTreeSet::new();
        let mut missing_modules = BTreeSet::new();
        for (key, bytes) in rows {
            let record = decode_primary(key.as_bytes(), bytes)?;
            if matches!(
                record.kind(),
                AgentArtifactKind::Role | AgentArtifactKind::Summary
            ) {
                match record.scope() {
                    AgentArtifactScope::File => {
                        missing_files.insert(record.scope_key().to_owned());
                    }
                    AgentArtifactScope::Module => {
                        missing_modules.insert(record.scope_key().to_owned());
                    }
                    _ => {}
                }
            }
        }
        if !missing_files.is_empty() || !missing_modules.is_empty() {
            visit_files(graph, self.budget, |file, _| {
                missing_files.remove(&file.normalized_path);
                let module = file
                    .normalized_path
                    .rsplit_once('/')
                    .map_or(".", |(parent, _)| parent);
                missing_modules.remove(module);
                Ok(())
            })?;
        }
        Ok((missing_files, missing_modules))
    }
}

fn semantic_error(error: crate::SemanticStorageError) -> RocksError {
    match feature_error(error) {
        StorageError::CorruptStoredValue { field } => RocksError::Corrupt { field },
        _ => RocksError::Operation {
            operation: "orphan-vector-source",
        },
    }
}
fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "derived-retention",
    }
}

#[cfg(test)]
mod tests;
