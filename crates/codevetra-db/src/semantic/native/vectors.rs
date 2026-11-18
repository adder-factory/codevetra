//! Binary vectors are reused across generations only after exact text revalidation.

use super::{NAMESPACE, load_model};
use crate::semantic::{
    common::{DocumentSource, validate_normalization},
    types::{
        EmbeddingBatchUpsertReport, EmbeddingBatchUpsertRequest, EmbeddingIndexStatus,
        EmbeddingModelSelector, EmbeddingModelState, EmbeddingPageCursor, PendingEmbeddingDocument,
        PendingEmbeddingPage, PendingEmbeddingPageRequest, RegisteredEmbeddingModel,
        RetireEmbeddingModelRequest, SemanticReadinessReport, SemanticReadinessRequest,
        SemanticReadinessState as Ready, SemanticStorageError as Error, VectorSearchHit,
        VectorSearchRequest,
    },
};
use crate::{
    CanonicalSearchDocument, NativeDatabase,
    artifacts::native::{bump_epoch, summaries::SummaryReader},
    rocks::{GraphSnapshot, ReadBudget, StateTransaction},
};
use codevetra_domain::{ContentDigest, DocumentId, GenerationId, SymbolKind};
use std::{ops::ControlFlow, time::Duration};

pub(super) const COSINE_CHECKPOINT_INTERVAL: usize = 128;
const COSINE_ROUNDING_ALLOWANCE: f64 = 1e-9;
const MAXIMUM_COSINE_DISTANCE: f64 = 2.0;

impl NativeDatabase {
    /// Inspect the native exact-cosine reader for an exact registered model.
    /// # Errors
    /// Rejects missing/mismatched models or exhausted native read bounds.
    pub async fn embedding_model_index_status(
        &self,
        selector: &EmbeddingModelSelector,
        timeout: Duration,
    ) -> Result<EmbeddingIndexStatus, Error> {
        let model = self.registered_embedding_model(selector, timeout).await?;
        Ok(EmbeddingIndexStatus {
            model_id: selector.model_id.clone(),
            index_name: format!("native-exact-cosine-v1/{}", selector.model_id),
            algorithm: "exact-cosine",
            ready: model.state == EmbeddingModelState::Active,
        })
    }

    /// Validate the native vector reader; it requires no external index creation.
    /// # Errors
    /// Rejects missing/mismatched models or exhausted native read bounds.
    pub async fn ensure_embedding_model_index(
        &self,
        selector: &EmbeddingModelSelector,
        timeout: Duration,
    ) -> Result<EmbeddingIndexStatus, Error> {
        self.embedding_model_index_status(selector, timeout).await
    }

