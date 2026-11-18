//! One navigation schema drives the equivalent MCP and generated CLI operation.

use std::fmt::Write as _;

use codevetra_agent::{PositionAnchor, PositionOperation, PositionRequest, ProjectCancellation};
use codevetra_domain::{NormalizedPath, SymbolId};
use codevetra_mcp::{ToolAnnotations, ToolContractError, ToolDefinition, ToolError, ToolResult};
use serde_json::{Map, Value, json};

use super::{
    CodeVetraMcpHandler, NAVIGATE_TOOL, ReadDefinition, invalid_arguments, json_result,
    optional_text, project_operation_error, read_definition, reject_present, reject_unknown,
    required_bounded_text,
};

const SYMBOL_ID_BYTES: usize = 64;
const PATH_BYTES: usize = 4096;
const CONTINUATION_BYTES: usize = 2048;
const DEFAULT_LIMIT: u32 = 50;
const MAXIMUM_LIMIT: u16 = 100;
const DEFAULT_TRACE_STEPS: u32 = 128;
const MAXIMUM_TRACE_STEPS: u32 = 512;
const DEFAULT_TRACE_DEPTH: u32 = 2;
const MAXIMUM_TRACE_DEPTH: u32 = 4;
const DEFAULT_TRACE_BYTES: u32 = 128 * 1024;
const MINIMUM_TRACE_BYTES: u32 = 16 * 1024;
const MAXIMUM_TRACE_BYTES: u32 = 512 * 1024;
const SUMMARY_STEPS: usize = 32;

pub(super) fn definition(
    annotations: ToolAnnotations,
) -> Result<ToolDefinition, ToolContractError> {
    read_definition(ReadDefinition {
        name: NAVIGATE_TOOL,
        description: "Navigate exact cursor occurrences to definitions, references by resolved symbol identity, or evidenced implementations/overrides. Uses one-based lines, zero-based UTF-8 byte columns and half-open byte ranges. Ambiguous, coarse, unsupported and stale evidence remain explicit; reference continuations never mix generations. trace returns bounded possible static execution paths for JS/TS with nested source steps, exact call evidence and explicit unknowns.",
        schema: json!({
            "operation":{"type":"string","enum":["definition","references","implementation","trace"]},
            "focusSymbolId":{"type":"string","minLength":1,"maxLength":SYMBOL_ID_BYTES,"description":"trace only: annotate an exact target/changed symbol without hiding guards."},
            "maxSteps":{"type":"integer","minimum":1,"maximum":MAXIMUM_TRACE_STEPS},
            "maxDepth":{"type":"integer","minimum":0,"maximum":MAXIMUM_TRACE_DEPTH},
            "maxBytes":{"type":"integer","minimum":MINIMUM_TRACE_BYTES,"maximum":MAXIMUM_TRACE_BYTES},
            "symbolId":{"type":"string","minLength":1,"maxLength":SYMBOL_ID_BYTES},
            "file":{"type":"string","minLength":1,"maxLength":PATH_BYTES},
            "line":{"type":"integer","minimum":1,"maximum":u32::MAX},
            "column":{"type":"integer","minimum":0,"maximum":u32::MAX},
            "limit":{"type":"integer","minimum":1,"maximum":MAXIMUM_LIMIT},
            "after":{"type":"string","minLength":1,"maxLength":CONTINUATION_BYTES}
        }),
        required: &["operation"],
        annotations,
    })
}

pub(super) async fn navigate(
    handler: &CodeVetraMcpHandler,
    arguments: Map<String, Value>,
    cancellation: ProjectCancellation,
) -> Result<ToolResult, ToolError> {
    if arguments.get("operation").and_then(Value::as_str) == Some("trace") {
        return static_trace(handler, &arguments, cancellation).await;
    }
    let request = parse(&arguments)?;
    let result = handler
        .runtime
        .navigate_position(request, cancellation)
        .await
        .map_err(|error| project_operation_error(&error))?;
    json_result(&result)
}

