//! Directional aggregation retains all sites before independently bounding each response.

use super::*;
use codevetra_db::{
    FileDependencyDirection, FileDependencyQuery, NativeDatabase,
    rocks::{AsyncProjectStore, StorageRuntime, WorkerPolicy},
};
use codevetra_domain::NormalizedPath;

fn dependency_facts() -> TestResult<CanonicalGenerationFacts> {
    let mut reference = edge(2, 3, "reference");
    reference.kind = EdgeKind::References;
    let mut contains = edge(1, 5, "containment");
    contains.kind = EdgeKind::Contains;
    let mut facts = raw_facts(
        6,
        vec![
            edge(1, 3, "first"),
            edge(1, 3, "second"),
            reference,
            edge(1, 4, "other-outgoing"),
            edge(5, 1, "first-incoming"),
            edge(5, 2, "second-incoming"),
            edge(6, 2, "other-incoming"),
            edge(1, 2, "same-file"),
            contains,
        ],
    );
    for (index, path) in [
        (2, "src/a.rs"),
        (3, "src/b.rs"),
        (4, "src/c.rs"),
        (5, "src/d.rs"),
    ] {
        let mut file = facts.files[0].clone();
        file.file_id = FileId::from_uuid_v8([index; 16]);
        file.normalized_path = path.into();
        facts.files.push(file);
    }
    for (symbol, file) in facts
        .symbols
        .iter_mut()
        .skip(2)
        .zip(facts.files.iter().skip(1))
    {
        symbol.file_id = file.file_id.clone();
    }
    canonical(facts)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_dependency_totals_keep_multiplicity_and_independent_direction_limits() -> TestResult {
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
    {
        let request_bound_input = input(1, dependency_facts()?)?;
        store
            .with_request(&request()?)
            .publish_graph(request_bound_input)
            .await
    }?;
    let database = NativeDatabase::new(store, &digest(1));
    let query = FileDependencyQuery::new(NormalizedPath::parse("src/lib.rs")?, 1)?;
    let result = serde_json::to_value(
        database
            .current_file_dependencies(database.project_id(), &query)
            .await?,
    )?;
    assert_eq!(
        result,
        serde_json::json!({
            "rows": [
                {"direction": "dependencies", "path": "src/a.rs", "language": "rust",
                 "edgeCount": 3, "siteCount": 9, "edgeKinds": ["calls", "references"]},
                {"direction": "dependents", "path": "src/c.rs", "language": "rust",
                 "edgeCount": 2, "siteCount": 6, "edgeKinds": ["calls"]}
            ],
            "counts": {"dependencies": 2, "dependents": 2},
            "truncated": {"dependencies": true, "dependents": true}
        })
    );
    for (direction, name) in [
        (FileDependencyDirection::Dependencies, "dependencies"),
        (FileDependencyDirection::Dependents, "dependents"),
    ] {
        let selected = database
            .current_file_dependencies(
                database.project_id(),
                &query.clone().with_direction(direction),
            )
            .await?;
        let selected = serde_json::to_value(selected)?;
        assert_eq!(selected["rows"].as_array().map(Vec::len), Some(1));
        assert_eq!(selected["rows"][0]["direction"], name);
        assert_eq!(selected["counts"][name], 2);
    }
    runtime.shutdown(&request()?).await?;
    Ok(())
}
