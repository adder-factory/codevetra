//! One argument contract for admin parsing and its advertised action schemas.

use super::{
    ADMIN_BIOMARKER_REFRESH_MAXIMUM_TIMEOUT_MS, ADMIN_MAXIMUM_WORKERS,
    ADMIN_QUERY_TIMEOUT_MAXIMUM_MS, NumericBounds, ROLE_CLASSIFICATION_MAXIMUM_LIMIT,
    StringArrayBounds, invalid_arguments, optional_integer, optional_text, reject_admin_extras,
};
use codevetra_agent::{ScipExportRequest, ScipImportLimits, ScipImportRequest};
use codevetra_mcp::ToolError;
use serde_json::{Map, Value, json};
use std::time::Duration;

pub(super) const SCIP_EXPORT_ROWS: NumericBounds<u64> = NumericBounds::new(1, 5_000_000, 5_000_000);
pub(super) const SCIP_IMPORT_ROWS: NumericBounds<u64> =
    NumericBounds::new(1, 10_000_000, 10_000_000);
pub(super) const SCIP_IMPORT_BYTES: NumericBounds<u64> =
    NumericBounds::new(1, 256 * 1024 * 1024, 256 * 1024 * 1024);
pub(super) const SCIP_EXPORT_TIMEOUT: NumericBounds<u64> =
    NumericBounds::new(100, 120_000, 30 * 60 * 1_000);
pub(super) const PRUNE_KEEP: NumericBounds<u8> = NumericBounds::new(0, 2, 32);
pub(super) const PRUNE_GENERATIONS: NumericBounds<u16> = NumericBounds::new(1, 4, 100);
pub(super) const PRUNE_VERSIONS: NumericBounds<u32> = NumericBounds::new(1, 10_000, 10_000);
pub(super) const MAXIMUM_FILTER_BYTES: usize = 4_096;
pub(super) const EXCLUDES: StringArrayBounds = StringArrayBounds::new(4_096, 4_096);

pub(super) fn scip_export(arguments: &Map<String, Value>) -> Result<ScipExportRequest, ToolError> {
    reject_admin_extras(arguments, &["out", "maximumRows", "timeoutMs"])?;
    let output = optional_text(arguments, "out")?.unwrap_or("index.scip");
    let rows = optional_integer(arguments, "maximumRows", SCIP_EXPORT_ROWS)?;
    let timeout = optional_integer(arguments, "timeoutMs", SCIP_EXPORT_TIMEOUT)?;
    ScipExportRequest::new(output, rows)
        .and_then(|request| request.with_timeout(Duration::from_millis(timeout)))
        .map_err(|_| invalid_arguments())
}

pub(super) fn scip_import(arguments: &Map<String, Value>) -> Result<ScipImportRequest, ToolError> {
    reject_admin_extras(
        arguments,
        &["in", "maximumRows", "maximumSourceBytes", "workers"],
    )?;
    let input = optional_text(arguments, "in")?.unwrap_or("index.scip");
    let rows = optional_integer(arguments, "maximumRows", SCIP_IMPORT_ROWS)?;
    let bytes = optional_integer(arguments, "maximumSourceBytes", SCIP_IMPORT_BYTES)?;
    let workers = optional_integer(
        arguments,
        "workers",
        NumericBounds::new(1, ADMIN_MAXIMUM_WORKERS, ADMIN_MAXIMUM_WORKERS),
    )?;
    let bytes = usize::try_from(bytes).map_err(|_| invalid_arguments())?;
    let rows = usize::try_from(rows).map_err(|_| invalid_arguments())?;
    let limits = ScipImportLimits::new(bytes, rows, workers).map_err(|_| invalid_arguments())?;
    ScipImportRequest::new(input, limits).map_err(|_| invalid_arguments())
}

pub(super) fn constraints() -> Value {
    json!([
        {
            "if": {"properties": {"action": {"const": "graph-export"}}, "required": ["action"]},
            "then": {"properties": {"limit": {"type": "integer", "minimum": 1, "maximum": crate::graph_export::MAXIMUM_NODE_LIMIT}}}
        },
        {
            "if": {"properties": {"action": {"const": "classify"}}, "required": ["action"]},
            "then": {"properties": {"limit": {"type": "integer", "minimum": 1, "maximum": ROLE_CLASSIFICATION_MAXIMUM_LIMIT}}}
        },
        {
            "if": {"properties": {"action": {"not": {"enum": ["graph-export", "classify", "summarize"]}}}, "required": ["action"]},
            "then": {"not": {"required": ["limit"]}}
        },
        {
            "if": {"properties": {"action": {"const": "scip-export"}}, "required": ["action"]},
            "then": {"properties": {"maximumRows": SCIP_EXPORT_ROWS.schema(), "timeoutMs": SCIP_EXPORT_TIMEOUT.schema()}}
        },
        {
            "if": {"properties": {"action": {"const": "scip-import"}}, "required": ["action"]},
            "then": {"properties": {"maximumRows": SCIP_IMPORT_ROWS.schema(), "maximumSourceBytes": SCIP_IMPORT_BYTES.schema()}}
        },
        {
            "if": {"properties": {"action": {"const": "biomarkers-refresh"}}, "required": ["action"]},
            "then": {
                "properties": {"timeoutMs": {"type":"integer", "minimum":1, "maximum": ADMIN_BIOMARKER_REFRESH_MAXIMUM_TIMEOUT_MS}},
                "not": {"required": ["databaseQueryTimeoutMs", "timeoutMs"]}
            }
        },
        {
            "if": {"properties": {"action": {"const": "llm-apply"}}, "required": ["action"]},
            "then": {"properties": {"timeoutMs": {"type":"integer", "minimum":1, "maximum": ADMIN_QUERY_TIMEOUT_MAXIMUM_MS}}}
        },
        {
            "if": {"properties": {"action": {"not": {"enum": ["biomarkers-refresh", "scip-export", "llm-apply"]}}}, "required": ["action"]},
            "then": {"not": {"required": ["timeoutMs"]}}
        },
        {
            "if": {"properties": {"action": {"not": {"enum": ["scip-export", "scip-import"]}}}, "required": ["action"]},
            "then": {"not": {"required": ["maximumRows"]}}
        },
        {
            "if": {"properties": {"action": {"not": {"const": "scip-import"}}}, "required": ["action"]},
            "then": {"not": {"required": ["maximumSourceBytes"]}}
        }
    ])
}