    /// Page exact current inputs lacking a digest-compatible native vector.
    /// # Errors
    /// Rejects changed cursors, invalid models, oversized text, or native read failure.
    pub async fn pending_current_embedding_documents(
        &self,
        request: PendingEmbeddingPageRequest,
    ) -> Result<PendingEmbeddingPage, Error> {
        self.require_project(&request.project_id)?;
        self.transaction(
            crate::rocks::StateAccess::read_only(None),
            request.statement_timeout,
            move |transaction, budget| {
                active(transaction, &request.model, budget)?;
                let generation = current(transaction)?;
                if request
                    .cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.generation_id != generation)
                {
                    return Err(Error::CurrentGenerationChanged);
                }
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let mut documents = Vec::new();
                let mut bytes = 0_u64;
                let mut more = false;
                let mut consumed = None;
                let mut examined = 0_u16;
                DocumentScan {
                    graph: &graph,
                    start: request.cursor.as_ref(),
                }
                .visit(budget, |ordinal, document, budget| {
                    if examined == 128 {
                        more = true;
                        return Ok(ControlFlow::Break(()));
                    }
                    examined += 1;
                    let id = document.document_id.clone();
                    let path = document.path.clone();
                    let (text, digest) =
                        SourceReader::new(transaction, &graph).render(document, budget)?;
                    if SourceReader::new(transaction, &graph)
                        .model(&request.model)
                        .read(&id, &digest, budget)?
                        .is_some()
                    {
                        consumed = Some((ordinal, id));
                        return Ok(ControlFlow::Continue(()));
                    }
                    let next = bytes.checked_add(text.len() as u64).ok_or(corrupt())?;
                    if !embedding_page_has_room(
                        documents.len(),
                        next,
                        (
                            usize::from(request.maximum_documents),
                            request.maximum_source_bytes,
                        ),
                    )? {
                        more = true;
                        return Ok(ControlFlow::Break(()));
                    }
                    bytes = next;
                    consumed = Some((ordinal, id.clone()));
                    documents.push(PendingEmbeddingDocument {
                        row_id: ordinal,
                        document_id: id,
                        path,
                        text,
                        source_digest: digest,
                    });
                    Ok(ControlFlow::Continue(()))
                })?;
                let next_cursor = if more {
                    consumed.map(|(ordinal, id)| EmbeddingPageCursor {
                        generation_id: generation.clone(),
                        after_row_id: ordinal,
                        after_document: id,
                    })
                } else {
                    None
                };
                Ok(PendingEmbeddingPage {
                    generation_id: generation,
                    documents,
                    next_cursor,
                })
            },
        )
        .await
    }

    /// Atomically validate and write a bounded batch under one publication fence.
    /// # Errors
    /// Rejects changed source/model/generation, missing documents, or invalid vectors.
    pub async fn upsert_current_document_embeddings(
        &self,
        request: EmbeddingBatchUpsertRequest,
    ) -> Result<EmbeddingBatchUpsertReport, Error> {
        self.require_project(&request.project_id)?;
        self.transaction(
            crate::rocks::StateAccess::write(
                Some(request.generation_id.clone()),
                2 * 1_048_576,
                129,
            ),
            request.statement_timeout,
            move |transaction, budget| {
                let model = active(transaction, &request.model, budget)?;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let mut writes = Vec::new();
                let mut source_bytes = 0_usize;
                for row in &request.rows {
                    validate_normalization(&row.vector, model.normalization)?;
                    let document = graph
                        .document(&row.document_id, budget)?
                        .ok_or(Error::DocumentNotFound)?;
                    let (text, digest) =
                        SourceReader::new(transaction, &graph).render(document, budget)?;
                    source_bytes = source_bytes.saturating_add(text.len());
                    if source_bytes > 16 * 1_048_576 {
                        return Err(Error::DocumentTooLarge);
                    }
                    if digest != row.source_digest {
                        return Err(Error::SourceDigestChanged);
                    }
                    let key = vector_key(&request.model, &row.document_id);
                    let value = encode_vector(&digest, &row.vector)?;
                    if transaction.get(NAMESPACE, &key, budget)?.as_deref()
                        != Some(value.as_slice())
                    {
                        writes.push((key, value));
                    }
                }
                drop(graph);
                drop(snapshot);
                let requested = u16::try_from(request.rows.len()).map_err(|_| corrupt())?;
                let written = u16::try_from(writes.len()).map_err(|_| corrupt())?;
                for (key, value) in writes {
                    transaction.put(NAMESPACE, &key, value)?;
                }
                if written > 0 {
                    bump_epoch(transaction, budget, b"vector_epoch")?;
                }
                Ok(EmbeddingBatchUpsertReport { requested, written })
            },
        )
        .await
    }

    /// Prove exact model identity, complete current text coverage, and a real cosine probe.
    /// # Errors
    /// Rejects project mismatch, corrupt vectors, or exhausted native read bounds.
    pub async fn semantic_readiness(
        &self,
        request: SemanticReadinessRequest,
    ) -> Result<SemanticReadinessReport, Error> {
        self.require_project(&request.project_id)?;
        let cache = self.semantic_cache.clone();
        self.transaction(
            crate::rocks::StateAccess::read_only(None),
            request.statement_timeout,
            move |transaction, budget| cache.readiness(transaction, &request.model, budget),
        )
        .await
    }

    /// Execute bounded exact cosine top-K against one complete current generation.
    /// # Errors
    /// Rejects incomplete vector coverage, stale generations, or invalid native state.
    pub async fn vector_top_k(
        &self,
        request: VectorSearchRequest,
    ) -> Result<Vec<VectorSearchHit>, Error> {
        self.require_project(&request.project_id)?;
        let cache = self.semantic_cache.clone();
        self.transaction(
            crate::rocks::StateAccess::read_only(Some(request.expected_generation_id.clone())),
            request.statement_timeout,
            move |transaction, budget| {
                let model = active(transaction, &request.model, budget)?;
                validate_normalization(&request.vector, model.normalization)?;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                cache.top_k(
                    super::cache::VectorQuery {
                        transaction,
                        graph: &graph,
                        model: &request.model,
                        vector: &request.vector,
                        limit: usize::from(request.limit),
                    },
                    budget,
                )
            },
        )
        .await
    }

    /// Retire a model only after its replacement proves complete current coverage.
    /// # Errors
    /// Rejects an unready replacement, mismatched identities, or native write failure.
    pub async fn retire_embedding_model(
        &self,
        request: RetireEmbeddingModelRequest,
    ) -> Result<RegisteredEmbeddingModel, Error> {
        self.transaction(
            crate::rocks::StateAccess::write(None, 1_048_576, 2),
            request.statement_timeout,
            move |transaction, budget| {
                let mut retiring = load_model(transaction, &request.retiring, budget)?;
                if !request
                    .replacement
                    .native_readiness(transaction, budget, |_, _, _| Ok(()))?
                    .ready()
                {
                    return Err(Error::ReplacementNotReady);
                }
                retiring.state = EmbeddingModelState::Retired;
                crate::artifacts::native::bump_epoch(transaction, budget, b"vector_epoch")?;
                transaction.put(
                    NAMESPACE,
                    &super::model_key(&request.retiring),
                    crate::native_database::encode(&retiring)?,
                )?;
                Ok(retiring)
            },
        )
        .await
    }
}

