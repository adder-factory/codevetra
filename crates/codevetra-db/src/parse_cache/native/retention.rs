//! Bounded header, payload, and interrupted-preparation collection.

use super::{
    Header, NAMESPACE, Prepared, check_header, chunk_key, corrupt, header_key, prepared_key,
};
use crate::{
    NativeDatabase, NativeParseCacheBudgetPressure, NativeParseCacheError,
    NativeParseCacheRetentionPolicy, NativeParseCacheRetentionReport, NativeParseCacheStats,
    native_database::{now, timestamp_order, visit_state},
    retrieval::native::{RankedRows, visit_files},
    rocks::{ReadBudget, RocksError, StateTransaction},
};
use codevetra_domain::{ContentDigest, ProjectId};
use std::collections::{BTreeMap, BTreeSet};

struct Inventory {
    stats: NativeParseCacheStats,
    contracts: BTreeMap<ContentDigest, (u64, String)>,
    candidates: RetentionCandidates,
    live: BTreeSet<(String, String, ContentDigest)>,
    workspace: usize,
    candidate_total: u64,
}

impl NativeDatabase {
    /// Collect obsolete extraction payloads and old interrupted preparations.
    /// The published graph and current extractor's exact source identities stay protected.
    /// # Errors
    /// Rejects project mismatch, corrupt cache metadata, or exhausted bounded work.
    pub async fn prune_native_parse_cache(
        &self,
        project: &ProjectId,
        protected: &ContentDigest,
        policy: NativeParseCacheRetentionPolicy,
    ) -> Result<NativeParseCacheRetentionReport, NativeParseCacheError> {
        let request = crate::native_database::request_for(std::time::Duration::from_secs(30))
            .map_err(crate::native_database::storage_error)
            .map_err(NativeParseCacheError::from)?;
        self.prune_native_parse_cache_with_request(project, policy.protecting(protected), &request)
            .await
    }

    /// Collect one cache batch under the calling operation's deadline and cancellation.
    /// # Errors
    /// Preserves the native request failure and rejects invalid or corrupt cache data.
    pub async fn prune_native_parse_cache_with_request(
        &self,
        project: &ProjectId,
        selection: crate::NativeParseCacheRetention<'_>,
        request: &crate::rocks::StorageRequest,
    ) -> Result<NativeParseCacheRetentionReport, NativeParseCacheError> {
        self.require_project(project)
            .map_err(NativeParseCacheError::from)?;
        let protected = selection.protected.clone();
        let policy = selection.policy;
        self.state_with_request(
            crate::rocks::StateAccess::write(None, PRUNE_WRITE_BYTES, PRUNE_WRITE_ROWS),
            request,
            move |transaction, budget| {
                let mut inventory = inventory(transaction, budget, policy.protecting(&protected))?;
                let before = inventory.stats;
                let CacheRemoval {
                    headers: removal,
                    rows: row_cost,
                    incomplete,
                } = plan_removal(&mut inventory, &protected, policy)?;
                let orphans = orphaned(
                    transaction,
                    budget,
                    PRUNE_WRITE_ROWS.saturating_sub(row_cost),
                )?;
                for header in &removal {
                    remove(transaction, header, true)?;
                }
                for (key, header, visible) in orphans {
                    if !visible {
                        remove(transaction, &header, false)?;
                    }
                    transaction.delete(NAMESPACE, &key)?;
                }
                inventory.stats.contracts = inventory
                    .contracts
                    .values()
                    .filter(|(count, _)| *count > 0)
                    .count() as u64;
                Ok(NativeParseCacheRetentionReport {
                    before,
                    after: inventory.stats,
                    rows_removed: removal.len() as u64,
                    payload_bytes_removed: before.payload_bytes - inventory.stats.payload_bytes,
                    deletion_limit_reached: incomplete
                        || (inventory.candidates.headers.len() as u64) < inventory.candidate_total,
                    pressure: NativeParseCacheBudgetPressure {
                        over_row_budget: inventory.stats.rows > policy.maximum_rows(),
                        over_payload_byte_budget: inventory.stats.payload_bytes
                            > policy.maximum_payload_bytes(),
                        over_contract_budget: inventory.stats.contracts
                            > u64::from(policy.maximum_contracts()),
                    },
                })
            },
        )
        .await
        .map_err(NativeParseCacheError::from)
    }
}

struct CacheRemoval {
    headers: Vec<Header>,
    rows: u32,
    incomplete: bool,
}

