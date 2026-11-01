//! Bounded call-local safety evidence. No source or literal is retained in sites.

use super::{is_callable_node, named_children, span_for, text_for};
use crate::{ExtractError, HealthOperationSite, SymbolHealthMetrics};
use codevetra_domain::biomarkers::SourceHealthMetric as Metric;
use tree_sitter::Node;

const MAX_BINDINGS: usize = 4096;
const MAX_SCOPE_NODES: usize = 100_000;
const MAX_SITES_PER_METRIC: usize = 16;
const MAX_ANCESTORS: usize = 256;

fn needs_scope(node: Node<'_>, source: &str) -> bool {
    matches!(
        node.kind(),
        "call_expression" | "new_expression" | "debugger_statement"
    ) || is_env_value(node, source)
        || outer_dynamic_sql(node, source)
}

#[derive(Default)]
pub(super) enum Analysis<'tree> {
    #[default]
    Pending,
    Ready(Scope<'tree>),
    Unavailable,
}

impl<'tree> Analysis<'tree> {
    pub(super) fn record(
        &mut self,
        metrics: &mut SymbolHealthMetrics,
        node: Node<'tree>,
        input: &super::HealthWalkInput<'tree, '_>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<(), ExtractError> {
        if matches!(self, Self::Unavailable) || !needs_scope(node, input.source) {
            return Ok(());
        }
        let result = (|| {
            if matches!(self, Self::Pending) {
                *self = Self::Ready(Scope::new(input.body, input.source, cancelled)?);
            }
            if let Self::Ready(scope) = self {
                scope.record(metrics, node, input.body, input.source, input.async_symbol)?;
            }
            Ok(())
        })();
        if result == Err(ExtractError::OutputLimit) {
            // Scope bounds are optional detector bounds, not extraction limits.
            // Drop any prefix so neither coverage nor findings imply a full scan.
            *self = Self::Unavailable;
            metrics.analyzed_language = None;
            metrics.operation_sites = crate::HealthOperationSites::default();
            metrics.unsafe_json_parse = 0;
            metrics.http_without_timeout = 0;
            metrics.unvalidated_env = 0;
            metrics.insecure_random = 0;
            metrics.dynamic_eval = 0;
            metrics.debug_logs = 0;
            metrics.sync_io_in_async = 0;
            metrics.insecure_hash = 0;
            metrics.sql_string_concatenation = 0;
            return Ok(());
        }
        result
    }
}

struct Binding<'tree> {
    name: String,
    scope: Node<'tree>,
    declaration: Node<'tree>,
}

pub(super) struct Scope<'tree> {
    bindings: Vec<Binding<'tree>>,
}

