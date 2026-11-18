use super::*;
use codevetra_agent::ScipExportRequest;

#[test]
fn argument_guidance_biomarker_modes_reject_filters_before_loading_a_project() {
    for mode in ["stats", "symbol", "coverage"] {
        let call = policy_call(
            BIOMARKERS_TOOL,
            json!({"mode": mode, "minSeverity": "private-value"}),
        );
        let error = parse_biomarker_request(&call.arguments)
            .err()
            .unwrap_or_else(|| panic!("unsupported filter accepted"));
        assert_eq!(error.code(), ToolErrorCode::InvalidArguments);
        assert!(
            error
                .wire_message()
                .contains(&format!("minSeverity is unavailable in mode={mode}"))
        );
        assert!(!error.wire_message().contains("private-value"));
    }
    for arguments in [
        json!({"mode":"stats"}),
        json!({"mode":"ranked","minSeverity":"info"}),
        json!({"mode":"coverage","biomarker":"magic_number"}),
        json!({"mode":"symbol","symbol":"selected"}),
    ] {
        let call = policy_call(BIOMARKERS_TOOL, arguments);
        assert!(parse_biomarker_request(&call.arguments).is_ok());
    }
    let definition = biomarkers_definition(read_only_annotations())
        .unwrap_or_else(|error| panic!("definition: {error}"));
    let properties = &definition.input_schema()["properties"];
    for field in [
        "minSeverity",
        "minCentrality",
        "minMetric",
        "maxMetric",
        "excludeFile",
        "limit",
    ] {
        assert!(
            properties[field]["description"]
                .as_str()
                .is_some_and(|description| description.contains("Ranked mode only")),
            "{field}"
        );
    }
}

#[test]
fn argument_guidance_biomarker_limits_are_discoverable_and_actionable() {
    let definition = biomarkers_definition(read_only_annotations())
        .unwrap_or_else(|error| panic!("definition: {error}"));
    let limit = &definition.input_schema()["properties"]["limit"];
    assert!(limit["description"].as_str().is_some_and(|description| {
        description.contains("1 to 500") && description.contains("Default: 30")
    }));
    for value in [
        json!(0),
        json!(1000),
        json!(-1),
        json!(1.5),
        json!("private-input"),
    ] {
        let call = policy_call(BIOMARKERS_TOOL, json!({"limit": value}));
        let error = RankedBiomarkerFilters::parse(&call.arguments)
            .err()
            .unwrap_or_else(|| panic!("invalid limit accepted"));
        assert_eq!(error.code(), ToolErrorCode::InvalidArguments);
        assert_eq!(
            error.wire_message(),
            "limit must be an integer from 1 to 500 (inclusive)"
        );
    }
    for limit in [1, 500] {
        let call = policy_call(BIOMARKERS_TOOL, json!({"limit": limit}));
        assert_eq!(
            RankedBiomarkerFilters::parse(&call.arguments)
                .unwrap_or_else(|error| panic!("valid limit: {error}"))
                .limit,
            limit
        );
    }
}

#[test]
fn argument_guidance_find_fields_describes_the_valid_combination() {
    let definition = find_definition(read_only_annotations())
        .unwrap_or_else(|error| panic!("definition: {error}"));
    let fields = &definition.input_schema()["properties"]["fields"];
    assert!(fields["description"].as_str().is_some_and(|description| {
        description.contains("compact=true") && description.contains("exact name")
    }));
    let call = policy_call(
        FIND_TOOL,
        json!({"by":"name", "query":"target", "fields":["id","path"]}),
    );
    let error = parse_find_name_shape(&call.arguments, false)
        .err()
        .unwrap_or_else(|| panic!("fields without compact accepted"));
    assert_eq!(error.wire_message(), "fields requires compact=true");
    let mut corrected = call.arguments;
    corrected.insert("compact".to_owned(), json!(true));
    let parsed = parse_find_name_shape(&corrected, false)
        .unwrap_or_else(|error| panic!("corrected request: {error}"));
    assert!(parsed.compact);
    assert_eq!(parsed.requested_fields, ["id", "path"]);
}

