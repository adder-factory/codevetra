//! Cached native detector results preserve complete counts and exact input fingerprints.

mod clones;
mod computation;
mod coverage;
mod documents;
mod file_counts;
mod framework;
mod inventory;
mod rules;

use super::{corrupt, workspace};
use crate::{
    NativeDatabase, StorageError, StructuralFindingRefresh,
    insights::{
        DEFAULT_INSIGHT_TIMEOUT, StructuralFinding, StructuralFindingComputation,
        StructuralFindingComputationState as Computation, StructuralFindingGroup,
        StructuralFindingGroupQuery, StructuralFindingQuery, StructuralFindingSeverity,
        StructuralFindingStats, validate_limit,
    },
    native_database::{
        dataset::{self, CommitGuard, Manifest},
        encode,
    },
    retrieval::native::retain,
    rocks::{
        ReadBudget, RevisionState, RocksError, StateNamespace, StateTransaction, StoredRevision,
    },
};
use codevetra_domain::{ContentDigest, GenerationId, ProjectId};
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::BTreeMap, time::Duration};

const NAME: &str = "structural-findings";
const NAMESPACE: StateNamespace = StateNamespace::Derived;
const INPUTS: &[&[u8]] = &[
    b"coverage_epoch",
    b"similarity_epoch",
    b"vector_epoch",
    b"summary_epoch",
];

struct Fingerprint {
    generation: GenerationId,
    previous: Option<StoredRevision>,
    digest: ContentDigest,
    guard: CommitGuard,
}
#[derive(Serialize, Deserialize)]
struct Run {
    fingerprint: ContentDigest,
    stats: StructuralFindingStats,
}
struct Prepared {
    fingerprint: Fingerprint,
    rows: Vec<StructuralFinding>,
    stats: StructuralFindingStats,
    _admission: tokio::sync::OwnedMutexGuard<()>,
}

impl NativeDatabase {
    /// Replace the complete detector relation only after source and auxiliary fences pass.
    /// # Errors
    /// Rejects exceeded work bounds, corrupt facts, or inputs changed during computation.
    pub async fn refresh_current_structural_findings(
        &self,
        project: &ProjectId,
        timeout: Duration,
    ) -> Result<StructuralFindingRefresh, StorageError> {
        self.require_project(project)?;
        Self::with_preparation_deadline(timeout, self.refresh_findings(timeout)).await
    }

    async fn refresh_findings(
        &self,
        timeout: Duration,
    ) -> Result<StructuralFindingRefresh, StorageError> {
        let current = self
            .state_with_timeout(None, 0, 0, timeout, |transaction, budget| {
                let Some(fingerprint) = fingerprint(transaction, budget)? else {
                    return Ok(None);
                };
                Ok(Some(cached(transaction, budget, &fingerprint)?.is_some()))
            })
            .await?;
        match current {
            None => return Ok(StructuralFindingRefresh::NoCurrentGeneration),
            Some(true) => return Ok(StructuralFindingRefresh::Current),
            Some(false) => {}
        }
        let prepared = computation::prepare(self, timeout)
            .await?
            .ok_or(StorageError::CurrentGenerationChanged)?;
        let run = Run {
            fingerprint: prepared.fingerprint.digest,
            stats: prepared.stats,
        };
        self.replace_dataset_guarded(
            NAMESPACE,
            NAME,
            Some(prepared.fingerprint.generation),
            &run,
            prepared.rows.into_iter(),
            prepared.fingerprint.guard,
        )
        .await?;
        Ok(StructuralFindingRefresh::Recomputed)
    }

    /// Read complete cached finding totals without recomputation.
    /// # Errors
    /// Rejects mismatched project, corrupt cached metadata, or exhausted native reads.
    pub async fn cached_current_structural_finding_stats(
        &self,
        project: &ProjectId,
    ) -> Result<Option<StructuralFindingStats>, StorageError> {
        self.require_project(project)?;
        self.state(None, 0, 0, |transaction, budget| {
            let Some(fingerprint) = fingerprint(transaction, budget)? else {
                return Ok(None);
            };
            cached(transaction, budget, &fingerprint).map(|run| run.map(|(_, run)| run.stats))
        })
        .await
    }

