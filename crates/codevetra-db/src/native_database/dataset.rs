//! Atomic visibility for larger typed auxiliary snapshots on the same state owner.

use super::{decode, encode, now};
use crate::{
    NativeDatabase, StorageError,
    rocks::{ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use codevetra_domain::{ContentDigest, GenerationId};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, time::Duration};

const ROWS_PER_PART: usize = 256;
const PART_BYTES: u32 = 4 * 1_048_576;
const TIMEOUT: Duration = Duration::from_secs(300);
// One published head must fit a typed reader's 1 GiB physical allowance,
// including page keys, the manifest, and other snapshot metadata.
const MAXIMUM_DATASET_BYTES: u64 = 512 * 1_048_576;
const MAXIMUM_DATASET_ROWS: u64 = 3_000_000;
const MAXIMUM_DATASET_PARTS: u32 = 12_000;
const MAXIMUM_METADATA_BYTES: usize = 1_048_576;
const PREPARATION_METADATA_ALLOWANCE: u32 = 1_048_576;
const GC_WRITE_BYTES: u32 = 16 * 1_048_576;
const GC_WRITE_ROWS: u32 = 10_000;
const GC_MAXIMUM_SNAPSHOTS: usize = 4;
const GC_MAXIMUM_PAYLOAD_ROWS: u32 = 9990;
const GC_MINIMUM_AGE_SECONDS: i64 = 600;

#[cfg(test)]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    token: String,
    pub(crate) generation: Option<GenerationId>,
    pub(crate) metadata: serde_json::Value,
    rows: u64,
    bytes: u64,
    parts: u32,
    digest: ContentDigest,
    created: String,
}

