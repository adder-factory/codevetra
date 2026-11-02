//! A real owner pauses after input capture to prove cross-stage fences and cancellation.

use super::*;
use crate::{
    GenerationFacts, GenerationValidationLimits,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
    validate_generation_facts,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    _root: tempfile::TempDir,
    runtime: StorageRuntime,
    database: NativeDatabase,
}

impl Fixture {
    async fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        // Hold the captured-input job while admitting a second full native
        // operation; this test needs two concurrent reservation credits.
        let runtime = StorageRuntime::new(WorkerPolicy {
            pending_bytes: 512 * 1_048_576,
            ..WorkerPolicy::default()
        })?;
        let identity = ContentDigest::from_bytes([1; 32]);
        let store = AsyncProjectStore::open(
            &runtime,
            root.path().to_owned(),
            identity.clone(),
            StorePolicy::default(),
            &request()?,
        )
        .await?;
        let fixture = Self {
            _root: root,
            runtime,
            database: NativeDatabase::new(store, &identity),
        };
        fixture.publish(1).await?;
        fixture
            .database
            .refresh_current_structural_findings(
                fixture.database.project_id(),
                Duration::from_secs(30),
            )
            .await?;
        Ok(fixture)
    }

    async fn publish(&self, number: u8) -> TestResult {
        let facts = validate_generation_facts(
            GenerationFacts::default(),
            GenerationValidationLimits::new(1_048_576, 4 * 1_048_576)?,
            || false,
        )?
        .0;
        let input = GraphPublicationInput::new(
            GraphRevision {
                generation_id: GenerationId::from_uuid_v8([number; 16]),
                source_revision: ContentDigest::from_bytes([number; 32]),
                base: if number == 1 {
                    GraphBase::Empty
                } else {
                    GraphBase::Current(GenerationId::from_uuid_v8([number - 1; 16]))
                },
            },
            facts,
            GraphPolicy::default(),
            &request()?,
        )?;
        self.database
            .store
            .publish_graph(input, &request()?)
            .await?;
        Ok(())
    }

    async fn head(&self) -> TestResult<Option<Vec<u8>>> {
        Ok(self
            .database
            .state(None, 0, 0, |transaction, budget| {
                transaction.get(
                    StateNamespace::Derived,
                    b"dataset/h/structural-findings",
                    budget,
                )
            })
            .await?)
    }

    fn pause_after_capture(
        &self,
    ) -> TestResult<(
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    )> {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        *self
            .database
            .store
            .state_gate
            .lock()
            .map_err(|_| "gate poisoned")? = Some(crate::rocks::StateTestGate {
            write_rows: 0,
            after_commit: true,
            entered,
            release: released,
        });
        Ok((started, release))
    }
}

fn request() -> TestResult<StorageRequest> {
    Ok(StorageRequest::new(Duration::from_secs(30))?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_analysis_rejects_changed_source_or_auxiliary_inputs_without_replacing_the_head()
-> TestResult {
    for change_source in [false, true] {
        let fixture = Fixture::new().await?;
        let before = fixture.head().await?;
        assert!(before.is_some());
        let (started, release) = fixture.pause_after_capture()?;
        let database = fixture.database.clone();
        let work = tokio::spawn(async move {
            prepare(&database, Duration::from_secs(30))
                .await
                .map(|prepared| prepared.is_some())
        });
        tokio::time::timeout(Duration::from_secs(3), started).await??;
        if change_source {
            fixture.publish(2).await?;
        } else {
            fixture
                .database
                .state(None, 64, 1, |transaction, _| {
                    transaction.put(StateNamespace::Metadata, b"coverage_epoch", encode(&1_u64)?)
                })
                .await?;
        }
        release.send(())?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), work).await??,
            Err(StorageError::CurrentGenerationChanged)
        ));
        assert_eq!(fixture.head().await?, before);
        fixture.runtime.shutdown(&request()?).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_analysis_retains_cancelled_work_and_cannot_renew_its_deadline() -> TestResult {
    for expire in [false, true] {
        let fixture = Fixture::new().await?;
        let before = fixture.head().await?;
        let (started, release) = fixture.pause_after_capture()?;
        let control = StorageRequest::new(if expire {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(30)
        })?;
        let inherited = control.clone();
        let database = fixture.database.clone();
        let work = tokio::spawn(async move {
            NativeDatabase::with_request(inherited, prepare(&database, Duration::from_secs(30)))
                .await
                .map(|prepared| prepared.is_some())
        });
        tokio::time::timeout(Duration::from_secs(3), started).await??;
        if expire {
            tokio::time::sleep(Duration::from_millis(600)).await;
        } else {
            control.cancel();
        }
        let returned_early = work.is_finished();
        release.send(())?;
        assert!(!returned_early, "an admitted storage stage was abandoned");
        assert!(
            tokio::time::timeout(Duration::from_secs(3), work)
                .await??
                .is_err()
        );
        assert_eq!(fixture.head().await?, before);
        fixture.runtime.shutdown(&request()?).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_inventory_holds_one_admission_until_consumed() -> TestResult {
    let fixture = Fixture::new().await?;
    let prepared = prepare(&fixture.database, Duration::from_secs(30))
        .await?
        .ok_or("missing inventory")?;
    assert!(matches!(
        prepare(&fixture.database.clone(), Duration::from_secs(30)).await,
        Err(StorageError::NativeCapacityExceeded)
    ));
    drop(prepared);
    assert!(
        prepare(&fixture.database, Duration::from_secs(30))
            .await?
            .is_some()
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}