#[test]
fn argument_guidance_numeric_errors_never_echo_values_or_unknown_keys() {
    let call = policy_call(
        FIND_TOOL,
        json!({"limit":"secret-value", "secret-key":"secret"}),
    );
    let error = optional_integer(&call.arguments, "limit", NumericBounds::new(1, 10_u16, 100))
        .err()
        .unwrap_or_else(|| panic!("invalid integer accepted"));
    assert!(
        error
            .wire_message()
            .contains("limit must be an integer from 1 to 100")
    );
    assert!(!error.wire_message().contains("secret"));
    assert!(
        !reject_unknown(&call.arguments, &["limit"])
            .err()
            .unwrap_or_else(|| panic!("unknown argument accepted"))
            .wire_message()
            .contains("secret")
    );
}

#[test]
fn argument_guidance_retains_schema_constraints_and_existing_descriptions() {
    let original = json!({
        "ratio": {"type":"number", "minimum":0.25, "maximum":1.0, "default":0.5, "description":"Minimum score."},
        "symbols": {"type":"array", "minItems":1, "maxItems":20},
        "handles": {"type":"array", "maxItems":64},
        "query": {"type":"string", "maxLength":4096, "description":"Search terms."},
        "offset": {"type":"integer", "minimum":0}
    });
    let mut properties = original.as_object().cloned().unwrap_or_default();
    argument_guidance::describe_bounds(&mut properties);
    assert_eq!(
        properties["ratio"]["description"],
        "Minimum score. Schema range: 0.25 to 1.0 (inclusive). Default: 0.5."
    );
    assert_eq!(
        properties["symbols"]["description"],
        "Item count: 1 to 20 (inclusive)."
    );
    assert_eq!(
        properties["handles"]["description"],
        "Item count: at most 64."
    );
    assert_eq!(properties["query"], original["query"]);
    assert_eq!(properties["offset"], original["offset"]);
    for (key, property) in &properties {
        for (constraint, value) in original[key]
            .as_object()
            .unwrap_or_else(|| panic!("object"))
        {
            if constraint != "description" {
                assert_eq!(&property[constraint], value);
            }
        }
    }
}

#[test]
fn argument_guidance_required_integer_does_not_recommend_omitting_it() {
    for value in [None, Some(json!(0)), Some(json!("secret"))] {
        let arguments = value.map_or_else(Map::new, |value| {
            Map::from_iter([("line".to_owned(), value)])
        });
        let error = required_integer(&arguments, "line", NumericBounds::new(1, 1_u32, u32::MAX))
            .err()
            .unwrap_or_else(|| panic!("invalid required integer accepted"));
        assert!(error.wire_message().contains("line"));
        assert!(error.wire_message().contains("1 to 4294967295"));
        assert!(!error.wire_message().contains("omit"));
        assert!(!error.wire_message().contains("secret"));
    }
}

fn policy_call(name: &str, arguments: Value) -> ToolCall {
    let Value::Object(arguments) = arguments else {
        panic!("object required");
    };
    ToolCall {
        name: name.to_owned(),
        arguments,
    }
}

