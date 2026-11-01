//! Exact identity and position filters over generation-fenced reference pages.

use codevetra_domain::{FileId, GenerationId, ProjectId, SymbolId};
use serde::{Deserialize, Serialize};

use super::{CurrentFileRecord, DEFAULT_INTERACTIVE_READ_TIMEOUT, native::file_record};
use crate::{
    NativeDatabase, ReferenceInput, StorageError,
    rocks::{RecordKey, RocksError},
};

const SCAN_LIMIT: usize = 16_384;
const PAGE_LIMIT: u16 = 100;

/// Identity filter applied before a navigation result limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceSelector {
    /// References whose retained span covers this file byte, including explicitly coarse spans.
    Occurrence {
        /// Exact indexed source file.
        file: FileId,
        /// Zero-based byte offset into that file.
        byte: u64,
    },
    /// Usages whose resolved target is exactly this identity.
    Target(SymbolId),
}

/// One bounded immutable-generation navigation page request.
pub struct ReferencePageRequest<'a> {
    /// Canonical project identity.
    pub project: &'a ProjectId,
    /// Expected current generation; no automatic rebase of a continuation.
    pub generation: &'a GenerationId,
    /// Exact position or resolved identity.
    pub selector: ReferenceSelector,
    /// Returned matching rows, between one and one hundred.
    pub limit: u16,
    /// Opaque previous continuation, bound to project, generation and selector.
    pub after: Option<&'a str>,
}

/// Occurrence and enclosing-file provenance; a target declaration is separate evidence.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentReferenceSite {
    /// Published file identity and content hash.
    pub file: CurrentFileRecord,
    /// Exact or explicitly coarse original occurrence evidence.
    pub occurrence: ReferenceInput,
}

/// A finite page which may require continuation even when no matching rows were found.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentReferencePage {
    /// Exact generation shared by every row and the continuation.
    pub generation: GenerationId,
    /// Matches in deterministic canonical record order.
    pub sites: Vec<CurrentReferenceSite>,
    /// Opaque continuation; a changed generation requires restarting the lookup.
    pub next: Option<String>,
    /// Whether more stored references remain to inspect.
    pub truncated: bool,
    /// Number inspected, independently of result count.
    pub scanned: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Continuation {
    project: ProjectId,
    generation: GenerationId,
    selector: ReferenceSelector,
    key: Vec<u8>,
}

impl ReferenceSelector {
    fn matches(&self, reference: &ReferenceInput) -> bool {
        match self {
            Self::Occurrence { file, byte } => {
                &reference.file_id == file
                    && reference.start_byte <= *byte
                    && *byte < reference.end_byte
            }
            Self::Target(symbol) => reference.target_symbol_id.as_ref() == Some(symbol),
        }
    }

    fn file(&self) -> Option<&FileId> {
        match self {
            Self::Occurrence { file, .. } => Some(file),
            Self::Target(_) => None,
        }
    }
}

impl NativeDatabase {
    /// Read bounded occurrences without same-name fallback or cross-generation continuation.
    /// # Errors
    /// Rejects invalid limits/cursors, publication changes and native read failures.
    pub async fn current_reference_page(
        &self,
        request: ReferencePageRequest<'_>,
    ) -> Result<CurrentReferencePage, StorageError> {
        self.require_project(request.project)?;
        if request.limit == 0 || request.limit > PAGE_LIMIT {
            return Err(StorageError::InvalidInput {
                field: "reference_page_limit",
            });
        }
        let after = parse_continuation(&request)?;
        let project = request.project.clone();
        let generation = request.generation.clone();
        let selector = request.selector;
        let limit = usize::from(request.limit);
        self.graph(
            request.generation,
            DEFAULT_INTERACTIVE_READ_TIMEOUT,
            move |graph, budget| {
                let ScannedReferences {
                    sites,
                    scanned,
                    next_key,
                } = scan_reference_sites(graph, budget, &selector, &generation, limit, after)?;
                let next = next_key
                    .map(|key| {
                        serde_json::to_string(&Continuation {
                            project,
                            generation: generation.clone(),
                            selector,
                            key: key.as_bytes().to_vec(),
                        })
                        .map_err(|_| RocksError::Format)
                    })
                    .transpose()?;
                Ok(CurrentReferencePage {
                    generation,
                    sites,
                    truncated: next.is_some(),
                    next,
                    scanned,
                })
            },
        )
        .await
    }
}

struct ScannedReferences {
    sites: Vec<CurrentReferenceSite>,
    scanned: usize,
    next_key: Option<RecordKey>,
}

fn scan_reference_sites(
    graph: &crate::rocks::GraphSnapshot<'_, '_>,
    budget: &mut crate::rocks::ReadBudget,
    selector: &ReferenceSelector,
    generation: &GenerationId,
    limit: usize,
    after: Option<RecordKey>,
) -> Result<ScannedReferences, RocksError> {
    let mut cursor = after;
    let mut sites = Vec::new();
    let mut scanned = 0;
    let mut next_key = None;
    'pages: loop {
        let page = graph.reference_sites(selector.file(), cursor.as_ref(), 256, budget)?;
        let (records, next) = (page.rows, page.next);
        let last_index = records.len().saturating_sub(1);
        for (index, (key, occurrence)) in records.into_iter().enumerate() {
            scanned += 1;
            append_matching_site(graph, budget, selector, generation, occurrence, &mut sites)?;
            if sites.len() == limit || scanned == SCAN_LIMIT {
                if index < last_index || next.is_some() {
                    next_key = Some(key);
                }
                break 'pages;
            }
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(ScannedReferences {
        sites,
        scanned,
        next_key,
    })
}

fn append_matching_site(
    graph: &crate::rocks::GraphSnapshot<'_, '_>,
    budget: &mut crate::rocks::ReadBudget,
    selector: &ReferenceSelector,
    generation: &GenerationId,
    occurrence: ReferenceInput,
    sites: &mut Vec<CurrentReferenceSite>,
) -> Result<(), RocksError> {
    if selector.matches(&occurrence) {
        let file = graph
            .file(&occurrence.file_id, budget)?
            .ok_or(RocksError::Format)?;
        sites.push(CurrentReferenceSite {
            file: file_record(generation, file)?,
            occurrence,
        });
    }
    Ok(())
}

fn parse_continuation(
    request: &ReferencePageRequest<'_>,
) -> Result<Option<RecordKey>, StorageError> {
    let invalid = || StorageError::InvalidInput {
        field: "reference_page_cursor",
    };
    let Some(text) = request.after else {
        return Ok(None);
    };
    if text.len() > 2048 {
        return Err(invalid());
    }
    let cursor: Continuation = serde_json::from_str(text).map_err(|_| invalid())?;
    if &cursor.project != request.project
        || &cursor.generation != request.generation
        || cursor.selector != request.selector
        || cursor.key.len() > 128
    {
        return Err(invalid());
    }
    RecordKey::new(cursor.key).map(Some).map_err(|_| invalid())
}
