//! Real native storage and retrieval preserve relevant edges under high fanout.

use std::{error::Error, time::Duration};

use codevetra_db::{
    CurrentGenerationLookup, CurrentGraphLookup, EdgeInput, FileInput, GenerationFacts,
    GenerationValidationLimits, GraphDirection, NativeDatabase, SymbolInput,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
    validate_generation_facts,
};
use codevetra_domain::{
    ContentDigest, EdgeKind, FileId, FileParseStatus, GenerationId, SymbolExecutionFlags,
    SymbolExportFlags, SymbolId,
};
use codevetra_search::{
    DeterministicRetriever, GraphPathRequest, GraphPathRequestInput, TraversalBudget,
    TraversalRequest,
};
use serde as _;
use serde_json as _;
use thiserror as _;

type TestResult = Result<(), Box<dyn Error>>;

fn symbol(value: u64) -> SymbolId {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&value.to_be_bytes());
    SymbolId::from_uuid_v8(bytes)
}

fn edge(source: u64, target: u64, kind: EdgeKind, sites: u32) -> EdgeInput {
    EdgeInput {
        source_symbol_id: symbol(source),
        target_symbol_id: symbol(target),
        kind,
        confidence: 1.0,
        provenance: "fixture".into(),
        site_count: sites,
    }
}

fn high_degree_facts() -> GenerationFacts {
    let file_id = FileId::from_uuid_v8([1; 16]);
    let digest = ContentDigest::from_bytes([1; 32]);
    let mut edges = Vec::new();
    for other in 10..2010 {
        // Unrelated higher-ranked candidates in both directions would previously
        // hide the single calls edge. Contains edges also exercise impact defaults.
        for (source, target, kind) in [
            (1, other, EdgeKind::References),
            (other, 2, EdgeKind::References),
            (other, 3, EdgeKind::Contains),
        ] {
            edges.push(edge(source, target, kind, 2));
        }
    }
    edges.extend([
        edge(1, 2, EdgeKind::Calls, 1),
        edge(4, 3, EdgeKind::Calls, 1),
    ]);
    GenerationFacts {
        files: vec![FileInput {
            file_id: file_id.clone(),
            normalized_path: "src/lib.rs".into(),
            language: "rust".into(),
            content_hash: digest.clone(),
            byte_size: 1,
            parse_status: FileParseStatus::Parsed,
        }],
        symbols: (1..2010)
            .map(|id| SymbolInput {
                symbol_id: symbol(id),
                file_id: file_id.clone(),
                symbol_kind: "function".into(),
                qualified_name: format!("function_{id}"),
                signature: "fn function()".into(),
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 1,
                structural_digest: digest.clone(),
                visibility: None,
                export: SymbolExportFlags::default(),
                execution: SymbolExecutionFlags::default(),
                declaration_only: false,
                betweenness_ppb: None,
                pagerank_ppb: None,
            })
            .collect(),
        edges,
        ..GenerationFacts::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_filters_precede_candidate_cap_for_traversal_and_paths() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let request = StorageRequest::new(Duration::from_secs(60))?;
    let identity = ContentDigest::from_bytes([9; 32]);
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity.clone(),
        StorePolicy::default(),
        &request,
    )
    .await?;
    let generation = GenerationId::from_uuid_v8([1; 16]);
    let facts = validate_generation_facts(
        high_degree_facts(),
        GenerationValidationLimits::new(64 * 1_048_576, 256 * 1_048_576)?,
        || false,
    )?
    .0;
    store
        .publish_graph(
            GraphPublicationInput::new(
                GraphRevision {
                    generation_id: generation.clone(),
                    source_revision: identity.clone(),
                    base: GraphBase::Empty,
                },
                facts,
                GraphPolicy::default(),
                &request,
            )?,
            &request,
        )
        .await?;
    let database = NativeDatabase::new(store, &identity);
    let project = database.project_id().clone();
    // Both frontier roots must survive. Two thousand unrelated candidates from
    // either root cannot consume the retained two-edge page.
    for (roots, direction) in [
        ([symbol(1), symbol(4)], GraphDirection::Outgoing),
        ([symbol(2), symbol(3)], GraphDirection::Incoming),
    ] {
        let edges = database
            .current_graph_edges(
                CurrentGraphLookup::new(
                    CurrentGenerationLookup::new(&project, &generation),
                    &roots,
                    direction,
                )
                .with_limit(2)
                .with_edge_kinds(&[EdgeKind::Calls])
                .with_minimum_confidence(1.0),
            )
            .await?;
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].source_symbol_id(), &symbol(1));
        assert_eq!(edges[1].source_symbol_id(), &symbol(4));
        assert!(edges.iter().all(|edge| edge.edge_kind() == "calls"));
    }
    let retriever = DeterministicRetriever::new(database);
    let traversal =
        |id| TraversalRequest::new(project.clone(), [symbol(id)], TraversalBudget::new(2, 10)?);
    for explicit in [false, true] {
        let mut outgoing = traversal(1)?;
        let mut incoming = traversal(2)?;
        if explicit {
            outgoing = outgoing.with_edge_kind(EdgeKind::Calls);
            incoming = incoming.with_edge_kind(EdgeKind::Calls);
        }
        let callees = retriever.callees(&outgoing).await?;
        assert_eq!(callees.nodes().len(), 1);
        assert_eq!(callees.nodes()[0].symbol().symbol_id(), &symbol(2));
        assert!(!callees.truncated());
        let reverse = retriever.callers(&incoming).await?;
        assert_eq!(reverse.nodes().len(), 1);
        assert_eq!(reverse.nodes()[0].symbol().symbol_id(), &symbol(1));
        assert!(!reverse.truncated());
    }
    let impact = retriever.impact(&traversal(3)?).await?;
    assert_eq!(impact.nodes().len(), 1);
    assert_eq!(impact.nodes()[0].symbol().symbol_id(), &symbol(4));
    assert!(!impact.truncated());
    let path = retriever
        .path(
            &GraphPathRequest::new(GraphPathRequestInput {
                project_id: project,
                start: symbol(1),
                target: symbol(2),
                budget: TraversalBudget::new(2, 10)?,
            })
            .with_edge_kind(EdgeKind::Calls),
        )
        .await?;
    assert_eq!(path.path().ok_or("missing direct path")?.len(), 2);
    assert!(!path.truncated());
    runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await?;
    Ok(())
}