#[test]
fn argument_guidance_admin_limits_follow_the_selected_action() {
    let definition = admin_definition().unwrap_or_else(|error| panic!("definition: {error}"));
    let schema = definition.input_schema();
    let description = schema["properties"]["limit"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(description.contains("graph-export: integer 1 to 50000, default 1000"));
    assert!(description.contains("classify: integer 1 to 1000000, default 100000"));
    assert_eq!(schema["properties"]["limit"]["maximum"], 10_000_000);
    for (action, maximum) in [("graph-export", 50_000), ("classify", 1_000_000)] {
        let branch = schema["allOf"]
            .as_array()
            .and_then(|branches| {
                branches
                    .iter()
                    .find(|branch| branch["if"]["properties"]["action"]["const"] == action)
            })
            .unwrap_or_else(|| panic!("missing action constraint"));
        assert_eq!(
            branch["then"]["properties"]["limit"],
            json!({"type":"integer", "minimum":1, "maximum":maximum})
        );
        assert_eq!(branch["if"]["required"], json!(["action"]));
    }
    assert_eq!(
        schema["allOf"][2]["then"],
        json!({"not":{"required":["limit"]}})
    );
    assert_eq!(parse_graph_export_limit(&Map::new()), Ok(1_000));
    for value in [1, 1_000, 50_000] {
        let arguments = policy_call(ADMIN_TOOL, json!({"limit":value})).arguments;
        assert_eq!(parse_graph_export_limit(&arguments), Ok(value));
    }
    for value in [json!(0), json!(50_001), json!(1.5), json!("secret")] {
        let arguments = policy_call(ADMIN_TOOL, json!({"limit":value})).arguments;
        let error = parse_graph_export_limit(&arguments)
            .err()
            .unwrap_or_else(|| panic!("invalid export limit accepted"));
        assert_eq!(
            error.wire_message(),
            "limit must be an integer from 1 to 50000 (inclusive)"
        );
    }
    for value in [1, 1_000_000] {
        let arguments = policy_call(ADMIN_TOOL, json!({"limit":value})).arguments;
        assert_eq!(
            optional_bounded_admin_u64(&arguments, "limit", ROLE_CLASSIFICATION_MAXIMUM_LIMIT),
            Ok(Some(value))
        );
    }
    let arguments = policy_call(ADMIN_TOOL, json!({"limit":1_000_001})).arguments;
    assert_eq!(
        optional_bounded_admin_u64(&arguments, "limit", ROLE_CLASSIFICATION_MAXIMUM_LIMIT)
            .err()
            .unwrap_or_else(|| panic!("invalid classification limit accepted"))
            .wire_message(),
        "limit must be an integer from 1 to 1000000 (inclusive)"
    );
    for (value, expected) in [
        (json!(-1), SummarySweepLimit::Uncapped),
        (json!(0), SummarySweepLimit::Bounded(0)),
        (json!(1.5), SummarySweepLimit::Bounded(2)),
    ] {
        let arguments = policy_call(ADMIN_TOOL, json!({"limit":value})).arguments;
        assert_eq!(
            parse_summary_sweep_limit(&arguments)
                .unwrap_or_else(|error| panic!("summary alias: {error}")),
            expected
        );
    }
}

fn generated_admin_limit(action: &str, limit: &str) -> Map<String, Value> {
    let parsed = crate::generated_cli::parse_from(["codevetra", "admin", action, "--limit", limit])
        .unwrap_or_else(|error| panic!("CLI parse: {error}"));
    let crate::generated_cli::ParsedCli::Tool(invocation) = parsed else {
        panic!("admin command must route through MCP");
    };
    assert_eq!(invocation.tool_name(), ADMIN_TOOL);
    assert_eq!(invocation.arguments()["action"], action);
    invocation.arguments().clone()
}

#[test]
fn argument_guidance_admin_cli_limits_reach_the_action_parsers() {
    for raw in ["1", "1000", "50000"] {
        let args = generated_admin_limit("graph-export", raw);
        assert!(args["limit"].is_u64());
        assert!(parse_graph_export_limit(&args).is_ok());
    }
    for raw in ["1", "100000", "1000000"] {
        let args = generated_admin_limit("classify", raw);
        assert!(args["limit"].is_u64());
        assert!(
            optional_bounded_admin_u64(&args, "limit", ROLE_CLASSIFICATION_MAXIMUM_LIMIT).is_ok()
        );
    }
    assert!(parse_graph_export_limit(&generated_admin_limit("graph-export", "50001")).is_err());
    assert!(
        optional_bounded_admin_u64(
            &generated_admin_limit("classify", "1000001"),
            "limit",
            ROLE_CLASSIFICATION_MAXIMUM_LIMIT
        )
        .is_err()
    );
    for (raw, expected) in [
        ("-1", SummarySweepLimit::Uncapped),
        ("0", SummarySweepLimit::Bounded(0)),
        ("1.5", SummarySweepLimit::Bounded(2)),
    ] {
        let args = generated_admin_limit("summarize", raw);
        assert_eq!(
            parse_summary_sweep_limit(&args)
                .unwrap_or_else(|error| panic!("summary parse: {error}")),
            expected
        );
    }
    for action in ["graph-export", "classify"] {
        let args = generated_admin_limit(action, "1.5");
        assert!(parse_graph_export_limit(&args).is_err());
        assert!(
            optional_bounded_admin_u64(&args, "limit", ROLE_CLASSIFICATION_MAXIMUM_LIMIT).is_err()
        );
    }
}

fn action_property<'a>(schema: &'a Value, action: &str, field: &str) -> &'a Value {
    &schema["allOf"]
        .as_array()
        .and_then(|branches| {
            branches
                .iter()
                .find(|branch| branch["if"]["properties"]["action"]["const"] == action)
        })
        .unwrap_or_else(|| panic!("missing action contract: {action}"))["then"]["properties"][field]
}

