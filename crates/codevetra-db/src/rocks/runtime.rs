use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use codevetra_domain::{ContentDigest, GenerationDigestVersion, GenerationId};
use tokio::sync::{Notify, watch};

use super::{
    ProjectOpening, ProjectStore, PruneReport, ReadBudget, RecordChange, Result, RevisionSnapshot,
    RevisionWriter, RocksError, StoredRevision,
};

mod group;
mod pool;
mod projects;

#[cfg(test)]
mod tests;

use group::StageGroup;
use pool::{JobCapacity, StoragePool};
use projects::{ProjectAccess, Projects};

/// Finite foreground concurrency and admission for one project owner.
///
/// Native cache/memtable policy is separate. Pending bytes account for owned
/// request buffers and their staging copies, rather than multiplying the native
/// batch limit by an unbounded number of Tokio tasks.
#[derive(Clone, Copy, Debug)]
pub struct WorkerPolicy {
    /// Dedicated blocking threads; at least half remain available to reads.
    pub threads: u8,
    /// Maximum admitted jobs, including both executing and queued jobs.
    pub pending_jobs: u16,
    /// Maximum admitted request/result reservations in bytes.
    pub pending_bytes: u32,
}

impl WorkerPolicy {
    const DEFAULT: Self = Self {
        threads: 4,
        pending_jobs: 32,
        pending_bytes: 256 * 1_048_576,
    };
    const MINIMUM_THREADS: u8 = 2;
    const MAXIMUM_THREADS: u8 = 16;
    const MAXIMUM_PENDING_JOBS: u16 = 1_024;
    const MINIMUM_PENDING_BYTES: u32 = 1_048_576;
    const MAXIMUM_PENDING_BYTES: u32 = 512 * 1_048_576;

    fn validate(self) -> Result<Self> {
        if !(Self::MINIMUM_THREADS..=Self::MAXIMUM_THREADS).contains(&self.threads)
            || !(u16::from(self.threads)..=Self::MAXIMUM_PENDING_JOBS).contains(&self.pending_jobs)
            || !(Self::MINIMUM_PENDING_BYTES..=Self::MAXIMUM_PENDING_BYTES)
                .contains(&self.pending_bytes)
        {
            return Err(RocksError::Limit {
                resource: "worker_policy",
            });
        }
        Ok(self)
    }
}

impl Default for WorkerPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Default)]
struct Cancellation {
    inner: Arc<CancelState>,
}

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    changed: Notify,
}

impl Cancellation {
    fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.changed.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        let changed = self.inner.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !self.is_cancelled() {
            changed.await;
        }
    }
}

/// Deadline and cooperative cancellation shared by a bounded storage request.
///
/// Queue wait counts against the deadline. Cancelling cannot interrupt a native
/// WAL sync already in progress; the owning workers retain and reap that work.
/// A completed commit returns its actual result instead of a false rollback.
#[derive(Clone)]
pub struct StorageRequest {
    deadline: Instant,
    cancellation: Cancellation,
    parent: Option<Cancellation>,
    closing: Option<Cancellation>,
    signals: Option<[watch::Receiver<bool>; 2]>,
}

