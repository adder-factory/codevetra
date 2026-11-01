//! Explicit client acknowledgements; no daemon-global source consumption history.

use super::{
    CodeVetraMcpHandler, ToolError, ToolErrorCode, ToolResult, invalid_arguments, safe_error,
};
use codevetra_agent::{FileSourceOptions, FileSourceRequest, ProjectCancellation};
use codevetra_domain::NormalizedPath;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_HANDLES: usize = 64;
const MAX_HANDLE_BYTES: usize = 8192;
const LIFETIME_SECONDS: u64 = 1800;
const MAX_RESULT_BYTES: usize = 1536 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceIdentity {
    project: String,
    generation: String,
    path: String,
    content_hash: String,
    start_byte: u64,
    end_byte: u64,
    text_digest: String,
    precision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Handle {
    version: u8,
    source: SourceIdentity,
    issued_at: u64,
}

pub(super) struct Reuse {
    acknowledged: Vec<(String, Handle)>,
    mode: String,
    now: u64,
    ignored: usize,
}

fn now() -> Result<u64, ToolError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| ToolError::internal())
}

fn decode(value: &str) -> Option<Handle> {
    if value.len() > MAX_HANDLE_BYTES {
        return None;
    }
    let handle: Handle = serde_json::from_str(value.strip_prefix("source-v1:")?).ok()?;
    (handle.version == 1
        && handle.source.precision == "exact_file_bytes"
        && handle.source.end_byte > handle.source.start_byte
        && handle.source.end_byte - handle.source.start_byte <= 256 * 1024
        && NormalizedPath::parse(&handle.source.path).is_ok())
    .then_some(handle)
}

fn encode(handle: &Handle) -> Result<String, ToolError> {
    let text = format!(
        "source-v1:{}",
        serde_json::to_string(handle).map_err(|_| ToolError::internal())?
    );
    if text.len() > MAX_HANDLE_BYTES {
        return Err(invalid_arguments());
    }
    Ok(text)
}

pub(super) fn supported(name: &str) -> bool {
    matches!(
        name,
        super::CONTEXT_TOOL
            | super::EXPLORE_TOOL
            | super::REVIEW_TOOL
            | super::NODE_TOOL
            | super::FILES_TOOL
    )
}

pub(super) fn schema(name: &str, value: &mut Value) {
    if !supported(name) {
        return;
    }
    value["sourceReuse"] = json!({"type":"string","enum":["offer","reuse","full","reset"],"description":"Opt in to exact source handles. reuse requires explicit knownSources held by this client; full/reset ignore acknowledgements. Opt-in responses use JSON with whole ToolResult byte accounting."});
    value["knownSources"] = json!({"type":"array","maxItems":MAX_HANDLES,"items":{"type":"string","minLength":1,"maxLength":MAX_HANDLE_BYTES},"description":"Only acknowledge source handles whose exact text this client still has. No server-side history is retained."});
}

pub(super) fn take(
    name: &str,
    arguments: &mut Map<String, Value>,
) -> Result<Option<Reuse>, ToolError> {
    if !supported(name) {
        return Ok(None);
    }
    let Some(mode) = arguments.remove("sourceReuse") else {
        if arguments.contains_key("knownSources") {
            return Err(invalid_arguments());
        }
        return Ok(None);
    };
    let mode = mode
        .as_str()
        .filter(|value| matches!(*value, "offer" | "reuse" | "full" | "reset"))
        .ok_or_else(invalid_arguments)?
        .to_owned();
    let values = match arguments.remove("knownSources") {
        None => Vec::new(),
        Some(Value::Array(values)) if values.len() <= MAX_HANDLES => values,
        Some(_) => return Err(invalid_arguments()),
    };
    let now = now()?;
    let mut acknowledged = Vec::new();
    let mut ignored = 0;
    for value in values {
        let text = value
            .as_str()
            .filter(|text| !text.is_empty() && text.len() <= MAX_HANDLE_BYTES)
            .ok_or_else(invalid_arguments)?;
        if let Some(handle) = decode(text)
            .filter(|handle| handle.issued_at <= now && now - handle.issued_at <= LIFETIME_SECONDS)
        {
            if mode == "reuse" {
                acknowledged.push((text.to_owned(), handle));
            }
        } else {
            ignored += 1;
        }
    }
    Ok(Some(Reuse {
        acknowledged,
        mode,
        now,
        ignored,
    }))
}

