use super::*;
use codevetra_db::rocks::StorageRequest;
use codevetra_mcp::{
    MODERN_PROTOCOL_VERSION, ProtocolServer, ServerConfig, ServerLimits, ServerMetadata,
    ToolProfile,
};
use serde_json::{Map, Value, json};
use std::{error::Error, fmt::Write as _, time::Duration};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explore_retains_large_native_allocations_without_exceeding_navigation_seed_bounds()
-> TestResult {
    let root = tempfile::tempdir()?;
    let mut source = String::new();
    for number in 0..30 {
        writeln!(
            source,
            "pub fn inventory_operation_{number}() -> u32 {{ {number} }}"
        )?;
    }
    std::fs::write(root.path().join("lib.rs"), source)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    for decision in ["native", "auto"] {
        let result = handler.call(ToolCall {
            name: super::super::EXPLORE_TOOL.to_owned(),
            arguments: json!({"query":"inventory operation", "mode":"deterministic", "decision":decision, "maxFiles":1})
                .as_object().cloned().ok_or("missing exploration arguments")?,
        }, ToolCallContext::local(Duration::from_secs(30))).await?;
        let result = result
            .structured_content()
            .ok_or("missing exploration result")?;
        assert_eq!(result["freshness"], "current");
        let evidence = &result["evidence"];
        assert!(
            evidence["sourceWindows"]
                .as_array()
                .is_some_and(|windows| windows.len() > 20)
        );
        assert_eq!(
            evidence["navigation"]["stop"],
            if decision == "native" {
                "native_requested"
            } else {
                "not_configured"
            }
        );
        assert_eq!(evidence["navigation"]["nativeSourceWindows"], 0);
    }
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codevetra_name_migration_reopens_the_original_database_and_preserves_policy() -> TestResult
{
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".codevetra"))?;
    std::fs::create_dir(root.path().join("private"))?;
    std::fs::write(
        root.path().join("main.ts"),
        "export function visible() { return 1; }\n",
    )?;
    std::fs::write(
        root.path().join("private/secret.ts"),
        "export function excluded() {}\n",
    )?;
    std::fs::write(root.path().join(".codevetraignore"), "private/**\n")?;
    let config = br#"{"maxFileSize":1048576,"llm":{"provider":"external","apiKeyEnv":"PROVIDER_OWNED_KEY"}}"#;
    std::fs::write(root.path().join(".codevetra/config.json"), config)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let original = RocksProjectRuntime::open(root.path(), &control).await?;
    let report = original
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    original.shutdown().await?;
    drop(original);
    // Model a legacy installation without modifying any durable database bytes.
    std::fs::rename(
        root.path().join(".codevetra"),
        root.path().join(".cartograph"),
    )?;
    std::fs::rename(
        root.path().join(".codevetraignore"),
        root.path().join(".cartographignore"),
    )?;
    assert!(
        RocksProjectRuntime::open(root.path(), &control)
            .await
            .is_err()
    );
    let migration_root = root.path().to_path_buf();
    tokio::task::spawn_blocking(move || codevetra_config::migrate_project_name(&migration_root))
        .await??;
    assert!(super::super::has_project_marker(root.path()).is_ok_and(|present| present));
    assert_eq!(
        std::fs::read(root.path().join(".codevetra/config.json"))?,
        config
    );
    let migrated = RocksProjectRuntime::open(root.path(), &control).await?;
    let status = migrated.status(&control).await?;
    assert_eq!(
        status
            .current
            .as_ref()
            .map(|revision| &revision.generation_id),
        Some(&report.revision.generation_id)
    );
    assert!(status.fresh);
    assert!(matches!(
        codevetra_config::migrate_project_name(root.path()),
        Err(codevetra_config::NameMigrationError::Busy)
    ));
    let handler = RocksHandler::new(migrated.clone(), false)?;
    let result = handler
        .call(
            ToolCall {
                name: super::super::FIND_TOOL.to_owned(),
                arguments: json!({"by":"name","query":"visible"})
                    .as_object()
                    .cloned()
                    .ok_or("arguments")?,
            },
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    assert!(serde_json::to_string(&result.structured_content())?.contains("visible"));
    let restricted = RocksHandler::new(migrated.clone(), false)?
        .restrict(ToolProfile::Full, &["cartograph_find".to_owned()]);
    assert!(
        restricted
            .tools()
            .iter()
            .all(|tool| tool.name() != "codevetra_find")
    );
    assert!(
        restricted
            .tools()
            .iter()
            .any(|tool| tool.name() == "codevetra_status")
    );
    assert!(
        restricted
            .call(
                ToolCall {
                    name: "codevetra_find".to_owned(),
                    arguments: serde_json::from_value(json!({"by":"name","query":"visible"}))?,
                },
                ToolCallContext::local(Duration::from_secs(30))
            )
            .await
            .is_err()
    );
    handler.close().await?;
    Ok(())
}