impl StorageRequest {
    /// Admit a request lasting at most five minutes, including its queue wait.
    ///
    /// # Errors
    /// Rejects zero, excessive, or unrepresentable deadlines.
    pub fn new(timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(RocksError::Limit {
                resource: "operation_deadline",
            });
        }
        Ok(Self {
            deadline: Instant::now()
                .checked_add(timeout)
                .ok_or(RocksError::Limit {
                    resource: "operation_deadline",
                })?,
            cancellation: Cancellation::default(),
            parent: None,
            closing: None,
            signals: None,
        })
    }

    /// Request cooperative cancellation of this request and its admitted work.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Narrow a request's deadline while preserving its cancellation and owner.
    /// # Errors
    /// Rejects invalid bounds or a request whose original control has stopped.
    pub fn with_timeout(&self, timeout: Duration) -> Result<Self> {
        self.check()?;
        let bound = Self::new(timeout)?;
        let mut request = self.clone();
        request.deadline = request.deadline.min(bound.deadline);
        Ok(request)
    }

    /// Bind a CPU stage's monotonic parent/stage cancellation signals and deadline.
    /// Existing cancellation, owner closure and the earlier deadline remain effective.
    /// Signals must transition only from false to true.
    /// # Errors
    /// Rejects stopped requests and rebinding an already controlled request.
    pub fn with_cancellation_signals(
        &self,
        signals: [watch::Receiver<bool>; 2],
        deadline: Instant,
    ) -> Result<Self> {
        self.check()?;
        if self.signals.is_some() {
            return Err(RocksError::InvalidInput {
                field: "request_cancellation_signals",
            });
        }
        let mut request = self.clone();
        request.deadline = request.deadline.min(deadline);
        if signals.iter().any(|signal| *signal.borrow()) {
            return Err(RocksError::Cancelled);
        }
        request.signals = Some(signals);
        request.check()?;
        Ok(request)
    }

    // Build secondary budgets before admission without renewing this request.
    pub(crate) fn remaining_timeout(&self) -> Result<Duration> {
        self.check()?;
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(RocksError::Limit {
                resource: "operation_deadline",
            })
    }

    /// Check cancellation, owner shutdown, and the original finite deadline.
    ///
    /// # Errors
    /// Returns cancelled, closed, or operation-deadline errors.
    pub fn check(&self) -> Result<()> {
        if self
            .closing
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            return Err(RocksError::Closed);
        }
        if self.cancellation.is_cancelled()
            || self.parent.as_ref().is_some_and(Cancellation::is_cancelled)
            || self.signals.as_ref().is_some_and(|signals| {
                signals
                    .iter()
                    .any(|signal| signal.has_changed().unwrap_or_else(|_| *signal.borrow()))
            })
        {
            return Err(RocksError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(RocksError::Limit {
                resource: "operation_deadline",
            });
        }
        Ok(())
    }

    fn child(&self, closing: Cancellation) -> Self {
        Self {
            deadline: self.deadline,
            cancellation: Cancellation::default(),
            parent: Some(self.cancellation.clone()),
            closing: Some(closing),
            signals: self.signals.clone(),
        }
    }

    /// Wait until cancellation, owner closure, or this request's deadline.
    /// Returns the exact bounded stop reason without cancelling admitted work.
    pub async fn stopped(&self) -> RocksError {
        tokio::select! {
            () = self.cancellation.cancelled() => {},
            () = optional_cancelled(self.parent.as_ref()) => {},
            () = optional_cancelled(self.closing.as_ref()) => {},
            () = optional_signals(self.signals.clone()) => {},
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(self.deadline)) => {},
        }
        self.check().err().unwrap_or(RocksError::Cancelled)
    }
}

async fn optional_signals(signals: Option<[watch::Receiver<bool>; 2]>) {
    let Some([mut parent, mut stage]) = signals else {
        return std::future::pending().await;
    };
    tokio::select! {
        () = watch_stopped(&mut parent) => {},
        () = watch_stopped(&mut stage) => {},
    }
}

