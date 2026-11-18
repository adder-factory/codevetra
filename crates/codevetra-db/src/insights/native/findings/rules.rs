//! Detector thresholds retain the source facts and explanatory evidence.

use super::{corrupt, inventory::Base};
use crate::{insights::StructuralFinding, rocks::RocksError};
use codevetra_domain::Visibility;
use num_traits::ToPrimitive as _;
use serde_json::{Value, json};

// Fixed policy values; changing these requires a detector-contract review.
const PERCENT_SCALE: f64 = 100.0;
const MINIMUM_PRIOR_LINES: u32 = 20;
const MINIMUM_GROWTH_RATIO: f64 = 1.5;
const MAXIMUM_COVERED_FRACTION: f64 = 0.5;
const MINIMUM_COVERAGE_PERCENTILE: f64 = 0.9;
const MINIMUM_COMPLEX_METHODS: u64 = 5;
const MAXIMUM_FOREIGN_CLASSES: usize = 2;
const MINIMUM_OWN_FIELD_RATIO: f64 = 1.0 / 3.0;

struct Rule {
    name: &'static str,
    metric: &'static str,
    thresholds: [f64; 3],
}

const RECENT_GROWTH: Rule = Rule {
    name: "recently_grew",
    metric: "growth_percent",
    thresholds: [150.0, 200.0, 300.0],
};
const LOW_COVERAGE: Rule = Rule {
    name: "low_coverage",
    metric: "uncovered_percent",
    thresholds: [50.0, 50.0, 80.0],
};
const STALE_DOCUMENTATION: Rule = Rule {
    name: "stale_doc",
    metric: "disjoint_documented_numbers",
    thresholds: [1.0, 999.0, 999.0],
};
const GOD_CLASS: Rule = Rule {
    name: "god_class",
    metric: "behavioral_methods",
    thresholds: [15.0, 40.0, 60.0],
};
const FEATURE_ENVY: Rule = Rule {
    name: "feature_envy",
    metric: "foreign_field_accesses",
    thresholds: [6.0, 12.0, 999.0],
};
const UNUSED_EXPORT: Rule = Rule {
    name: "unused_export",
    metric: "external_incoming_edges",
    thresholds: [1.0, 1.0, 2.0],
};
const HIGH_FAN_OUT: Rule = Rule {
    name: "high_fan_out",
    metric: "outgoing_dependencies",
    thresholds: [25.0, 50.0, 100.0],
};
const UNRESOLVED_REFERENCES: Rule = Rule {
    name: "unresolved_reference_pressure",
    metric: "unresolved_sites",
    thresholds: [15.0, 40.0, 100.0],
};

