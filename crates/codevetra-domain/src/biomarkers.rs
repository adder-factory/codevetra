//! Source-local detector policy shared by native findings and immutable comparisons.

use serde::{Deserialize, Serialize};

mod capabilities;
pub use capabilities::{
    DetectorCapability, DetectorCoverage, DetectorCoverageState, DetectorEvidence, DetectorInput,
    DetectorLanguages, DetectorScope, detector_capabilities, detector_capability,
};

/// Fences persisted findings when source-local formulas or eligibility change.
pub const SOURCE_RULE_CONTRACT: &str = "source_rules_v4";

/// A metric extracted from one callable, independent of a storage adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceHealthMetric {
    /// The `code_lines` source metric.
    CodeLines,
    /// The `cyclomatic` source metric.
    Cyclomatic,
    /// The `max_nesting` source metric.
    MaxNesting,
    /// The `max_conditional_operands` source metric.
    MaxConditionalOperands,
    /// The `parameter_count` source metric.
    ParameterCount,
    /// The `empty_body` source metric.
    EmptyBody,
    /// The `magic_numbers` source metric.
    MagicNumbers,
    /// The `hardcoded_urls` source metric.
    HardcodedUrls,
    /// The `accidental_quadratic` source metric.
    AccidentalQuadratic,
    /// The `empty_catches` source metric.
    EmptyCatches,
    /// The `sync_io_in_async` source metric.
    SyncIoInAsync,
    /// The `sequential_await_loops` source metric.
    SequentialAwaitLoops,
    /// The `ts_any_casts` source metric.
    TsAnyCasts,
    /// The `ts_suppressions` source metric.
    TsSuppressions,
    /// The `debug_logs` source metric.
    DebugLogs,
    /// The `incomplete_markers` source metric.
    IncompleteMarkers,
    /// The `dynamic_eval` source metric.
    DynamicEval,
    /// The `insecure_hash` source metric.
    InsecureHash,
    /// The `insecure_random` source metric.
    InsecureRandom,
    /// The `http_without_timeout` source metric.
    HttpWithoutTimeout,
    /// The `sql_string_concatenation` source metric.
    SqlStringConcatenation,
    /// The `unsafe_json_parse` source metric.
    UnsafeJsonParse,
    /// The `unvalidated_env` source metric.
    UnvalidatedEnv,
    /// The `secrets_score` source metric.
    SecretsScore,
    /// The `brain_risk` source metric.
    BrainRisk,
}

impl SourceHealthMetric {
    const fn unit(self) -> &'static str {
        match self {
            Self::CodeLines => "lines",
            Self::Cyclomatic => "cyclomatic",
            Self::MaxNesting => "max_nesting",
            Self::MaxConditionalOperands => "conditional_operands",
            Self::ParameterCount => "parameters",
            Self::SecretsScore => "confidence_percent",
            Self::BrainRisk => "composite_risk",
            Self::EmptyBody
            | Self::MagicNumbers
            | Self::HardcodedUrls
            | Self::AccidentalQuadratic
            | Self::EmptyCatches
            | Self::SyncIoInAsync
            | Self::SequentialAwaitLoops
            | Self::TsAnyCasts
            | Self::TsSuppressions
            | Self::DebugLogs
            | Self::IncompleteMarkers
            | Self::DynamicEval
            | Self::InsecureHash
            | Self::InsecureRandom
            | Self::HttpWithoutTimeout
            | Self::SqlStringConcatenation
            | Self::UnsafeJsonParse
            | Self::UnvalidatedEnv => "occurrences",
        }
    }

    /// Canonical metadata field; composite risk is calculated by the evaluator.
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::CodeLines => "code_lines",
            Self::Cyclomatic => "cyclomatic",
            Self::MaxNesting => "max_nesting",
            Self::MaxConditionalOperands => "max_conditional_operands",
            Self::ParameterCount => "parameter_count",
            Self::EmptyBody => "empty_body",
            Self::MagicNumbers => "magic_numbers",
            Self::HardcodedUrls => "hardcoded_urls",
            Self::AccidentalQuadratic => "accidental_quadratic",
            Self::EmptyCatches => "empty_catches",
            Self::SyncIoInAsync => "sync_io_in_async",
            Self::SequentialAwaitLoops => "sequential_await_loops",
            Self::TsAnyCasts => "ts_any_casts",
            Self::TsSuppressions => "ts_suppressions",
            Self::DebugLogs => "debug_logs",
            Self::IncompleteMarkers => "incomplete_markers",
            Self::DynamicEval => "dynamic_eval",
            Self::InsecureHash => "insecure_hash",
            Self::InsecureRandom => "insecure_random",
            Self::HttpWithoutTimeout => "http_without_timeout",
            Self::SqlStringConcatenation => "sql_string_concatenation",
            Self::UnsafeJsonParse => "unsafe_json_parse",
            Self::UnvalidatedEnv => "unvalidated_env",
            Self::SecretsScore => "secrets_score",
            Self::BrainRisk => "brain_risk",
        }
    }
}