fn identity(value: &Value, project: &str) -> Option<SourceIdentity> {
    if !value.get("fresh")?.as_bool()?
        || value.get("live_source").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let file = value.get("symbol").or_else(|| value.get("file"))?;
    let excerpt = value.get("excerpt")?;
    let text = excerpt.get("text")?.as_str()?;
    let start_byte = excerpt
        .get("start_byte")
        .or_else(|| excerpt.get("startByte"))?
        .as_u64()?;
    let end_byte = excerpt
        .get("end_byte")
        .or_else(|| excerpt.get("endByte"))?
        .as_u64()?;
    if text.is_empty() || end_byte.checked_sub(start_byte)? != text.len() as u64 {
        return None;
    }
    Some(SourceIdentity {
        project: project.to_owned(),
        generation: file.get("generation_id")?.as_str()?.to_owned(),
        path: file.get("path")?.as_str()?.to_owned(),
        content_hash: value
            .get("file_content_hash")
            .or_else(|| file.get("content_hash"))?
            .as_str()?
            .to_owned(),
        start_byte,
        end_byte,
        text_digest: blake3::hash(text.as_bytes()).to_hex().to_string(),
        precision: "exact_file_bytes".to_owned(),
    })
}

#[derive(Default)]
struct Counts {
    offered: usize,
    referenced: usize,
    source_bytes: usize,
    reused_bytes: usize,
}