fn parse(arguments: &Map<String, Value>) -> Result<PositionRequest, ToolError> {
    reject_unknown(
        arguments,
        &[
            "operation",
            "symbolId",
            "file",
            "line",
            "column",
            "limit",
            "after",
        ],
    )?;
    let operation = match optional_text(arguments, "operation")? {
        Some("definition") => PositionOperation::Definition,
        Some("references") => PositionOperation::References,
        Some("implementation") => PositionOperation::Implementation,
        _ => return Err(invalid_arguments()),
    };
    let anchor = if let Some(symbol) = optional_text(arguments, "symbolId")? {
        reject_present(arguments, &["file", "line", "column"])?;
        PositionAnchor::Symbol(SymbolId::parse(symbol).map_err(|_| invalid_arguments())?)
    } else {
        let path = NormalizedPath::parse(required_bounded_text(arguments, "file", PATH_BYTES)?)
            .map_err(|_| invalid_arguments())?;
        PositionAnchor::Cursor {
            path,
            line: number(arguments, "line", None)?,
            column: number(arguments, "column", None)?,
        }
    };
    let limit = u16::try_from(number(arguments, "limit", Some(DEFAULT_LIMIT))?)
        .map_err(|_| invalid_arguments())?;
    let after = optional_text(arguments, "after")?.map(str::to_owned);
    if !(1..=MAXIMUM_LIMIT).contains(&limit)
        || !valid_continuation(after.as_deref(), operation)
        || matches!(anchor, PositionAnchor::Cursor { line: 0, .. })
    {
        return Err(invalid_arguments());
    }
    Ok(PositionRequest {
        operation,
        anchor,
        limit,
        after,
    })
}

fn valid_continuation(after: Option<&str>, operation: PositionOperation) -> bool {
    after.is_none_or(|value| {
        operation == PositionOperation::References
            && !value.is_empty()
            && value.len() <= CONTINUATION_BYTES
    })
}

fn number(
    arguments: &Map<String, Value>,
    name: &str,
    default: Option<u32>,
) -> Result<u32, ToolError> {
    match arguments.get(name) {
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(invalid_arguments),
        None => default.ok_or_else(invalid_arguments),
    }
}

async fn static_trace(
    handler: &CodeVetraMcpHandler,
    arguments: &Map<String, Value>,
    cancellation: ProjectCancellation,
) -> Result<ToolResult, ToolError> {
    reject_unknown(
        arguments,
        &[
            "operation",
            "symbolId",
            "focusSymbolId",
            "maxSteps",
            "maxDepth",
            "maxBytes",
        ],
    )?;
    let symbol_id = SymbolId::parse(required_bounded_text(
        arguments,
        "symbolId",
        SYMBOL_ID_BYTES,
    )?)
    .map_err(|_| invalid_arguments())?;
    let focus = optional_text(arguments, "focusSymbolId")?
        .map(SymbolId::parse)
        .transpose()
        .map_err(|_| invalid_arguments())?;
    let report = handler
        .runtime
        .static_trace(
            codevetra_agent::StaticTraceRequest {
                symbol_id,
                focus,
                maximum_steps: usize::try_from(number(
                    arguments,
                    "maxSteps",
                    Some(DEFAULT_TRACE_STEPS),
                )?)
                .map_err(|_| invalid_arguments())?,
                maximum_depth: usize::try_from(number(
                    arguments,
                    "maxDepth",
                    Some(DEFAULT_TRACE_DEPTH),
                )?)
                .map_err(|_| invalid_arguments())?,
                maximum_bytes: usize::try_from(number(
                    arguments,
                    "maxBytes",
                    Some(DEFAULT_TRACE_BYTES),
                )?)
                .map_err(|_| invalid_arguments())?,
            },
            cancellation,
        )
        .await
        .map_err(|error| project_operation_error(&error))?;
    let mut summary = format!(
        "Possible static execution paths: {} definitions, {} steps; partial={}; fresh={}. Nested parent indices and call links preserve branches and reuse/recursion boundaries. No runtime execution, feasibility or concurrent order is proved.",
        report.functions.len(),
        report
            .functions
            .iter()
            .map(|function| function.syntax.steps.len())
            .sum::<usize>(),
        report.partial,
        report.fresh
    );
    let mut shown = 0;
    for function in &report.functions {
        for step in &function.syntax.steps {
            if shown == SUMMARY_STEPS {
                break;
            }
            let _written = write!(
                summary,
                "\n{}:{} step={} parent={:?} {} [{}]",
                function.symbol.path().as_str(),
                step.span.start_line,
                step.id,
                step.parent,
                step.kind,
                step.relation
            );
            shown += 1;
        }
    }
    summary.push_str("\nThe structured report contains exact spans, bounded source previews, call provenance, coverage and omitted-evidence reasons.");
    super::text_json_result(&report, summary)
}