/// Exact metadata read set captured while computing a derived dataset.
#[derive(Default)]
pub(crate) struct CommitGuard(pub(crate) Vec<(&'static [u8], Option<Vec<u8>>)>);

/// A dataset's destination, generation and auxiliary read fence travel together.
/// Rows remain a separate streaming input; metadata is encoded before native work starts.
pub(crate) struct DatasetReplacement<M> {
    pub(crate) namespace: StateNamespace,
    pub(crate) name: String,
    pub(crate) expected: Option<GenerationId>,
    pub(crate) metadata: M,
    pub(crate) guard: CommitGuard,
}

impl CommitGuard {
    pub(crate) fn capture(
        transaction: &StateTransaction<'_>,
        keys: &[&'static [u8]],
        budget: &mut ReadBudget,
    ) -> Result<Self, RocksError> {
        if keys.len() > 8 {
            return Err(RocksError::InvalidInput {
                field: "dataset_guard",
            });
        }
        let mut entries = Vec::new();
        for &key in keys {
            let bytes = transaction.get(StateNamespace::Metadata, key, budget)?;
            if bytes.as_ref().is_some_and(|bytes| bytes.len() > 4096) {
                return Err(corrupt());
            }
            entries.push((key, bytes));
        }
        Ok(Self(entries))
    }
    fn check(
        &self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
    ) -> Result<(), RocksError> {
        for (key, expected) in &self.0 {
            if transaction.get(StateNamespace::Metadata, key, budget)? != *expected {
                return Err(RocksError::GenerationChanged);
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Prepared {
    name: String,
    manifest: Manifest,
    reclaimed: u32,
}

impl Prepared {
    fn new(name: &str, manifest: &Manifest) -> Self {
        Self {
            name: name.to_owned(),
            manifest: manifest.clone(),
            reclaimed: 0,
        }
    }
}

/// Committed cleanup progress for obsolete auxiliary snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NativeAuxiliaryPruneReport {
    /// Fully collected old or interrupted snapshots.
    pub snapshots_removed: u32,
    /// Payload pages deleted in this committed batch.
    pub pages_removed: u32,
    /// More eligible work remains beyond the independent batch limits.
    pub incomplete: bool,
    /// Independently committed cold-orphan and demand-queue cleanup.
    pub derived: Option<super::retention::NativeDerivedPruneReport>,
    /// Stable native error when auxiliary pages could not be collected.
    pub pages_error: Option<String>,
    /// Stable native error when cold-orphan collection could not complete.
    pub derived_error: Option<String>,
}

impl NativeAuxiliaryPruneReport {
    /// Whether either independently committed maintenance phase failed.
    #[must_use]
    pub const fn maintenance_failed(&self) -> bool {
        self.pages_error.is_some() || self.derived_error.is_some()
    }
}

impl NativeDatabase {
    /// Prepare bounded pages and atomically publish one auxiliary snapshot.
    /// A concurrent replacement fails its pointer comparison; partial pages stay invisible.
    pub(crate) async fn replace_dataset<T, I, M>(
        &self,
        replacement: DatasetReplacement<M>,
        rows: I,
    ) -> Result<(), StorageError>
    where
        T: Serialize + Send + 'static,
        I: Iterator<Item = T> + Send,
        M: Serialize + Sync,
    {
        validate_name(&replacement.name)?;
        let metadata = encode(&replacement.metadata).map_err(super::storage_error)?;
        if metadata.len() > MAXIMUM_METADATA_BYTES {
            return Err(StorageError::InvalidInput {
                field: "dataset_metadata_bytes",
            });
        }
        let metadata =
            serde_json::from_slice(&metadata).map_err(|_| StorageError::InvalidInput {
                field: "dataset_metadata",
            })?;
        let replacement = DatasetReplacement {
            namespace: replacement.namespace,
            name: replacement.name,
            expected: replacement.expected,
            metadata,
            guard: replacement.guard,
        };
        Self::with_preparation_deadline(TIMEOUT, self.prepare_dataset(replacement, rows)).await
    }

    async fn prepare_dataset<T, I>(
        &self,
        replacement: DatasetReplacement<serde_json::Value>,
        mut rows: I,
    ) -> Result<(), StorageError>
    where
        T: Serialize + Send + 'static,
        I: Iterator<Item = T> + Send,
    {
        let DatasetReplacement {
            namespace,
            name,
            expected,
            metadata,
            guard,
        } = replacement;
        let pointer = head_key(&name);
        let before = self
            .state(
                crate::rocks::StateAccess::read_only(None),
                move |transaction, budget| transaction.get(namespace, &pointer, budget),
            )
            .await?;
        let mut manifest = Manifest {
            token: token()?,
            generation: expected.clone(),
            metadata,
            rows: 0,
            bytes: 0,
            parts: 0,
            digest: ContentDigest::from_bytes([0; 32]),
            created: now(),
        };
        let mut digest = blake3::Hasher::new();
        self.write_preparation(namespace, Prepared::new(&name, &manifest), None)
            .await?;
        loop {
            let part = rows.by_ref().take(ROWS_PER_PART).collect::<Vec<_>>();
            if part.is_empty() {
                break;
            }
            let count = part.len();
            let encoded = self
                .state(crate::rocks::StateAccess::read_only(None), move |_, _| {
                    let mut encoded = BoundedWriter(Vec::new());
                    serde_json::to_writer(&mut encoded, &part).map_err(|_| RocksError::Limit {
                        resource: "dataset_part_bytes",
                    })?;
                    Ok(encoded.0)
                })
                .await?;
            manifest.rows =
                manifest
                    .rows
                    .checked_add(count as u64)
                    .ok_or(StorageError::InvalidInput {
                        field: "dataset_rows",
                    })?;
            manifest.bytes = manifest.bytes.checked_add(encoded.len() as u64).ok_or(
                StorageError::InvalidInput {
                    field: "dataset_bytes",
                },
            )?;
            if manifest.rows > MAXIMUM_DATASET_ROWS || manifest.bytes > MAXIMUM_DATASET_BYTES {
                return Err(StorageError::InvalidInput {
                    field: "dataset_capacity",
                });
            }
            digest.update(&encoded);
            let part = manifest.parts;
            manifest.parts = manifest
                .parts
                .checked_add(1)
                .ok_or(StorageError::InvalidInput {
                    field: "dataset_parts",
                })?;
            self.write_preparation(
                namespace,
                Prepared::new(&name, &manifest),
                Some((part, encoded)),
            )
            .await?;
        }
        manifest.digest = ContentDigest::from_bytes(*digest.finalize().as_bytes());
        self.state(
            crate::rocks::StateAccess::write(
                expected,
                PART_BYTES + PREPARATION_METADATA_ALLOWANCE,
                3,
            ),
            move |transaction, budget| {
                if transaction.get(namespace, &head_key(&name), budget)? != before {
                    return Err(RocksError::GenerationChanged);
                }
                guard.check(transaction, budget)?;
                if let Some(epoch) = input_epoch(namespace, &name) {
                    crate::artifacts::native::bump_epoch(transaction, budget, epoch)?;
                }
                transaction.put(
                    namespace,
                    &prepared_key(&name, &manifest.token),
                    encode(&Prepared::new(&name, &manifest))?,
                )?;
                transaction.put(namespace, &head_key(&name), encode(&manifest)?)
            },
        )
        .await
    }

    async fn write_preparation(
        &self,
        namespace: StateNamespace,
        prepared: Prepared,
        part: Option<(u32, Vec<u8>)>,
    ) -> Result<(), StorageError> {
        let marker = prepared_key(&prepared.name, &prepared.manifest.token);
        let encoded = encode(&prepared).map_err(super::storage_error)?;
        let key = part
            .as_ref()
            .map(|(part, _)| part_key(&prepared.name, &prepared.manifest.token, *part));
        self.state(
            crate::rocks::StateAccess::write(None, PART_BYTES + PREPARATION_METADATA_ALLOWANCE, 2),
            move |transaction, _| {
                if let Some((_, bytes)) = part {
                    transaction.put(namespace, key.as_deref().ok_or(corrupt())?, bytes)?;
                }
                transaction.put(namespace, &marker, encoded)
            },
        )
        .await
    }

    pub(crate) async fn clear_dataset(
        &self,
        namespace: StateNamespace,
        name: &str,
    ) -> Result<bool, StorageError> {
        validate_name(name)?;
        let key = head_key(name);
        let epoch = input_epoch(namespace, name);
        self.state(
            crate::rocks::StateAccess::write(None, 4096, 2),
            move |transaction, budget| {
                let present = transaction.get(namespace, &key, budget)?.is_some();
                if present && let Some(epoch) = epoch {
                    crate::artifacts::native::bump_epoch(transaction, budget, epoch)?;
                }
                transaction.delete(namespace, &key)?;
                Ok(present)
            },
        )
        .await
    }

    /// Collect old auxiliary pages in one bounded, resumable transaction.
    /// Live heads stay protected; incomplete preparation has a ten minute grace.
    /// # Errors
    /// Rejects corrupt manifests or exhausted physical reads without partial deletion.
    pub async fn prune_auxiliary_snapshots(
        &self,
    ) -> Result<NativeAuxiliaryPruneReport, StorageError> {
        let request = crate::native_database::request_for(std::time::Duration::from_secs(30))
            .map_err(super::storage_error)?;
        self.prune_auxiliary_snapshots_with_request(&request).await
    }

    /// Collect one auxiliary batch under the original request's deadline and cancellation.
    /// # Errors
    /// Rejects stopped requests before worker admission and preserves atomic rollback.
    pub async fn prune_auxiliary_snapshots_with_request(
        &self,
        request: &crate::rocks::StorageRequest,
    ) -> Result<NativeAuxiliaryPruneReport, StorageError> {
        let pages = self.prune_auxiliary_pages_with_request(request).await;
        let derived = self.prune_derived_state_with_request(request).await;
        let mut report = match pages {
            Ok(report) => report,
            Err(error) => {
                if derived.is_err() {
                    return Err(error);
                }
                NativeAuxiliaryPruneReport {
                    incomplete: true,
                    pages_error: Some(error.to_string()),
                    ..NativeAuxiliaryPruneReport::default()
                }
            }
        };
        match derived {
            Ok(derived) => {
                report.incomplete |= derived.incomplete;
                report.derived = Some(derived);
            }
            Err(error) => {
                report.incomplete = true;
                report.derived_error = Some(error.to_string());
            }
        }
        Ok(report)
    }

    async fn prune_auxiliary_pages_with_request(
        &self,
        request: &crate::rocks::StorageRequest,
    ) -> Result<NativeAuxiliaryPruneReport, StorageError> {
        self.state_with_request(
            crate::rocks::StateAccess::write(None, GC_WRITE_BYTES, GC_WRITE_ROWS),
            request,
            |transaction, budget| {
                let mut plan = AuxiliaryGc::default();
                let timestamp = now().parse::<jiff::Timestamp>().map_err(|_| corrupt())?;
                for namespace in [
                    StateNamespace::History,
                    StateNamespace::Coverage,
                    StateNamespace::Derived,
                    StateNamespace::Artifacts,
                ] {
                    if plan.at_capacity() {
                        plan.report.incomplete = true;
                        break;
                    }
                    plan.scan_namespace(
                        GcScan {
                            reader: Reader::new(transaction, namespace),
                            timestamp,
                        },
                        budget,
                    )?;
                }
                apply_gc_cursors(transaction, plan.cursors)?;
                apply_gc_plans(transaction, plan.plans)?;
                Ok(plan.report)
            },
        )
        .await
    }
}

#[derive(Default)]
struct AuxiliaryGc {
    plans: Vec<(StateNamespace, Vec<u8>, u32, Prepared)>,
    cursors: Vec<(String, Option<Vec<u8>>)>,
    report: NativeAuxiliaryPruneReport,
    rows: u32,
}

#[derive(Clone, Copy)]
struct GcScan<'a, 'store> {
    reader: Reader<'a, 'store>,
    timestamp: jiff::Timestamp,
}

impl AuxiliaryGc {
    fn at_capacity(&self) -> bool {
        self.plans.len() >= GC_MAXIMUM_SNAPSHOTS || self.rows >= GC_MAXIMUM_PAYLOAD_ROWS
    }

    fn scan_namespace(
        &mut self,
        scan: GcScan<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<(), RocksError> {
        let Reader {
            transaction,
            namespace,
        } = scan.reader;
        let cursor_key = format!("gc/dataset/{namespace:?}");
        let after = transaction.get(StateNamespace::Metadata, cursor_key.as_bytes(), budget)?;
        let page = transaction
            .scan(namespace, b"dataset/s/")
            .page(after.as_deref(), 64, budget)?;
        self.report.incomplete |= page.next_after.is_some();
        let mut visited = None;
        let mut exhausted = true;
        for (key, bytes) in page.rows {
            if self.at_capacity() {
                self.report.incomplete = true;
                exhausted = false;
                break;
            }
            visited = Some(key.as_bytes().to_vec());
            self.consider_prepared(scan, budget, (key.as_bytes(), &bytes))?;
        }
        let next = if page.next_after.is_some() || !exhausted {
            visited
        } else {
            None
        };
        self.cursors.push((cursor_key, next));
        Ok(())
    }

    fn consider_prepared(
        &mut self,
        scan: GcScan<'_, '_>,
        budget: &mut ReadBudget,
        record: (&[u8], &[u8]),
    ) -> Result<(), RocksError> {
        let Reader {
            transaction,
            namespace,
        } = scan.reader;
        let timestamp = scan.timestamp;
        let (key, bytes) = record;
        let mut prepared: Prepared = super::decode(bytes)?;
        validate_manifest(&prepared.manifest)?;
        let created = prepared
            .manifest
            .created
            .parse::<jiff::Timestamp>()
            .map_err(|_| corrupt())?;
        if timestamp.as_second().saturating_sub(created.as_second()) < GC_MINIMUM_AGE_SECONDS {
            return Ok(());
        }
        validate_name(&prepared.name).map_err(|_| corrupt())?;
        if key != prepared_key(&prepared.name, &prepared.manifest.token)
            || prepared.manifest.parts > MAXIMUM_DATASET_PARTS
            || prepared.reclaimed > prepared.manifest.parts
        {
            return Err(corrupt());
        }
        if Reader::new(transaction, namespace)
            .at(&prepared.name)
            .manifest(budget)?
            .is_some_and(|head| head.token == prepared.manifest.token)
        {
            return Ok(());
        }
        let first = prepared.reclaimed;
        let end = prepared
            .manifest
            .parts
            .min(first.saturating_add(GC_MAXIMUM_PAYLOAD_ROWS - self.rows));
        self.rows = self.rows.saturating_add(end - first).saturating_add(1);
        prepared.reclaimed = end;
        self.report.pages_removed += end - first;
        if end == prepared.manifest.parts {
            self.report.snapshots_removed += 1;
        } else {
            self.report.incomplete = true;
        }
        self.plans.push((namespace, key.to_vec(), first, prepared));
        Ok(())
    }
}

fn apply_gc_plans(
    transaction: &mut StateTransaction<'_>,
    plans: Vec<(StateNamespace, Vec<u8>, u32, Prepared)>,
) -> Result<(), RocksError> {
    for (namespace, key, first, prepared) in plans {
        for part in first..prepared.reclaimed {
            transaction.delete(
                namespace,
                &part_key(&prepared.name, &prepared.manifest.token, part),
            )?;
        }
        if prepared.reclaimed == prepared.manifest.parts {
            transaction.delete(namespace, &key)?;
        } else {
            transaction.put(namespace, &key, encode(&prepared)?)?;
        }
    }
    Ok(())
}

fn apply_gc_cursors(
    transaction: &mut StateTransaction<'_>,
    cursors: Vec<(String, Option<Vec<u8>>)>,
) -> Result<(), RocksError> {
    for (cursor, next) in cursors {
        if let Some(next) = next {
            transaction.put(StateNamespace::Metadata, cursor.as_bytes(), next)?;
        } else {
            transaction.delete(StateNamespace::Metadata, cursor.as_bytes())?;
        }
    }
    Ok(())
}

/// Dataset reads retain one transaction snapshot and namespace across metadata and payload pages.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'a, 'store> {
    transaction: &'a StateTransaction<'store>,
    namespace: StateNamespace,
}

impl<'a, 'store> Reader<'a, 'store> {
    pub(crate) const fn new(
        transaction: &'a StateTransaction<'store>,
        namespace: StateNamespace,
    ) -> Self {
        Self {
            transaction,
            namespace,
        }
    }
    pub(crate) const fn at(self, name: &'a str) -> Dataset<'a, 'store> {
        Dataset { reader: self, name }
    }
}

pub(crate) struct Dataset<'a, 'store> {
    reader: Reader<'a, 'store>,
    name: &'a str,
}

impl Dataset<'_, '_> {
    pub(crate) fn manifest(&self, budget: &mut ReadBudget) -> Result<Option<Manifest>, RocksError> {
        let Reader {
            transaction,
            namespace,
        } = self.reader;
        let name = self.name;

        transaction
            .get(namespace, &head_key(name), budget)?
            .map(|bytes| {
                let manifest = decode::<Manifest>(&bytes)?;
                validate_manifest(&manifest)?;
                Ok(manifest)
            })
            .transpose()
    }
}

impl Dataset<'_, '_> {
    pub(crate) fn visit<T: DeserializeOwned>(
        &self,
        manifest: &Manifest,
        budget: &mut ReadBudget,
        mut visitor: impl FnMut(T, &mut ReadBudget) -> Result<(), RocksError>,
    ) -> Result<(), RocksError> {
        let Reader {
            transaction,
            namespace,
        } = self.reader;
        let name = self.name;

        validate_manifest(manifest)?;
        let mut rows = 0_u64;
        let mut bytes = 0_u64;
        let mut digest = blake3::Hasher::new();
        for part in 0..manifest.parts {
            let encoded = transaction
                .get(namespace, &part_key(name, &manifest.token, part), budget)?
                .ok_or(corrupt())?;
            if encoded.len() > PART_BYTES as usize {
                return Err(corrupt());
            }
            bytes = bytes.checked_add(encoded.len() as u64).ok_or(corrupt())?;
            digest.update(&encoded);
            let values: Vec<T> = decode(&encoded)?;
            if values.is_empty() || values.len() > ROWS_PER_PART {
                return Err(corrupt());
            }
            rows = rows.checked_add(values.len() as u64).ok_or(corrupt())?;
            for value in values {
                budget.checkpoint()?;
                visitor(value, budget)?;
            }
        }
        if rows != manifest.rows
            || bytes != manifest.bytes
            || ContentDigest::from_bytes(*digest.finalize().as_bytes()) != manifest.digest
        {
            return Err(corrupt());
        }
        Ok(())
    }
}

impl Reader<'_, '_> {
    pub(crate) fn list(
        &self,
        prefix: &str,
        budget: &mut ReadBudget,
    ) -> Result<BTreeMap<String, Manifest>, RocksError> {
        let Self {
            transaction,
            namespace,
        } = *self;

        let mut output = BTreeMap::new();
        let prefix = format!("dataset/h/{prefix}");
        let mut bytes = 0_usize;
        super::visit_state::<Manifest>(
            &transaction.scan(namespace, prefix.as_bytes()),
            budget,
            |key, manifest, _| {
                validate_manifest(&manifest)?;
                bytes = bytes.saturating_add(key.len() + encode(&manifest)?.len());
                if bytes > 16 * 1_048_576 || output.len() >= 8192 {
                    return Err(RocksError::Limit {
                        resource: "dataset_headers",
                    });
                }
                let name = std::str::from_utf8(key.get(b"dataset/h/".len()..).ok_or(corrupt())?)
                    .map_err(|_| corrupt())?;
                output.insert(name.to_owned(), manifest);
                Ok(())
            },
        )?;
        Ok(output)
    }
}

fn head_key(name: &str) -> Vec<u8> {
    format!("dataset/h/{name}").into_bytes()
}
fn prepared_key(name: &str, token: &str) -> Vec<u8> {
    format!("dataset/s/{name}/{token}").into_bytes()
}

fn valid_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_manifest(manifest: &Manifest) -> Result<(), RocksError> {
    if !valid_token(&manifest.token) || manifest.created.parse::<jiff::Timestamp>().is_err() {
        return Err(corrupt());
    }
    if manifest.rows > MAXIMUM_DATASET_ROWS
        || manifest.parts > MAXIMUM_DATASET_PARTS
        || manifest.bytes > MAXIMUM_DATASET_BYTES
    {
        return Err(corrupt());
    }
    // Empty heads have no payload. Every nonempty part represents one to ROWS_PER_PART rows.
    if manifest.parts == 0 && (manifest.rows != 0 || manifest.bytes != 0) {
        return Err(corrupt());
    }
    let parts = u64::from(manifest.parts);
    if !(parts..=parts * ROWS_PER_PART as u64).contains(&manifest.rows) {
        return Err(corrupt());
    }
    Ok(())
}

fn input_epoch(namespace: StateNamespace, name: &str) -> Option<&'static [u8]> {
    if namespace == StateNamespace::Coverage {
        Some(b"coverage_epoch")
    } else if namespace == StateNamespace::Derived && name == "similarity" {
        Some(b"similarity_epoch")
    } else {
        None
    }
}
fn part_key(name: &str, token: &str, part: u32) -> Vec<u8> {
    format!("dataset/b/{name}/{token}/{part:08x}").into_bytes()
}
fn corrupt() -> RocksError {
    RocksError::Corrupt { field: "dataset" }
}

fn validate_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty()
        || name.len() > 256
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-'))
    {
        return Err(StorageError::InvalidInput {
            field: "dataset_name",
        });
    }
    Ok(())
}

fn token() -> Result<String, StorageError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| StorageError::DatabaseOperation {
        operation: "dataset-identity",
    })?;
    Ok(blake3::Hash::from_bytes(bytes).to_hex().to_string())
}

struct BoundedWriter(Vec<u8>);
impl std::io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > PART_BYTES as usize {
            return Err(std::io::Error::other("dataset part exceeds its byte bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
