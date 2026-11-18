//! Qualified graph predicates and entry points share the generation they actually read.

use super::*;
use codevetra_db::{
    CurrentEntryPointsLookup, CurrentGenerationLookup, EntryPointBucket, NativeDatabase,
    QualifiedSymbolQuery,
    rocks::{AsyncProjectStore, StorageRuntime, WorkerPolicy},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qualified_relationships_preserve_direction_kind_intersection_and_generation() -> TestResult
{
    let mut dependency = edge(3, 2, "type");
    dependency.kind = EdgeKind::TypeOf;
    let mut raw = raw_facts(4, vec![edge(1, 2, "call"), dependency, edge(2, 4, "call")]);
    for symbol in &mut raw.symbols {
        symbol.export.exported = true;
    }
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        codevetra_db::rocks::ProjectOpening::new(
            root.path().to_owned(),
            digest(1),
            StorePolicy::default(),
        ),
        &request()?,
    )
    .await?;
    let published = {
        let request_bound_input = input(1, canonical(raw)?)?;
        store
            .with_request(&request()?)
            .publish_graph(request_bound_input)
            .await
    }?;
    let database = NativeDatabase::new(store, &digest(1));
    let generation = &published.revision.generation_id;
    let lookup = CurrentGenerationLookup::new(database.project_id(), generation);
    let query = QualifiedSymbolQuery::new(lookup, "", 10);
    let caller = ["function_1".to_owned()];
    let target = ["function_2".to_owned()];
    for (query, expected) in [
        (query.with_callers_of(&target), vec![symbol(1)]),
        (query.with_callees_of(&caller), vec![symbol(2)]),
        (query.with_depends_on(&target), vec![symbol(1), symbol(3)]),
        (
            query.with_callers_of(&target).with_callees_of(&caller),
            vec![],
        ),
    ] {
        let result = database.qualified_current_symbols(query).await?;
        assert_eq!(
            result
                .hits()
                .iter()
                .map(|hit| hit.symbol().symbol_id().clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            result
                .hits()
                .iter()
                .all(|hit| hit.symbol().generation_id() == generation)
        );
    }
    let public = database
        .current_entry_points(CurrentEntryPointsLookup::new(
            lookup,
            EntryPointBucket::PublicExports,
            1,
        ))
        .await?;
    let public = serde_json::to_value(public)?;
    assert_eq!(public["total"], 2);
    assert_eq!(public["truncated"], true);
    assert_eq!(public["symbols"][0]["qualified_name"], "function_1");
    assert_eq!(public["symbols"][0]["generation_id"], generation.as_str());
    runtime.shutdown(&request()?).await?;
    Ok(())
}
