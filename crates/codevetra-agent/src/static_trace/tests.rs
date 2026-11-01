use super::*;
use crate::IndexOptions;
use codevetra_db::{CurrentGenerationLookup, ExactTextLookup};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_retains_a_bounded_prefix_without_claiming_final_freshness()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    std::fs::write(
        root.path().join("trace.ts"),
        "export function entry() { return target(); }\nexport function target() { return 7; }\n",
    )?;
    let runtime = ProjectRuntime::open(root.path()).await?;
    let indexed = runtime
        .index(IndexOptions::default().with_history_refresh(false))
        .await?;
    let symbols = runtime
        .database()
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(&indexed.project_id, &indexed.generation_id),
            "entry",
            10,
        ))
        .await?;
    assert_eq!(symbols.len(), 1);
    let request = || StaticTraceRequest {
        symbol_id: symbols[0].symbol_id().clone(),
        focus: None,
        maximum_steps: 128,
        maximum_depth: 2,
        maximum_bytes: 16 * 1024,
    };
    let mut cancellation = ProjectCancellation::new();
    cancellation.trace_cancel_after = Some(1);
    let partial = runtime.static_trace(request(), cancellation).await?;
    assert!(partial.partial);
    assert!(!partial.fresh);
    assert_eq!(partial.functions.len(), 1);
    assert_eq!(partial.functions[0].symbol.qualified_name(), "entry");
    assert!(
        partial
            .unknowns
            .contains(&"cancelled_generation_freshness_not_rechecked")
    );
    assert!(
        partial.functions[0]
            .calls
            .iter()
            .any(|call| call.target.is_some() && !call.expanded)
    );
    assert_eq!(partial.encoded_bytes, serde_json::to_vec(&partial)?.len());
    assert!(partial.encoded_bytes <= request().maximum_bytes);
    // The cancelled work has been reaped: the same owner serves the complete retry.
    let complete = runtime
        .static_trace(request(), ProjectCancellation::new())
        .await?;
    assert!(complete.fresh);
    assert_eq!(complete.functions.len(), 2);
    let cancelled = ProjectCancellation::new();
    cancelled.cancel();
    assert!(matches!(
        runtime.static_trace(request(), cancelled).await,
        Err(ProjectError::RequestCancelled)
    ));
    runtime.close().await?;
    Ok(())
}