impl<'tree> Scope<'tree> {
    pub(super) fn new(
        body: Node<'tree>,
        source: &str,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<Self, ExtractError> {
        validate_ancestry(body)?;
        let mut scope = Self {
            bindings: Vec::new(),
        };
        let mut work = vec![(body, true)];
        scope.collect_enclosing_bindings(body, source, &mut work)?;
        let mut visited = 0_usize;
        while let Some((node, descend)) = work.pop() {
            visited += 1;
            if visited > MAX_SCOPE_NODES {
                return Err(ExtractError::OutputLimit);
            }
            if visited.is_multiple_of(256) && cancelled() {
                return Err(ExtractError::Cancelled);
            }
            scope.declaration(node, source)?;
            if is_callable_node(node.kind()) {
                continue;
            }
            if descend
                || matches!(
                    node.kind(),
                    "export_statement"
                        | "lexical_declaration"
                        | "variable_declaration"
                        | "import_statement"
                        | "import_clause"
                        | "named_imports"
                )
            {
                work.extend(named_children(node).map(|child| (child, descend)));
            }
        }
        Ok(scope)
    }

    fn collect_enclosing_bindings(
        &mut self,
        body: Node<'tree>,
        source: &str,
        work: &mut Vec<(Node<'tree>, bool)>,
    ) -> Result<(), ExtractError> {
        let mut ancestor = body.parent();
        while let Some(node) = ancestor {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                self.pattern(parameters, node, node, source)?;
            }
            if matches!(node.kind(), "program" | "statement_block") {
                work.extend(named_children(node).map(|child| (child, false)));
            }
            ancestor = node.parent();
        }
        Ok(())
    }

    fn declaration(&mut self, node: Node<'tree>, source: &str) -> Result<(), ExtractError> {
        validate_ancestry(node)?;
        let pattern = match node.kind() {
            "variable_declarator" | "function_declaration" | "class_declaration" => {
                node.child_by_field_name("name")
            }
            "import_specifier" => node
                .child_by_field_name("alias")
                .or_else(|| node.child_by_field_name("name")),
            "namespace_import" => named_children(node).find(|child| child.kind() == "identifier"),
            "identifier"
                if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "import_clause") =>
            {
                Some(node)
            }
            _ => None,
        };
        if let Some(pattern) = pattern {
            let function_scoped = node.kind() == "variable_declarator"
                && node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "variable_declaration");
            let mut scope = node.parent();
            while let Some(parent) = scope {
                if parent.kind() == "program"
                    || (function_scoped && is_callable_node(parent.kind()))
                    || (!function_scoped && parent.kind() == "statement_block")
                {
                    return self.pattern(pattern, parent, node, source);
                }
                scope = parent.parent();
            }
        }
        Ok(())
    }

    fn pattern(
        &mut self,
        pattern: Node<'tree>,
        scope: Node<'tree>,
        declaration: Node<'tree>,
        source: &str,
    ) -> Result<(), ExtractError> {
        let mut pending = vec![pattern];
        let mut visited = 0;
        while let Some(node) = pending.pop() {
            visited += 1;
            if visited > MAX_BINDINGS {
                return Err(ExtractError::OutputLimit);
            }
            if matches!(
                node.kind(),
                "identifier" | "shorthand_property_identifier_pattern"
            ) {
                if self.bindings.len() >= MAX_BINDINGS {
                    return Err(ExtractError::OutputLimit);
                }
                self.bindings.push(Binding {
                    name: text_for(source, node).to_owned(),
                    scope,
                    declaration,
                });
            } else if !matches!(node.kind(), "type_annotation" | "string" | "number") {
                // Binding patterns only: initializer expressions do not declare names.
                if let Some(left) = node.child_by_field_name("left") {
                    pending.push(left);
                } else {
                    pending.extend(named_children(node));
                }
            }
        }
        Ok(())
    }

    fn binding(&self, name: &str, at: Node<'_>) -> Option<&Binding<'tree>> {
        self.bindings
            .iter()
            .filter(|binding| binding.name == name && contains(binding.scope, at))
            .min_by_key(|binding| binding.scope.byte_range().len())
    }

    pub(super) fn record(
        &self,
        metrics: &mut SymbolHealthMetrics,
        node: Node<'tree>,
        body: Node<'tree>,
        source: &str,
        async_symbol: bool,
    ) -> Result<(), ExtractError> {
        validate_ancestry(node)?;
        if outer_dynamic_sql(node, source) {
            add(metrics, Metric::SqlStringConcatenation, node)?;
        }
        if node.kind() == "debugger_statement" {
            add(metrics, Metric::DebugLogs, node)?;
        }
        if node.kind() == "new_expression"
            && node
                .child_by_field_name("constructor")
                .is_some_and(|constructor| {
                    constructor.kind() == "identifier"
                        && text_for(source, constructor) == "Function"
                })
            && self.binding("Function", node).is_none()
        {
            add(metrics, Metric::DynamicEval, node)?;
        }
        if node.kind() == "call_expression" {
            let Some(function) = node.child_by_field_name("function") else {
                return Ok(());
            };
            self.record_call(metrics, node, function, source, async_symbol)?;
        }
        if is_env_value(node, source)
            && self.binding("process", node).is_none()
            && !self.validated_env(node, body, source)
        {
            add(metrics, Metric::UnvalidatedEnv, node)?;
        }
        Ok(())
    }

    fn record_call(
        &self,
        metrics: &mut SymbolHealthMetrics,
        node: Node<'tree>,
        function: Node<'tree>,
        source: &str,
        async_symbol: bool,
    ) -> Result<(), ExtractError> {
        if member(function, source) == Some(("Math", "random"))
            && self.binding("Math", node).is_none()
            && security_context(node, source)
        {
            add(metrics, Metric::InsecureRandom, node)?;
        }
        self.record_named_calls(metrics, node, function, source, async_symbol)?;
        if member(function, source) == Some(("JSON", "parse"))
            && self.binding("JSON", node).is_none()
            && !inside_caught_try(node)
        {
            add(metrics, Metric::UnsafeJsonParse, node)?;
        }
        let fetch = function.kind() == "identifier"
            && text_for(source, function) == "fetch"
            && self.binding("fetch", node).is_none();
        let axios = member(function, source).is_some_and(|(receiver, method)| {
            receiver == "axios"
                && matches!(
                    method,
                    "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
                )
                && self.imported_from("axios", node, source, "axios")
        });
        if (fetch || axios) && !request_has_guard(node, source, axios) {
            add(metrics, Metric::HttpWithoutTimeout, node)?;
        }
        Ok(())
    }

    fn record_named_calls(
        &self,
        metrics: &mut SymbolHealthMetrics,
        node: Node<'tree>,
        function: Node<'tree>,
        source: &str,
        async_symbol: bool,
    ) -> Result<(), ExtractError> {
        let name = (function.kind() == "identifier").then(|| text_for(source, function));
        if name == Some("eval") && self.binding("eval", node).is_none() {
            add(metrics, Metric::DynamicEval, node)?;
        }
        if member(function, source).is_some_and(|(object, method)| {
            object == "console" && matches!(method, "log" | "debug")
        }) && self.binding("console", node).is_none()
        {
            add(metrics, Metric::DebugLogs, node)?;
        }
        if async_symbol
            && name.is_some_and(|name| {
                matches!(
                    name,
                    "readFileSync"
                        | "writeFileSync"
                        | "appendFileSync"
                        | "execSync"
                        | "execFileSync"
                        | "spawnSync"
                ) && (self.binding(name, node).is_none()
                    || self.imported_from(name, node, source, "node:fs")
                    || self.imported_from(name, node, source, "fs")
                    || self.imported_from(name, node, source, "node:child_process")
                    || self.imported_from(name, node, source, "child_process"))
            })
        {
            add(metrics, Metric::SyncIoInAsync, node)?;
        }
        if name == Some("createHash")
            && (self.binding("createHash", node).is_none()
                || self.imported_from("createHash", node, source, "crypto")
                || self.imported_from("createHash", node, source, "node:crypto"))
            && node
                .child_by_field_name("arguments")
                .and_then(|args| named_children(args).next())
                .is_some_and(|algorithm| {
                    algorithm.kind() == "string"
                        && matches!(
                            text_for(source, algorithm).trim_matches(['\'', '"']),
                            "md5" | "sha1"
                        )
                })
        {
            add(metrics, Metric::InsecureHash, node)?;
        }
        Ok(())
    }

    fn imported_from(&self, name: &str, at: Node<'_>, source: &str, module: &str) -> bool {
        let Some(binding) = self.binding(name, at) else {
            return false;
        };
        let mut node = Some(binding.declaration);
        while let Some(current) = node {
            if current.kind() == "import_statement" {
                return current.child_by_field_name("source").is_some_and(|value| {
                    text_for(source, value).trim_matches(['\'', '"']) == module
                });
            }
            if matches!(current.kind(), "program" | "statement_block") {
                break;
            }
            node = current.parent();
        }
        false
    }

    fn validated_env(&self, node: Node<'tree>, body: Node<'tree>, source: &str) -> bool {
        let Some(arguments) = node.parent().filter(|parent| parent.kind() == "arguments") else {
            return false;
        };
        let Some(call) = arguments.parent().filter(|parent| contains(body, *parent)) else {
            return false;
        };
        let Some(function) = call.child_by_field_name("function") else {
            return false;
        };
        if !function
            .child_by_field_name("property")
            .is_some_and(|property| matches!(text_for(source, property), "parse" | "safeParse"))
        {
            return false;
        }
        function
            .child_by_field_name("object")
            .is_some_and(|schema| self.zod_schema(schema, call, source, 0))
    }

    fn zod_schema(&self, schema: Node<'tree>, at: Node<'tree>, source: &str, depth: u8) -> bool {
        if depth >= 8 {
            return false;
        }
        if schema.kind() == "identifier" {
            return self
                .binding(text_for(source, schema), at)
                .is_some_and(|binding| {
                    binding.declaration.parent().is_some_and(|parent| {
                        parent.kind() == "lexical_declaration"
                            && text_for(source, parent).starts_with("const ")
                    }) && binding
                        .declaration
                        .child_by_field_name("value")
                        .is_some_and(|value| self.zod_schema(value, at, source, depth + 1))
                });
        }
        let Some(function) = schema.child_by_field_name("function") else {
            return false;
        };
        if let Some((receiver, method)) = member(function, source) {
            return receiver == "z"
                && matches!(
                    method,
                    "string" | "number" | "boolean" | "object" | "enum" | "array" | "literal"
                )
                && self.imported_from("z", at, source, "zod");
        }
        false
    }
}