fn embedding_page_has_room(
    documents: usize,
    next_bytes: u64,
    maximum: (usize, u64),
) -> Result<bool, Error> {
    if documents < maximum.0 && next_bytes <= maximum.1 {
        return Ok(true);
    }
    if documents == 0 {
        return Err(Error::DocumentTooLarge);
    }
    Ok(false)
}

pub(super) fn active(
    transaction: &StateTransaction<'_>,
    model: &EmbeddingModelSelector,
    budget: &mut ReadBudget,
) -> Result<RegisteredEmbeddingModel, Error> {
    let model = load_model(transaction, model, budget)?;
    if model.state == EmbeddingModelState::Retired {
        return Err(Error::ModelRetired);
    }
    Ok(model)
}
pub(super) fn current(transaction: &StateTransaction<'_>) -> Result<GenerationId, Error> {
    transaction
        .current()?
        .map(|revision| revision.generation_id)
        .ok_or(Error::CurrentGenerationUnavailable)
}

/// Render source against the graph that owns its generation identity.
#[derive(Clone, Copy)]
pub(crate) struct SourceReader<'a, 'view, 'store> {
    summaries: SummaryReader<'a, 'view, 'store>,
}

impl<'a, 'view, 'store> SourceReader<'a, 'view, 'store> {
    pub(crate) const fn new(
        transaction: &'a StateTransaction<'store>,
        graph: &'a GraphSnapshot<'view, 'store>,
    ) -> Self {
        Self {
            summaries: SummaryReader::new(transaction, graph),
        }
    }

    pub(crate) fn render(
        &self,
        document: CanonicalSearchDocument,
        budget: &mut ReadBudget,
    ) -> Result<(String, ContentDigest), Error> {
        let summary = self.summaries.embedding_summary(&document, budget)?;
        DocumentSource {
            path: document.path,
            language: document.language,
            kind: document.kind,
            qualified_name: document.qualified_name,
            code: document.code,
            natural_text: document.natural_text,
            summary,
        }
        .render()
    }

