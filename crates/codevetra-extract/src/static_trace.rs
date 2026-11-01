//! Bounded JS/TS syntax ordering. This is not an execution or feasibility proof.

use crate::{ExtractError, NativeGrammar};
use codevetra_domain::SourceLanguage;
use serde::Serialize;
use tree_sitter::{Node, Parser};

/// Original file coordinates for a source-backed step.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceSpan {
    /// Inclusive UTF-8 byte offset.
    pub start_byte: u64,
    /// Exclusive UTF-8 byte offset.
    pub end_byte: u64,
    /// One-based original source line.
    pub start_line: u32,
    /// One-based original source line.
    pub end_line: u32,
}

/// A step nested by parent identity; vector order is source order within each parent.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxTraceStep {
    /// Local step index, stable for identical source/options.
    pub id: usize,
    /// Enclosing branch/statement step. None denotes the selected body.
    pub parent: Option<usize>,
    /// AST operation class, never an observed runtime event.
    pub kind: String,
    /// Branch/cleanup/loop/callback semantics separate from source position.
    pub relation: String,
    /// Exact validated original source coordinates.
    pub span: TraceSpan,
    /// Bounded verbatim node prefix, explicitly not necessarily the complete node.
    pub preview: String,
    /// Whether preview omits bytes; span still describes the complete node.
    pub preview_truncated: bool,
    /// Exact callee occurrence byte for generation-fenced identity resolution.
    pub call_anchor: Option<u64>,
}

/// Finite source syntax evidence; unknowns are independent of graph completeness.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxTrace {
    /// Nested ordered steps.
    pub steps: Vec<SyntaxTraceStep>,
    /// Work/depth/step bounds or unsupported syntax prevented a complete trace.
    pub partial: bool,
    /// Stable reasons, not arbitrary parser diagnostics.
    pub unknowns: Vec<&'static str>,
}

/// Source observation already paired with a fresh indexed symbol by the caller.
pub struct StaticTraceInput<'a> {
    /// Exact source window, at most 256 KiB.
    pub source: &'a str,
    /// Native source language; initially JS/JSX/TS/TSX only.
    pub language: SourceLanguage,
    /// Original byte offset of the supplied source window.
    pub byte_offset: u64,
    /// Original one-based starting line.
    pub start_line: u32,
    /// Exact indexed declaration's inclusive start in the original file.
    pub symbol_start: u64,
    /// Exact indexed declaration's exclusive end in the original file.
    pub symbol_end: u64,
    /// Maximum retained steps, 1..=512.
    pub maximum_steps: usize,
    /// Maximum nested syntax depth, 1..=32.
    pub maximum_depth: usize,
}

/// Parse a supported selected body with the existing cancellation-aware parser.
/// # Errors
/// Returns grammar, invalid-bound or parser errors. Cancellation retains an explicit partial prefix. Unsupported
/// languages return explicit unknown coverage without trying a different grammar.
pub fn static_syntax_trace(
    input: StaticTraceInput<'_>,
    mut cancelled: impl FnMut() -> bool,
) -> Result<SyntaxTrace, ExtractError> {
    let mut output = SyntaxTrace {
        steps: Vec::new(),
        partial: false,
        unknowns: Vec::new(),
    };
    if cancelled() {
        output.partial = true;
        output.unknowns.push("cancelled");
        return Ok(output);
    }
    if input.source.len() > 256 * 1024
        || input.maximum_steps == 0
        || input.maximum_steps > 512
        || input.maximum_depth == 0
        || input.maximum_depth > 32
    {
        return Err(ExtractError::OutputLimit);
    }
    let window_end = input
        .byte_offset
        .checked_add(input.source.len() as u64)
        .ok_or(ExtractError::InvalidSpan)?;
    if input.start_line == 0
        || input.symbol_start < input.byte_offset
        || input.symbol_end > window_end
        || input.symbol_start > input.symbol_end
    {
        return Err(ExtractError::InvalidSpan);
    }
    if !matches!(
        input.language,
        SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    ) {
        output.unknowns.push("unsupported_language");
        output.partial = true;
        return Ok(output);
    }
    let mut parser = Parser::new();
    parser
        .set_language(
            &NativeGrammar::for_source_language(input.language)
                .ok_or(ExtractError::UnsupportedLanguage)?
                .language(),
        )
        .map_err(|_| ExtractError::GrammarUnavailable)?;
    let tree = match crate::native::parse_with_cancellation(
        &mut parser,
        input.source.as_bytes(),
        &mut cancelled,
    ) {
        Ok(tree) => tree,
        Err(ExtractError::Cancelled) => {
            output.partial = true;
            output.unknowns.push("cancelled");
            return Ok(output);
        }
        Err(error) => return Err(error),
    };
    if tree.root_node().has_error() {
        output.partial = true;
        output.unknowns.push("partial_parse");
    }
    let mut walker = Walker {
        input,
        output,
        cancelled: &mut cancelled,
        visited: 0,
    };
    let root = walker.find_body(tree.root_node())?;
    if let Some(root) = root {
        walker.visit(root, None, "body", 0)?;
    } else {
        walker.unknown("unsupported_or_missing_callable_body");
    }
    Ok(walker.output)
}