/// Increasing metrics represent increasing risk for every source-local rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskDirection {
    /// A larger value is worse; equal values carry unchanged risk.
    Higher,
}

/// Authoritative source-local formula, severity and eligibility policy.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRule {
    /// Stable public detector identifier.
    pub name: &'static str,
    /// Public metric unit.
    pub metric_name: &'static str,
    /// Source metric or composite formula selector.
    pub metric: SourceHealthMetric,
    /// Inclusive info, warning and error thresholds.
    pub thresholds: [f64; 3],
    /// Minimum owned code lines for maintainability noise control only.
    pub minimum_code_lines: u32,
    /// Meaning of a metric change in finding deltas.
    pub risk_direction: RiskDirection,
}

impl SourceRule {
    const fn new(name: &'static str, metric: SourceHealthMetric, thresholds: [f64; 3]) -> Self {
        Self {
            name,
            metric_name: metric.unit(),
            metric,
            thresholds,
            minimum_code_lines: 0,
            risk_direction: RiskDirection::Higher,
        }
    }

    const fn with_minimum_lines(mut self, minimum_code_lines: u32) -> Self {
        self.minimum_code_lines = minimum_code_lines;
        self
    }
}

/// A privacy-safe operation location in file coordinates, with a half-open byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthOperationSite {
    /// Exact metric to which this occurrence contributes.
    pub metric: SourceHealthMetric,
    /// One-based lines, zero-based UTF-8 byte columns, half-open byte offsets.
    pub span: crate::SourceSpan,
}