fn plan_removal(
    inventory: &mut Inventory,
    protected: &ContentDigest,
    policy: NativeParseCacheRetentionPolicy,
) -> Result<CacheRemoval, RocksError> {
    let allowed = retained_contracts(inventory, protected, policy);
    let mut plan = CacheRemoval {
        headers: Vec::new(),
        rows: 0,
        incomplete: false,
    };
    for header in &inventory.candidates.headers {
        let obsolete = !allowed.contains(&header.key.extractor_contract_digest)
            || (header.key.extractor_contract_digest == *protected && !is_live(inventory, header));
        if !obsolete && !over_capacity(inventory, policy) {
            continue;
        }
        let cost = header.chunks.checked_add(2).ok_or(corrupt())?;
        if plan.headers.len()
            >= usize::try_from(policy.maximum_deletions()).map_err(|_| corrupt())?
            || plan.rows.saturating_add(cost) > 98_000
        {
            plan.incomplete = true;
            continue;
        }
        plan.rows += cost;
        plan.headers.push(header.clone());
        inventory.stats.rows = inventory.stats.rows.checked_sub(1).ok_or(corrupt())?;
        inventory.stats.payload_bytes = inventory
            .stats
            .payload_bytes
            .checked_sub(header.bytes as u64)
            .ok_or(corrupt())?;
        if let Some((count, _)) = inventory
            .contracts
            .get_mut(&header.key.extractor_contract_digest)
        {
            *count = count.checked_sub(1).ok_or(corrupt())?;
        }
    }
    Ok(plan)
}

impl Inventory {
    fn record_header(&mut self, header: &Header) -> Result<(), RocksError> {
        check_header(header, &header.key)?;
        self.stats.rows = self.stats.rows.checked_add(1).ok_or(corrupt())?;
        self.stats.payload_bytes = self
            .stats
            .payload_bytes
            .checked_add(header.bytes as u64)
            .ok_or(corrupt())?;
        if !self
            .contracts
            .contains_key(&header.key.extractor_contract_digest)
        {
            if self.contracts.len() >= 8192 {
                return Err(workspace());
            }
            self.contracts.insert(
                header.key.extractor_contract_digest.clone(),
                (0, header.last_used.clone()),
            );
        }
        let contract = self
            .contracts
            .get_mut(&header.key.extractor_contract_digest)
            .ok_or(corrupt())?;
        contract.0 += 1;
        if timestamp_order(&header.last_used, &contract.1).is_gt() {
            contract.1.clone_from(&header.last_used);
        }
        Ok(())
    }
}

fn inventory(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    selection: crate::NativeParseCacheRetention<'_>,
) -> Result<Inventory, RocksError> {
    let crate::NativeParseCacheRetention { protected, policy } = selection;
    let mut inventory = Inventory {
        stats: NativeParseCacheStats::default(),
        contracts: BTreeMap::new(),
        candidates: RetentionCandidates::new(0),
        live: BTreeSet::new(),
        workspace: 0,
        candidate_total: 0,
    };
    if transaction.current()?.is_some() {
        let snapshot = transaction.graph_snapshot()?;
        let graph = snapshot.graph(budget)?;
        visit_files(&graph, budget, |file, _| {
            inventory.workspace = inventory
                .workspace
                .saturating_add(file.normalized_path.len() + file.language.len() + 256);
            if inventory.workspace > 16 * 1_048_576 {
                return Err(workspace());
            }
            inventory
                .live
                .insert((file.normalized_path, file.language, file.content_hash));
            Ok(())
        })?;
    }
    visit_state::<Header>(
        &transaction.scan(NAMESPACE, b"h/"),
        budget,
        |_, header, _| inventory.record_header(&header),
    )?;
    inventory.stats.contracts = inventory.contracts.len() as u64;
    let allowed = retained_contracts(&inventory, protected, policy);
    let capacity = over_capacity(&inventory, policy);
    inventory.candidates = RetentionCandidates::new(
        usize::try_from(policy.maximum_deletions())
            .map_err(|_| corrupt())?
            .min(MAXIMUM_CANDIDATES),
    );
    visit_state::<Header>(
        &transaction.scan(NAMESPACE, b"h/"),
        budget,
        |_, header, _| {
            let protected_live =
                header.key.extractor_contract_digest == *protected && is_live(&inventory, &header);
            let obsolete = !allowed.contains(&header.key.extractor_contract_digest)
                || (header.key.extractor_contract_digest == *protected && !protected_live);
            if !protected_live && (obsolete || capacity) {
                inventory.candidate_total += 1;
                inventory.candidates.retain(header)?;
            }
            Ok(())
        },
    )?;
    Ok(inventory)
}