struct Walker<'a, 'b> {
    input: StaticTraceInput<'a>,
    output: SyntaxTrace,
    cancelled: &'b mut dyn FnMut() -> bool,
    visited: usize,
}

impl Walker<'_, '_> {
    fn unknown(&mut self, reason: &'static str) {
        self.output.partial = true;
        if !self.output.unknowns.contains(&reason) {
            self.output.unknowns.push(reason);
        }
    }
    fn poll(&mut self) -> bool {
        if (self.cancelled)() {
            self.unknown("cancelled");
            return false;
        }
        self.visited += 1;
        if self.visited > 32_768 {
            self.unknown("ast_work_limit");
            return false;
        }
        true
    }
    fn find_body<'tree>(&mut self, root: Node<'tree>) -> Result<Option<Node<'tree>>, ExtractError> {
        let mut pending = vec![(root, 0)];
        let mut selected = None;
        while let Some((node, depth)) = pending.pop() {
            if !self.poll() {
                break;
            }
            if depth > 64 {
                self.unknown("declaration_search_depth_limit");
                continue;
            }
            let start = self.input.byte_offset + node.start_byte() as u64;
            let end = self.input.byte_offset + node.end_byte() as u64;
            if start >= self.input.symbol_start
                && end <= self.input.symbol_end
                && callable(node.kind())
            {
                if selected.is_some() {
                    self.unknown("ambiguous_callable_body");
                    return Ok(None);
                }
                selected = node.child_by_field_name("body");
                continue;
            }
            if end < self.input.symbol_start || start > self.input.symbol_end {
                continue;
            }
            self.queue_declaration_children(node, depth, &mut pending)?;
        }
        Ok(selected)
    }
    fn queue_declaration_children<'tree>(
        &mut self,
        node: Node<'tree>,
        depth: usize,
        pending: &mut Vec<(Node<'tree>, usize)>,
    ) -> Result<(), ExtractError> {
        for index in (0..node.named_child_count()).rev() {
            if pending.len() >= 4096 {
                self.unknown("declaration_search_width_limit");
                break;
            }
            if let Some(child) =
                node.named_child(u32::try_from(index).map_err(|_| ExtractError::OutputLimit)?)
            {
                pending.push((child, depth + 1));
            }
        }
        Ok(())
    }
    fn emit(
        &mut self,
        node: Node<'_>,
        parent: Option<usize>,
        kind: &str,
        relation: &str,
    ) -> Result<Option<usize>, ExtractError> {
        if self.output.steps.len() >= self.input.maximum_steps {
            self.unknown("step_limit");
            return Ok(None);
        }
        let source = self
            .input
            .source
            .get(node.byte_range())
            .ok_or(ExtractError::InvalidSpan)?;
        let preview_end = source.floor_char_boundary(source.len().min(160));
        let id = self.output.steps.len();
        let call_anchor = if node.kind() == "call_expression" || node.kind() == "new_expression" {
            node.child_by_field_name("function")
                .or_else(|| node.child_by_field_name("constructor"))
                .map(|callee| {
                    let name = callee.child_by_field_name("property").unwrap_or(callee);
                    self.input.byte_offset + name.start_byte() as u64
                })
        } else {
            None
        };
        self.output.steps.push(SyntaxTraceStep {
            id,
            parent,
            kind: kind.to_owned(),
            relation: relation.to_owned(),
            span: TraceSpan {
                start_byte: self.input.byte_offset + node.start_byte() as u64,
                end_byte: self.input.byte_offset + node.end_byte() as u64,
                start_line: self
                    .input
                    .start_line
                    .checked_add(
                        u32::try_from(node.start_position().row)
                            .map_err(|_| ExtractError::InvalidSpan)?,
                    )
                    .ok_or(ExtractError::InvalidSpan)?,
                end_line: self
                    .input
                    .start_line
                    .checked_add(
                        u32::try_from(node.end_position().row)
                            .map_err(|_| ExtractError::InvalidSpan)?,
                    )
                    .ok_or(ExtractError::InvalidSpan)?,
            },
            preview: source[..preview_end].to_owned(),
            preview_truncated: preview_end != source.len(),
            call_anchor,
        });
        Ok(Some(id))
    }
    fn visit(
        &mut self,
        node: Node<'_>,
        parent: Option<usize>,
        relation: &str,
        depth: usize,
    ) -> Result<(), ExtractError> {
        if !self.poll() {
            return Ok(());
        }
        if depth > self.input.maximum_depth {
            self.unknown("syntax_depth_limit");
            return Ok(());
        }
        if self.output.steps.len() >= self.input.maximum_steps {
            self.unknown("step_limit");
            return Ok(());
        }
        let kind = node.kind();
        if callable(kind) {
            self.emit(
                node,
                parent,
                "deferred_callable",
                "declaration_or_callback_not_executed_inline",
            )?;
            return Ok(());
        }
        let visible = relation != "source_order"
            || matches!(
                kind,
                "if_statement"
                    | "switch_statement"
                    | "switch_case"
                    | "switch_default"
                    | "try_statement"
                    | "catch_clause"
                    | "finally_clause"
                    | "for_statement"
                    | "for_in_statement"
                    | "while_statement"
                    | "do_statement"
                    | "return_statement"
                    | "throw_statement"
                    | "break_statement"
                    | "continue_statement"
                    | "call_expression"
                    | "new_expression"
                    | "await_expression"
                    | "yield_expression"
                    | "assignment_expression"
                    | "augmented_assignment_expression"
                    | "update_expression"
                    | "lexical_declaration"
                    | "variable_declaration"
                    | "ternary_expression"
                    | "binary_expression"
                    | "ERROR"
                    | "with_statement"
            );
        let relation = call_relation(node, relation);
        let next_parent = if visible {
            self.emit(node, parent, kind, relation)?
        } else {
            parent
        };
        if matches!(kind, "ERROR" | "with_statement" | "yield_expression") {
            self.unknown("unsupported_syntax");
        }
        if matches!(
            kind,
            "for_statement" | "for_in_statement" | "while_statement" | "do_statement"
        ) {
            self.unknown("loop_iterations_and_feasibility_unknown");
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let branch = child_relation(node, child);
            self.visit(child, next_parent, branch, depth + 1)?;
            if self.output.steps.len() >= self.input.maximum_steps {
                self.unknown("step_limit");
                break;
            }
        }
        Ok(())
    }
}

