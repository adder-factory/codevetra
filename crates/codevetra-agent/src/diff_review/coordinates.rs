//! A supplied patch has no implicit relationship to the current graph.
use super::{
    BTreeSet, DiffFileKind, DiffReviewError, DiffReviewInput, NormalizedPath, ProjectId,
    UnifiedDiffComparison, UnifiedDiffHunk, require_not_cancelled,
};
use codevetra_db::CurrentFileLookup;
use codevetra_domain::{ContentDigest, GenerationId};

pub(super) struct VerifiedCoordinates {
    pub(super) generation: Option<GenerationId>,
    pub(super) paths: BTreeSet<NormalizedPath>,
}

impl VerifiedCoordinates {
    pub(super) fn complete(&self, comparison: &UnifiedDiffComparison) -> bool {
        comparison.files.iter().all(|file| {
            self.paths
                .contains(file.old_path.as_ref().unwrap_or(&file.path))
        })
    }
}

pub(super) fn hash_line(hasher: &mut blake3::Hasher, line: &str) {
    hasher.update(line.as_bytes());
    hasher.update(b"\n");
}

impl crate::ProjectRuntime {
    pub(super) async fn verify_diff_coordinates(
        &self,
        input: &DiffReviewInput<'_>,
        comparison: &UnifiedDiffComparison,
        project: Option<&ProjectId>,
    ) -> Result<VerifiedCoordinates, DiffReviewError> {
        let mut verified = VerifiedCoordinates {
            generation: None,
            paths: BTreeSet::new(),
        };
        let Some(project) = project else {
            return Ok(verified);
        };
        let Some(generation) = self
            .database
            .current_generation_record(project)
            .await
            .map_err(|_| DiffReviewError::RetrievalUnavailable)?
        else {
            return Ok(verified);
        };
        verified.generation = Some(generation.generation_id().clone());
        let Ok(commit) = crate::compare::resolve_commit(&self.root, input.base_ref).await else {
            return Ok(verified);
        };
        for file in &comparison.files {
            require_not_cancelled(&input.cancellation)?;
            if file.kind == DiffFileKind::Added {
                continue;
            }
            let path = file.old_path.as_ref().unwrap_or(&file.path);
            let indexed = self
                .database
                .exact_current_file_by_path(CurrentFileLookup::new(
                    project,
                    generation.generation_id(),
                    path,
                ))
                .await
                .map_err(|_| DiffReviewError::RetrievalUnavailable)?;
            let Some(indexed) = indexed else {
                continue;
            };
            let Ok(Some(blob)) = crate::compare::git_blob(&self.root, &commit, path).await else {
                continue;
            };
            require_not_cancelled(&input.cancellation)?;
            let verification = FileVerification {
                expected: indexed.content_hash().clone(),
                hunks: file.hunks.clone(),
                cancellation: input.cancellation.clone(),
            };
            let matches = tokio::task::spawn_blocking(move || verification.matches_file(&blob))
                .await
                .map_err(|_| DiffReviewError::RetrievalUnavailable)??;
            if matches {
                verified.paths.insert(path.clone());
            }
        }
        Ok(verified)
    }
}

struct FileVerification {
    expected: ContentDigest,
    hunks: Vec<UnifiedDiffHunk>,
    cancellation: crate::ProjectCancellation,
}

impl FileVerification {
    fn matches_file(&self, blob: &[u8]) -> Result<bool, DiffReviewError> {
        let Self {
            expected,
            hunks,
            cancellation,
        } = self;
        let mut hasher = blake3::Hasher::new();
        for chunk in blob.chunks(64 * 1024) {
            require_not_cancelled(cancellation)?;
            hasher.update(chunk);
        }
        if &ContentDigest::from_bytes(*hasher.finalize().as_bytes()) != expected {
            return Ok(false);
        }
        let Ok(source) = std::str::from_utf8(blob) else {
            return Ok(false);
        };
        let mut cursor = HunkCursor {
            lines: source.lines(),
            consumed: 0,
            cancellation,
        };
        for hunk in hunks {
            require_not_cancelled(cancellation)?;
            if !cursor.matches_hunk(hunk)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

// The consumed-line counter cannot be paired with a different source iterator.
struct HunkCursor<'a> {
    lines: std::str::Lines<'a>,
    consumed: u32,
    cancellation: &'a crate::ProjectCancellation,
}

impl HunkCursor<'_> {
    fn matches_hunk(&mut self, hunk: &UnifiedDiffHunk) -> Result<bool, DiffReviewError> {
        let Self {
            lines,
            consumed,
            cancellation,
        } = self;
        let offset = if hunk.old_lines == 0 {
            hunk.old_start
        } else {
            hunk.old_start.saturating_sub(1)
        };
        if offset < *consumed {
            return Ok(false);
        }
        while *consumed < offset {
            if consumed.is_multiple_of(1024) {
                require_not_cancelled(cancellation)?;
            }
            if lines.next().is_none() {
                return Ok(false);
            }
            *consumed += 1;
        }
        let mut hasher = blake3::Hasher::new();
        for _ in 0..hunk.old_lines {
            if consumed.is_multiple_of(1024) {
                require_not_cancelled(cancellation)?;
            }
            let Some(line) = lines.next() else {
                return Ok(false);
            };
            hash_line(&mut hasher, line);
            *consumed = consumed
                .checked_add(1)
                .ok_or(DiffReviewError::InvalidDiff)?;
        }
        Ok(hasher.finalize().as_bytes() == &hunk.old_content_digest)
    }
}

pub(super) async fn require_current(
    runtime: &crate::ProjectRuntime,
    project: &ProjectId,
    coordinates: &VerifiedCoordinates,
) -> Result<(), DiffReviewError> {
    let current = runtime
        .database
        .current_generation_record(project)
        .await
        .map_err(|_| DiffReviewError::RetrievalUnavailable)?;
    if current
        .as_ref()
        .map(codevetra_db::CurrentGenerationRecord::generation_id)
        == coordinates.generation.as_ref()
    {
        Ok(())
    } else {
        Err(DiffReviewError::RetrievalUnavailable)
    }
}
