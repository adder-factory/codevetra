//! Position navigation never substitutes an enclosing declaration for an occurrence.

use std::collections::BTreeSet;

use codevetra_db::{
    CurrentFileLookup, CurrentGenerationLookup, CurrentGraphEdge, CurrentGraphLookup,
    CurrentReferenceSite, CurrentSymbolRecord, CurrentSymbolSetLookup, GraphDirection,
    ReferencePageRequest, ReferenceSelector, ReferenceSpanPrecision,
};
use codevetra_domain::{EdgeKind, GenerationDigestVersion, GenerationId, NormalizedPath, SymbolId};
use serde::Serialize;

use crate::{ProjectCancellation, ProjectError, ProjectRuntime};

/// Public operation using exact indexed identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionOperation {
    /// Resolve targets of the exact occurrence under a cursor.
    Definition,
    /// Return references to one resolved identity.
    References,
    /// Return explicitly indexed implementation/override relationships.
    Implementation,
}

/// Caller-selected identity or byte-encoded editor position.
pub enum PositionAnchor {
    /// Already resolved declaration identity.
    Symbol(SymbolId),
    /// One-based line, zero-based UTF-8 byte column; token ranges are half-open.
    Cursor {
        /// Canonical project-relative source path.
        path: NormalizedPath,
        /// One-based source line.
        line: u32,
        /// Zero-based UTF-8 byte column, excluding CRLF terminators.
        column: u32,
    },
}

/// Bounded navigation request, shared by CLI and MCP.
pub struct PositionRequest {
    /// Navigation operation.
    pub operation: PositionOperation,
    /// Exact symbol or editor cursor.
    pub anchor: PositionAnchor,
    /// One through one hundred returned rows.
    pub limit: u16,
    /// Reference continuation returned by a previous page.
    pub after: Option<String>,
}

/// Resolution disposition; empty results never silently become an enclosing function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionState {
    /// One target identity is evidenced.
    Resolved,
    /// More than one target, or a mixture of resolved and unresolved occurrences.
    Ambiguous,
    /// Exact occurrence exists without a supported target.
    Unresolved,
    /// Only coarse evidence or no supported implementation relation is available.
    Unsupported,
    /// No exact occurrence covers this cursor, including whitespace/comments.
    NoOccurrence,
    /// Live source differs; no cursor mapping or reuse was performed.
    Stale,
}

/// Source-backed navigation result with separate usage and declaration spans.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionResult {
    /// Requested navigation behavior.
    pub operation: PositionOperation,
    /// Exact immutable evidence generation.
    pub generation: GenerationId,
    /// Whether the complete observed source manifest matches this generation.
    pub fresh: bool,
    /// Explicit resolution disposition.
    pub state: PositionState,
    /// Stable explanation, never source text.
    pub reason: &'static str,
    /// Occurrences under the input cursor, separate from returned reference usages.
    pub anchor_sites: Vec<CurrentReferenceSite>,
    /// Evidenced definition candidates, including ambiguity.
    pub definitions: Vec<CurrentSymbolRecord>,
    /// Usages scoped to exactly one resolved identity.
    pub references: Vec<CurrentReferenceSite>,
    /// Implementation/override targets; these are not ordinary call edges.
    pub implementations: Vec<CurrentSymbolRecord>,
    /// Preserved implementation provenance, confidence and represented site count.
    pub relations: Vec<CurrentGraphEdge>,
    /// Reference continuation; always tied to project, generation and target.
    pub next: Option<String>,
    /// More evidence may exist beyond the declared limits.
    pub truncated: bool,
    /// UTF-8 columns differ from UTF-16 editor columns; conversion must be explicit.
    pub coordinates: &'static str,
}

impl PositionResult {
    fn new(operation: PositionOperation, generation: GenerationId, fresh: bool) -> Self {
        Self {
            operation,
            generation,
            fresh,
            state: PositionState::NoOccurrence,
            reason: "no_indexed_occurrence",
            anchor_sites: Vec::new(),
            definitions: Vec::new(),
            references: Vec::new(),
            implementations: Vec::new(),
            relations: Vec::new(),
            next: None,
            truncated: false,
            coordinates: "one_based_line_zero_based_utf8_byte_column_half_open_bytes",
        }
    }