fn validate_ancestry(node: Node<'_>) -> Result<(), ExtractError> {
    let mut current = node.parent();
    for _ in 0..MAX_ANCESTORS {
        let Some(parent) = current else {
            return Ok(());
        };
        current = parent.parent();
    }
    if current.is_none() {
        Ok(())
    } else {
        Err(ExtractError::OutputLimit)
    }
}

fn outer_dynamic_sql(node: Node<'_>, source: &str) -> bool {
    if !super::dynamic_sql_shape(node, source) {
        return false;
    }
    let mut parent = node.parent();
    while let Some(node) = parent {
        if super::dynamic_sql_shape(node, source) {
            return false;
        }
        if node.kind() != "parenthesized_expression" {
            break;
        }
        parent = node.parent();
    }
    true
}

fn security_context(mut node: Node<'_>, source: &str) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "variable_declarator" | "function_declaration" | "method_definition"
        ) && parent
            .child_by_field_name("name")
            .is_some_and(|name| super::contains_security_word(text_for(source, name)))
        {
            return true;
        }
        if is_callable_node(parent.kind()) {
            break;
        }
        node = parent;
    }
    false
}

fn contains(outer: Node<'_>, inner: Node<'_>) -> bool {
    outer.start_byte() <= inner.start_byte() && outer.end_byte() >= inner.end_byte()
}

