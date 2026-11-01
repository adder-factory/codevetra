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

pub(super) async fn verify(
    runtime: &crate::ProjectRuntime,
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
    let Some(generation) = runtime
        .database
        .current_generation_record(project)
        .await
        .map_err(|_| DiffReviewError::RetrievalUnavailable)?
    else {
        return Ok(verified);
    };
    verified.generation = Some(generation.generation_id().clone());
    let Ok(commit) = crate::compare::resolve_commit(&runtime.root, input.base_ref).await else {
        return Ok(verified);
    };
    for file in &comparison.files {
        require_not_cancelled(&input.cancellation)?;
        if file.kind == DiffFileKind::Added {
            continue;
        }
        let path = file.old_path.as_ref().unwrap_or(&file.path);
        let indexed = runtime
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
        let Ok(Some(blob)) = crate::compare::git_blob(&runtime.root, &commit, path).await else {
            continue;
        };
        require_not_cancelled(&input.cancellation)?;
        let expected = indexed.content_hash().clone();
        let hunks = file.hunks.clone();
        let cancellation = input.cancellation.clone();
        let matches = tokio::task::spawn_blocking(move || {
            matches_file(&blob, &expected, &hunks, &cancellation)
        })
        .await
        .map_err(|_| DiffReviewError::RetrievalUnavailable)??;
        if matches {
            verified.paths.insert(path.clone());
        }
    }
    Ok(verified)
}

fn matches_file(
    blob: &[u8],
    expected: &ContentDigest,
    hunks: &[UnifiedDiffHunk],
    cancellation: &crate::ProjectCancellation,
) -> Result<bool, DiffReviewError> {
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
    let mut lines = source.lines();
    let mut consumed = 0_u32;
    for hunk in hunks {
        require_not_cancelled(cancellation)?;
        if !matches_hunk_lines(&mut lines, &mut consumed, hunk, cancellation)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn matches_hunk_lines(
    lines: &mut std::str::Lines<'_>,
    consumed: &mut u32,
    hunk: &UnifiedDiffHunk,
    cancellation: &crate::ProjectCancellation,
) -> Result<bool, DiffReviewError> {
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
    if hasher.finalize().as_bytes() != &hunk.old_content_digest {
        return Ok(false);
    }
    Ok(true)
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
