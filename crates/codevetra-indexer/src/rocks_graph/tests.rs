use std::{error::Error, fs, time::Duration};

use codevetra_db::{
    GraphDirection,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphRevision, ReadBudget, RevisionSnapshot,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
};
use codevetra_domain::{ContentDigest, EdgeKind, GenerationId};
use codevetra_extract::{DiscoveryLimits, SourceLimits, SourceRoot};
use tokio::time::Instant;

use super::*;
use crate::{
    NativePipelineConfig, NativePipelineDeadlines, NativePipelineLimits, NativePipelineParallelism,
    NativeRetainedLimits, StageCapacity, stage::test_stage_runner,
};

type TestResult = Result<(), Box<dyn Error>>;

fn config() -> Result<NativePipelineConfig, Box<dyn Error>> {
    Ok(NativePipelineConfig::new(
        NativePipelineLimits::new(
            DiscoveryLimits::new(32, 1_048_576)?,
            SourceLimits::new(1_048_576)?,
            NativeRetainedLimits::new(4 * 1_048_576, 32 * 1_048_576)?,
        ),
        NativePipelineParallelism::new(StageCapacity::new(4, 4), StageCapacity::new(4, 4))?,
        NativePipelineDeadlines::new(
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(2),
        )?,
    ))
}

fn request() -> Result<StorageRequest, RocksError> {
    StorageRequest::new(Duration::from_secs(60))
}

fn budget() -> Result<ReadBudget, RocksError> {
    ReadBudget::new(20_000, 8 * 1_048_576, Duration::from_secs(30))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_cross_file_edit_replaces_both_directions_and_keeps_unresolved_references()
-> TestResult {
    let source = tempfile::tempdir()?;
    fs::create_dir(source.path().join("src"))?;
    fs::write(
        source.path().join("Cargo.toml"),
        "[package]\nname = \"graph_probe\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(
        source.path().join("src/lib.rs"),
        "mod provider; pub fn caller() { provider::target(); }\n",
    )?;
    fs::write(
        source.path().join("src/provider.rs"),
        "pub fn target() {}\n",
    )?;
    let storage_root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        codevetra_db::rocks::ProjectOpening::new(
            storage_root.path().to_path_buf(),
            ContentDigest::from_bytes([1; 32]),
            StorePolicy::default(),
        ),
        &request()?,
    )
    .await?;
    let mut previous = None;
    for sequence in 1..=3_u8 {
        if sequence == 2 {
            fs::write(
                source.path().join("src/provider.rs"),
                "pub fn replacement() {}\n",
            )?;
        }
        if sequence == 3 {
            fs::write(
                source.path().join("src/lib.rs"),
                "mod provider; pub fn caller() { provider::replacement(); }\n",
            )?;
        }
        let identity = GraphRevision {
            generation_id: GenerationId::from_uuid_v8([sequence; 16]),
            source_revision: ContentDigest::from_bytes([sequence; 32]),
            base: previous
                .clone()
                .map_or(GraphBase::Empty, GraphBase::Current),
        };
        let (runner, tasks, cancellation) = test_stage_runner(16, 256 * 1_048_576).await;
        let (input, _) = build_native_rocks_graph(
            &runner,
            NativeGenerationBuild::new(SourceRoot::open(source.path())?, config()?),
            GraphPreparation::new(identity, GraphPolicy::default(), &request()?),
        )
        .await?;
        drop(cancellation);
        let reaped = tasks
            .close_abort_and_reap_retained(Instant::now() + Duration::from_secs(10))
            .await;
        assert!(reaped.all_joined && !reaped.worker_failed && !reaped.unobserved_results);
        let published = {
            let request_bound_input = input;
            store
                .with_request(&request()?)
                .publish_graph(request_bound_input)
                .await
        }?;
        previous = Some(published.revision.generation_id.clone());
        {
            let request_bound_expected = previous.clone();
            let request_bound_budget = budget()?;
            store
                .with_request(&request()?)
                .read(
                    request_bound_expected,
                    request_bound_budget,
                    move |snapshot, budget| verify_native_graph(snapshot, budget, sequence),
                )
                .await
        }?;
    }
    runtime.shutdown(&request()?).await?;
    Ok(())
}

fn verify_native_graph(
    snapshot: &RevisionSnapshot<'_>,
    budget: &mut ReadBudget,
    sequence: u8,
) -> Result<(), RocksError> {
    let graph = snapshot.graph(budget)?;
    // Identify actual native declarations through their canonical symbol keys.
    let page = snapshot
        .scan_prefix(codevetra_db::rocks::Collection::Graph, b"cg1/s/")
        .page(None, 100, budget)?;
    let symbols: Vec<codevetra_db::SymbolInput> = page
        .rows
        .into_iter()
        .map(|(_, bytes)| {
            serde_json::from_slice(&bytes).map_err(|_| RocksError::Corrupt {
                field: "probe_symbol",
            })
        })
        .collect::<Result<_, _>>()?;
    let caller = symbols
        .iter()
        .find(|symbol| symbol.qualified_name.ends_with("caller"))
        .ok_or(RocksError::Corrupt {
            field: "probe_caller",
        })?;
    let calls = graph
        .adjacency(&caller.symbol_id, GraphDirection::Outgoing)
        .page(None, 100, budget)?
        .links
        .into_iter()
        .filter(|edge| edge.kind() == EdgeKind::Calls)
        .collect::<Vec<_>>();
    if sequence == 2 {
        assert!(calls.is_empty());
        assert!(
            !symbols
                .iter()
                .any(|symbol| symbol.qualified_name.ends_with("target"))
        );
        let references = graph.references(&caller.file_id).page(None, 100, budget)?.0;
        assert!(
            references
                .iter()
                .any(|reference| reference.reference_name.contains("target")
                    && reference.target_symbol_id.is_none())
        );
    } else {
        assert_eq!(calls.len(), 1);
        let target = calls[0].target_symbol_id();
        let reverse = graph
            .adjacency(target, GraphDirection::Incoming)
            .page(None, 100, budget)?;
        assert!(
            reverse
                .links
                .iter()
                .any(|edge| edge.kind() == EdgeKind::Calls
                    && edge.source_symbol_id() == &caller.symbol_id)
        );
    }
    Ok(())
}
