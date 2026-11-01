//! Detector thresholds retain the source facts and explanatory evidence.

use super::{corrupt, inventory::Base};
use crate::{insights::StructuralFinding, rocks::RocksError};
use codevetra_domain::Visibility;
use num_traits::ToPrimitive as _;
use serde_json::{Value, json};

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
        output.push(finding(
            base,
            candidate.rule.name,
            candidate.rule.metric_name,
            candidate.value,
            candidate.severity.as_str(),
            detail(base, candidate.rule.name)?,
        ));
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
        && let Some(prior) = base.prior.filter(|prior| *prior >= 20)
        && f64::from(lines(base)) > f64::from(prior) * 1.5
    {
        append(
            base,
            rows,
            "recently_grew",
            "growth_percent",
            f64::from(lines(base)) * 100.0 / f64::from(prior),
            [150.0, 200.0, 300.0],
        )?;
    }
    if let Some((fraction, percentile)) = base
        .coverage
        .filter(|(fraction, percentile)| *fraction <= 0.5 && *percentile >= 0.9)
    {
        let _ = percentile;
        append(
            base,
            rows,
            "low_coverage",
            "uncovered_percent",
            (1.0 - fraction) * 100.0,
            [50.0, 50.0, 80.0],
        )?;
    }
    Ok(())
}
fn relational_rules(base: &Base<'_>, rows: &mut Vec<StructuralFinding>) -> Result<(), RocksError> {
    let kind = base.symbol.symbol_kind.as_str();
    if kind == "constant" {
        append(
            base,
            rows,
            "stale_doc",
            "disjoint_documented_numbers",
            base.document.health("stale_doc_numbers"),
            [1.0, 999.0, 999.0],
        )?;
    }
    if matches!(kind, "class" | "struct" | "module") && base.topology.complex >= 5 {
        append(
            base,
            rows,
            "god_class",
            "behavioral_methods",
            number(base.topology.behavioral)?,
            [15.0, 40.0, 60.0],
        )?;
    }
    if kind == "method"
        && let Some(envy) = base.envy.filter(|envy| envy.classes.len() <= 2)
        && number(envy.own)? / number(envy.own.checked_add(envy.foreign).ok_or(corrupt())?.max(1))?
            < 1.0 / 3.0
    {
        append(
            base,
            rows,
            "feature_envy",
            "foreign_field_accesses",
            number(envy.foreign)?,
            [6.0, 12.0, 999.0],
        )?;
    }
    if unused_export(base) {
        append(
            base,
            rows,
            "unused_export",
            "external_incoming_edges",
            1.0,
            [1.0, 1.0, 2.0],
        )?;
    }
    if matches!(
        kind,
        "function" | "method" | "component" | "class" | "struct" | "module"
    ) && !base.document.facade_factory
        && !base.topology.framework
    {
        append(
            base,
            rows,
            "high_fan_out",
            "outgoing_dependencies",
            number(base.topology.outgoing)?,
            [25.0, 50.0, 100.0],
        )?;
    }
    append(
        base,
        rows,
        "unresolved_reference_pressure",
        "unresolved_sites",
        number(base.unresolved)?,
        [15.0, 40.0, 100.0],
    )
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
fn append(
    base: &Base<'_>,
    rows: &mut Vec<StructuralFinding>,
    name: &str,
    metric: &str,
    value: f64,
    thresholds: [f64; 3],
) -> Result<(), RocksError> {
    if !value.is_finite() {
        return Err(corrupt());
    }
    if value >= thresholds[0] {
        let severity = if value >= thresholds[2] {
            "error"
        } else if value >= thresholds[1] {
            "warning"
        } else {
            "info"
        };
        rows.push(finding(
            base,
            name,
            metric,
            value,
            severity,
            detail(base, name)?,
        ));
    }
    Ok(())
}
pub(super) fn finding(
    base: &Base<'_>,
    name: &str,
    metric: &str,
    value: f64,
    severity: &str,
    detail: Option<Value>,
) -> StructuralFinding {
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