/// All shared source-local rules. Graph, history and coverage rules remain separate.
pub const SOURCE_RULES: &[SourceRule] = &[
    SourceRule::new(
        "large_method",
        SourceHealthMetric::CodeLines,
        [100.0, 100.0, 200.0],
    ),
    SourceRule::new(
        "complex_method",
        SourceHealthMetric::Cyclomatic,
        [15.0, 15.0, 25.0],
    ),
    SourceRule::new(
        "nested_complexity",
        SourceHealthMetric::MaxNesting,
        [5.0, 5.0, 7.0],
    ),
    SourceRule::new(
        "complex_conditional",
        SourceHealthMetric::MaxConditionalOperands,
        [6.0, 6.0, 8.0],
    ),
    SourceRule::new(
        "long_parameter_list",
        SourceHealthMetric::ParameterCount,
        [4.0, 5.0, 7.0],
    ),
    SourceRule::new(
        "empty_function_body",
        SourceHealthMetric::EmptyBody,
        [1.0, 999.0, 999.0],
    ),
    SourceRule::new(
        "magic_number",
        SourceHealthMetric::MagicNumbers,
        [3.0, 5.0, 8.0],
    )
    .with_minimum_lines(5),
    SourceRule::new(
        "hardcoded_url",
        SourceHealthMetric::HardcodedUrls,
        [1.0, 2.0, 3.0],
    ),
    SourceRule::new(
        "accidental_quadratic",
        SourceHealthMetric::AccidentalQuadratic,
        [1.0, 1.0, 5.0],
    ),
    SourceRule::new(
        "empty_catch",
        SourceHealthMetric::EmptyCatches,
        [1.0, 1.0, 3.0],
    ),
    SourceRule::new(
        "sync_io_in_async",
        SourceHealthMetric::SyncIoInAsync,
        [1.0, 1.0, 3.0],
    ),
    SourceRule::new(
        "forof_await",
        SourceHealthMetric::SequentialAwaitLoops,
        [1.0, 2.0, 5.0],
    ),
    SourceRule::new(
        "ts_any_cast",
        SourceHealthMetric::TsAnyCasts,
        [2.0, 3.0, 5.0],
    ),
    SourceRule::new(
        "ts_ignore_suppression",
        SourceHealthMetric::TsSuppressions,
        [1.0, 1.0, 3.0],
    ),
    SourceRule::new(
        "agent_debug_log",
        SourceHealthMetric::DebugLogs,
        [1.0, 1.0, 5.0],
    ),
    SourceRule::new(
        "incomplete_marker",
        SourceHealthMetric::IncompleteMarkers,
        [1.0, 5.0, 50.0],
    ),
    SourceRule::new(
        "dynamic_eval",
        SourceHealthMetric::DynamicEval,
        [1.0, 1.0, 1.0],
    ),
    SourceRule::new(
        "insecure_hash",
        SourceHealthMetric::InsecureHash,
        [1.0, 1.0, 3.0],
    ),
    SourceRule::new(
        "random_for_security",
        SourceHealthMetric::InsecureRandom,
        [1.0, 1.0, 1.0],
    ),
    SourceRule::new(
        "http_no_timeout",
        SourceHealthMetric::HttpWithoutTimeout,
        [1.0, 1.0, 3.0],
    ),
    SourceRule::new(
        "sql_string_concat",
        SourceHealthMetric::SqlStringConcatenation,
        [1.0, 1.0, 2.0],
    ),
    SourceRule::new(
        "unsafe_json_parse",
        SourceHealthMetric::UnsafeJsonParse,
        [1.0, 1.0, 3.0],
    ),
    SourceRule::new(
        "env_no_validation",
        SourceHealthMetric::UnvalidatedEnv,
        [1.0, 1.0, 3.0],
    ),
    SourceRule::new(
        "secrets_handling",
        SourceHealthMetric::SecretsScore,
        [50.0, 50.0, 70.0],
    ),
    SourceRule::new(
        "brain_method",
        SourceHealthMetric::BrainRisk,
        [5.0, 10.0, 20.0],
    ),
];

/// Scope eligibility is explicit, including intentionally unevaluated populations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRuleScope {
    /// Executable production function, method or component.
    Callable,
    /// A declaration signature has no executable body.
    DeclarationOnly,
    /// Conventional test/fixture or inline test ownership is excluded.
    Test,
    /// Top-level statements and other symbol kinds have no callable analysis.
    Unsupported,
}
impl SourceRuleScope {
    /// Classify scope without conflating excluded declarations with clean code.
    #[must_use]
    pub fn from_declaration(kind: &str, declaration_only: bool, test: bool) -> Self {
        if test {
            Self::Test
        } else if declaration_only {
            Self::DeclarationOnly
        } else if matches!(kind, "function" | "method" | "component") {
            Self::Callable
        } else {
            Self::Unsupported
        }
    }
}

/// Context affecting eligibility, supplied by each typed adapter.
#[derive(Clone, Copy, Debug)]
pub struct SourceRuleContext {
    /// Actual metrics producer; absence never means a completed clean evaluation.
    pub analyzed_language: Option<crate::SourceLanguage>,
    /// Source population and its exclusion reason.
    pub scope: SourceRuleScope,
    /// Framework-owned signatures are exempt from parameter-count policy.
    pub framework_owned: bool,
    /// Secret evidence is actionable rather than a name-only heuristic.
    pub secrets_actionable: bool,
    /// Bounded syntax proof of an unguarded Rust value mapping, never inferred from counts.
    pub flat_value_mapping: bool,
}

