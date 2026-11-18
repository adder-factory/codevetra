//! Native vector audit and bounded cleanup, without external catalog operations.

use super::{
    NAMESPACE,
    vectors::{SourceReader, corrupt, documents_scan},
};
use crate::semantic::{
    maintenance::{EmbeddingStorageAudit, RetiredEmbeddingCleanupReport},
    types::{
        EmbeddingModelSelector, EmbeddingModelState, RegisteredEmbeddingModel,
        SemanticStorageError as Error,
    },
};
use crate::{
    NativeDatabase,
    native_database::visit_state,
    rocks::{ReadBudget, RecordKey, StateTransaction},
};
use codevetra_domain::{DocumentId, ProjectId};
use std::{ops::ControlFlow, time::Duration};

const RETIRED_WRITE_BYTES: u32 = 4 * 1_048_576;
const RETIRED_WRITE_ROWS: u32 = 10_001;
const MAXIMUM_RETIRED_VECTORS: usize = 256;
const RETIRED_SCAN_PAGE: u16 = 64;

impl NativeDatabase {
    /// Audit active, obsolete, and retired binary vectors against current source.
    /// # Errors
    /// Rejects corrupt identities/vectors, project mismatch, or bounded read exhaustion.
    pub async fn embedding_storage_audit(
        &self,
        project: &ProjectId,
        timeout: Duration,
    ) -> Result<EmbeddingStorageAudit, Error> {
        self.require_project(project)?;
        self.transaction(
            crate::rocks::StateAccess::read_only(None),
            timeout,
            |transaction, budget| {
                let mut report = EmbeddingStorageAudit {
                    active_models: 0,
                    retired_models: 0,
                    current_documents: 0,
                    current_embeddings: 0,
                    historical_embeddings: 0,
                    retired_model_embeddings: 0,
                    model_indexes: 0,
                };
                let mut models = Vec::new();
                visit_state(
                    &transaction.scan(NAMESPACE, b"m/"),
                    budget,
                    |_, model: RegisteredEmbeddingModel, _| {
                        if model.state == EmbeddingModelState::Active {
                            report.active_models += 1;
                        } else {
                            report.retired_models += 1;
                        }
                        models.push(model);
                        if models.len() > 256 {
                            return Err(crate::rocks::RocksError::Limit {
                                resource: "embedding_models",
                            });
                        }
                        Ok(())
                    },
                )?;
                let current = transaction.current()?;
                let all = report.audit_vectors(transaction, budget, &models)?;
                if current.is_some() {
                    let snapshot = transaction.graph_snapshot()?;
                    let graph = snapshot.graph(budget)?;
                    documents_scan(&graph, budget, |_, document, budget| {
                        report.current_documents += 1;
                        let id = document.document_id.clone();
                        let (_, digest) =
                            SourceReader::new(transaction, &graph).render(document, budget)?;
                        for model in &models {
                            report.current_embeddings += u64::from(
                                SourceReader::new(transaction, &graph)
                                    .model(&model.selector)
                                    .read(&id, &digest, budget)?
                                    .is_some(),
                            );
                        }
                        Ok(ControlFlow::Continue(()))
                    })?;
                }
                report.historical_embeddings = all
                    .checked_sub(report.current_embeddings)
                    .ok_or(corrupt())?;
                Ok(report)
            },
        )
        .await
    }