async fn watch_stopped(signal: &mut watch::Receiver<bool>) {
    loop {
        if *signal.borrow() {
            return;
        }
        if signal.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

async fn optional_cancelled(cancellation: Option<&Cancellation>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Async access to the shared, multithreaded owner of one project database.
///
/// All native work, including snapshots and opening the directory, runs on
/// dedicated bounded workers. Clones share those workers and the native handle.
/// The owner calls [`StorageRuntime::shutdown`] before dropping its guard; Drop also
/// closes and joins workers as a synchronous final safety net.
#[derive(Clone)]
pub struct AsyncProjectStore {
    project: ProjectAccess,
    pool: Arc<StoragePool>,
    #[cfg(test)]
    pub(crate) state_gate: Arc<std::sync::Mutex<Option<StateTestGate>>>,
    #[cfg(test)]
    pub(crate) graph_reclaim_gate: Arc<std::sync::Mutex<Option<GraphReclaimTestGate>>>,
}

#[cfg(test)]
pub(crate) struct GraphReclaimTestGate {
    pub(crate) entered: tokio::sync::oneshot::Sender<()>,
    pub(crate) reclaim: std::sync::mpsc::Receiver<()>,
    pub(crate) reclaimed: tokio::sync::oneshot::Sender<()>,
    pub(crate) finish: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl GraphReclaimTestGate {
    fn wait(self, reclaim: impl FnOnce() -> Result<()>) -> Result<()> {
        let _entered = self.entered.send(());
        self.reclaim
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| super::operation("fixture-gate"))?;
        reclaim()?;
        let _reclaimed = self.reclaimed.send(());
        self.finish
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| super::operation("fixture-gate"))
    }
}

#[cfg(test)]
pub(crate) struct StateTestGate {
    pub(crate) write_rows: u32,
    pub(crate) after_commit: bool,
    pub(crate) entered: tokio::sync::oneshot::Sender<()>,
    pub(crate) release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl StateTestGate {
    fn wait(self) -> Result<()> {
        let _entered = self.entered.send(());
        self.release
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| super::operation("fixture-gate"))
    }
}

/// Owns foreground workers across asynchronous requests and their cancellation.
///
/// Keep this guard outside request futures, so abandoning an open/read/write
/// future leaves its native work tracked. Shutdown is awaited before the
/// owner goes out of scope. One process may share this bounded worker owner
/// across several independently budgeted project database handles.
pub struct StorageRuntime {
    pool: Arc<StoragePool>,
    projects: Arc<Projects>,
}

impl StorageRuntime {
    /// Start a finite set of supervised storage threads.
    ///
    /// # Errors
    /// Rejects invalid policy or failure to start the owned worker threads.
    pub fn new(policy: WorkerPolicy) -> Result<Self> {
        let projects = Arc::new(Projects::default());
        let finalize = Arc::clone(&projects);
        Ok(Self {
            pool: Arc::new(StoragePool::new(policy, move || finalize.close())?),
            projects,
        })
    }

    /// Close admission and drain workers within the caller's finite deadline.
    ///
    /// If draining is incomplete, retain this owner and retry with a fresh
    /// request. Native calls cannot safely be detached or forcibly interrupted.
    /// Dropping the owner still joins synchronously as a final safeguard.
    ///
    /// # Errors
    /// Returns cancellation or a deadline error without losing worker ownership.
    pub async fn shutdown(&self, request: &StorageRequest) -> Result<()> {
        self.pool.close();
        self.pool.drain(request).await
    }
}

impl Drop for StorageRuntime {
    fn drop(&mut self) {
        self.pool.close();
        self.pool.join();
    }
}

/// An exact writer fence with a drain barrier for all admitted staging jobs.
#[derive(Clone)]
pub struct AsyncRevisionWriter {
    writer: RevisionWriter,
    group: Arc<StageGroup>,
}

impl AsyncRevisionWriter {
    /// Reserved immutable generation identity.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        self.writer.generation_id()
    }

    /// Monotonic project-local reservation sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.writer.sequence()
    }
}

/// A borrowed project handle bound to one parent request's control and deadline.
///
/// Each operation still receives a separate worker admission, child control,
/// and snapshot. This view owns no worker, writer, reservation, or transaction.
#[derive(Clone, Copy)]
pub struct RequestedProject<'a> {
    store: &'a AsyncProjectStore,
    request: &'a StorageRequest,
}

impl AsyncProjectStore {
    /// Bind subsequent operations to this project and parent request.
    /// Construction performs no validation, admission, or native work.
    #[must_use]
    pub const fn with_request<'a>(&'a self, request: &'a StorageRequest) -> RequestedProject<'a> {
        RequestedProject {
            store: self,
            request,
        }
    }

    /// Open one project database without blocking a Tokio executor thread.
    ///
    /// # Errors
    /// Returns policy, directory, ownership, lock, deadline, or worker errors.
    pub async fn open(
        runtime: &StorageRuntime,
        opening: ProjectOpening,
        request: &StorageRequest,
    ) -> Result<Self> {
        let pool = Arc::clone(&runtime.pool);
        let projects = Arc::clone(&runtime.projects);
        let project = pool
            .run(request, JobCapacity::write(64 * 1_024), move |control| {
                projects.open(opening, &control)
            })
            .await?;
        Ok(Self {
            project,
            pool,
            #[cfg(test)]
            state_gate: Arc::default(),
            #[cfg(test)]
            graph_reclaim_gate: Arc::default(),
        })
    }
}

