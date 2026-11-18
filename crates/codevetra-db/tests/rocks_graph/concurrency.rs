use codevetra_db::rocks::{AsyncProjectStore, StorageRuntime, WorkerPolicy};

use super::*;

#[tokio::test(flavor = "current_thread")]
async fn canonical_publication_uses_storage_workers_and_returns_owned_evidence() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        codevetra_db::rocks::ProjectOpening::new(
            root.path().to_path_buf(),
            digest(1),
            StorePolicy::default(),
        ),
        &request()?,
    )
    .await?;
    let first_input = input(1, facts(2, vec![edge(1, 2, "native")])?)?;
    let publication = {
        let request_bound_input = first_input;
        store
            .with_request(&request()?)
            .publish_graph(request_bound_input)
            .await
    }?;
    let cancelled = request()?;
    cancelled.cancel();
    let result = store
        .with_request(&cancelled)
        .publish_graph(input(2, facts(2, vec![])?)?)
        .await;
    let failure = result.err().ok_or("cancelled graph input was admitted")?;
    assert_eq!(failure.primary, RocksError::Cancelled);
    assert_eq!(failure.cleanup_failure, None);
    {
        let request_bound_expected = Some(publication.revision.generation_id);
        let request_bound_budget = ReadBudget::new(1_000, 1_048_576, Duration::from_secs(30))?;
        store
            .with_request(&request()?)
            .read(
                request_bound_expected,
                request_bound_budget,
                |snapshot, budget| {
                    let graph = snapshot.graph(budget)?;
                    assert_eq!(graph.info().edges, 1);
                    assert_eq!(
                        graph
                            .adjacency(&symbol(1), GraphDirection::Outgoing)
                            .page(None, 10, budget)?
                            .links
                            .len(),
                        1
                    );
                    Ok(())
                },
            )
            .await
    }?;
    runtime.shutdown(&request()?).await?;
    Ok(())
}