fn call_relation<'a>(node: Node<'_>, relation: &'a str) -> &'a str {
    if node.kind() != "call_expression" {
        return relation;
    }
    if node
        .parent()
        .is_some_and(|parent| parent.kind() == "await_expression")
    {
        "awaited_call"
    } else {
        "call_evaluation_order_separate_from_source_order"
    }
}

fn callable(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "function_expression"
            | "generator_function_declaration"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
    )
}
fn child_relation(parent: Node<'_>, child: Node<'_>) -> &'static str {
    let field = |name| parent.child_by_field_name(name) == Some(child);
    match parent.kind() {
        "if_statement" | "ternary_expression" if field("condition") => "guard",
        "if_statement" | "ternary_expression" if field("consequence") => "guard_true",
        "if_statement" | "ternary_expression" if field("alternative") => "guard_false",
        "switch_statement" if field("value") => "switch_discriminant",
        "switch_case" if field("value") => "case_guard",
        "switch_case" | "switch_default" => "case_body_fallthrough_possible",
        "try_statement" if field("handler") => "exception_path",
        "try_statement" if field("finalizer") => "cleanup_on_normal_or_abrupt_exit",
        "try_statement" => "try_body",
        "for_statement" | "for_in_statement" | "while_statement" | "do_statement"
            if field("body") =>
        {
            "loop_body_repeated_order_not_linear"
        }
        "for_statement" | "for_in_statement" | "while_statement" | "do_statement" => {
            "loop_control_source_order_not_execution_order"
        }
        "return_statement" | "throw_statement" => "operand_before_abrupt_exit",
        "binary_expression" => "expression_operand_short_circuit_possible",
        "arguments" if callable(child.kind()) => "callback_argument_not_executed_inline",
        _ => "source_order",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn input(source: &str) -> StaticTraceInput<'_> {
        StaticTraceInput {
            source,
            language: SourceLanguage::TypeScript,
            byte_offset: 100,
            start_line: 7,
            symbol_start: 100,
            symbol_end: 100 + source.len() as u64,
            maximum_steps: 512,
            maximum_depth: 32,
        }
    }

    #[test]
    fn static_trace_retains_branch_guards_cleanup_and_deferred_callback_boundaries() -> TestResult {
        let source = "async function endpoint(allowed: boolean, choice: number) {\nif (!allowed) { throw Error('😀'); } else { validate(); }\ntry { for (let i=0; i<2; i++) { if (i) mutate(); } switch(choice) { case 1: commit(); break; default: return reject(); } await persist(); queueMicrotask(() => later()); } catch (error) { recover(); } finally { cleanup(); }\nreturn done();\n}";
        let output = static_syntax_trace(input(source), || false)?;
        assert!(
            output
                .steps
                .iter()
                .any(|step| step.relation == "guard" && step.preview == "(!allowed)")
        );
        let guard = output
            .steps
            .iter()
            .find(|step| step.relation == "guard")
            .ok_or("guard")?;
        let mutation = output
            .steps
            .iter()
            .find(|step| step.kind == "call_expression" && step.preview == "mutate()")
            .ok_or("mutation")?;
        assert!(guard.span.start_byte < mutation.span.start_byte);
        for relation in [
            "guard_true",
            "guard_false",
            "exception_path",
            "cleanup_on_normal_or_abrupt_exit",
            "loop_body_repeated_order_not_linear",
            "awaited_call",
        ] {
            assert!(
                output.steps.iter().any(|step| step.relation == relation),
                "missing {relation}: {output:?}"
            );
        }
        assert!(
            output
                .steps
                .iter()
                .any(|step| step.kind == "deferred_callable")
        );
        assert!(
            !output
                .steps
                .iter()
                .any(|step| step.kind == "call_expression" && step.preview == "later()")
        );
        assert!(output.steps.iter().any(|step| step.kind == "switch_case"));
        assert!(
            output
                .steps
                .iter()
                .any(|step| step.kind == "return_statement")
        );
        for step in &output.steps {
            let start = usize::try_from(step.span.start_byte - 100)?;
            let end = usize::try_from(step.span.end_byte - 100)?;
            assert!(source.get(start..end).is_some());
            assert!(source[start..end].starts_with(&step.preview));
            assert!(step.parent.is_none_or(|parent| parent < step.id));
            assert!(step.span.start_line >= 7);
        }
        Ok(())
    }

    #[test]
    fn static_trace_is_deterministic_bounded_and_explicit_about_unsupported_languages() -> TestResult
    {
        let source = "function recursive() { if (ok) recursive(); else return; }";
        let first = static_syntax_trace(input(source), || false)?;
        let second = static_syntax_trace(input(source), || false)?;
        assert_eq!(serde_json::to_vec(&first)?, serde_json::to_vec(&second)?);
        let mut bounded = input(source);
        bounded.maximum_steps = 3;
        let output = static_syntax_trace(bounded, || false)?;
        assert_eq!(output.steps.len(), 3);
        assert!(output.unknowns.contains(&"step_limit"));
        let mut unsupported = input(source);
        unsupported.language = SourceLanguage::Rust;
        assert!(
            static_syntax_trace(unsupported, || false)?
                .unknowns
                .contains(&"unsupported_language")
        );
        let cancelled = static_syntax_trace(input(source), || true)?;
        assert!(cancelled.partial && cancelled.unknowns.contains(&"cancelled"));
        assert!(cancelled.steps.is_empty());
        Ok(())
    }
}
