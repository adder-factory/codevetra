//! Storage-neutral feature services share the existing native project owner.

use std::time::Duration;

const GRAPH_READ_ROWS: usize = 2_000_000;

tokio::task_local! {
    static ACTIVE_REQUEST: StorageRequest;
}

pub(crate) fn request_for(timeout: Duration) -> Result<StorageRequest, RocksError> {
    ACTIVE_REQUEST
        .try_with(|request| request.with_timeout(timeout))
        .unwrap_or_else(|_| StorageRequest::new(timeout))
}
pub(crate) mod dataset;
mod graph_read;
pub(crate) mod retention;

use codevetra_domain::{ContentDigest, GenerationId, ProjectId};

use crate::{
    StorageError,
    rocks::{
        AsyncProjectStore, GraphSnapshot, ReadBudget, RocksError, StateAccess, StateTransaction,
        StorageRequest,
    },
};

/// Typed feature services bound to one project's native database handle.
/// Clones share the same multithreaded owner; this type never opens a database.
#[derive(Clone)]
pub struct NativeDatabase {
    pub(crate) store: AsyncProjectStore,
    pub(crate) project_id: ProjectId,
    pub(crate) root_identity: String,
    pub(crate) finding_computation: std::sync::Arc<tokio::sync::Mutex<()>>,
    pub(crate) semantic_cache: std::sync::Arc<crate::semantic::native::cache::SemanticCache>,
}

impl NativeDatabase {
    /// Derive a native control before crossing a spawned-task boundary.
    /// # Errors
    /// Rejects elapsed/cancelled parents and invalid timeout bounds.
    pub fn request_control(timeout: Duration) -> Result<StorageRequest, StorageError> {
        request_for(timeout).map_err(storage_error)
    }

    /// Run typed feature calls under the caller's original storage control.
    /// Nested calls keep their own shorter bounds and cannot extend this deadline.
    pub async fn with_request<T>(
        request: StorageRequest,
        operation: impl std::future::Future<Output = T>,
    ) -> T {
        ACTIVE_REQUEST.scope(request, operation).await
    }

    pub(crate) fn with_preparation_deadline<T, E>(
        timeout: Duration,
        operation: impl std::future::Future<Output = Result<T, E>>,
    ) -> impl std::future::Future<Output = Result<T, E>>
    where
        E: From<StorageError>,
    {
        let operation = Box::pin(operation);
        async move {
            // Every admitted native job inherits this cap and retains its owning
            // wait after expiry, including when a WAL commit has already started.
            let request = request_for(timeout)
                .map_err(storage_error)
                .map_err(E::from)?;
            Self::with_request(request, operation).await
        }
    }
    /// Bind services to the already-opened owner and its verified project identity.
    #[must_use]
    pub fn new(store: AsyncProjectStore, identity: &ContentDigest) -> Self {
        let mut bytes = [0_u8; 16];
        let hash = blake3::derive_key(
            "cartograph-native-project-id-v1",
            identity.as_str().as_bytes(),
        );
        bytes.copy_from_slice(&hash[..16]);
        Self {
            store,
            project_id: ProjectId::from_uuid_v8(bytes),
            root_identity: codevetra_domain::project_root_identity(identity),
            semantic_cache: std::sync::Arc::default(),
            finding_computation: std::sync::Arc::default(),
        }
    }

    /// Stable native project identity, independent of generation publication.
    #[must_use]
    pub const fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub(crate) fn require_project(&self, project: &ProjectId) -> Result<(), StorageError> {
        if project == &self.project_id {
            Ok(())
        } else {
            Err(StorageError::InvalidInput {
                field: "project_id",
            })
        }
    }

    pub(crate) async fn graph<T, F>(
        &self,
        expected: &GenerationId,
        timeout: Duration,
        read: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&GraphSnapshot<'_, '_>, &mut ReadBudget) -> Result<T, RocksError>
            + Send
            + 'static,
    {
        self.graph_request(expected, timeout, 1024 * 1_048_576)?
            .read(read)
            .await
    }

    pub(crate) async fn state<T, F>(
        &self,
        access: StateAccess,
        operation: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut StateTransaction<'_>, &mut ReadBudget) -> Result<T, RocksError>
            + Send
            + 'static,
    {
        self.state_with_timeout(access, Duration::from_secs(30), operation)
            .await
    }

    pub(crate) async fn current_graph<T, F>(
        &self,
        project: &ProjectId,
        timeout: Duration,
        read: F,
    ) -> Result<Option<T>, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&GraphSnapshot<'_, '_>, &mut ReadBudget) -> Result<T, RocksError>
            + Send
            + 'static,
    {
        self.require_project(project)?;
        let Some(current) = self
            .current_generation_record_bounded(project, timeout)
            .await?
        else {
            return Ok(None);
        };
        self.graph(current.generation_id(), timeout, read)
            .await
            .map(Some)
    }