pub(super) fn evaluate(base: &Base<'_>) -> Result<Vec<StructuralFinding>, RocksError> {
    use codevetra_domain::biomarkers::{
        SourceHealthMetric, SourceRuleContext, SourceRuleScope, evaluate_source_rules,
    };
    let context = SourceRuleContext {
        analyzed_language: base.document.analyzed_language,
        scope: SourceRuleScope::from_declaration(
            &base.symbol.symbol_kind,
            base.symbol.declaration_only,
            base.document.test,
        ),
        framework_owned: base.topology.framework,
        secrets_actionable: base.document.secrets_actionable,
        flat_value_mapping: base.document.syntax.flat_value_mapping,
    };
    let mut output = Vec::new();
    for candidate in evaluate_source_rules(context, |metric| {
        if metric == SourceHealthMetric::CodeLines {
            base.document
                .health_or(metric.field(), f64::from(lines(base)))
        } else {
            base.document.health(metric.field())
        }
    }) {
        output.push(
            FindingEvidence {
                name: candidate.rule.name,
                metric: candidate.rule.metric_name,
                value: candidate.value,
                severity: candidate.severity.as_str(),
                detail: detail(base, candidate.rule.name)?,
            }
            .project(base),
        );
    }
    if callable(base) {
        auxiliary_callable_rules(base, &mut output)?;
    }
    relational_rules(base, &mut output)?;
    Ok(output)
}
fn auxiliary_callable_rules(
    base: &Base<'_>,
    rows: &mut Vec<StructuralFinding>,
) -> Result<(), RocksError> {
    if base.recent_prior
        && let Some(prior) = base.prior.filter(|prior| *prior >= MINIMUM_PRIOR_LINES)
        && f64::from(lines(base)) > f64::from(prior) * MINIMUM_GROWTH_RATIO
    {
        RECENT_GROWTH.append(
            base,
            rows,
            f64::from(lines(base)) * PERCENT_SCALE / f64::from(prior),
        )?;
    }
    if let Some((fraction, percentile)) = base.coverage.filter(|(fraction, percentile)| {
        *fraction <= MAXIMUM_COVERED_FRACTION && *percentile >= MINIMUM_COVERAGE_PERCENTILE
    }) {
        let _ = percentile;
        LOW_COVERAGE.append(base, rows, (1.0 - fraction) * PERCENT_SCALE)?;
    }
    Ok(())
}
fn relational_rules(base: &Base<'_>, rows: &mut Vec<StructuralFinding>) -> Result<(), RocksError> {
    let kind = base.symbol.symbol_kind.as_str();
    if kind == "constant" {
        STALE_DOCUMENTATION.append(base, rows, base.document.health("stale_doc_numbers"))?;
    }
    if matches!(kind, "class" | "struct" | "module")
        && base.topology.complex >= MINIMUM_COMPLEX_METHODS
    {
        GOD_CLASS.append(base, rows, number(base.topology.behavioral)?)?;
    }
    if kind == "method"
        && let Some(envy) = base
            .envy
            .filter(|envy| envy.classes.len() <= MAXIMUM_FOREIGN_CLASSES)
        && number(envy.own)? / number(envy.own.checked_add(envy.foreign).ok_or(corrupt())?.max(1))?
            < MINIMUM_OWN_FIELD_RATIO
    {
        FEATURE_ENVY.append(base, rows, number(envy.foreign)?)?;
    }
    if unused_export(base) {
        UNUSED_EXPORT.append(base, rows, 1.0)?;
    }
    if matches!(
        kind,
        "function" | "method" | "component" | "class" | "struct" | "module"
    ) && !base.document.syntax.facade_factory
        && !base.topology.framework
    {
        HIGH_FAN_OUT.append(base, rows, number(base.topology.outgoing)?)?;
    }
    UNRESOLVED_REFERENCES.append(base, rows, number(base.unresolved)?)
}
fn unused_export(base: &Base<'_>) -> bool {
    matches!(
        base.symbol.symbol_kind.as_str(),
        "function" | "method" | "class" | "component" | "constant"
    ) && base.symbol.export.exported
        && base.external == 0
        && base.file.language != "python"
        && base.file.normalized_path != "src/index.ts"
        && !base.file.normalized_path.ends_with(".d.ts")
        && !base.symbol.declaration_only
        && !base.topology.function_local
        && !base.topology.framework
        && base.symbol.visibility != Some(Visibility::Public)
        && !base.document.test
}
impl Rule {
    fn severity(&self, value: f64) -> Result<Option<&'static str>, RocksError> {
        if !value.is_finite() {
            return Err(corrupt());
        }
        if value < self.thresholds[0] {
            return Ok(None);
        }
        Ok(Some(if value >= self.thresholds[2] {
            "error"
        } else if value >= self.thresholds[1] {
            "warning"
        } else {
            "info"
        }))
    }

    fn append(
        &self,
        base: &Base<'_>,
        rows: &mut Vec<StructuralFinding>,
        value: f64,
    ) -> Result<(), RocksError> {
        if let Some(severity) = self.severity(value)? {
            rows.push(
                FindingEvidence {
                    name: self.name,
                    metric: self.metric,
                    value,
                    severity,
                    detail: detail(base, self.name)?,
                }
                .project(base),
            );
        }
        Ok(())
    }
}

pub(super) struct FindingEvidence {
    pub(super) name: &'static str,
    pub(super) metric: &'static str,
    pub(super) value: f64,
    pub(super) severity: &'static str,
    pub(super) detail: Option<Value>,
}