#[test]
fn argument_guidance_admin_timeout_schema_matches_each_handler() {
    let definition = admin_definition().unwrap_or_else(|error| panic!("definition: {error}"));
    let schema = definition.input_schema();
    assert_eq!(schema["properties"]["timeoutMs"]["maximum"], 1_800_000);
    let description = schema["properties"]["timeoutMs"]["description"]
        .as_str()
        .unwrap_or_default();
    for (action, minimum, maximum) in [
        ("scip-export", 100, 1_800_000),
        ("biomarkers-refresh", 1, 300_000),
        ("llm-apply", 1, 600_000),
    ] {
        let property = action_property(schema, action, "timeoutMs");
        assert_eq!(property["minimum"], minimum);
        assert_eq!(property["maximum"], maximum);
        assert!(description.contains(&format!("{action} {minimum} to {maximum}")));
    }
    assert_eq!(
        action_property(schema, "scip-export", "timeoutMs")["default"],
        120_000
    );
    for timeout in [100, 300_001, 1_800_000] {
        let args = policy_call(
            ADMIN_TOOL,
            json!({"action":"scip-export", "timeoutMs":timeout}),
        )
        .arguments;
        let expected = ScipExportRequest::new("index.scip", 5_000_000)
            .and_then(|request| request.with_timeout(Duration::from_millis(timeout)))
            .unwrap_or_else(|error| panic!("fixture: {error}"));
        assert_eq!(admin_arguments::scip_export(&args), Ok(expected));
    }
    for timeout in [
        json!(99),
        json!(1_800_001),
        json!(100.5),
        json!("private-value"),
    ] {
        let args = policy_call(
            ADMIN_TOOL,
            json!({"action":"scip-export", "timeoutMs":timeout}),
        )
        .arguments;
        let error = admin_arguments::scip_export(&args)
            .err()
            .unwrap_or_else(|| panic!("invalid SCIP timeout accepted"));
        assert_eq!(
            error.wire_message(),
            "timeoutMs must be an integer from 100 to 1800000 (inclusive)"
        );
    }
}

