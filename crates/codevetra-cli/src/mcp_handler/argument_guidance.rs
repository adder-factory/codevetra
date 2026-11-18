//! Keep numeric and batch limits visible when hosts project schemas to tool signatures.

use serde_json::{Map, Value};
use std::fmt::Write as _;

const BIOMARKER_SELECTION_FIELDS: &[&str] = &[
    "symbol",
    "symbols",
    "biomarker",
    "minSeverity",
    "minCentrality",
    "minMetric",
    "maxMetric",
    "excludeFile",
    "limit",
];

impl super::BiomarkerMode {
    pub(super) fn validate_arguments(
        self,
        arguments: &Map<String, Value>,
    ) -> Result<(), super::ToolError> {
        for &field in BIOMARKER_SELECTION_FIELDS {
            let allowed = match field {
                "symbol" | "symbols" => self == Self::Symbol,
                "biomarker" => matches!(self, Self::Ranked | Self::Coverage),
                _ => self == Self::Ranked,
            };
            if !allowed && arguments.contains_key(field) {
                return Err(super::safe_error(
                    super::ToolErrorCode::InvalidArguments,
                    format!(
                        "{field} is unavailable in mode={}; omit it or choose its supported mode",
                        self.as_str()
                    ),
                ));
            }
        }
        Ok(())
    }
}

pub(super) fn describe_bounds(properties: &mut Map<String, Value>) {
    for property in properties.values_mut() {
        let Some(guidance) = bounds_description(property) else {
            continue;
        };
        let description = property["description"].as_str().unwrap_or_default();
        property["description"] = Value::String(if description.is_empty() {
            guidance
        } else {
            format!("{description} {guidance}")
        });
    }
}

fn bounds_description(property: &Value) -> Option<String> {
    let (label, minimum, maximum) = match property["type"].as_str()? {
        "integer" | "number" => ("Schema range", "minimum", "maximum"),
        "array" => ("Item count", "minItems", "maxItems"),
        _ => return None,
    };
    let maximum = property[maximum].as_number()?;
    let mut description = match property[minimum].as_number() {
        Some(minimum) => format!("{label}: {minimum} to {maximum} (inclusive)."),
        None => format!("{label}: at most {maximum}."),
    };
    if let Some(default) = property["default"].as_number() {
        let _ = write!(description, " Default: {default}.");
    }
    Some(description)
}

const REVIEW_MODE_ARGUMENTS: &[(&str, &[&str])] = &[
    (
        "context",
        &[
            "baseRef",
            "sourceDetail",
            "maxChangedFiles",
            "diff",
            "maxCallersPerSymbol",
            "maxCalleesPerSymbol",
            "maxCoChangeWarnings",
            "minCoChangeJaccard",
            "minDiffMagnitude",
            "pathFilter",
        ],
    ),
    (
        "neighbors",
        &["files", "symbols", "k", "dedupeByName", "modelId"],
    ),
    (
        "risk",
        &[
            "topN",
            "limit",
            "minCentrality",
            "coverageSource",
            "pathFilter",
        ],
    ),
    (
        "agent-audit",
        &["perDetectorLimit", "minSeverity", "pathFilter"],
    ),
    ("numerical", &["topN", "limit", "pathFilter"]),
    ("trust", &["deep", "timeoutMs"]),
];

pub(super) fn review_arguments(mode: &str) -> Option<&'static [&'static str]> {
    REVIEW_MODE_ARGUMENTS
        .iter()
        .find_map(|(name, fields)| (*name == mode).then_some(*fields))
}

pub(super) fn describe_review_modes(properties: &mut Map<String, Value>) {
    for (field, property) in properties {
        let modes = REVIEW_MODE_ARGUMENTS
            .iter()
            .filter_map(|(mode, fields)| fields.contains(&field.as_str()).then_some(*mode))
            .collect::<Vec<_>>();
        if modes.is_empty() {
            continue;
        }
        let guidance = format!(
            "Accepted review modes: {}. Omit in other modes.",
            modes.join(", ")
        );
        let description = property["description"].as_str().unwrap_or_default();
        property["description"] = Value::String(if description.is_empty() {
            guidance
        } else {
            format!("{description} {guidance}")
        });
    }
}