fn member<'source>(node: Node<'_>, source: &'source str) -> Option<(&'source str, &'source str)> {
    if node.kind() != "member_expression" {
        return None;
    }
    let object = node.child_by_field_name("object")?;
    let property = node.child_by_field_name("property")?;
    (object.kind() == "identifier" && property.kind() == "property_identifier")
        .then(|| (text_for(source, object), text_for(source, property)))
}

fn inside_caught_try(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "try_statement"
            && parent.child_by_field_name("body") == Some(node)
            && parent.child_by_field_name("handler").is_some()
        {
            return true;
        }
        // A variable initializer inherits its enclosing try. A nested callable
        // executes independently and must establish its own exception guard.
        if is_callable_node(parent.kind()) {
            break;
        }
        node = parent;
    }
    false
}

fn request_has_guard(call: Node<'_>, source: &str, axios: bool) -> bool {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return false;
    };
    let mut values = named_children(arguments);
    let _destination = values.next();
    let options_index = usize::from(
        axios
            && call
                .child_by_field_name("function")
                .and_then(|function| member(function, source))
                .is_some_and(|(_, method)| matches!(method, "post" | "put" | "patch")),
    );
    let Some(options) = values
        .nth(options_index)
        .filter(|node| node.kind() == "object")
    else {
        return false;
    };
    named_children(options).any(|property| {
        if property.kind() == "shorthand_property_identifier" {
            return text_for(source, property) == "signal";
        }
        let Some(key) = property.child_by_field_name("key") else {
            return false;
        };
        let Some(value) = property.child_by_field_name("value") else {
            return false;
        };
        let name = text_for(source, key).trim_matches(['\'', '"']);
        (name == "signal" || (axios && name == "timeout"))
            && !matches!(
                text_for(source, value),
                "undefined" | "null" | "false" | "0"
            )
    })
}