#[test]
fn argument_guidance_scip_import_and_export_have_distinct_admitted_envelopes() {
    let definition = admin_definition().unwrap_or_else(|error| panic!("definition: {error}"));
    let schema = definition.input_schema();
    assert_eq!(schema["properties"]["maximumRows"]["maximum"], 10_000_000);
    assert_eq!(
        schema["properties"]["maximumSourceBytes"]["maximum"],
        268_435_456
    );
    for (action, maximum) in [("scip-export", 5_000_000), ("scip-import", 10_000_000)] {
        let property = action_property(schema, action, "maximumRows");
        assert_eq!(property["maximum"], maximum);
        assert_eq!(property["default"], maximum);
        for rows in [1, maximum] {
            let args =
                policy_call(ADMIN_TOOL, json!({"action":action, "maximumRows":rows})).arguments;
            if action == "scip-export" {
                assert!(admin_arguments::scip_export(&args).is_ok());
            } else {
                assert!(admin_arguments::scip_import(&args).is_ok());
            }
        }
        for rows in [0, maximum + 1] {
            let args =
                policy_call(ADMIN_TOOL, json!({"action":action, "maximumRows":rows})).arguments;
            if action == "scip-export" {
                assert!(admin_arguments::scip_export(&args).is_err());
            } else {
                assert!(admin_arguments::scip_import(&args).is_err());
            }
        }
    }
    for (bytes, valid) in [
        (0, false),
        (1, true),
        (268_435_456, true),
        (268_435_457, false),
    ] {
        let args = policy_call(
            ADMIN_TOOL,
            json!({"action":"scip-import", "maximumSourceBytes":bytes}),
        )
        .arguments;
        assert_eq!(admin_arguments::scip_import(&args).is_ok(), valid);
    }
    for extra in ["timeoutMs", "limit", "out"] {
        let args = policy_call(ADMIN_TOOL, json!({"action":"scip-import", extra:1})).arguments;
        assert!(admin_arguments::scip_import(&args).is_err());
    }
}

#[test]
fn argument_guidance_scip_cli_keeps_defaults_owned_by_its_handler() {
    for action in ["scip-export", "scip-import"] {
        let crate::generated_cli::ParsedCli::Tool(invocation) =
            crate::generated_cli::parse_from(["codevetra", "admin", action])
                .unwrap_or_else(|error| panic!("CLI: {error}"))
        else {
            panic!("tool route");
        };
        assert!(!invocation.arguments().contains_key("maximumRows"));
        assert!(!invocation.arguments().contains_key("maximumSourceBytes"));
        assert!(!invocation.arguments().contains_key("timeoutMs"));
        if action == "scip-export" {
            assert!(admin_arguments::scip_export(invocation.arguments()).is_ok());
        } else {
            assert!(admin_arguments::scip_import(invocation.arguments()).is_ok());
        }
    }
    let crate::generated_cli::ParsedCli::Tool(invocation) = crate::generated_cli::parse_from([
        "codevetra",
        "admin",
        "scip-export",
        "--timeout-ms",
        "1800000",
    ])
    .unwrap_or_else(|error| panic!("CLI: {error}")) else {
        panic!("tool route");
    };
    assert_eq!(invocation.arguments()["timeoutMs"], 1_800_000);
    assert!(admin_arguments::scip_export(invocation.arguments()).is_ok());
}

#[test]
fn argument_guidance_review_timeout_names_its_actual_mode_without_echoing_input() {
    let definition = review_definition(read_only_annotations())
        .unwrap_or_else(|error| panic!("review definition: {error}"));
    let properties = &definition.input_schema()["properties"];
    for (field, expected_modes) in [
        ("timeoutMs", "trust"),
        ("baseRef", "context"),
        ("k", "neighbors"),
        ("minCentrality", "risk"),
        ("minSeverity", "agent-audit"),
        ("limit", "risk, numerical"),
    ] {
        assert!(
            properties[field]["description"]
                .as_str()
                .is_some_and(|description| description
                    .contains(&format!("Accepted review modes: {expected_modes}."))),
            "{field}"
        );
    }
    let call = policy_call(
        REVIEW_TOOL,
        json!({"mode":"context", "timeoutMs":"private-value"}),
    );
    let error = validate_review_mode_arguments(&call.arguments, "context")
        .err()
        .unwrap_or_else(|| panic!("context timeout accepted"));
    assert!(
        error
            .wire_message()
            .contains("timeoutMs is not accepted for review mode context")
    );
    assert!(!error.wire_message().contains("private-value"));
    for (mode, arguments) in [
        ("trust", json!({"mode":"trust", "timeoutMs":30000})),
        ("context", json!({"mode":"context", "baseRef":"HEAD"})),
        ("numerical", json!({"mode":"numerical", "limit":10})),
    ] {
        let call = policy_call(REVIEW_TOOL, arguments);
        assert!(validate_review_mode_arguments(&call.arguments, mode).is_ok());
    }
}