    pub(crate) async fn state_with_timeout<T, F>(
        &self,
        access: StateAccess,
        timeout: Duration,
        operation: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut StateTransaction<'_>, &mut ReadBudget) -> Result<T, RocksError>
            + Send
            + 'static,
    {
        let request = request_for(timeout).map_err(storage_error)?;
        self.state_with_request(access, &request, operation).await
    }

    pub(crate) async fn state_with_request<T, F>(
        &self,
        access: StateAccess,
        request: &StorageRequest,
        operation: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut StateTransaction<'_>, &mut ReadBudget) -> Result<T, RocksError>
            + Send
            + 'static,
    {
        let timeout = request.remaining_timeout().map_err(storage_error)?;
        self.store
            .with_request(request)
            .state(access, budget(timeout)?, operation)
            .await
            .map_err(storage_error)
    }

    pub(crate) async fn transaction<T, E, F>(
        &self,
        access: StateAccess,
        timeout: Duration,
        operation: F,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<StorageError> + Send + 'static,
        F: FnOnce(&mut StateTransaction<'_>, &mut ReadBudget) -> Result<T, E> + Send + 'static,
    {
        self.state_with_timeout(access, timeout, move |transaction, budget| {
            Ok(match operation(transaction, budget) {
                Ok(result) => Ok(result),
                Err(error) => {
                    transaction.rollback();
                    Err(error)
                }
            })
        })
        .await
        .map_err(E::from)?
    }
}

fn budget(timeout: Duration) -> Result<ReadBudget, StorageError> {
    ReadBudget::streaming(2_000_000, 1024 * 1_048_576, timeout).map_err(storage_error)
}

pub(crate) fn storage_error(error: RocksError) -> StorageError {
    match error {
        RocksError::ReplayConflict => StorageError::ImmutableConflict,
        RocksError::InvalidInput { field } => StorageError::InvalidInput { field },
        RocksError::GenerationChanged => StorageError::CurrentGenerationChanged,
        RocksError::GenerationUnavailable => StorageError::GenerationNotFound,
        RocksError::Corrupt { field } => StorageError::CorruptStoredValue { field },
        RocksError::Limit {
            resource: "deadline" | "read_deadline" | "operation_deadline",
        } => StorageError::StatementTimeout {
            operation: "native-request",
        },
        RocksError::Limit { .. } => StorageError::NativeCapacityExceeded,
        RocksError::Format => StorageError::NativeRecordInvalid,
        _ => StorageError::DatabaseOperation {
            operation: "native-storage",
        },
    }
}

pub(crate) fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, RocksError> {
    serde_json::to_vec(value).map_err(|_| RocksError::Corrupt {
        field: "state_encoding",
    })
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, RocksError> {
    serde_json::from_slice(bytes).map_err(|_| RocksError::Corrupt {
        field: "state_encoding",
    })
}

pub(crate) fn visit_state<T: serde::de::DeserializeOwned>(
    scan: &crate::rocks::StateScan<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(Vec<u8>, T, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let mut cursor = None;
    loop {
        let page = scan.page(
            cursor.as_ref().map(crate::rocks::RecordKey::as_bytes),
            256,
            budget,
        )?;
        for (key, bytes) in page.rows {
            visit(key.as_bytes().to_vec(), decode(&bytes)?, budget)?;
        }
        cursor = page.next_after;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

pub(crate) fn now() -> String {
    jiff::Timestamp::now().to_string()
}

pub(crate) fn timestamp_order(a: &str, b: &str) -> std::cmp::Ordering {
    // Stored timestamps are produced by the native clock formatter. Malformed
    // timestamps are rejected by each record decoder before this comparator.
    a.parse::<jiff::Timestamp>()
        .ok()
        .cmp(&b.parse::<jiff::Timestamp>().ok())
}

impl NativeDatabase {
    /// Read bounded project-local native storage telemetry.
    /// # Errors
    /// Returns redacted native telemetry or request failures.
    pub async fn storage_usage(
        &self,
        timeout: Duration,
    ) -> Result<crate::rocks::NativeStorageUsage, StorageError> {
        let request = request_for(timeout).map_err(storage_error)?;
        self.store
            .with_request(&request)
            .storage_usage()
            .await
            .map_err(storage_error)
    }
}