const MAXIMUM_CANDIDATES: usize = 33_000;
const CANDIDATE_ENTRY_OVERHEAD: usize = 512;
const CANDIDATE_WORKSPACE_BYTES: usize = 32 * 1_048_576;
const PRUNE_WRITE_BYTES: u32 = 16 * 1_048_576;
const PRUNE_WRITE_ROWS: u32 = 100_000;

struct RetentionCandidates {
    headers: RankedRows<Header, fn(&Header, &Header) -> std::cmp::Ordering>,
    bytes: usize,
}

impl RetentionCandidates {
    fn new(maximum: usize) -> Self {
        Self {
            headers: RankedRows::new(maximum, |a, b| {
                timestamp_order(&a.last_used, &b.last_used).then(a.key.path.cmp(&b.key.path))
            }),
            bytes: 0,
        }
    }

    fn retain(&mut self, header: Header) -> Result<(), RocksError> {
        if !self.headers.would_retain(&header) {
            return Ok(());
        }
        if self.headers.is_full() {
            let last = self.headers.last().ok_or(corrupt())?;
            self.bytes = self
                .bytes
                .saturating_sub(last.key.path.as_str().len() + CANDIDATE_ENTRY_OVERHEAD);
        }
        self.bytes = self
            .bytes
            .saturating_add(header.key.path.as_str().len() + CANDIDATE_ENTRY_OVERHEAD);
        if self.bytes > CANDIDATE_WORKSPACE_BYTES {
            return Err(workspace());
        }
        self.headers.insert(header);
        Ok(())
    }
}

fn retained_contracts(
    inventory: &Inventory,
    protected: &ContentDigest,
    policy: NativeParseCacheRetentionPolicy,
) -> BTreeSet<ContentDigest> {
    let mut contracts = inventory
        .contracts
        .iter()
        .filter(|(contract, _)| *contract != protected)
        .collect::<Vec<_>>();
    contracts.sort_by(|a, b| timestamp_order(&b.1.1, &a.1.1).then(a.0.cmp(b.0)));
    contracts
        .into_iter()
        .take(usize::from(policy.maximum_contracts().saturating_sub(1)))
        .map(|(contract, _)| contract.clone())
        .chain(std::iter::once(protected.clone()))
        .collect()
}

fn is_live(inventory: &Inventory, header: &Header) -> bool {
    inventory.live.contains(&(
        header.key.path.as_str().to_owned(),
        header.key.language.as_str().to_owned(),
        header.key.content_hash.clone(),
    ))
}

fn over_capacity(inventory: &Inventory, policy: NativeParseCacheRetentionPolicy) -> bool {
    inventory.stats.rows > policy.maximum_rows()
        || inventory.stats.payload_bytes > policy.maximum_payload_bytes()
}

type Orphan = (Vec<u8>, Header, bool);

fn orphaned(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    maximum_rows: u32,
) -> Result<Vec<Orphan>, RocksError> {
    let timestamp = now().parse::<jiff::Timestamp>().map_err(|_| corrupt())?;
    let mut output = Vec::new();
    let mut rows = 0_u32;
    visit_state::<Prepared>(
        &transaction.scan(NAMESPACE, b"s/"),
        budget,
        |key, prepared, budget| {
            let created = prepared
                .created
                .parse::<jiff::Timestamp>()
                .map_err(|_| corrupt())?;
            if timestamp.as_second().saturating_sub(created.as_second()) < 600 {
                return Ok(());
            }
            check_header(&prepared.header, &prepared.header.key)?;
            let visible = transaction
                .get(NAMESPACE, &header_key(&prepared.header.key)?, budget)?
                .map(|bytes| crate::native_database::decode::<Header>(&bytes))
                .transpose()?
                .is_some_and(|header| header.digest == prepared.header.digest);
            let cost = if visible {
                1
            } else {
                prepared.header.chunks.saturating_add(1)
            };
            if rows.saturating_add(cost) <= maximum_rows && output.len() < 2000 {
                rows += cost;
                output.push((key, prepared.header, visible));
            }
            Ok(())
        },
    )?;
    Ok(output)
}

fn remove(
    transaction: &mut StateTransaction<'_>,
    header: &Header,
    visible: bool,
) -> Result<(), RocksError> {
    for part in 0..header.chunks {
        transaction.delete(NAMESPACE, &chunk_key(header, part)?)?;
    }
    if visible {
        transaction.delete(NAMESPACE, &header_key(&header.key)?)?;
    }
    transaction.delete(NAMESPACE, &prepared_key(header)?)
}

fn workspace() -> RocksError {
    RocksError::Limit {
        resource: "cache_retention_workspace",
    }
}