fn project_sources(
    value: &mut Value,
    project: &str,
    reuse: &Reuse,
    counts: &mut Counts,
    depth: usize,
    cancellation: &ProjectCancellation,
) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err(safe_error(
            ToolErrorCode::Unavailable,
            "source projection cancelled",
        ));
    }
    if depth > 64 {
        return Err(ToolError::internal());
    }
    if let Some(source) = identity(value, project) {
        return project_source(value, source, reuse, counts);
    }
    match value {
        Value::Object(map) => {
            for child in map.values_mut() {
                project_sources(child, project, reuse, counts, depth + 1, cancellation)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                project_sources(child, project, reuse, counts, depth + 1, cancellation)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn project_source(
    value: &mut Value,
    source: SourceIdentity,
    reuse: &Reuse,
    counts: &mut Counts,
) -> Result<(), ToolError> {
    let bytes =
        usize::try_from(source.end_byte - source.start_byte).map_err(|_| ToolError::internal())?;
    let known = reuse
        .acknowledged
        .iter()
        .find(|(_, handle)| handle.source == source);
    let handle = if let Some((text, _)) = known {
        text.clone()
    } else {
        encode(&Handle {
            version: 1,
            source,
            issued_at: reuse.now,
        })?
    };
    if counts.offered >= MAX_HANDLES {
        counts.source_bytes += bytes;
        return Ok(());
    }
    counts.offered += 1;
    value["sourceHandle"] = Value::String(handle.clone());
    if known.is_some() {
        let before = serde_json::to_vec(value)
            .map_err(|_| ToolError::internal())?
            .len();
        let old_excerpt = value["excerpt"].clone();
        value["excerpt"]
            .as_object_mut()
            .ok_or_else(ToolError::internal)?
            .remove("text");
        value["sourceReference"] = json!({"handle":handle,"state":"acknowledged_exact_source","expand":"files --format read --source-handle <handle>; sourceReuse=full/reset always emits source"});
        if serde_json::to_vec(value)
            .map_err(|_| ToolError::internal())?
            .len()
            < before
        {
            counts.referenced += 1;
            counts.reused_bytes += bytes;
            return Ok(());
        }
        value["excerpt"] = old_excerpt;
        value
            .as_object_mut()
            .ok_or_else(ToolError::internal)?
            .remove("sourceReference");
    }
    counts.source_bytes += bytes;
    Ok(())
}

fn result(value: &Value) -> Result<ToolResult, ToolError> {
    super::json_result_with_compaction(value, true)
}

pub(super) fn apply(
    original: ToolResult,
    project: &str,
    reuse: Reuse,
    cancellation: &ProjectCancellation,
) -> Result<ToolResult, ToolError> {
    let Some(structured) = original.structured_content() else {
        return Ok(original);
    };
    let baseline = serde_json::to_vec(&original)
        .map_err(|_| ToolError::internal())?
        .len();
    let mut payload = Value::Object(structured.clone());
    let mut counts = Counts::default();
    project_sources(&mut payload, project, &reuse, &mut counts, 0, cancellation)?;
    payload["sourceReuse"] = json!({"mode":Value::String(reuse.mode),"handlesOffered":counts.offered,"references":counts.referenced,"newSourceBytes":counts.source_bytes,"reusedSourceBytes":counts.reused_bytes,"ignoredExpiredOrUnknownHandles":reuse.ignored,"retainedServerHistory":0,"handleLifetimeSeconds":LIFETIME_SECONDS,"baselineResultBytes":baseline,"emittedResultBytes":0,"totalResponseBytesSaved":0,"totalResponseBytesAdded":0,"accounting":"JSON serialized MCP ToolResult including text and structured content; excludes transport envelope. References assert client possession, never inspection or safety."});
    for _ in 0..8 {
        let output = result(&payload)?;
        let bytes = serde_json::to_vec(&output)
            .map_err(|_| ToolError::internal())?
            .len();
        if bytes > MAX_RESULT_BYTES {
            return Err(safe_error(
                ToolErrorCode::Unavailable,
                "source reuse result exceeds bounded output; request fewer symbols or a smaller source window",
            ));
        }
        if payload["sourceReuse"]["emittedResultBytes"].as_u64() == Some(bytes as u64) {
            return Ok(output);
        }
        payload["sourceReuse"]["emittedResultBytes"] = json!(bytes);
        payload["sourceReuse"]["totalResponseBytesSaved"] = json!(baseline.saturating_sub(bytes));
        payload["sourceReuse"]["totalResponseBytesAdded"] = json!(bytes.saturating_sub(baseline));
    }
    Err(ToolError::internal())
}

pub(super) async fn recover(
    handler: &CodeVetraMcpHandler,
    arguments: &Map<String, Value>,
    cancellation: ProjectCancellation,
) -> Result<ToolResult, ToolError> {
    super::reject_present(
        arguments,
        &[
            "file",
            "byteOffset",
            "byteLimit",
            "lineOffset",
            "lineLimit",
            "allowStale",
        ],
    )?;
    let token = super::required_bounded_text(arguments, "sourceHandle", MAX_HANDLE_BYTES)?;
    let handle = decode(token).ok_or_else(|| {
        safe_error(
            ToolErrorCode::InvalidArguments,
            "unknown source handle; recover using files --format read --file <path>",
        )
    })?;
    if handle.source.project != handler.runtime.root_identity() {
        return Err(invalid_arguments());
    }
    // Expiry prevents reuse; explicit recovery may renew an expired handle only
    // after the complete current source/hash/range identity has been revalidated.
    let source = handler
        .runtime
        .file_source_with_cancellation(
            FileSourceRequest::new(
                NormalizedPath::parse(&handle.source.path).map_err(|_| invalid_arguments())?,
                FileSourceOptions::bytes(
                    handle.source.start_byte,
                    u32::try_from(handle.source.end_byte - handle.source.start_byte)
                        .map_err(|_| invalid_arguments())?,
                )
                .map_err(|_| invalid_arguments())?,
            ),
            cancellation.clone(),
        )
        .await
        .map_err(|error| super::project_operation_error(&error))?;
    let mut payload = json!({"format":"read","source":source});
    if identity(&payload["source"], handler.runtime.root_identity()).as_ref()
        != Some(&handle.source)
    {
        return Err(safe_error(
            ToolErrorCode::Unavailable,
            "source handle is stale or its generation was replaced; recover current source with files --format read --file <handle.source.path>",
        ));
    }
    payload["recovery"] = json!({"state":"revalidated_full_source","previousHandleExpired":now()?.saturating_sub(handle.issued_at) > LIFETIME_SECONDS});
    apply(
        result(&payload)?,
        handler.runtime.root_identity(),
        Reuse {
            acknowledged: Vec::new(),
            mode: "full".to_owned(),
            now: now()?,
            ignored: 0,
        },
        &cancellation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    fn source(text: &str, generation: &str, start: u64) -> Value {
        json!({"fresh":true,"file":{"generation_id":generation,"path":"src/a.ts","content_hash":"hash"},"excerpt":{"startByte":start,"endByte":start + text.len() as u64,"text":text}})
    }

    fn options(mode: &str, known: Vec<String>) -> Result<Reuse, ToolError> {
        take(
            super::super::FILES_TOOL,
            &mut Map::from_iter([
                ("sourceReuse".to_owned(), json!(mode)),
                (
                    "knownSources".to_owned(),
                    Value::Array(known.into_iter().map(Value::String).collect()),
                ),
            ]),
        )?
        .ok_or_else(ToolError::internal)
    }

    #[test]
    fn source_reuse_is_explicit_exact_and_accounts_for_actual_wire_bytes() -> TestResult {
        let text = "a\\\"😀".repeat(2000);
        let original = result(&json!({"source":source(&text,"g1",0)}))?;
        let offered = apply(
            original.clone(),
            "project",
            options("offer", vec![])?,
            &ProjectCancellation::new(),
        )?;
        let handle = offered.structured_content().ok_or("payload")?["source"]["sourceHandle"]
            .as_str()
            .ok_or("handle")?
            .to_owned();
        let reused = apply(
            original.clone(),
            "project",
            options("reuse", vec![handle.clone()])?,
            &ProjectCancellation::new(),
        )?;
        let payload = reused.structured_content().ok_or("payload")?;
        assert!(payload["source"]["excerpt"].get("text").is_none());
        assert_eq!(payload["sourceReuse"]["references"], 1);
        assert_eq!(
            payload["sourceReuse"]["emittedResultBytes"],
            serde_json::to_vec(&reused)?.len()
        );
        let emitted = serde_json::to_vec(&reused)?.len();
        let baseline = serde_json::to_vec(&original)?.len();
        assert_eq!(
            payload["sourceReuse"]["totalResponseBytesSaved"],
            baseline - emitted
        );
        for (project, mode, handles) in [
            ("project", "reuse", vec![]),
            ("other", "reuse", vec![handle.clone()]),
            ("project", "full", vec![handle.clone()]),
            ("project", "reset", vec![handle.clone()]),
        ] {
            let full = apply(
                original.clone(),
                project,
                options(mode, handles)?,
                &ProjectCancellation::new(),
            )?;
            assert_eq!(
                full.structured_content().ok_or("payload")?["source"]["excerpt"]["text"],
                text
            );
        }
        for changed in [
            source(&text, "g2", 0),
            source(&text, "g1", 1),
            source("new", "g1", 0),
        ] {
            let full = apply(
                result(&json!({"source":changed}))?,
                "project",
                options("reuse", vec![handle.clone()])?,
                &ProjectCancellation::new(),
            )?;
            assert_eq!(
                full.structured_content().ok_or("payload")?["sourceReuse"]["references"],
                0
            );
        }
        let mut expired = decode(&handle).ok_or("decode")?;
        expired.issued_at = 0;
        let options = options("reuse", vec![encode(&expired)?, "unknown".to_owned()])?;
        assert_eq!(options.ignored, 2);
        assert!(options.acknowledged.is_empty());
        Ok(())
    }

    #[test]
    fn source_reuse_bounds_cancellation_and_projection_are_fail_closed() -> TestResult {
        assert!(options("reuse", vec!["unknown".to_owned(); 65]).is_err());
        assert!(options("reuse", vec!["x".repeat(8193)]).is_err());
        let token = ProjectCancellation::new();
        token.cancel();
        assert!(
            apply(
                result(&source("body", "g", 0))?,
                "p",
                options("offer", vec![])?,
                &token
            )
            .is_err()
        );
        let payload = json!({"source":source(&"x".repeat(MAX_RESULT_BYTES),"g",0)});
        assert!(
            apply(
                result(&payload)?,
                "p",
                options("offer", vec![])?,
                &ProjectCancellation::new()
            )
            .is_err()
        );
        // Rejected response state cannot poison the next call: there is no retained history.
        let next = apply(
            result(&source("body", "g", 0))?,
            "p",
            options("reuse", vec![])?,
            &ProjectCancellation::new(),
        )?;
        assert_eq!(
            next.structured_content().ok_or("payload")?["excerpt"]["text"],
            "body"
        );
        Ok(())
    }
}