    /// Read cached totals or the explicit not-computed state.
    /// # Errors
    /// Rejects mismatched project and failed bounded native reads.
    pub async fn current_structural_finding_stats(
        &self,
        project: &ProjectId,
    ) -> Result<StructuralFindingStats, StorageError> {
        Ok(self
            .cached_current_structural_finding_stats(project)
            .await?
            .unwrap_or_default())
    }

    /// Compute complete totals under an explicit diagnostic deadline without publishing.
    /// # Errors
    /// Rejects invalid deadlines and exceeded work or memory bounds.
    pub async fn current_structural_finding_stats_bounded(
        &self,
        project: &ProjectId,
        timeout: Duration,
    ) -> Result<StructuralFindingStats, StorageError> {
        self.require_project(project)?;
        if timeout.is_zero() || timeout > DEFAULT_INSIGHT_TIMEOUT {
            return Err(StorageError::InvalidInput {
                field: "statement_timeout",
            });
        }
        Ok(computation::prepare(self, timeout)
            .await?
            .map(|prepared| prepared.stats)
            .unwrap_or_default())
    }

    /// Read an info-or-worse ranked page from a complete current detector relation.
    /// # Errors
    /// Rejects invalid bounds, corrupt findings, or exhausted bounded reads.
    pub async fn current_structural_findings(
        &self,
        project: &ProjectId,
        limit: u16,
    ) -> Result<Vec<StructuralFinding>, StorageError> {
        self.query_current_structural_findings(
            project,
            &StructuralFindingQuery::new(limit)?
                .with_minimum_severity(StructuralFindingSeverity::Info),
        )
        .await
    }

    /// Apply every source, detector, severity, metric and centrality filter before ranking.
    /// # Errors
    /// Rejects invalid bounds, corrupt findings, or exhausted native reads.
    pub async fn query_current_structural_findings(
        &self,
        project: &ProjectId,
        request: &StructuralFindingQuery,
    ) -> Result<Vec<StructuralFinding>, StorageError> {
        self.require_project(project)?;
        validate_limit(request.limit)?;
        let request = request.clone();
        self.state(None, 0, 0, move |transaction, budget| {
            let mut output = Vec::new();
            visit_current(transaction, budget, |row, _| {
                if matches(&row, &request)? {
                    retain(&mut output, row, usize::from(request.limit), order);
                }
                if output.iter().map(row_bytes).sum::<usize>() > 32 * 1_048_576 {
                    return Err(workspace());
                }
                Ok(())
            })?;
            Ok(output)
        })
        .await
    }

    /// Count every matching finding before the independent response row bound.
    /// # Errors
    /// Rejects invalid bounds, corrupt findings, or exhausted bounded reads.
    pub async fn count_current_structural_findings(
        &self,
        project: &ProjectId,
        request: &StructuralFindingQuery,
    ) -> Result<u64, StorageError> {
        self.require_project(project)?;
        validate_limit(request.limit)?;
        let request = request.clone();
        self.state(None, 0, 0, move |transaction, budget| {
            let mut count = 0_u64;
            visit_current(transaction, budget, |row, _| {
                count += u64::from(matches(&row, &request)?);
                Ok(())
            })?;
            Ok(count)
        })
        .await
    }