fn trace_fixture_entry_id(value: &Value) -> Option<String> {
    if value.get("qualified_name").and_then(Value::as_str) == Some("endpoint") {
        return value.get("symbol_id")?.as_str().map(str::to_owned);
    }
    match value {
        Value::Object(map) => map.values().find_map(trace_fixture_entry_id),
        Value::Array(rows) => rows.iter().find_map(trace_fixture_entry_id),
        _ => None,
    }
}
async fn boundary_graph(
    handler: &RocksHandler,
    start: &str,
    direction: &str,
) -> Result<Value, Box<dyn Error + Send + Sync>> {
    let result = handler
        .call(
            ToolCall {
                name: super::super::GRAPH_TOOL.to_owned(),
                arguments: json!({"start":start,"direction":direction,"depth":6,"limit":100})
                    .as_object()
                    .cloned()
                    .ok_or("arguments")?,
            },
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    Ok(serde_json::to_value(result.structured_content())?)
}
fn graph_contains_name(value: &Value, name: &str) -> bool {
    if value.get("qualified_name").and_then(Value::as_str) == Some(name) {
        return true;
    }
    match value {
        Value::Object(map) => map.values().any(|value| graph_contains_name(value, name)),
        Value::Array(rows) => rows.iter().any(|value| graph_contains_name(value, name)),
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn application_boundaries_survive_native_publication_and_invalidate_on_registration_edit()
-> TestResult {
    let root = tempfile::tempdir()?;
    let source = "import { EventEmitter } from 'node:events'; import { onEvent } from './worker'; const bus = new EventEmitter(); bus.on('same',onEvent); export function send() { bus.emit('same'); }\n";
    std::fs::write(root.path().join("events.ts"), source)?;
    std::fs::write(
        root.path().join("worker.ts"),
        "export function onEvent() { return 1; }\n",
    )?;
    std::fs::write(
        root.path().join("events.test.ts"),
        "import { send } from './events'; export function test_event_delivery() { send(); }\n",
    )?;
    for arguments in [
        vec!["init", "-b", "main"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .current_dir(root.path())
                .args(arguments)
                .output()?
                .status
                .success()
        );
    }
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    let forward = boundary_graph(&handler, "send", "callees").await?;
    assert!(graph_contains_name(&forward, "onEvent"), "{forward}");
    assert!(serde_json::to_string(&forward)?.contains("possible_scoped_destination"));
    let reverse = boundary_graph(&handler, "onEvent", "impact").await?;
    assert!(graph_contains_name(&reverse, "send"), "{reverse}");
    std::fs::write(
        root.path().join("worker.ts"),
        "export function onEvent() { return 2; }\n",
    )?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    boundary_consumer_evidence(&handler).await?;
    std::fs::write(
        root.path().join("events.ts"),
        source.replace("bus.on('same'", "bus.on('different'"),
    )?;
    let warm = native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let changed = boundary_graph(&handler, "send", "callees").await?;
    assert!(!graph_contains_name(&changed, "onEvent"), "{changed}");
    assert!(serde_json::to_string(&changed)?.contains("no_supported_registration"));
    let rebuilt = native
        .index(
            codevetra_agent::RocksIndexOptions {
                force: true,
                ..Default::default()
            },
            &control,
        )
        .await?;
    assert_eq!(
        warm.revision.content_digest,
        rebuilt.revision.content_digest
    );
    assert!(!graph_contains_name(
        &boundary_graph(&handler, "send", "callees").await?,
        "onEvent"
    ));
    std::fs::write(root.path().join("events.ts"), "export function send() {}\n")?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let deleted = boundary_graph(&handler, "send", "callees").await?;
    assert!(!serde_json::to_string(&deleted)?.contains("boundary-v1"));
    handler.close().await?;
    Ok(())
}

async fn boundary_consumer_evidence(handler: &RocksHandler) -> TestResult {
    for (name, arguments, expected) in [
        (
            super::super::GRAPH_TOOL,
            json!({"start":"send","to":"onEvent","direction":"path","depth":8}),
            "event_dispatch",
        ),
        (
            super::super::AFFECTED_TOOL,
            json!({"files":["worker.ts"],"depth":8}),
            "events.test.ts",
        ),
        (
            super::super::CONTEXT_TOOL,
            json!({"task":"explain how send dispatches to onEvent","exactName":"send","maxNodes":50,"mode":"deterministic","format":"json","code":false,"workingTree":"off"}),
            "boundary-v1",
        ),
        (
            super::super::REVIEW_TOOL,
            json!({"mode":"context","baseRef":"HEAD","maxCallersPerSymbol":20}),
            "boundary-v1",
        ),
    ] {
        let result = handler
            .call(
                ToolCall {
                    name: name.to_owned(),
                    arguments: serde_json::from_value(arguments)?,
                },
                ToolCallContext::local(Duration::from_secs(30)),
            )
            .await?;
        let value = serde_json::to_value(result.structured_content())?;
        let encoded = serde_json::to_string(&value)?;
        assert!(encoded.contains(expected), "{name}: {encoded}");
        assert!(
            encoded.contains("application-boundaries-v1"),
            "{name}: missing coverage"
        );
        assert!(
            encoded.contains("generation"),
            "{name}: missing generation identity"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_trace_expands_exact_calls_once_and_exposes_recursion_staleness_and_limits()
-> TestResult {
    let root = tempfile::tempdir()?;
    let source = "export function mutate() { return 1; }\nexport async function endpoint(allowed: boolean) { if (!allowed) { throw Error('stop'); } try { await mutate(); endpoint(false); } finally { cleanup(); } }\n";
    std::fs::write(root.path().join("entry.ts"), source)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    let call = |name: &str, arguments: Value| ToolCall {
        name: name.to_owned(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
    };
    let found = handler
        .call(
            call(
                super::super::NODE_TOOL,
                json!({"symbol":"endpoint","code":false}),
            ),
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    let id = trace_fixture_entry_id(&serde_json::to_value(found.structured_content())?)
        .ok_or("entry id")?;
    let trace = handler
        .call(
            call(
                super::super::NAVIGATE_TOOL,
                json!({"operation":"trace","symbolId":id,"maxSteps":128,"maxDepth":2}),
            ),
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    let report = trace.structured_content().ok_or("trace")?;
    let functions = report["functions"].as_array().ok_or("functions")?;
    assert_eq!(functions.len(), 2, "{report:?}");
    let root_trace = &functions[0];
    let calls = root_trace["calls"].as_array().ok_or("calls")?;
    assert!(calls.iter().any(|call| call["target"] == id
        && call["state"] == "definition_reuse_or_recursion_boundary"
        && call["expanded"] == true));
    assert!(
        calls
            .iter()
            .any(|call| call["state"] == "resolved_static_call" && call["expanded"] == true)
    );
    assert!(
        calls
            .iter()
            .any(|call| call["state"] == "unresolved_ambiguous_or_coarse_call")
    );
    assert_eq!(report["encodedBytes"], serde_json::to_vec(report)?.len());
    let limited = handler
        .call(
            call(
                super::super::NAVIGATE_TOOL,
                json!({"operation":"trace","symbolId":id,"maxSteps":2,"maxDepth":0}),
            ),
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    assert_eq!(
        limited.structured_content().ok_or("limited")?["partial"],
        true
    );
    std::fs::write(
        root.path().join("entry.ts"),
        format!("// changed\n{source}"),
    )?;
    let stale = handler
        .call(
            call(
                super::super::NAVIGATE_TOOL,
                json!({"operation":"trace","symbolId":id}),
            ),
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    let stale = stale.structured_content().ok_or("stale")?;
    assert_eq!(stale["fresh"], false);
    assert_eq!(stale["functions"], json!([]));
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_reuse_recovers_long_line_bytes_and_invalidates_changed_generations() -> TestResult {
    let root = tempfile::tempdir()?;
    let text = format!(
        "export function value() {{ /* {} */ return 1; }}\n",
        "😀a".repeat(60_000)
    );
    std::fs::write(root.path().join("entry.ts"), &text)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    let call = |arguments: Value| ToolCall {
        name: super::super::FILES_TOOL.to_owned(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
    };
    let first = handler.call(call(json!({"format":"read","file":"entry.ts","byteOffset":0,"byteLimit":1024,"sourceReuse":"offer"})), ToolCallContext::local(Duration::from_secs(30))).await?;
    let first = first.structured_content().ok_or("first")?;
    let source = &first["evidence"]["source"];
    let token = source["sourceHandle"].as_str().ok_or("handle")?.to_owned();
    let end = source["excerpt"]["endByte"].as_u64().ok_or("end")?;
    let second = handler
        .call(
            call(json!({"format":"read","file":"entry.ts","byteOffset":end,"byteLimit":1024})),
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    let second = second.structured_content().ok_or("second")?;
    let rest = &second["evidence"]["source"]["excerpt"];
    assert_eq!(rest["startByte"], end);
    let end2 = usize::try_from(rest["endByte"].as_u64().ok_or("end2")?)?;
    assert_eq!(
        format!(
            "{}{}",
            source["excerpt"]["text"].as_str().ok_or("text")?,
            rest["text"].as_str().ok_or("text")?
        ),
        &text[..end2]
    );
    let recovered = handler
        .call(
            call(json!({"format":"read","sourceHandle":token})),
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    assert_eq!(
        recovered.structured_content().ok_or("recovered")?["source"]["excerpt"]["text"],
        source["excerpt"]["text"]
    );
    // Exact source also resolves through a canonical directory alias.
    let alias = root.path().join(".").display().to_string();
    let alias = handler
        .call(
            call(json!({"format":"read","sourceHandle":token,"projectPath":alias})),
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    assert_eq!(
        alias.structured_content().ok_or("alias")?["source"]["excerpt"]["text"],
        source["excerpt"]["text"]
    );
    std::fs::write(root.path().join("entry.ts"), format!("// moved\n{text}"))?;
    assert!(
        handler
            .call(
                call(json!({"format":"read","sourceHandle":token})),
                ToolCallContext::local(Duration::from_secs(30))
            )
            .await
            .is_err()
    );
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    assert!(
        handler
            .call(
                call(json!({"format":"read","sourceHandle":token})),
                ToolCallContext::local(Duration::from_secs(30))
            )
            .await
            .is_err()
    );
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_allocation_review_keeps_changed_guards_and_cleanup_or_marks_source_incomplete()
-> TestResult {
    let root = tempfile::tempdir()?;
    let file = root.path().join("entry.ts");
    std::fs::write(
        &file,
        "export function save(allowed: boolean) { if (allowed) throw Error(); try { mutate(); } finally { cleanup(); } }\n",
    )?;
    for arguments in [
        vec!["init", "--quiet"],
        vec!["add", "entry.ts"],
        vec![
            "-c",
            "user.name=Navigation Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        let status = std::process::Command::new("git")
            .current_dir(root.path())
            .args(arguments)
            .status()?;
        assert!(status.success());
    }
    std::fs::write(
        &file,
        "export function save(allowed: boolean) { if (!allowed) throw Error(); try { mutate(); } finally { cleanup(); } }\n",
    )?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    for (detail, complete) in [("adaptive", true), ("outline", false)] {
        let result = handler
            .call(
                ToolCall {
                    name: super::super::REVIEW_TOOL.to_owned(),
                    arguments: json!({"mode":"context","baseRef":"HEAD","sourceDetail":detail})
                        .as_object()
                        .cloned()
                        .ok_or("missing review arguments")?,
                },
                ToolCallContext::local(Duration::from_secs(30)),
            )
            .await?;
        let evidence = result.structured_content().ok_or("missing review result")?;
        let allocation = &evidence["sourceAllocation"];
        assert_eq!(allocation["requiredBodiesComplete"], complete);
        let body = allocation["windows"]
            .as_array()
            .ok_or("missing review windows")?
            .iter()
            .find(|row| row["symbol"]["qualified_name"] == "save")
            .ok_or("missing changed function")?;
        if complete {
            let text = body["excerpt"]["text"]
                .as_str()
                .ok_or("missing required body")?;
            assert!(text.contains("if (!allowed) throw Error()"));
            assert!(text.contains("finally { cleanup(); }"));
        } else {
            assert!(body["excerpt"].is_null());
            assert!(
                allocation["incompletePriority"]
                    .as_array()
                    .is_some_and(|ids| !ids.is_empty())
            );
        }
    }
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_allocation_keeps_selected_bodies_and_labels_same_file_sibling_outlines()
-> TestResult {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join("src"))?;
    let mut source = "pub fn chosen() { let _value = 1; }\npub fn orchestrate(allowed: bool) { if !allowed { return; } chosen(); }\n".to_owned();
    for number in 0..12 {
        writeln!(
            source,
            "pub fn sibling_{number}() {{ let _value = {number}; }}"
        )?;
    }
    std::fs::write(root.path().join("src/app.rs"), source)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    let response = handler.call(ToolCall {
        name: super::super::CONTEXT_TOOL.to_owned(),
        arguments: json!({"task":"change orchestrate through chosen", "exactName":"orchestrate", "exactPath":"src/app.rs",
            "format":"json", "retrievalMode":"deterministic", "workingTree":"off", "localLearning":"off", "maxNodes":30})
            .as_object().cloned().ok_or("missing context arguments")?,
    }, ToolCallContext::local(Duration::from_secs(30))).await?;
    let evidence = response
        .structured_content()
        .ok_or("missing context evidence")?;
    let windows = evidence["evidence"]["sourceWindows"]
        .as_array()
        .ok_or("missing source windows")?;
    for name in ["orchestrate", "chosen"] {
        let row = windows
            .iter()
            .find(|row| row["symbol"]["qualified_name"] == name)
            .ok_or("missing priority body")?;
        assert_eq!(row["representation"], "full");
        assert!(row["omitted_ranges"].as_array().is_some_and(Vec::is_empty));
    }
    let siblings = windows
        .iter()
        .filter(|row| {
            row["symbol"]["qualified_name"]
                .as_str()
                .is_some_and(|name| name.starts_with("sibling_"))
        })
        .collect::<Vec<_>>();
    assert!(
        !siblings.is_empty(),
        "fixture must include peripheral siblings"
    );
    for row in siblings {
        assert_eq!(row["representation"], "outline");
        assert!(row["excerpt"].is_null());
        assert!(
            row["omitted_ranges"]
                .as_array()
                .is_some_and(|ranges| !ranges.is_empty())
        );
    }
    assert_eq!(
        evidence["evidence"]["sourceWindowStatus"]["requiredBodiesComplete"],
        true
    );
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_allocation_accounts_for_json_escaping_and_reports_missing_required_body()
-> TestResult {
    use codevetra_agent::{
        ProjectCancellation, SourceAllocationOptions, SourceSelection, SourceSelectionPolicy,
    };
    use codevetra_db::{CurrentGenerationLookup, ExactTextLookup};
    let root = tempfile::tempdir()?;
    let source = format!("pub fn huge() {{ /* {} */ }}\n", "\\\"λ".repeat(20_000));
    std::fs::write(root.path().join("lib.rs"), source)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let database = native.database();
    let generation = database
        .current_generation_record(database.project_id())
        .await?
        .ok_or("missing generation")?;
    let symbols = database
        .exact_current_symbols_by_name(ExactTextLookup::new(
            CurrentGenerationLookup::new(database.project_id(), generation.generation_id()),
            "huge",
            10,
        ))
        .await?;
    let id = symbols
        .first()
        .ok_or("missing huge symbol")?
        .symbol_id()
        .clone();
    let runtime = ProjectRuntime::from_native(native.clone());
    for (limit, complete) in [(16 * 1024, false), (256 * 1024, true)] {
        let allocation = runtime
            .allocate_source_with_cancellation(
                generation.generation_id(),
                vec![SourceSelection {
                    symbol_id: id.clone(),
                    prioritized: true,
                }],
                SourceAllocationOptions::new(SourceSelectionPolicy::Full, limit)?,
                ProjectCancellation::new(),
            )
            .await?
            .ok_or("generation changed")?;
        assert_eq!(
            serde_json::to_vec(&allocation)?.len(),
            allocation.encoded_bytes
        );
        assert!(allocation.encoded_bytes <= limit);
        assert_eq!(allocation.required_bodies_complete, complete);
        assert_eq!(allocation.incomplete_priority.is_empty(), complete);
    }
    native.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn position_navigation_preserves_imported_scip_occurrences_and_implementations() -> TestResult
{
    use codevetra_agent::{
        ProjectCancellation, ScipExportRequest, ScipImportLimits, ScipImportRequest,
    };
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join("src"))?;
    let source = "export interface Task { execute(): void; }\nexport class Worker implements Task { execute() {} }\nexport function entry() { return new Worker(); }\n";
    std::fs::write(root.path().join("src/task.ts"), source)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let runtime = ProjectRuntime::from_native(native.clone());
    runtime
        .export_scip_with_cancellation(
            ScipExportRequest::new("navigation.scip", 10_000)?,
            ProjectCancellation::new(),
        )
        .await?;
    runtime
        .import_scip_with_cancellation(
            ScipImportRequest::new(
                "navigation.scip",
                ScipImportLimits::new(1_048_576, 10_000, 4)?,
            )?,
            ProjectCancellation::new(),
        )
        .await
        .map_err(|error| format!("SCIP import: {error:?}"))?;
    let handler = RocksHandler::new(native.clone(), false)?;
    let line = source.lines().nth(1).ok_or("missing implementation")?;
    let definition = position_result(&handler, json!({"operation":"definition","file":"src/task.ts","line":2,"column":line.find("Task").ok_or("missing interface reference")?})).await?;
    assert_eq!(definition["state"], "resolved", "{definition}");
    assert!(
        definition["anchorSites"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|site| site["occurrence"]["resolution_provenance"]
                .as_str()
                .is_some_and(|text| text.contains("scip")))
    );
    let target = definition["definitions"][0]["symbol_id"].clone();
    let implementations = position_result(
        &handler,
        json!({"operation":"implementation","symbolId":target}),
    )
    .await?;
    assert_eq!(implementations["state"], "resolved", "{implementations}");
    assert!(
        implementations["implementations"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|symbol| symbol["qualified_name"] == "Worker")
    );
    assert!(
        implementations["relations"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn position_navigation_resolves_aliases_and_keeps_references_on_one_identity() -> TestResult {
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join("src"))?;
    std::fs::write(
        root.path().join("src/left.ts"),
        "export function work() { return 1; }\n",
    )?;
    std::fs::write(
        root.path().join("src/right.ts"),
        "export function work() { return 2; }\nexport function unrelated() { return work(); }\n",
    )?;
    let entry = "import { work as run } from './left';\r\nexport function entry() { /* α😀 */ run(); run(); }\r\nexport function shadow(run: () => void) { run(); }\r\n";
    std::fs::write(root.path().join("src/entry.ts"), entry)?;
    let control = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &control).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    let source_line = entry.lines().nth(1).ok_or("missing line")?;
    let column = source_line.find("run()").ok_or("missing call")?;
    let resolved = position_result(
        &handler,
        json!({"operation":"definition","file":"src/entry.ts","line":2,"column":column}),
    )
    .await?;
    assert_eq!(resolved["state"], "resolved", "{resolved}");
    assert_eq!(resolved["definitions"][0]["path"], "src/left.ts");
    assert_eq!(
        resolved["anchorSites"][0]["occurrence"]["span_precision"],
        "exact"
    );
    let symbol = resolved["definitions"][0]["symbol_id"].clone();
    let page = position_result(
        &handler,
        json!({"operation":"references","symbolId":symbol,"limit":1}),
    )
    .await?;
    assert_eq!(page["references"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["truncated"], true);
    let next = page["next"].clone();
    let following = position_result(
        &handler,
        json!({"operation":"references","symbolId":symbol,"limit":100,"after":next}),
    )
    .await?;
    assert_eq!(following["truncated"], false);
    for site in page["references"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(following["references"].as_array().into_iter().flatten())
    {
        assert_eq!(site["file"]["path"], "src/entry.ts");
        assert_eq!(site["occurrence"]["target_symbol_id"], symbol);
    }
    for position in [source_line.find("α").ok_or("missing comment")?, column + 3] {
        let empty = position_result(
            &handler,
            json!({"operation":"definition","file":"src/entry.ts","line":2,"column":position}),
        )
        .await?;
        assert_ne!(empty["state"], "resolved");
        assert!(empty["definitions"].as_array().is_some_and(Vec::is_empty));
    }
    let shadow_line = entry.lines().nth(2).ok_or("missing shadow line")?;
    let shadow = position_result(&handler, json!({"operation":"definition","file":"src/entry.ts","line":3,"column":shadow_line.rfind("run()").ok_or("missing shadow")?})).await?;
    assert!(
        shadow["definitions"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|row| row["symbol_id"] != symbol)
    );
    std::fs::write(root.path().join("src/entry.ts"), format!("\n{entry}"))?;
    let stale = position_result(
        &handler,
        json!({"operation":"definition","file":"src/entry.ts","line":2,"column":column}),
    )
    .await?;
    assert_eq!(stale["state"], "stale");
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &control)
        .await?;
    assert!(
        position_result(
            &handler,
            json!({"operation":"references","symbolId":symbol,"after":next})
        )
        .await
        .is_err()
    );
    handler.close().await?;
    Ok(())
}

async fn position_result(
    handler: &RocksHandler,
    arguments: Value,
) -> Result<Value, Box<dyn Error + Send + Sync>> {
    let result = handler
        .call(
            ToolCall {
                name: super::super::NAVIGATE_TOOL.to_owned(),
                arguments: arguments.as_object().cloned().ok_or("missing arguments")?,
            },
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    result
        .structured_content()
        .cloned()
        .map(Value::Object)
        .ok_or_else(|| "missing structured navigation result".into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detector_coverage_distinguishes_unsupported_excluded_missing_and_evaluated() -> TestResult
{
    let root = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join("src"))?;
    std::fs::create_dir(root.path().join("tests"))?;
    std::fs::write(
        root.path().join("src/lib.rs"),
        "pub fn rust_entry() -> u32 { 1 }\n",
    )?;
    std::fs::write(
        root.path().join("src/api.ts"),
        "export function danger(input: string) { return eval(input); }\nexport function safe(input: string) { return input; }\n",
    )?;
    std::fs::write(
        root.path().join("tests/api.test.ts"),
        "export function fixture(input: string) { return eval(input); }\n",
    )?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &request).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &request)
        .await?;
    let handler = RocksHandler::new(native.clone(), false)?;
    let pending = coverage_result(&handler).await?;
    assert_eq!(pending["evidence"]["state"], "not_computed");
    assert_eq!(
        pending["evidence"]["catalog"].as_array().map(Vec::len),
        Some(35)
    );
    native
        .database()
        .refresh_current_structural_findings(
            native.database().project_id(),
            Duration::from_secs(30),
        )
        .await?;
    let ready = coverage_result(&handler).await?;
    let rows = ready["evidence"]["nativeSymbolPopulation"]
        .as_array()
        .ok_or("missing coverage")?;
    let find = |language, detector| {
        rows.iter()
            .find(|row| row["language"] == language && row["detector"] == detector)
            .ok_or("missing detector population")
    };
    let unsupported = find("rust", "dynamic_eval")?;
    assert_eq!(unsupported["state"], "unsupported");
    assert_eq!(unsupported["evaluatedSymbols"], 0);
    assert!(
        unsupported["unsupportedSymbols"]
            .as_u64()
            .ok_or("missing count")?
            > 0
    );
    let evaluated = find("typescript", "dynamic_eval")?;
    assert_eq!(evaluated["state"], "evaluated");
    assert_eq!(evaluated["findingCount"], 1);
    assert_eq!(evaluated["evaluatedSymbols"], 2);
    assert!(
        evaluated["excludedSymbols"]
            .as_u64()
            .ok_or("missing exclusions")?
            > 0
    );
    let missing = find("rust", "low_coverage")?;
    assert_eq!(missing["state"], "unavailable");
    assert_eq!(missing["evaluatedSymbols"], 0);
    assert_eq!(
        ready["evidence"]["projectLayerAnalysis"]["state"],
        "not_configured"
    );
    handler.close().await?;
    Ok(())
}

async fn coverage_result(handler: &RocksHandler) -> Result<Value, Box<dyn Error + Send + Sync>> {
    let result = handler
        .call(
            ToolCall {
                name: "codevetra_biomarkers".to_owned(),
                arguments: serde_json::from_value(json!({"mode":"coverage", "format":"json"}))?,
            },
            ToolCallContext::local(Duration::from_secs(30)),
        )
        .await?;
    result
        .structured_content()
        .cloned()
        .map(Value::Object)
        .ok_or_else(|| "missing structured coverage".into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_name_limit_prefers_definitions_over_parameters() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut source = String::new();
    for number in 0..30 {
        writeln!(
            source,
            "pub fn consumer_{number}(shared: usize) -> usize {{ shared }}"
        )?;
    }
    std::fs::write(root.path().join("a.rs"), source)?;
    std::fs::write(root.path().join("z.rs"), "pub mod shared {}\n")?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let native = RocksProjectRuntime::open(root.path(), &request).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &request)
        .await?;
    let handler = RocksHandler::new(native, false)?;
    for (filter, kind) in [(None, "module"), (Some("parameter"), "parameter")] {
        let mut arguments = serde_json::from_value::<Map<String, Value>>(
            json!({"by":"name", "query":"shared", "limit":1}),
        )?;
        if let Some(filter) = filter {
            arguments.insert("kind".to_owned(), json!(filter));
        }
        let result = handler
            .call(
                ToolCall {
                    name: "codevetra_find".to_owned(),
                    arguments,
                },
                ToolCallContext::local(Duration::from_secs(30)),
            )
            .await?;
        let content = result.structured_content().ok_or("missing evidence")?;
        assert_eq!(content["evidence"]["rows"][0]["kind"], kind);
        assert!(content["evidence"]["truncated"].as_bool().unwrap_or(false));
    }
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structural_role_sweep_reports_truncation_and_completes_across_cached_pages() -> TestResult
{
    use super::super::{
        ProjectCancellation, ProjectRuntime, RoleClassificationSweepRequest, RoleClassifier,
        STRUCTURAL_ROLE_MODEL, run_role_classification_sweep,
    };
    let root = tempfile::tempdir()?;
    let mut source = String::new();
    for number in 0..600 {
        writeln!(source, "pub fn worker_{number}() -> u32 {{ {number} }}")?;
    }
    std::fs::write(root.path().join("lib.rs"), source)?;
    let native =
        RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    native
        .index(
            codevetra_agent::RocksIndexOptions::default(),
            &StorageRequest::new(Duration::from_secs(30))?,
        )
        .await?;
    let runtime = Arc::new(ProjectRuntime::from_native(native.clone()));
    for (limit, candidates, truncated) in [(1, 1, true), (1000, 599, false), (1, 0, false)] {
        let report = tokio::time::timeout(
            Duration::from_secs(10),
            run_role_classification_sweep(RoleClassificationSweepRequest {
                runtime: runtime.clone(),
                classifier: RoleClassifier::Structural,
                model: STRUCTURAL_ROLE_MODEL.to_owned(),
                cancellation: ProjectCancellation::new(),
                limit,
                concurrency: 2,
            }),
        )
        .await??;
        assert_eq!(report["candidates"], candidates);
        assert_eq!(report["classified"], candidates);
        assert_eq!(report["truncated"], truncated);
        assert_eq!(report["backendRequests"], 0);
    }
    native.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_after_name_page_read_retries_the_whole_evidence_read() -> TestResult {
    let root = tempfile::tempdir()?;
    let request = StorageRequest::new(Duration::from_secs(30))?;
    std::fs::write(root.path().join("lib.rs"), "pub fn original() {}\n")?;
    let native = RocksProjectRuntime::open(root.path(), &request).await?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &request)
        .await?;
    let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
    let pause = Arc::new(super::super::TracePause {
        stage: "find-name-complete",
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
    let reading = handler.clone();
    let work = tokio::spawn(async move {
        reading
            .call(
                ToolCall {
                    name: "codevetra_find".to_owned(),
                    arguments: serde_json::from_value(json!({"by":"name","query":"replacement"}))?,
                },
                ToolCallContext::local(Duration::from_secs(30)),
            )
            .await
            .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })
    });
    tokio::time::timeout(Duration::from_secs(3), pause.entered.notified()).await?;
    std::fs::write(root.path().join("lib.rs"), "pub fn replacement() {}\n")?;
    native
        .index(codevetra_agent::RocksIndexOptions::default(), &request)
        .await?;
    *handler.handler.session_trace.pause.lock().await = None;
    pause.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), work).await???;
    let evidence = result.structured_content().ok_or("evidence missing")?;
    assert_eq!(evidence["freshness"], "current");
    assert_eq!(evidence["evidence"]["rows"][0]["name"], "replacement");
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn name_lookup_retry_checks_the_actual_returned_generations_live_freshness() -> TestResult {
    for allow_stale in [false, true] {
        let root = tempfile::tempdir()?;
        let request = StorageRequest::new(Duration::from_secs(30))?;
        std::fs::write(root.path().join("lib.rs"), "pub fn original() {}\n")?;
        let native = RocksProjectRuntime::open(root.path(), &request).await?;
        native
            .index(codevetra_agent::RocksIndexOptions::default(), &request)
            .await?;
        let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
        let pause = Arc::new(super::super::TracePause {
            stage: "find-name-lookup",
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
        let reading = handler.clone();
        let work = tokio::spawn(async move {
            reading
                .call(
                    ToolCall {
                        name: "codevetra_find".to_owned(),
                        arguments: serde_json::from_value(
                            json!({"by":"name","query":"replacement","allowStale":allow_stale}),
                        )?,
                    },
                    ToolCallContext::local(Duration::from_secs(30)),
                )
                .await
                .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })
        });
        tokio::time::timeout(Duration::from_secs(3), pause.entered.notified()).await?;
        std::fs::write(root.path().join("lib.rs"), "pub fn replacement() {}\n")?;
        native
            .index(codevetra_agent::RocksIndexOptions::default(), &request)
            .await?;
        // The retry can read G1, but these newer bytes are not in that graph.
        std::fs::write(root.path().join("lib.rs"), "pub fn still_newer() {}\n")?;
        *handler.handler.session_trace.pause.lock().await = None;
        pause.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(5), work).await??;
        if allow_stale {
            let result = result?;
            let evidence = result.structured_content().ok_or("evidence missing")?;
            assert_eq!(evidence["freshness"], "stale");
            assert_eq!(evidence["evidence"]["rows"][0]["name"], "replacement");
        } else {
            assert!(
                result.is_err(),
                "stale replacement generation was labeled current"
            );
        }
        handler.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_wrapper_drains_cancelled_trace_initialization_without_a_late_session()
-> TestResult {
    cancel_trace_stage("initialize").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_wrapper_drains_cancelled_trace_recording_and_keeps_completed_tool_result()
-> TestResult {
    cancel_trace_stage("record").await
}

async fn cancel_trace_stage(stage: &'static str) -> TestResult {
    let root = tempfile::tempdir()?;
    let native =
        RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
    let pause = Arc::new(super::super::TracePause {
        stage,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
    let protocol = ProtocolServer::from_shared(
        ServerConfig::new(
            ServerMetadata::codevetra(),
            ToolProfile::Full,
            ServerLimits::default(),
        ),
        handler.clone(),
    )?;
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let server_task = tokio::spawn(async move { protocol.serve(server_read, server_write).await });
    let (client_read, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":MODERN_PROTOCOL_VERSION,"io.modelcontextprotocol/clientInfo":{"name":"native-lifecycle","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}});
    writer.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":"traced","method":"tools/call","params":{"_meta":meta,"name":"codevetra_playbook","arguments":{}}})).as_bytes()).await?;
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified()).await?;
    writer.write_all(format!("{}\n{}\n",json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"traced","reason":"test cancellation"}}),json!({"jsonrpc":"2.0","id":"ping","method":"ping","params":{"_meta":meta}})).as_bytes()).await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line)).await??;
    let ping: Value = serde_json::from_str(&line)?;
    assert_eq!(
        ping["id"], "ping",
        "native call completed before its admitted work was released"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(25),
            reader.read_line(&mut String::new())
        )
        .await
        .is_err()
    );
    pause.release.notify_one();
    line.clear();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
    let actual: Value = serde_json::from_str(&line)?;
    assert_eq!(actual["id"], "traced");
    if stage == "record" {
        assert_ne!(
            actual["result"]["isError"], true,
            "a completed business result was lost: {actual}"
        );
    } else {
        assert!(
            actual["error"].is_object() || actual["result"]["isError"] == true,
            "cancelled trace initialization started new business work: {actual}"
        );
    }
    let sessions = native
        .database()
        .list_mcp_sessions(native.database().project_id(), 10)
        .await?;
    assert_eq!(sessions.len(), usize::from(stage == "record"));
    for session in sessions {
        assert_eq!(session.tool_count(), 0);
    }
    writer.shutdown().await?;
    tokio::time::timeout(Duration::from_secs(5), server_task).await???;
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_wrapper_cancelled_cold_admission_keeps_cached_project_responsive() -> TestResult
{
    let root = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    std::fs::create_dir(other.path().join(".codevetra"))?;
    let native =
        RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    let handler = Arc::new(RocksHandler::new(native, false)?);
    let opening = handler.handler.project_cache.opening.lock().await;
    let context = ToolCallContext::local(Duration::from_secs(5));
    let cancelled = context.clone();
    let routed = handler.clone();
    let arguments = serde_json::Map::from_iter([(
        "projectPath".to_owned(),
        json!(other.path().to_str().ok_or("other path")?),
    )]);
    let waiter = tokio::spawn(async move {
        routed
            .call(
                ToolCall {
                    name: "codevetra_playbook".to_owned(),
                    arguments,
                },
                cancelled,
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    let cached = tokio::time::timeout(
        Duration::from_secs(1),
        handler.call(
            ToolCall {
                name: "codevetra_playbook".to_owned(),
                arguments: serde_json::Map::new(),
            },
            ToolCallContext::local(Duration::from_secs(5)),
        ),
    )
    .await??;
    assert!(!cached.is_error());
    context.cancellation().cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await??
            .is_err()
    );
    assert!(!other.path().join(".codevetra/data/CURRENT").exists());
    drop(opening);
    let expired = ToolCallContext::local(Duration::from_nanos(1));
    tokio::time::sleep(Duration::from_millis(1)).await;
    let rejected = handler
        .call(
            ToolCall {
                name: "codevetra_playbook".to_owned(),
                arguments: serde_json::Map::from_iter([(
                    "projectPath".to_owned(),
                    json!(other.path().to_str().ok_or("other path")?),
                )]),
            },
            expired,
        )
        .await;
    assert!(rejected.is_err());
    assert!(!other.path().join(".codevetra/data/CURRENT").exists());
    handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_control_replies_bypass_trace_initialization_and_recording() -> TestResult {
    use super::super::{
        AdminAction, AdminJobRequest, ProjectCancellation, ProjectError, TracePause,
        TraceQueryTools,
    };
    for stage in ["initialize", "record"] {
        let root = tempfile::tempdir()?;
        let native =
            RocksProjectRuntime::open(root.path(), &StorageRequest::new(Duration::from_secs(30))?)
                .await?;
        let handler = Arc::new(RocksHandler::new(native.clone(), false)?);
        if stage == "record" {
            assert!(
                TraceQueryTools(&handler.handler)
                    .ensure_trace_session()
                    .await
                    .is_some()
            );
        }
        let pause = Arc::new(TracePause {
            stage,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *handler.handler.session_trace.pause.lock().await = Some(pause.clone());
        let cancellation = ProjectCancellation::new();
        let wait = cancellation.clone();
        let started = handler
            .handler
            .admin_jobs
            .start(AdminJobRequest {
                action: AdminAction::Index,
                cancellation,
                operation: async move {
                    wait.cancelled().await;
                    Err::<Value, ProjectError>(ProjectError::RequestCancelled)
                },
            })
            .await?;
        for (action, expected) in [("status", "running"), ("cancel", "cancelling")] {
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                handler.call(
                    ToolCall {
                        name: "codevetra_admin".to_owned(),
                        arguments: serde_json::from_value(
                            json!({"action":action, "jobId":started.job_id}),
                        )?,
                    },
                    ToolCallContext::local(Duration::from_secs(5)),
                ),
            )
            .await??;
            let value: Value =
                serde_json::from_str(result.primary_text().ok_or("control result missing")?)?;
            assert_eq!(value["status"], expected);
        }
        let sessions = native
            .database()
            .list_mcp_sessions(native.database().project_id(), 10)
            .await?;
        assert_eq!(sessions.len(), usize::from(stage == "record"));
        assert!(sessions.iter().all(|session| session.tool_count() == 0));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), pause.entered.notified())
                .await
                .is_err()
        );
        *handler.handler.session_trace.pause.lock().await = None;
        handler.close().await?;
    }
    Ok(())
}