/// Ordered severity, shared by both projections and delta comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFindingSeverity {
    /// Informational maintainability candidate.
    Info,
    /// Warning-level candidate requiring review.
    Warning,
    /// Highest configured candidate severity; not proof of exploitability.
    Error,
}
impl SourceFindingSeverity {
    /// Stable public severity spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One applicable source-local candidate with its authoritative rule.
pub struct SourceRuleFinding {
    /// Policy used to evaluate this candidate.
    pub rule: &'static SourceRule,
    /// Extracted or calculated metric value.
    pub value: f64,
    /// Severity derived from this rule's thresholds.
    pub severity: SourceFindingSeverity,
}

/// Evaluate at most one finding per registered rule with deterministic ordering.
#[must_use]
pub fn evaluate_source_rules(
    context: SourceRuleContext,
    health: impl Fn(SourceHealthMetric) -> f64,
) -> Vec<SourceRuleFinding> {
    use SourceHealthMetric as M;
    if context.scope != SourceRuleScope::Callable {
        return Vec::new();
    }
    let Some(language) = context.analyzed_language else {
        return Vec::new();
    };
    let lines = health(M::CodeLines);
    SOURCE_RULES
        .iter()
        .filter_map(|rule| {
            if !capabilities::source_capability(rule.name, rule.metric)
                .languages
                .admits(language.as_str())
                || lines < f64::from(rule.minimum_code_lines)
                || (rule.metric == M::ParameterCount && context.framework_owned)
                || (rule.metric == M::SecretsScore && !context.secrets_actionable)
                || rule
                    .excludes_value_mapping(context.analyzed_language, context.flat_value_mapping)
            {
                return None;
            }
            let value = source_rule_value(rule.metric, lines, &health)?;
            if !value.is_finite() || value < rule.thresholds[0] {
                return None;
            }
            let severity = if value >= rule.thresholds[2] {
                SourceFindingSeverity::Error
            } else if value >= rule.thresholds[1] {
                SourceFindingSeverity::Warning
            } else {
                SourceFindingSeverity::Info
            };
            Some(SourceRuleFinding {
                rule,
                value,
                severity,
            })
        })
        .collect()
}

// Composite-risk calibration. These scales and eligibility floors are part of
// SOURCE_RULE_CONTRACT; changing one requires a new persisted-rule contract.
const BRAIN_MINIMUM_LINES: f64 = 50.0;
const BRAIN_MINIMUM_COMPLEXITY: f64 = 8.0;
const BRAIN_LINES_SCALE: f64 = 100.0;
const BRAIN_COMPLEXITY_SCALE: f64 = 10.0;
const BRAIN_NESTING_SCALE: f64 = 3.0;
const BRAIN_CONDITIONAL_SCALE: f64 = 4.0;

fn source_rule_value(
    metric: SourceHealthMetric,
    lines: f64,
    health: &impl Fn(SourceHealthMetric) -> f64,
) -> Option<f64> {
    use SourceHealthMetric as M;
    if metric != M::BrainRisk {
        return Some(health(metric));
    }
    let complexity = health(M::Cyclomatic);
    if lines < BRAIN_MINIMUM_LINES || complexity < BRAIN_MINIMUM_COMPLEXITY {
        return None;
    }
    Some(
        (lines / BRAIN_LINES_SCALE)
            * (complexity / BRAIN_COMPLEXITY_SCALE)
            * (health(M::MaxNesting) / BRAIN_NESTING_SCALE).max(1.0)
            * (health(M::MaxConditionalOperands) / BRAIN_CONDITIONAL_SCALE).max(1.0),
    )
}

/// Exact operation evidence remains separate from the enclosing symbol range.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFindingLocations {
    /// `operation` when sites are available, otherwise explicitly `symbol`.
    pub precision: &'static str,
    /// Stable coordinate convention for all sites.
    pub coordinates: &'static str,
    /// Complete operation count, absent for non-occurrence/coarse metrics.
    pub total_count: Option<f64>,
    /// At most sixteen sites for this detector, ordered by source location.
    pub sites: Vec<crate::SourceSpan>,
    /// Additional operations exist beyond the retained sites.
    pub truncated: bool,
}