    pub(super) const fn model(
        self,
        model: &'a EmbeddingModelSelector,
    ) -> VectorReader<'a, 'view, 'store> {
        VectorReader {
            source: self,
            model,
        }
    }
}

/// Model-specific vectors and exact source digests share one state/graph view.
#[derive(Clone, Copy)]
pub(super) struct VectorReader<'a, 'view, 'store> {
    source: SourceReader<'a, 'view, 'store>,
    model: &'a EmbeddingModelSelector,
}

fn vector_key(model: &EmbeddingModelSelector, document: &DocumentId) -> Vec<u8> {
    format!("v/{}/{document}", model.model_id).into_bytes()
}
const DIGEST_BYTES: usize = 64;
const DIMENSION_BYTES: usize = size_of::<u16>();
const VECTOR_HEADER_BYTES: usize = DIGEST_BYTES + DIMENSION_BYTES;
const COMPONENT_BYTES: usize = size_of::<f32>();

fn encode_vector(digest: &ContentDigest, vector: &[f32]) -> Result<Vec<u8>, Error> {
    let mut output = Vec::with_capacity(VECTOR_HEADER_BYTES + vector.len() * COMPONENT_BYTES);
    output.extend_from_slice(digest.as_str().as_bytes());
    output.extend_from_slice(
        &u16::try_from(vector.len())
            .map_err(|_| corrupt())?
            .to_le_bytes(),
    );
    for value in vector {
        output.extend_from_slice(&value.to_le_bytes());
    }
    Ok(output)
}
impl VectorReader<'_, '_, '_> {
    pub(super) fn render(
        &self,
        document: CanonicalSearchDocument,
        budget: &mut ReadBudget,
    ) -> Result<(String, ContentDigest), Error> {
        self.source.render(document, budget)
    }

    pub(super) fn read(
        &self,
        document: &DocumentId,
        digest: &ContentDigest,
        budget: &mut ReadBudget,
    ) -> Result<Option<Vec<f32>>, Error> {
        let Some(bytes) = self.source.summaries.transaction().get(
            NAMESPACE,
            &vector_key(self.model, document),
            budget,
        )?
        else {
            return Ok(None);
        };
        let (stored_digest, vector) = decode_vector(&bytes, self.model.dimension)?;
        Ok((stored_digest == *digest).then_some(vector))
    }
}
pub(crate) fn decode_vector(
    bytes: &[u8],
    dimension: u16,
) -> Result<(ContentDigest, Vec<f32>), Error> {
    if bytes.len() != VECTOR_HEADER_BYTES + usize::from(dimension) * COMPONENT_BYTES {
        return Err(corrupt());
    }
    let digest =
        ContentDigest::parse(std::str::from_utf8(&bytes[..DIGEST_BYTES]).map_err(|_| corrupt())?)
            .map_err(|_| corrupt())?;
    let stored = u16::from_le_bytes(
        bytes[DIGEST_BYTES..VECTOR_HEADER_BYTES]
            .try_into()
            .map_err(|_| corrupt())?,
    );
    if stored != dimension {
        return Err(corrupt());
    }
    let mut vector = Vec::with_capacity(usize::from(dimension));
    let mut magnitude = 0_f64;
    for bytes in bytes[VECTOR_HEADER_BYTES..]
        .as_chunks::<COMPONENT_BYTES>()
        .0
    {
        let value = f32::from_le_bytes(*bytes);
        if !value.is_finite() {
            return Err(corrupt());
        }
        magnitude += f64::from(value).powi(2);
        vector.push(value);
    }
    if magnitude == 0.0 || !magnitude.is_finite() {
        return Err(corrupt());
    }
    Ok((digest, vector))
}