    fn resolve_occurrences(&mut self) -> BTreeSet<SymbolId> {
        let exact = self
            .anchor_sites
            .iter()
            .filter(|site| site.occurrence.span_precision == ReferenceSpanPrecision::Exact);
        let targets = exact
            .clone()
            .filter_map(|site| site.occurrence.target_symbol_id.clone())
            .collect::<BTreeSet<_>>();
        let unresolved = exact
            .clone()
            .any(|site| site.occurrence.target_symbol_id.is_none());
        (self.state, self.reason) = if self.truncated {
            (PositionState::Ambiguous, "occurrence_scan_incomplete")
        } else if targets.len() > 1 || (!targets.is_empty() && unresolved) {
            (PositionState::Ambiguous, "multiple_or_incomplete_targets")
        } else if targets.len() == 1 {
            (PositionState::Resolved, "exact_occurrence_target")
        } else if unresolved {
            (PositionState::Unresolved, "exact_occurrence_without_target")
        } else if !self.anchor_sites.is_empty() {
            (
                PositionState::Unsupported,
                "coarse_span_cannot_identify_cursor_token",
            )
        } else {
            (PositionState::NoOccurrence, "no_indexed_occurrence")
        };
        targets
    }
}

impl ProjectRuntime {
    /// Navigate a cursor or symbol identity with exact generation and source fences.
    /// # Errors
    /// Rejects invalid coordinates, stale continuations, cancellation and bounded read failures.
    pub async fn navigate_position(
        &self,
        request: PositionRequest,
        cancellation: ProjectCancellation,
    ) -> Result<PositionResult, ProjectError> {
        if request.limit == 0
            || request.limit > 100
            || request
                .after
                .as_ref()
                .is_some_and(|value| value.len() > 2048)
            || (request.after.is_some() && request.operation != PositionOperation::References)
        {
            return Err(ProjectError::InvalidOptions);
        }
        crate::cancellable_project_read(
            &cancellation,
            self.navigate_position_inner(request, cancellation.clone()),
        )
        .await
    }

    async fn navigate_position_inner(
        &self,
        request: PositionRequest,
        cancellation: ProjectCancellation,
    ) -> Result<PositionResult, ProjectError> {
        let snapshot = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let current = snapshot
            .current
            .as_ref()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let path = match &request.anchor {
            PositionAnchor::Cursor { path, .. } => Some(path.clone()),
            PositionAnchor::Symbol(_) => None,
        };
        let source = self.scan_source(path, cancellation.clone()).await?;
        let fresh = current.source_revision == source.digest.as_str()
            && current.digest_version == GenerationDigestVersion::CURRENT;
        let mut result =
            PositionResult::new(request.operation, current.generation_id.clone(), fresh);
        if !fresh {
            result.state = PositionState::Stale;
            result.reason = "source_or_extraction_contract_changed";
            return Ok(result);
        }
        let targets = self
            .resolve_position_anchor(&request, &snapshot, &source, &cancellation, &mut result)
            .await?;
        if !targets.is_empty() {
            let ids = targets.into_iter().collect::<Vec<_>>();
            result.definitions = self
                .database()
                .current_symbols_by_ids(CurrentSymbolSetLookup::new(
                    &snapshot.project_id,
                    &result.generation,
                    &ids,
                ))
                .await
                .map_err(|error| position_storage_error(&error))?;
            if result.definitions.is_empty() && matches!(request.anchor, PositionAnchor::Symbol(_))
            {
                result.state = PositionState::Unresolved;
                result.reason = "symbol_not_in_current_generation";
            } else if result.definitions.len() != ids.len() {
                return Err(ProjectError::SourceContextUnavailable);
            }
        }
        if result.state == PositionState::Resolved {
            self.navigate_resolved(&snapshot.project_id, &request, &mut result)
                .await?;
        }
        let final_generation = self
            .database()
            .current_generation_record(&snapshot.project_id)
            .await
            .map_err(|error| position_storage_error(&error))?;
        if final_generation
            .as_ref()
            .is_none_or(|generation| generation.generation_id() != &result.generation)
        {
            return Err(ProjectError::SourceContextUnavailable);
        }
        Ok(result)
    }

    async fn resolve_position_anchor(
        &self,
        request: &PositionRequest,
        snapshot: &codevetra_db::ProjectSnapshot,
        source: &crate::SourceRevision,
        cancellation: &ProjectCancellation,
        result: &mut PositionResult,
    ) -> Result<BTreeSet<SymbolId>, ProjectError> {
        let targets = match &request.anchor {
            PositionAnchor::Symbol(id) => {
                result.state = PositionState::Resolved;
                result.reason = "selected_symbol_identity";
                BTreeSet::from([id.clone()])
            }
            PositionAnchor::Cursor { path, line, column } => {
                let file = self
                    .database()
                    .exact_current_file_by_path(CurrentFileLookup::new(
                        &snapshot.project_id,
                        &result.generation,
                        path,
                    ))
                    .await
                    .map_err(|_| ProjectError::SourceContextUnavailable)?
                    .ok_or(ProjectError::FileNotFound)?;
                let capture = source
                    .captures
                    .get(path)
                    .ok_or(ProjectError::SourceContextUnavailable)?;
                if &capture.content_hash != file.content_hash() {
                    return Err(ProjectError::SourceContextUnavailable);
                }
                let captured = capture.source.clone();
                let token = cancellation.clone();
                let (line, column) = (*line, *column);
                let byte = tokio::task::spawn_blocking(move || {
                    cursor_offset(&captured, line, column, &token)
                })
                .await
                .map_err(|_| ProjectError::SourceContextUnavailable)??;
                let page = self
                    .database()
                    .current_reference_page(ReferencePageRequest {
                        project: &snapshot.project_id,
                        generation: &result.generation,
                        selector: ReferenceSelector::Occurrence {
                            file: file.file_id().clone(),
                            byte,
                        },
                        limit: 100,
                        after: None,
                    })
                    .await
                    .map_err(|error| position_storage_error(&error))?;
                result.anchor_sites = page.sites;
                result.truncated = page.truncated;
                result.resolve_occurrences()
            }
        };
        Ok(targets)
    }