impl SourceRule {
    /// Flat mappings are intentionally outside branching-logic risk, not evaluated as clean.
    #[must_use]
    pub fn excludes_value_mapping(
        &self,
        language: Option<crate::SourceLanguage>,
        flat_value_mapping: bool,
    ) -> bool {
        flat_value_mapping
            && language == Some(crate::SourceLanguage::Rust)
            && matches!(
                self.metric,
                SourceHealthMetric::Cyclomatic | SourceHealthMetric::BrainRisk
            )
    }

    /// Project retained locations without inventing precision for coarse detectors.
    #[must_use]
    pub fn locations(
        &self,
        value: f64,
        evidence: &[HealthOperationSite],
    ) -> SourceFindingLocations {
        let mut sites = evidence
            .iter()
            .filter(|site| site.metric == self.metric)
            .map(|site| site.span)
            .collect::<Vec<_>>();
        sites.sort_unstable();
        sites.dedup();
        sites.truncate(16);
        let exact = matches!(
            self.metric,
            SourceHealthMetric::UnsafeJsonParse
                | SourceHealthMetric::HttpWithoutTimeout
                | SourceHealthMetric::UnvalidatedEnv
                | SourceHealthMetric::InsecureRandom
                | SourceHealthMetric::DynamicEval
                | SourceHealthMetric::DebugLogs
                | SourceHealthMetric::SyncIoInAsync
                | SourceHealthMetric::InsecureHash
                | SourceHealthMetric::SqlStringConcatenation
        );
        let retained = f64::from(u32::try_from(sites.len()).unwrap_or(u32::MAX));
        SourceFindingLocations {
            precision: if sites.is_empty() {
                "symbol"
            } else {
                "operation"
            },
            coordinates: "utf8_bytes_half_open; lines_one_based; columns_zero_based_bytes",
            total_count: exact.then_some(value),
            truncated: exact && value > retained,
            sites,
        }
    }
}

#[cfg(test)]
mod mapping_precision_tests {
    use super::*;

    #[test]
    fn mapping_exception_is_rust_specific_and_preserves_unrelated_risks() {
        for language in [
            crate::SourceLanguage::Rust,
            crate::SourceLanguage::TypeScript,
        ] {
            for flat_value_mapping in [false, true] {
                let findings = evaluate_source_rules(
                    SourceRuleContext {
                        analyzed_language: Some(language),
                        scope: SourceRuleScope::Callable,
                        framework_owned: false,
                        secrets_actionable: true,
                        flat_value_mapping,
                    },
                    |metric| match metric {
                        SourceHealthMetric::CodeLines => 200.0,
                        SourceHealthMetric::Cyclomatic => 50.0,
                        SourceHealthMetric::MagicNumbers => 10.0,
                        SourceHealthMetric::HardcodedUrls => 3.0,
                        SourceHealthMetric::SecretsScore => 100.0,
                        _ => 0.0,
                    },
                );
                let has = |name| findings.iter().any(|finding| finding.rule.name == name);
                let excluded = language == crate::SourceLanguage::Rust && flat_value_mapping;
                assert_eq!(has("complex_method"), !excluded);
                assert_eq!(has("brain_method"), !excluded);
                for name in [
                    "large_method",
                    "magic_number",
                    "hardcoded_url",
                    "secrets_handling",
                ] {
                    assert!(has(name), "{name}");
                }
            }
        }
    }
}