impl EmbeddingModelSelector {
    pub(super) fn native_readiness(
        &self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        mut visit: impl FnMut(u64, DocumentId, Vec<f32>) -> Result<(), Error>,
    ) -> Result<SemanticReadinessReport, Error> {
        let mut report = SemanticReadinessReport {
            model_id: self.model_id.clone(),
            generation_id: transaction
                .current()?
                .map(|revision| revision.generation_id),
            documents: 0,
            embedded: 0,
            index_ready: false,
            query_probe_ready: false,
            state: Ready::ModelMissing,
        };
        let model = match load_model(transaction, self, budget) {
            Ok(model) => model,
            Err(Error::ModelNotFound) => return Ok(report),
            Err(Error::ModelMismatch) => {
                report.state = Ready::ModelMismatch;
                return Ok(report);
            }
            Err(error) => return Err(error),
        };
        if model.state == EmbeddingModelState::Retired {
            report.state = Ready::ModelRetired;
            return Ok(report);
        }
        report.index_ready = true;
        let Some(_) = &report.generation_id else {
            report.state = Ready::NoCurrentGeneration;
            return Ok(report);
        };
        let snapshot = transaction.graph_snapshot()?;
        let graph = snapshot.graph(budget)?;
        let expected_documents = graph.info().documents;
        let (documents, embedded) = SourceReader::new(transaction, &graph).model(self).visit(
            budget,
            |id, vector, budget| {
                validate_normalization(&vector, model.normalization)?;
                if !report.query_probe_ready {
                    report.query_probe_ready = cosine_distance(&vector, &vector, budget)?.abs()
                        <= COSINE_ROUNDING_ALLOWANCE;
                }
                visit(expected_documents, id, vector)
            },
        )?;
        report.documents = documents;
        report.embedded = embedded;
        report.state = if report.documents == 0 {
            Ready::NoDocuments
        } else if report.documents != report.embedded {
            Ready::CoverageIncomplete
        } else if !report.query_probe_ready {
            Ready::QueryProbeFailed
        } else {
            Ready::Ready
        };
        Ok(report)
    }
}

impl VectorReader<'_, '_, '_> {
    pub(super) fn visit(
        &self,
        budget: &mut ReadBudget,
        mut visit: impl FnMut(DocumentId, Vec<f32>, &ReadBudget) -> Result<(), Error>,
    ) -> Result<(u64, u64), Error> {
        let (mut documents, mut embedded) = (0_u64, 0_u64);
        let mut cursor = None;
        loop {
            let (rows, next) =
                self.source
                    .summaries
                    .graph()
                    .documents(cursor.as_ref(), 64, budget)?;
            let mut inputs = Vec::with_capacity(rows.len());
            for document in rows {
                documents += 1;
                let id = document.document_id.clone();
                let (_, digest) = self.source.render(document, budget)?;
                inputs.push((id, digest));
            }
            embedded += self.visit_batch(inputs, budget, &mut visit)?;
            cursor = next;
            if cursor.is_none() {
                return Ok((documents, embedded));
            }
        }
    }

    fn visit_batch(
        &self,
        inputs: Vec<(DocumentId, ContentDigest)>,
        budget: &mut ReadBudget,
        visit: &mut impl FnMut(DocumentId, Vec<f32>, &ReadBudget) -> Result<(), Error>,
    ) -> Result<u64, Error> {
        if inputs.is_empty() {
            return Ok(0);
        }
        let keys = inputs
            .iter()
            .map(|(id, _)| vector_key(self.model, id))
            .collect::<Vec<_>>();
        let values = self
            .source
            .summaries
            .transaction()
            .get_many(NAMESPACE, &keys, budget)?;
        let mut embedded = 0;
        for ((id, digest), value) in inputs.into_iter().zip(values) {
            budget.checkpoint()?;
            if let Some(value) = value {
                let (stored, vector) = decode_vector(&value, self.model.dimension)?;
                if stored == digest {
                    embedded += 1;
                    visit(id, vector, budget)?;
                }
            }
        }
        Ok(embedded)
    }
}