    /// Rank detector groups independently so a noisy rule cannot starve others.
    /// # Errors
    /// Rejects malformed cached rows and exhausted read or retained-result budgets.
    pub async fn query_current_structural_findings_per_detector(
        &self,
        project: &ProjectId,
        request: &StructuralFindingGroupQuery,
    ) -> Result<StructuralFindingGroup, StorageError> {
        self.require_project(project)?;
        let request = request.clone();
        self.state(None, 0, 0, move |transaction, budget| {
            let mut counts = request
                .findings
                .iter()
                .map(|name| (name.clone(), 0_u64))
                .collect::<BTreeMap<_, _>>();
            let mut groups = BTreeMap::<String, Vec<StructuralFinding>>::new();
            let mut bytes = 0_usize;
            let computation = visit_current(transaction, budget, |row, _| {
                if let Some(count) = counts.get_mut(&row.finding)
                    && rank(&row.severity)? >= request.minimum_severity.rank()
                    && (!request.exclude_fixtures || !framework::fixture(&row.path)?)
                    && request.path_prefix.as_ref().is_none_or(|prefix| {
                        row.path == *prefix
                            || row
                                .path
                                .strip_prefix(prefix)
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    })
                {
                    *count += 1;
                    let rows = groups.entry(row.finding.clone()).or_default();
                    let before = rows.iter().map(row_bytes).sum::<usize>();
                    retain(rows, row, usize::from(request.per_finding_limit), order);
                    bytes = bytes
                        .checked_sub(before)
                        .and_then(|bytes| {
                            bytes.checked_add(rows.iter().map(row_bytes).sum::<usize>())
                        })
                        .ok_or_else(workspace)?;
                    if bytes > 32 * 1_048_576 {
                        return Err(workspace());
                    }
                }
                Ok(())
            })?;
            Ok(StructuralFindingGroup {
                findings: groups.into_values().flatten().collect(),
                counts,
                computation,
            })
        })
        .await
    }
}

