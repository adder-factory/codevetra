//! Mapping identity must survive extraction, clone selection, and persisted findings.

use super::*;

fn mapping_function(name: &str, domain: &str, changed_output: bool) -> String {
    let mut source = format!("fn {name}(kind: {domain}) -> u32 {{\n    match kind {{\n");
    for variant in 'A'..='T' {
        let value = if changed_output && variant == 'A' {
            1001
        } else {
            1000
        };
        writeln!(source, "        {domain}::{variant} => {value},")
            .unwrap_or_else(|error| panic!("mapping fixture failed: {error}"));
    }
    source.push_str("    }\n}\n");
    source
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapping_clone_classes_keep_changed_outputs_but_separate_pattern_domains() {
    let project = live_project_fixture();
    let source = [
        mapping_function("original", "Kind", false),
        mapping_function("copied", "Kind", true)
            .replace("Kind::A", "/* independently maintained */ Kind::A"),
        mapping_function("unrelated", "OtherKind", false),
    ]
    .join("\n");
    std::fs::write(project.path().join("tables.rs"), source)
        .unwrap_or_else(|error| panic!("mapping fixture write failed: {error}"));
    let runtime = ProjectRuntime::open(project.path())
        .await
        .unwrap_or_else(|error| panic!("mapping runtime failed: {error}"));
    let indexed = runtime
        .index(IndexOptions::default().with_history_refresh(false))
        .await
        .unwrap_or_else(|error| panic!("mapping index failed: {error}"));
    runtime
        .database()
        .refresh_current_structural_findings(&indexed.project_id, Duration::from_secs(30))
        .await
        .unwrap_or_else(|error| panic!("mapping findings refresh failed: {error}"));
    let findings = structural_findings_json(&runtime, &indexed).await;
    let near = findings
        .as_array()
        .unwrap_or_else(|| panic!("expected findings array"))
        .iter()
        .filter(|finding| {
            finding["finding"] == "duplicate_code" && finding["detail"]["cloneType"] == "near"
        })
        .collect::<Vec<_>>();
    assert_eq!(near.len(), 1, "{findings:#}");
    assert_eq!(near[0]["detail"]["classSize"], 2);
    let mut members = std::collections::BTreeSet::new();
    members.insert(near[0]["qualifiedName"].as_str().unwrap_or_default());
    for member in near[0]["detail"]["members"]
        .as_array()
        .unwrap_or_else(|| panic!("missing clone members"))
    {
        members.insert(member["qualifiedName"].as_str().unwrap_or_default());
    }
    assert_eq!(
        members,
        std::collections::BTreeSet::from(["original", "copied"]),
        "{findings:#}"
    );
    runtime
        .close()
        .await
        .unwrap_or_else(|error| panic!("mapping shutdown failed: {error}"));
}
