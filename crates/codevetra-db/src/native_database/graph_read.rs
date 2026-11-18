//! One admitted immutable graph read binds its generation, request, and allowance.

use super::{
    AsyncProjectStore, Duration, GRAPH_READ_ROWS, GenerationId, GraphSnapshot, NativeDatabase,
    ReadBudget, RocksError, StorageError, StorageRequest, request_for, storage_error,
};

pub(crate) struct GraphRead<'database> {
    store: &'database AsyncProjectStore,
    expected: GenerationId,
    request: StorageRequest,
    budget: ReadBudget,
}

impl NativeDatabase {
    /// Call from the polling async operation so admission starts on execution.
    pub(crate) fn graph_request(
        &self,
        expected: &GenerationId,
        timeout: Duration,
        maximum_bytes: usize,
    ) -> Result<GraphRead<'_>, StorageError> {
        // Preserve parent cancellation/deadline precedence over budget validation.
        let request = request_for(timeout).map_err(storage_error)?;
        let budget = ReadBudget::streaming(GRAPH_READ_ROWS, maximum_bytes, timeout)
            .map_err(storage_error)?;
        Ok(GraphRead {
            store: &self.store,
            expected: expected.clone(),
            request,
            budget,
        })
    }
}

impl<'database> GraphRead<'database> {
    pub(crate) fn read<T, F>(
        self,
        read: F,
    ) -> impl Future<Output = Result<T, StorageError>> + Send + 'database
    where
        T: Send + 'static,
        F: FnOnce(&GraphSnapshot<'_, '_>, &mut ReadBudget) -> Result<T, RocksError>
            + Send
            + 'static,
    {
        // Contain the admitted read's state instead of growing every caller's
        // future by another request, generation, budget, and worker future.
        Box::pin(async move {
            self.store
                .with_request(&self.request)
                .read(Some(self.expected), self.budget, move |snapshot, budget| {
                    read(&snapshot.graph(budget)?, budget)
                })
                .await
                .map_err(storage_error)
        })
    }
}