fn fingerprint(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<Option<Fingerprint>, RocksError> {
    let Some(current) = transaction.current()? else {
        return Ok(None);
    };
    let mut cursor = None;
    let mut previous: Option<StoredRevision> = None;
    loop {
        let (rows, next) = transaction.revisions(cursor, 256, budget)?;
        for row in rows {
            if row.state == RevisionState::Superseded
                && row.sequence < current.sequence
                && previous
                    .as_ref()
                    .is_none_or(|previous| previous.sequence < row.sequence)
            {
                previous = Some(row);
            }
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    let guard = CommitGuard::capture(transaction, INPUTS, budget)?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"cartograph-native-findings-v2");
    hash.update(codevetra_domain::biomarkers::SOURCE_RULE_CONTRACT.as_bytes());
    hash.update(current.generation_id.as_str().as_bytes());
    if let Some(previous) = &previous {
        hash.update(previous.generation_id.as_str().as_bytes());
    }
    hash.update(&encode(&guard.0)?);
    hash.update(
        &jiff::Timestamp::now()
            .as_second()
            .div_euclid(86400)
            .to_be_bytes(),
    );
    for contract in [
        include_str!("findings/computation.rs"),
        include_str!("findings/documents.rs"),
        include_str!("findings/inventory.rs"),
        include_str!("findings/file_counts.rs"),
        include_str!("findings/rules.rs"),
        include_str!("findings/clones.rs"),
        include_str!("findings/clones/groups.rs"),
        include_str!("findings/framework.rs"),
        include_str!("findings/coverage.rs"),
    ] {
        hash.update(contract.as_bytes());
    }
    Ok(Some(Fingerprint {
        generation: current.generation_id,
        previous,
        digest: ContentDigest::from_bytes(*hash.finalize().as_bytes()),
        guard,
    }))
}
fn cached(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    fingerprint: &Fingerprint,
) -> Result<Option<(Manifest, Run)>, RocksError> {
    let Some(manifest) = dataset::manifest(transaction, NAMESPACE, NAME, budget)? else {
        return Ok(None);
    };
    if manifest.generation.as_ref() != Some(&fingerprint.generation) {
        return Ok(None);
    }
    let run: Run = serde_json::from_value(manifest.metadata.clone()).map_err(|_| corrupt())?;
    if run.fingerprint != fingerprint.digest {
        return Ok(None);
    }
    if run.stats.state != Computation::Ready
        || !run.stats.code_health_score.is_finite()
        || !(0.0..=100.0).contains(&run.stats.code_health_score)
        || run
            .stats
            .info_findings
            .checked_add(run.stats.warning_findings)
            .and_then(|total| total.checked_add(run.stats.error_findings))
            != Some(run.stats.total_findings)
        || run
            .stats
            .by_finding
            .values()
            .try_fold(0_u64, |sum, count| sum.checked_add(*count))
            != Some(run.stats.total_findings)
    {
        return Err(corrupt());
    }
    Ok(Some((manifest, run)))
}
fn visit_current(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(StructuralFinding, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<Option<StructuralFindingComputation>, RocksError> {
    let Some(fingerprint) = fingerprint(transaction, budget)? else {
        return Ok(None);
    };
    let Some((manifest, run)) = cached(transaction, budget, &fingerprint)? else {
        return Ok(None);
    };
    dataset::visit(
        transaction,
        NAMESPACE,
        NAME,
        &manifest,
        budget,
        |row: StructuralFinding, budget| {
            if !row.metric.is_finite()
                || !row.degree_centrality.is_finite()
                || !(0.0..=1.0).contains(&row.degree_centrality)
                || row.start_line == 0
                || row.end_line < row.start_line
            {
                return Err(corrupt());
            }
            rank(&row.severity)?;
            visit(row, budget)
        },
    )?;
    Ok(Some(StructuralFindingComputation {
        generation_id: fingerprint.generation,
        input_fingerprint: fingerprint.digest,
        stats: run.stats,
    }))
}

fn matches(row: &StructuralFinding, request: &StructuralFindingQuery) -> Result<bool, RocksError> {
    Ok(request
        .finding
        .as_ref()
        .is_none_or(|finding| finding == &row.finding)
        && rank(&row.severity)? >= request.minimum_severity.rank()
        && request
            .minimum_metric
            .is_none_or(|metric| row.metric >= metric)
        && request
            .maximum_metric
            .is_none_or(|metric| row.metric <= metric)
        && request
            .minimum_centrality
            .is_none_or(|centrality| row.degree_centrality >= centrality)
        && request
            .excluded_path_prefix
            .as_ref()
            .is_none_or(|prefix| !row.path.starts_with(prefix))
        && request
            .path_prefix
            .as_ref()
            .is_none_or(|prefix| row.path.starts_with(prefix))
        && (request.symbol_ids.is_empty()
            || request
                .symbol_ids
                .iter()
                .any(|id| id.as_str() == row.symbol_id))
        && (!request.exclude_fixtures || !framework::fixture(&row.path)?))
}
fn rank(severity: &str) -> Result<i16, RocksError> {
    match severity {
        "error" => Ok(3),
        "warning" => Ok(2),
        "info" => Ok(1),
        _ => Err(corrupt()),
    }
}
fn order(a: &StructuralFinding, b: &StructuralFinding) -> Ordering {
    // Stored and computed severities are validated before entering this comparator.
    rank(&b.severity)
        .unwrap_or(0)
        .cmp(&rank(&a.severity).unwrap_or(0))
        .then(b.degree_centrality.total_cmp(&a.degree_centrality))
        .then(b.metric.total_cmp(&a.metric))
        .then(a.path.cmp(&b.path))
        .then(a.start_line.cmp(&b.start_line))
        .then(a.finding.cmp(&b.finding))
        .then(a.symbol_id.cmp(&b.symbol_id))
}
fn row_bytes(row: &StructuralFinding) -> usize {
    row.path.len()
        + row.qualified_name.len()
        + row.finding.len()
        + row.metric_name.len()
        + 512
        + row.detail.as_ref().map_or(0, |detail| {
            serde_json::to_vec(detail).map_or(usize::MAX / 2, |bytes| bytes.len())
        })
}

// A singleton packed-key B-tree owns a complete leaf allocation. Later
// entries include minimum node occupancy and allocator overhead on the pinned
// 64-bit Rust toolchain; the initial root must not be amortized across siblings.
fn packed_tree_bytes(entries: usize) -> usize {
    if entries == 0 {
        0
    } else {
        384 + (entries - 1) * 64
    }
}

fn packed_tree_insertion_bytes(entries: usize) -> usize {
    if entries == 0 { 384 } else { 64 }
}