    async fn navigate_resolved(
        &self,
        project: &codevetra_domain::ProjectId,
        request: &PositionRequest,
        result: &mut PositionResult,
    ) -> Result<(), ProjectError> {
        let Some(target) = result.definitions.first() else {
            return Err(ProjectError::SourceContextUnavailable);
        };
        match request.operation {
            PositionOperation::Definition => {}
            PositionOperation::References => {
                let page = self
                    .database()
                    .current_reference_page(ReferencePageRequest {
                        project,
                        generation: &result.generation,
                        selector: ReferenceSelector::Target(target.symbol_id().clone()),
                        limit: request.limit,
                        after: request.after.as_deref(),
                    })
                    .await
                    .map_err(|error| position_storage_error(&error))?;
                result.references = page.sites;
                result.next = page.next;
                result.truncated = page.truncated;
            }
            PositionOperation::Implementation => {
                let roots = [target.symbol_id().clone()];
                let kinds = [EdgeKind::Implements, EdgeKind::Overrides];
                result.relations = self
                    .database()
                    .current_graph_edges(
                        CurrentGraphLookup::new(
                            CurrentGenerationLookup::new(project, &result.generation),
                            &roots,
                            GraphDirection::Incoming,
                        )
                        .with_limit(request.limit)
                        .with_edge_kinds(&kinds),
                    )
                    .await
                    .map_err(|error| position_storage_error(&error))?;
                result.truncated = result.relations.len() == usize::from(request.limit);
                let ids = result
                    .relations
                    .iter()
                    .map(|edge| edge.source_symbol_id().clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                if ids.is_empty() {
                    result.state = PositionState::Unsupported;
                    result.reason = "no_indexed_implementation_or_override_evidence";
                } else {
                    result.implementations = self
                        .database()
                        .current_symbols_by_ids(CurrentSymbolSetLookup::new(
                            project,
                            &result.generation,
                            &ids,
                        ))
                        .await
                        .map_err(|error| position_storage_error(&error))?;
                }
            }
        }
        Ok(())
    }
}

fn position_storage_error(error: &codevetra_db::StorageError) -> ProjectError {
    match error {
        codevetra_db::StorageError::InvalidInput { .. } => ProjectError::InvalidOptions,
        _ => ProjectError::SourceContextUnavailable,
    }
}

fn cursor_offset(
    source: &str,
    line: u32,
    column: u32,
    cancellation: &ProjectCancellation,
) -> Result<u64, ProjectError> {
    if line == 0 {
        return Err(ProjectError::InvalidOptions);
    }
    let mut offset = 0_usize;
    for (index, text) in source.split_inclusive('\n').enumerate() {
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        if u32::try_from(index).map_err(|_| ProjectError::InvalidOptions)? == line - 1 {
            let text = text.strip_suffix('\n').unwrap_or(text);
            let text = text.strip_suffix('\r').unwrap_or(text);
            let column = usize::try_from(column).map_err(|_| ProjectError::InvalidOptions)?;
            if column > text.len() || !text.is_char_boundary(column) {
                return Err(ProjectError::InvalidOptions);
            }
            return u64::try_from(offset + column).map_err(|_| ProjectError::InvalidOptions);
        }
        offset += text.len();
    }
    Err(ProjectError::InvalidOptions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_coordinates_preserve_utf8_non_bmp_crlf_and_half_open_boundaries() {
        let source = "α😀\r\ncall();\n";
        let cancellation = ProjectCancellation::new();
        assert_eq!(cursor_offset(source, 1, 2, &cancellation).ok(), Some(2));
        assert_eq!(cursor_offset(source, 1, 6, &cancellation).ok(), Some(6));
        assert_eq!(cursor_offset(source, 2, 0, &cancellation).ok(), Some(8));
        for column in [1, 3, 4, 5, 7] {
            assert!(cursor_offset(source, 1, column, &cancellation).is_err());
        }
        cancellation.cancel();
        assert!(matches!(
            cursor_offset(source, 2, 0, &cancellation),
            Err(ProjectError::RequestCancelled)
        ));
    }
}