fn is_env_value(node: Node<'_>, source: &str) -> bool {
    if !matches!(node.kind(), "member_expression" | "subscript_expression") {
        return false;
    }
    if node
        .child_by_field_name("object")
        .is_some_and(|object| member(object, source) == Some(("process", "env")))
    {
        return true;
    }
    member(node, source) == Some(("process", "env"))
        && !node
            .parent()
            .is_some_and(|parent| parent.child_by_field_name("object") == Some(node))
}

fn add(
    metrics: &mut SymbolHealthMetrics,
    metric: Metric,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    let count = match metric {
        Metric::UnsafeJsonParse => &mut metrics.unsafe_json_parse,
        Metric::HttpWithoutTimeout => &mut metrics.http_without_timeout,
        Metric::UnvalidatedEnv => &mut metrics.unvalidated_env,
        Metric::InsecureRandom => &mut metrics.insecure_random,
        Metric::DynamicEval => &mut metrics.dynamic_eval,
        Metric::DebugLogs => &mut metrics.debug_logs,
        Metric::SyncIoInAsync => &mut metrics.sync_io_in_async,
        Metric::InsecureHash => &mut metrics.insecure_hash,
        Metric::SqlStringConcatenation => &mut metrics.sql_string_concatenation,
        _ => return Err(ExtractError::InvalidSpan),
    };
    *count = count.checked_add(1).ok_or(ExtractError::OutputLimit)?;
    if metrics
        .operation_sites
        .as_slice()
        .iter()
        .filter(|site| site.metric == metric)
        .count()
        < MAX_SITES_PER_METRIC
    {
        metrics.operation_sites.push(HealthOperationSite {
            metric,
            span: span_for(node)?,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ancestry_overflow_marks_health_unavailable() {
        let source = format!(
            "function deep(input) {{ JSON.parse(input); {}JSON.parse(input);{} }}",
            "{".repeat(300),
            "}".repeat(300)
        );
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .unwrap_or_else(|error| panic!("grammar: {error}"));
        let tree = parser
            .parse(&source, None)
            .unwrap_or_else(|| panic!("tree"));
        let declaration = tree
            .root_node()
            .named_child(0)
            .unwrap_or_else(|| panic!("function"));
        let metrics = super::super::symbol_health_metrics(
            super::super::SymbolHealthInput {
                declaration,
                body: declaration.child_by_field_name("body"),
                symbol_kind: codevetra_domain::SymbolKind::Function,
                symbol_name: "deep",
                signature: None,
                docstring: None,
                language: codevetra_domain::SourceLanguage::TypeScript,
                async_symbol: false,
                source: &source,
            },
            &mut || false,
        )
        .unwrap_or_else(|error| panic!("health: {error}"));
        assert_eq!(metrics.analyzed_language, None);
        assert!(metrics.operation_sites.is_empty());
        assert_eq!(metrics.unsafe_json_parse, 0);
        assert_eq!(metrics.parameter_count, 1);
    }

    #[test]
    fn ancestry_limit_is_inclusive_and_overflow_is_explicit() {
        let source = format!(
            "{}value;{}",
            "{".repeat(MAX_ANCESTORS),
            "}".repeat(MAX_ANCESTORS)
        );
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .unwrap_or_else(|error| panic!("grammar: {error}"));
        let tree = parser
            .parse(&source, None)
            .unwrap_or_else(|| panic!("tree"));
        let mut node = tree.root_node();
        for _ in 0..MAX_ANCESTORS {
            node = node
                .named_child(0)
                .unwrap_or_else(|| panic!("nested block"));
        }
        assert_eq!(validate_ancestry(node), Ok(()));
        let child = node.named_child(0).unwrap_or_else(|| panic!("statement"));
        assert_eq!(validate_ancestry(child), Err(ExtractError::OutputLimit));
    }
}