pub(super) fn documents_scan(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    visit: impl FnMut(u64, CanonicalSearchDocument, &mut ReadBudget) -> Result<ControlFlow<()>, Error>,
) -> Result<(), Error> {
    DocumentScan { graph, start: None }.visit(budget, visit)
}

/// A continuation is interpreted only against the graph selected by its caller.
pub(super) struct DocumentScan<'a, 'view, 'store> {
    pub(super) graph: &'a GraphSnapshot<'view, 'store>,
    pub(super) start: Option<&'a EmbeddingPageCursor>,
}

impl DocumentScan<'_, '_, '_> {
    pub(super) fn visit(
        &self,
        budget: &mut ReadBudget,
        mut visit: impl FnMut(
            u64,
            CanonicalSearchDocument,
            &mut ReadBudget,
        ) -> Result<ControlFlow<()>, Error>,
    ) -> Result<(), Error> {
        let mut cursor = self
            .start
            .map(|cursor| GraphSnapshot::document_cursor(&cursor.after_document))
            .transpose()?;
        let mut ordinal = self.start.map_or(0, |cursor| cursor.after_row_id);
        loop {
            let (documents, next) = self.graph.documents(cursor.as_ref(), 64, budget)?;
            for document in documents {
                budget.checkpoint()?;
                ordinal = ordinal.checked_add(1).ok_or(corrupt())?;
                if visit(ordinal, document, budget)?.is_break() {
                    return Ok(());
                }
            }
            cursor = next;
            if cursor.is_none() {
                return Ok(());
            }
        }
    }
}

pub(super) fn top_k(
    query: super::cache::VectorQuery<'_, '_, '_>,
    budget: &mut ReadBudget,
) -> Result<Vec<VectorSearchHit>, Error> {
    let super::cache::VectorQuery {
        transaction,
        graph,
        model,
        vector,
        limit,
    } = query;
    let mut selected = crate::retrieval::native::RankedRows::new(limit, ScoredDocument::compare);
    let (documents, embedded) = SourceReader::new(transaction, graph).model(model).visit(
        budget,
        |id, candidate, budget| {
            let distance = cosine_distance(vector, &candidate, budget)?;
            selected.insert(ScoredDocument { id, distance });
            Ok(())
        },
    )?;
    if documents == 0 {
        return Err(Error::NotReady {
            state: Ready::NoDocuments,
        });
    }
    if documents != embedded {
        return Err(Error::NotReady {
            state: Ready::CoverageIncomplete,
        });
    }
    selected
        .into_iter()
        .map(|candidate| candidate.hydrate(graph, budget))
        .collect()
}

pub(super) fn cosine_distance(
    left: &[f32],
    right: &[f32],
    budget: &ReadBudget,
) -> Result<f64, Error> {
    if left.len() != right.len() {
        return Err(corrupt());
    }
    let (mut dot, mut a, mut b) = (0_f64, 0_f64, 0_f64);
    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        if index % COSINE_CHECKPOINT_INTERVAL == 0 {
            budget.checkpoint()?;
        }
        let (left, right) = (f64::from(*left), f64::from(*right));
        dot += left * right;
        a += left * left;
        b += right * right;
    }
    checked_cosine_distance(1.0 - dot / (a.sqrt() * b.sqrt()))
}

pub(super) fn checked_cosine_distance(distance: f64) -> Result<f64, Error> {
    if !distance.is_finite()
        || !(-COSINE_ROUNDING_ALLOWANCE..=MAXIMUM_COSINE_DISTANCE + COSINE_ROUNDING_ALLOWANCE)
            .contains(&distance)
    {
        return Err(corrupt());
    }
    Ok(distance.clamp(0.0, MAXIMUM_COSINE_DISTANCE))
}

/// Canonical rank and hydration shared by cached, scanned, and neighbor searches.
pub(super) struct ScoredDocument {
    pub(super) id: DocumentId,
    pub(super) distance: f64,
}