impl FindingEvidence {
    pub(super) fn project(self, base: &Base<'_>) -> StructuralFinding {
        let Self {
            name,
            metric,
            value,
            severity,
            detail,
        } = self;
        let mut detail = detail.unwrap_or_else(|| json!({}));
        if let Some(rule) = codevetra_domain::biomarkers::SOURCE_RULES
            .iter()
            .find(|rule| rule.name == name)
        {
            detail["locations"] = json!(rule.locations(value, &base.document.operation_sites));
        } else {
            detail["locations"] =
                json!({"precision":"symbol", "sites":[], "totalCount":null, "truncated":false});
        }
        StructuralFinding {
            symbol_id: base.symbol.symbol_id.to_string(),
            path: base.file.normalized_path.clone(),
            qualified_name: base.symbol.qualified_name.clone(),
            finding: name.to_owned(),
            severity: severity.to_owned(),
            start_line: base.symbol.start_line,
            end_line: base.symbol.end_line,
            metric_name: metric.to_owned(),
            metric: value,
            degree_centrality: base.centrality,
            outgoing_edges: base.topology.outgoing,
            unresolved_references: base.unresolved,
            detail: Some(detail),
        }
    }
}

fn detail(base: &Base<'_>, name: &str) -> Result<Option<Value>, RocksError> {
    let h = |key| base.document.health(key);
    Ok(match name {
        "hardcoded_url" => Some(json!({ "category": if h("hardcoded_url_requests") > 0.0 {
            if h("hardcoded_url_configuration") > 0.0 { "mixed_endpoint_use" } else { "request_destination" }
        } else { "service_configuration" }, "requestDestinations":h("hardcoded_url_requests"),
            "serviceConfiguration":h("hardcoded_url_configuration"), "presentationAbstentions":h("hardcoded_url_presentation_abstentions") })),
        "forof_await" => Some(json!({ "awaitedCall":"owned_await_expression",
            "controlFlowReason":"independent_iterations_without_loop_carried_dependency_or_post_await_exit",
            "dependencyAbstentions":h("serial_await_dependency_loops"), "controlFlowAbstentions":h("serial_await_control_flow_loops"),
            "declaredSerialIntentAbstentions":h("serial_await_intent_loops") })),
        "unresolved_reference_pressure" => Some(
            json!({ "category":"graph_resolution_coverage", "compilerDiagnostic":false,
            "meaning":"references_not_yet_bound_by_codevetra", "verification":"confirm_with_the_language_toolchain_before_editing_source" }),
        ),
        "high_fan_out" => {
            let [calls, modules, types, composition, data] = base.topology.groups;
            let total = calls
                .checked_add(modules)
                .and_then(|n| n.checked_add(types))
                .and_then(|n| n.checked_add(composition))
                .and_then(|n| n.checked_add(data))
                .ok_or(corrupt())?;
            Some(
                json!({ "role":"mixed_orchestration", "dependencyGroups":{ "calls":calls, "modules":modules, "types":types,
                "composition":composition, "dataFlow":data, "other":base.topology.outgoing.saturating_sub(total) },
                "facadeDelegates":h("facade_factory_delegates") }),
            )
        }
        _ => None,
    })
}
fn callable(base: &Base<'_>) -> bool {
    matches!(
        base.symbol.symbol_kind.as_str(),
        "function" | "method" | "component"
    )
}
fn lines(base: &Base<'_>) -> u32 {
    base.symbol.end_line - base.symbol.start_line + 1
}
fn number(value: u64) -> Result<f64, RocksError> {
    value.to_f64().ok_or(corrupt())
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn relational_rule_names_units_and_thresholds_preserve_the_existing_contract() {
        for (rule, name, metric, thresholds) in [
            (
                &RECENT_GROWTH,
                "recently_grew",
                "growth_percent",
                [150.0, 200.0, 300.0],
            ),
            (
                &LOW_COVERAGE,
                "low_coverage",
                "uncovered_percent",
                [50.0, 50.0, 80.0],
            ),
            (
                &STALE_DOCUMENTATION,
                "stale_doc",
                "disjoint_documented_numbers",
                [1.0, 999.0, 999.0],
            ),
            (
                &GOD_CLASS,
                "god_class",
                "behavioral_methods",
                [15.0, 40.0, 60.0],
            ),
            (
                &FEATURE_ENVY,
                "feature_envy",
                "foreign_field_accesses",
                [6.0, 12.0, 999.0],
            ),
            (
                &UNUSED_EXPORT,
                "unused_export",
                "external_incoming_edges",
                [1.0, 1.0, 2.0],
            ),
            (
                &HIGH_FAN_OUT,
                "high_fan_out",
                "outgoing_dependencies",
                [25.0, 50.0, 100.0],
            ),
            (
                &UNRESOLVED_REFERENCES,
                "unresolved_reference_pressure",
                "unresolved_sites",
                [15.0, 40.0, 100.0],
            ),
        ] {
            assert_eq!(
                (rule.name, rule.metric, rule.thresholds),
                (name, metric, thresholds)
            );
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert!(rule.severity(value).is_err());
            }
            assert_eq!(rule.severity(-1.0), Ok(None));
        }
        assert_eq!(MINIMUM_PRIOR_LINES, 20);
        assert_eq!(MINIMUM_GROWTH_RATIO, 1.5);
        assert_eq!(MAXIMUM_COVERED_FRACTION, 0.5);
        assert_eq!(MINIMUM_COVERAGE_PERCENTILE, 0.9);
        assert_eq!(MINIMUM_COMPLEX_METHODS, 5);
        assert_eq!(MAXIMUM_FOREIGN_CLASSES, 2);
        assert_eq!(MINIMUM_OWN_FIELD_RATIO, 1.0 / 3.0);
        assert_eq!(PERCENT_SCALE, 100.0);
    }

    #[test]
    fn source_history_and_coverage_severity_boundaries_preserve_equal_thresholds() {
        let info = Some("info");
        let warning = Some("warning");
        let error = Some("error");
        for (rule, cases) in [
            (
                &RECENT_GROWTH,
                [
                    (149.5, None),
                    (150.0, info),
                    (199.5, info),
                    (200.0, warning),
                    (299.5, warning),
                    (300.0, error),
                ],
            ),
            (
                &LOW_COVERAGE,
                [
                    (0.0, None),
                    (49.5, None),
                    (50.0, warning),
                    (50.5, warning),
                    (79.5, warning),
                    (80.0, error),
                ],
            ),
            (
                &STALE_DOCUMENTATION,
                [
                    (0.0, None),
                    (0.5, None),
                    (1.0, info),
                    (1.5, info),
                    (998.5, info),
                    (999.0, error),
                ],
            ),
        ] {
            assert_severities(rule, cases);
        }
    }

    #[test]
    fn topology_severity_boundaries_preserve_inclusive_and_equal_thresholds() {
        let info = Some("info");
        let warning = Some("warning");
        let error = Some("error");
        for (rule, cases) in [
            (
                &GOD_CLASS,
                [
                    (14.5, None),
                    (15.0, info),
                    (39.5, info),
                    (40.0, warning),
                    (59.5, warning),
                    (60.0, error),
                ],
            ),
            (
                &FEATURE_ENVY,
                [
                    (5.5, None),
                    (6.0, info),
                    (11.5, info),
                    (12.0, warning),
                    (998.5, warning),
                    (999.0, error),
                ],
            ),
            (
                &UNUSED_EXPORT,
                [
                    (0.0, None),
                    (0.5, None),
                    (1.0, warning),
                    (1.5, warning),
                    (2.0, error),
                    (2.5, error),
                ],
            ),
            (
                &HIGH_FAN_OUT,
                [
                    (24.5, None),
                    (25.0, info),
                    (49.5, info),
                    (50.0, warning),
                    (99.5, warning),
                    (100.0, error),
                ],
            ),
            (
                &UNRESOLVED_REFERENCES,
                [
                    (14.5, None),
                    (15.0, info),
                    (39.5, info),
                    (40.0, warning),
                    (99.5, warning),
                    (100.0, error),
                ],
            ),
        ] {
            assert_severities(rule, cases);
        }
    }

    fn assert_severities(rule: &Rule, cases: [(f64, Option<&str>); 6]) {
        for (value, expected) in cases {
            assert_eq!(
                rule.severity(value),
                Ok(expected),
                "{} at {value}",
                rule.name
            );
        }
    }
}