impl RequestedProject<'_> {
    /// Run one atomic mutable-state transaction on an owned storage worker.
    /// Graph-derived writes can require the exact current generation. Saved
    /// sessions and notes remain usable before the first graph is published.
    /// # Errors
    /// Returns admission, generation-fence, read/write bounds, or storage errors.
    pub async fn state<T, F>(
        self,
        access: super::StateAccess,
        budget: ReadBudget,
        operation: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut super::StateTransaction<'_>, &mut ReadBudget) -> Result<T> + Send + 'static,
    {
        let store = self.store;
        let request = self.request;
        let reservation =
            budget
                .reservation_bytes()?
                .checked_add(access.maximum_write_bytes.checked_mul(3).ok_or(
                    RocksError::Limit {
                        resource: "worker_bytes",
                    },
                )?)
                .ok_or(RocksError::Limit {
                    resource: "worker_bytes",
                })?;
        let project = store.project.clone();
        #[cfg(test)]
        let state_gate = store.state_gate.clone();
        store
            .pool
            .run(
                request,
                JobCapacity::new(reservation, access.maximum_write_rows != 0),
                move |control| {
                    #[cfg(test)]
                    let gate = {
                        let mut gate = state_gate
                            .lock()
                            .map_err(|_| super::operation("fixture-gate"))?;
                        if gate
                            .as_ref()
                            .is_some_and(|gate| gate.write_rows == access.maximum_write_rows)
                        {
                            gate.take()
                        } else {
                            None
                        }
                    };
                    #[cfg(test)]
                    let gate = match gate {
                        Some(gate) if !gate.after_commit => {
                            gate.wait()?;
                            None
                        }
                        gate => gate,
                    };
                    let result = project.run(&control, |store| {
                        let mut budget = budget.with_control(control.clone());
                        store.state(&access, &mut budget, operation)
                    });
                    #[cfg(test)]
                    if let Some(gate) = gate {
                        gate.wait()?;
                    }
                    result
                },
            )
            .await
    }

    /// Persist a complete canonical graph and publish only its changed records.
    ///
    /// Preparation, native I/O, publication, and bounded retention run on one
    /// owned storage worker while other workers continue serving reads.
    ///
    /// # Errors
    /// Returns admission, stale-base, request, format, or native storage errors.
    pub async fn publish_graph(
        self,
        input: super::GraphPublicationInput,
    ) -> std::result::Result<super::GraphPublication, super::GraphPublicationError> {
        let store = self.store;
        let request = self.request;
        let bytes = input.reservation_bytes()?;
        let maximum_reclamation = input.reclaimable_bytes()?;
        let project = store.project.clone();
        #[cfg(test)]
        let graph_reclaim_gate = store.graph_reclaim_gate.clone();
        store
            .pool
            .run_reclaiming(
                request,
                JobCapacity::write(bytes).reclaiming(maximum_reclamation),
                move |control, credit| {
                    #[cfg(test)]
                    let mut gate = graph_reclaim_gate
                        .lock()
                        .map_err(|_| super::operation("fixture-gate"))?
                        .take();
                    project.run(&control, |store| {
                        Ok(store.publish_graph_reclaiming(input, &control, |bytes| {
                            if bytes == 0 {
                                return Ok(());
                            }
                            #[cfg(test)]
                            if let Some(gate) = gate.take() {
                                return gate.wait(|| credit.release(bytes));
                            }
                            credit.release(bytes)
                        }))
                    })
                },
            )
            .await?
    }

    /// Reserve one revision on a blocking storage worker.
    ///
    /// # Errors
    /// Returns ownership, lifecycle, admission, or request errors.
    pub async fn begin_revision(
        self,
        generation_id: GenerationId,
        source_revision: ContentDigest,
        lease_duration: Duration,
    ) -> Result<AsyncRevisionWriter> {
        let store = self.store;
        let request = self.request;
        let project = store.project.clone();
        let writer = store
            .pool
            .run(request, JobCapacity::write(4_096), move |control| {
                project.run(&control, |store| {
                    store.begin_revision(generation_id, source_revision, lease_duration)
                })
            })
            .await?;
        Ok(AsyncRevisionWriter {
            writer,
            group: Arc::new(StageGroup::default()),
        })
    }

    /// Stage independent batches concurrently under bounded byte admission.
    ///
    /// # Errors
    /// Rejects sealed/draining writers and propagates native/request errors.
    pub async fn stage(
        self,
        writer: &AsyncRevisionWriter,
        changes: Vec<RecordChange>,
    ) -> Result<StoredRevision> {
        let store = self.store;
        let request = self.request;
        let registration = writer.group.register()?;
        let mut bytes = super::engine::MAXIMUM_RECORD_BYTES * 2 + 4_096;
        for change in &changes {
            bytes = bytes
                .checked_add(change.admission_bytes()?)
                .ok_or(RocksError::Limit {
                    resource: "worker_bytes",
                })?;
        }
        let bytes = u32::try_from(bytes).map_err(|_| RocksError::Limit {
            resource: "worker_bytes",
        })?;
        let project = store.project.clone();
        let writer = writer.writer.clone();
        store
            .pool
            .run(request, JobCapacity::write(bytes), move |control| {
                let mut registration = registration;
                let result = project.run(&control, |store| {
                    store.stage_controlled(&writer, &changes, Some(&control))
                });
                registration.complete(result.is_ok());
                result
            })
            .await
    }

    /// Close staging admission, drain its jobs, and seal a validated generation.
    ///
    /// # Errors
    /// Returns drain/deadline, fence, lifecycle, or native errors. A stopped
    /// drain stays closed and can be retried with a fresh request.
    pub async fn seal(
        self,
        writer: &AsyncRevisionWriter,
        content_digest: ContentDigest,
        digest_version: GenerationDigestVersion,
    ) -> Result<StoredRevision> {
        let store = self.store;
        let request = self.request;
        writer.group.close_and_drain(request, true).await?;
        let project = store.project.clone();
        let writer = writer.writer.clone();
        store
            .pool
            .run(request, JobCapacity::write(4_096), move |control| {
                project.run(&control, |store| {
                    store.seal(&writer, content_digest, digest_version)
                })
            })
            .await
    }

    /// Publish a sealed complete generation with its atomic durable pointer.
    ///
    /// # Errors
    /// Returns request, fence, lifecycle, or native storage errors.
    pub async fn publish(self, writer: &AsyncRevisionWriter) -> Result<StoredRevision> {
        let store = self.store;
        let request = self.request;
        let project = store.project.clone();
        let writer = writer.writer.clone();
        store
            .pool
            .run(request, JobCapacity::write(4_096), move |control| {
                project.run(&control, |store| store.publish(&writer))
            })
            .await
    }

    /// Run bounded lookups against one current read snapshot on a worker.
    ///
    /// The closure returns owned evidence; it cannot retain a native snapshot
    /// past the job. Byte admission reserves the read allowance, its copies,
    /// and a maximum native record buffer. The caller shares the supplied read
    /// budget across every lookup.
    ///
    /// # Errors
    /// Returns generation, request, read-budget, or native storage errors.
    pub async fn read<T, F>(
        self,
        expected: Option<GenerationId>,
        budget: ReadBudget,
        read: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&RevisionSnapshot<'_>, &mut ReadBudget) -> Result<T> + Send + 'static,
    {
        let store = self.store;
        let request = self.request;
        let project = store.project.clone();
        let reservation_bytes = budget.reservation_bytes()?;
        store
            .pool
            .run(
                request,
                JobCapacity::read(reservation_bytes),
                move |control| {
                    project.run(&control, |store| {
                        let snapshot = store.current_snapshot(expected.as_ref())?;
                        let mut budget = budget.with_control(control.clone());
                        read(&snapshot, &mut budget)
                    })
                },
            )
            .await
    }

    /// Renew the exact writer fence on a storage worker.
    ///
    /// # Errors
    /// Returns request, lease-policy, or writer-fence errors.
    pub async fn heartbeat(
        self,
        writer: &AsyncRevisionWriter,
        lease_duration: Duration,
    ) -> Result<()> {
        let store = self.store;
        let request = self.request;
        let project = store.project.clone();
        let writer = writer.writer.clone();
        store
            .pool
            .run(request, JobCapacity::read(4_096), move |control| {
                project.run(&control, |store| store.heartbeat(&writer, lease_duration))
            })
            .await
    }

    /// Drain staging and terminalize owned incomplete work.
    ///
    /// # Errors
    /// Returns drain, request, exact-fence, or storage errors.
    pub async fn fail(self, writer: &AsyncRevisionWriter) -> Result<()> {
        let store = self.store;
        let request = self.request;
        writer.group.close_and_drain(request, false).await?;
        let project = store.project.clone();
        let writer = writer.writer.clone();
        store
            .pool
            .run(request, JobCapacity::write(4_096), move |control| {
                project.run(&control, |store| store.fail(&writer))
            })
            .await
    }

    /// Collect a finite resumable retention batch on a storage worker.
    ///
    /// # Errors
    /// Returns request, budget, writer-busy, or native storage errors.
    pub async fn prune(
        self,
        policy: super::RetentionPolicy,
        budget: ReadBudget,
    ) -> Result<PruneReport> {
        let store = self.store;
        let request = self.request;
        let project = store.project.clone();
        store
            .pool
            .run(
                request,
                JobCapacity::write(64 * 1_048_576),
                move |control| {
                    let mut budget = budget.with_control(control.clone());
                    project.run(&control, |store| store.prune(policy, &mut budget))
                },
            )
            .await
    }
}

impl RequestedProject<'_> {
    /// Read project-local file allocation and bounded native property estimates.
    /// # Errors
    /// Returns closed-owner, cancellation, property or directory-enumeration failures.
    pub async fn storage_usage(self) -> Result<super::NativeStorageUsage> {
        let store = self.store;
        let request = self.request;
        let project = store.project.clone();
        store
            .pool
            .run(request, JobCapacity::read(1_048_576), move |control| {
                project.run(&control, super::ProjectStore::storage_usage)
            })
            .await
    }
}