impl ScoredDocument {
    pub(super) fn compare(&self, other: &Self) -> std::cmp::Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then(self.id.cmp(&other.id))
    }

    pub(super) fn hydrate(
        self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<VectorSearchHit, Error> {
        let document = graph.document(&self.id, budget)?.ok_or(corrupt())?;
        let symbol_kind = document
            .symbol_id
            .as_ref()
            .map(|id| graph.symbol(id, budget))
            .transpose()?
            .flatten()
            .and_then(|symbol| SymbolKind::from_stable_str(&symbol.symbol_kind));
        let rerank = crate::semantic::rerank::render_rerank_text(&document, "")?;
        Ok(VectorSearchHit {
            generation_id: graph.generation_id().clone(),
            document_id: document.document_id,
            file_id: document.file_id,
            symbol_id: document.symbol_id,
            path: document.path,
            language: document.language,
            document_kind: document.kind,
            qualified_name: document.qualified_name,
            symbol_kind,
            distance: self.distance,
            rerank_text: Some(rerank),
        })
    }
}

pub(super) fn corrupt() -> Error {
    Error::CorruptStoredValue {
        field: "native-vector",
    }
}

impl SourceReader<'_, '_, '_> {
    /// Test exact source-compatible vectors without retaining the corpus's vector payloads.
    pub(crate) fn embedded_symbols(
        &self,
        model_id: &codevetra_domain::ModelId,
        budget: &mut ReadBudget,
    ) -> Result<std::collections::BTreeSet<codevetra_domain::SymbolId>, Error> {
        let transaction = self.summaries.transaction();
        let graph = self.summaries.graph();
        let bytes = transaction
            .get(NAMESPACE, format!("m/{model_id}").as_bytes(), budget)?
            .ok_or(Error::ModelNotFound)?;
        let model: RegisteredEmbeddingModel = crate::native_database::decode(&bytes)?;
        if model.selector.model_id != *model_id {
            return Err(Error::ModelMismatch);
        }
        active(transaction, &model.selector, budget)?;
        let mut output = std::collections::BTreeSet::new();
        documents_scan(graph, budget, |_, document, budget| {
            if document.kind == codevetra_domain::DocumentKind::Symbol
                && let Some(id) = document.symbol_id.clone()
            {
                let document_id = document.document_id.clone();
                let (_, digest) = self.render(document, budget)?;
                if self
                    .model(&model.selector)
                    .read(&document_id, &digest, budget)?
                    .is_some()
                {
                    output.insert(id);
                    if output.len() > 131_072 {
                        return Err(Error::InvalidInput {
                            field: "embedding_symbol_workspace",
                        });
                    }
                }
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })?;
        Ok(output)
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    #[test]
    fn vector_encoding_preserves_the_persisted_binary_layout() -> Result<(), Error> {
        let digest = ContentDigest::parse(&"a".repeat(64)).map_err(|_| corrupt())?;
        let fixture = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x02\x00\x00\x00\x80\x3f\x00\x00\x00\xbf";
        assert_eq!(encode_vector(&digest, &[1.0, -0.5])?, fixture);
        assert_eq!(decode_vector(fixture, 2)?, (digest, vec![1.0, -0.5]));
        assert!(decode_vector(fixture, 1).is_err());
        assert!(decode_vector(&fixture[..fixture.len() - 1], 2).is_err());
        let mut trailing = fixture.to_vec();
        trailing.push(0);
        assert!(decode_vector(&trailing, 2).is_err());
        let mut invalid_digest = fixture.to_vec();
        invalid_digest[0] = b'g';
        assert!(decode_vector(&invalid_digest, 2).is_err());
        let mut invalid_dimension = fixture.to_vec();
        invalid_dimension[64] = 1;
        assert!(decode_vector(&invalid_dimension, 2).is_err());
        for payload in [
            [0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0x80, 0x7f, 0, 0, 0, 0],
            [0, 0, 0xc0, 0x7f, 0, 0, 0, 0],
        ] {
            let mut invalid = fixture[..66].to_vec();
            invalid.extend_from_slice(&payload);
            assert!(decode_vector(&invalid, 2).is_err());
        }
        Ok(())
    }
}
