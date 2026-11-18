//! Generation-fenced reranker inputs over the existing native graph owner.

use std::{collections::BTreeSet, fmt, time::Duration};

use codevetra_domain::{DocumentId, GenerationId, ProjectId};

use super::types::{SemanticStorageError, invalid, validate_timeout};
use crate::{
    CanonicalSearchDocument, CurrentGenerationLookup, NativeDatabase,
    artifacts::native::summaries::SummaryReader,
};

const MAXIMUM_DOCUMENTS: usize = 100;
const MAXIMUM_TEXT_BYTES: usize = 1_536;
// Per-field allocations preserve name/path identity and leave bounded room for
// intent and source evidence. The total output ceiling still applies first.
const IDENTITY_TEXT_BYTES: usize = 192;
const KIND_TEXT_BYTES: usize = 32;
const INTENT_TEXT_BYTES: usize = 384;
const CODE_TEXT_BYTES: usize = 512;
const FIELD_SEPARATOR: &str = ": ";
const FIELD_TERMINATOR: char = '\n';

/// A bounded list of unique documents from one exact current generation.
pub struct RerankDocumentsRequest {
    project_id: ProjectId,
    expected_generation_id: GenerationId,
    documents: Vec<DocumentId>,
    timeout: Duration,
}

impl RerankDocumentsRequest {
    /// Validate the source hydration window and native deadline before storage admission.
    /// # Errors
    /// Rejects empty/oversized lists, duplicate document identities, or invalid deadlines.
    pub fn new(
        generation: CurrentGenerationLookup<'_>,
        documents: Vec<DocumentId>,
        timeout: Duration,
    ) -> Result<Self, SemanticStorageError> {
        validate_timeout(timeout)?;
        if documents.is_empty()
            || documents.len() > MAXIMUM_DOCUMENTS
            || documents.iter().collect::<BTreeSet<_>>().len() != documents.len()
        {
            return Err(invalid("rerank_documents"));
        }
        Ok(Self {
            project_id: generation.project_id().clone(),
            expected_generation_id: generation.expected_generation_id().clone(),
            documents,
            timeout,
        })
    }
}

/// Transient bounded model text; source bytes are excluded from debug output and serialization.
pub struct RerankDocument {
    document_id: DocumentId,
    text: String,
}

impl RerankDocument {
    /// Identity echoed in the request's original order.
    #[must_use]
    pub const fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    /// Compact declaration, intent, and source evidence for this cross-encoder attempt.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for RerankDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RerankDocument")
            .field("document_id", &self.document_id)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

impl NativeDatabase {
    /// Hydrate a merged lexical/semantic shortlist under one bounded immutable snapshot.
    ///
    /// This does not require embeddings. It never changes the embedding source or its digest.
    /// # Errors
    /// Rejects a different project/generation, missing documents, or exhausted native bounds.
    pub async fn rerank_documents(
        &self,
        request: RerankDocumentsRequest,
    ) -> Result<Vec<RerankDocument>, SemanticStorageError> {
        self.require_project(&request.project_id)?;
        self.transaction(
            crate::rocks::StateAccess::read_only(Some(request.expected_generation_id.clone())),
            request.timeout,
            move |transaction, budget| {
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                request
                    .documents
                    .into_iter()
                    .map(|document_id| {
                        let document = graph.document(&document_id, budget)?.ok_or(
                            SemanticStorageError::CorruptStoredValue {
                                field: "rerank_document",
                            },
                        )?;
                        let summary = SummaryReader::new(transaction, &graph)
                            .embedding_summary(&document, budget)?;
                        Ok(RerankDocument {
                            document_id,
                            text: render_rerank_text(&document, &summary)?,
                        })
                    })
                    .collect()
            },
        )
        .await
    }
}

pub(crate) fn render_rerank_text(
    document: &CanonicalSearchDocument,
    summary: &str,
) -> Result<String, SemanticStorageError> {
    let mut output = String::new();
    output
        .try_reserve(MAXIMUM_TEXT_BYTES)
        .map_err(|_| super::types::database_error("render-rerank-document"))?;
    for (label, value, maximum) in [
        (
            "name",
            document.qualified_name.as_str(),
            IDENTITY_TEXT_BYTES,
        ),
        ("path", document.path.as_str(), IDENTITY_TEXT_BYTES),
        ("kind", document.kind.as_str(), KIND_TEXT_BYTES),
        ("summary", summary, INTENT_TEXT_BYTES),
        (
            "documentation",
            document.natural_text.as_str(),
            INTENT_TEXT_BYTES,
        ),
        ("code", document.code.as_str(), CODE_TEXT_BYTES),
    ] {
        let remaining = MAXIMUM_TEXT_BYTES.saturating_sub(
            output.len() + label.len() + FIELD_SEPARATOR.len() + FIELD_TERMINATOR.len_utf8(),
        );
        let end = value.floor_char_boundary(value.len().min(maximum).min(remaining));
        if end > 0 {
            output.push_str(label);
            output.push_str(FIELD_SEPARATOR);
            output.push_str(&value[..end]);
            output.push(FIELD_TERMINATOR);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_text_prioritizes_intent_and_bounds_utf8_without_changing_embedding_inputs() {
        let document = CanonicalSearchDocument {
            document_id: DocumentId::from_uuid_v8([3; 16]),
            file_id: None,
            symbol_id: None,
            path: "src/queue.rs".into(),
            language: "rust".into(),
            kind: codevetra_domain::DocumentKind::Symbol,
            qualified_name: "Queue::drain".into(),
            code: "源".repeat(2000),
            natural_text: "Drain pending work after cancellation.".into(),
            metadata_json: "{}".into(),
        };
        let text = render_rerank_text(&document, "Wait for admitted work to complete.")
            .unwrap_or_else(|error| panic!("render failed: {error}"));
        assert!(
            text.starts_with("name: Queue::drain\npath: src/queue.rs\nkind: symbol\nsummary: ")
        );
        assert!(text.find("summary:") < text.find("documentation:"));
        assert!(text.find("documentation:") < text.find("code:"));
        assert!(text.len() <= MAXIMUM_TEXT_BYTES);
        assert_eq!(document.code.len(), 6000);
        let result = RerankDocument {
            document_id: document.document_id.clone(),
            text,
        };
        let debug = format!("{result:?}");
        assert!(!debug.contains("Drain pending"));
        assert!(!debug.contains("Queue::drain"));
    }

    #[test]
    fn hydration_request_rejects_unbounded_duplicate_and_expired_inputs() {
        let project = ProjectId::from_uuid_v8([1; 16]);
        let generation = GenerationId::from_uuid_v8([2; 16]);
        let id = DocumentId::from_uuid_v8([3; 16]);
        for documents in [Vec::new(), vec![id.clone(); 2], vec![id.clone(); 101]] {
            assert!(
                RerankDocumentsRequest::new(
                    CurrentGenerationLookup::new(&project, &generation),
                    documents,
                    Duration::from_secs(10)
                )
                .is_err()
            );
        }
        assert!(
            RerankDocumentsRequest::new(
                CurrentGenerationLookup::new(&project, &generation),
                vec![id],
                Duration::ZERO
            )
            .is_err()
        );
    }
}