    /// Delete at most 256 vectors from explicitly retired models per call.
    /// # Errors
    /// Rejects corrupt models, project mismatch, or bounded native read/write failure.
    pub async fn cleanup_retired_embeddings(
        &self,
        project: &ProjectId,
        confirm: bool,
        timeout: Duration,
    ) -> Result<RetiredEmbeddingCleanupReport, Error> {
        self.require_project(project)?;
        self.transaction(
            crate::rocks::StateAccess::write(None, RETIRED_WRITE_BYTES, RETIRED_WRITE_ROWS),
            timeout,
            move |transaction, budget| {
                let mut retired = Vec::new();
                visit_state(
                    &transaction.scan(NAMESPACE, b"m/"),
                    budget,
                    |_, model: RegisteredEmbeddingModel, _| {
                        if model.state == EmbeddingModelState::Retired {
                            retired.push(model.selector);
                        }
                        if retired.len() > 256 {
                            return Err(crate::rocks::RocksError::Limit {
                                resource: "embedding_models",
                            });
                        }
                        Ok(())
                    },
                )?;
                let RetiredCandidates {
                    keys,
                    candidates,
                    incomplete,
                } = retired_candidates(transaction, budget, &retired)?;
                let deleted = if confirm { keys.len() as u64 } else { 0 };
                if confirm {
                    delete_retired_vectors(transaction, budget, keys)?;
                }

                Ok(RetiredEmbeddingCleanupReport {
                    dry_run: !confirm,
                    candidate_embeddings: candidates,
                    deleted_embeddings: deleted,
                    dropped_model_indexes: 0,
                    incomplete,
                })
            },
        )
        .await
    }
}

fn delete_retired_vectors(
    transaction: &mut StateTransaction<'_>,
    budget: &mut ReadBudget,
    keys: Vec<RecordKey>,
) -> Result<(), Error> {
    let has_deletions = !keys.is_empty();
    for key in keys {
        transaction.delete(NAMESPACE, key.as_bytes())?;
        crate::native_database::retention::forget_orphan(transaction, "vector", key.as_bytes())?;
    }
    if has_deletions {
        crate::artifacts::native::bump_epoch(transaction, budget, b"vector_epoch")?;
    }
    Ok(())
}

impl EmbeddingStorageAudit {
    fn audit_vectors(
        &mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        models: &[RegisteredEmbeddingModel],
    ) -> Result<u64, Error> {
        let mut all = 0_u64;
        for model in models {
            let prefix = format!("v/{}/", model.selector.model_id);
            let mut cursor = None;
            loop {
                let page = transaction.scan(NAMESPACE, prefix.as_bytes()).page(
                    cursor.as_ref().map(crate::rocks::RecordKey::as_bytes),
                    256,
                    budget,
                )?;
                for (key, bytes) in page.rows {
                    let text = std::str::from_utf8(key.as_bytes()).map_err(|_| corrupt())?;
                    DocumentId::parse(text.strip_prefix(&prefix).ok_or(corrupt())?)
                        .map_err(|_| corrupt())?;
                    super::vectors::decode_vector(&bytes, model.selector.dimension)?;
                    all += 1;
                    self.retired_model_embeddings +=
                        u64::from(model.state == EmbeddingModelState::Retired);
                }
                cursor = page.next_after;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(all)
    }
}

#[derive(Default)]
struct RetiredCandidates {
    keys: Vec<RecordKey>,
    candidates: u64,
    incomplete: bool,
}

fn retired_candidates(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    retired: &[EmbeddingModelSelector],
) -> Result<RetiredCandidates, Error> {
    let mut output = RetiredCandidates::default();
    for model in retired {
        if output.scan_model(transaction, budget, model)? {
            output.incomplete = true;
            break;
        }
    }
    Ok(output)
}

impl RetiredCandidates {
    fn scan_model(
        &mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        model: &EmbeddingModelSelector,
    ) -> Result<bool, Error> {
        let prefix = format!("v/{}/", model.model_id);
        let mut cursor = None;
        loop {
            let page = transaction.scan(NAMESPACE, prefix.as_bytes()).page(
                cursor.as_ref().map(RecordKey::as_bytes),
                RETIRED_SCAN_PAGE,
                budget,
            )?;
            self.candidates += page.rows.len() as u64;
            for (key, _) in page.rows {
                if self.keys.len() < MAXIMUM_RETIRED_VECTORS {
                    self.keys.push(key);
                }
            }
            cursor = page.next_after;
            if self.keys.len() >= MAXIMUM_RETIRED_VECTORS {
                return Ok(true);
            }
            if cursor.is_none() {
                return Ok(false);
            }
        }
    }
}
