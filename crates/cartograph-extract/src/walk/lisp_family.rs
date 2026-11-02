//! Clojure and Common Lisp structural extraction.
//!
//! Both grammars parse every form as a plain list, so declarations, imports,
//! and calls are recognized from the list head symbol. A head that is not a
//! declaration, import, binding, or special form is an invocation of that
//! symbol. Quoted data is never evaluated, so it never declares or calls.

mod clojure;
mod common_lisp;

use cartograph_domain::SourceLanguage;
use tree_sitter::Node;

use crate::ExtractError;

use super::{
    ExtractionBuilder,
    syntax::{descendants, named_children},
};

/// Index of the first form after a list's head: a definition's name, a binding
/// form's bindings, or an import form's first argument.
const AFTER_HEAD: usize = 1;
/// Index of the first form after a list's head and name: a definition's body.
const AFTER_NAME: usize = 2;
/// Child nodes of a list that are not among its forms.
const NON_FORM_KINDS: &[&str] = &[
    "comment",
    "block_comment",
    "dis_expr",
    "meta_lit",
    "old_meta_lit",
];
/// Reader forms whose contents are evaluated inside a syntax-quoted template.
const UNQUOTE_KINDS: &[&str] = &["unquoting_lit", "unquote_splicing_lit"];

/// Dispatch one Clojure or Common Lisp node; `false` lets the walker descend.
pub(super) fn visit_declaration(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    depth: usize,
) -> Result<bool, ExtractError> {
    match (builder.context.snapshot.language(), node.kind()) {
        // Quoted data and discarded forms are never evaluated, so nothing in
        // them declares or calls anything. Clojure reader metadata annotates
        // a form (type hints, docs, flags); it is read structurally where it
        // matters and is never walked as code.
        (_, "quoting_lit" | "dis_expr")
        | (SourceLanguage::Clojure, "meta_lit" | "old_meta_lit") => Ok(true),
        (_, "syn_quoting_lit") => {
            visit_syntax_quote(builder, node, depth)?;
            Ok(true)
        }
        // `#(head args...)` is an anonymous function whose body is one list
        // form, so its head is classified exactly like a list's.
        (SourceLanguage::Clojure, "list_lit" | "anon_fn_lit") => {
            clojure::visit_list(builder, node, depth)
        }
        (SourceLanguage::CommonLisp, "list_lit") => common_lisp::visit_list(builder, node, depth),
        (SourceLanguage::CommonLisp, "defun") => common_lisp::visit_defun(builder, node, depth),
        _ => Ok(false),
    }
}

/// Direct child forms of a list, without comments, discarded forms, or the
/// reader metadata that may precede a list's head.
fn form_children(node: Node<'_>) -> Vec<Node<'_>> {
    named_children(node)
        .filter(|child| !NON_FORM_KINDS.contains(&child.kind()))
        .collect()
}

/// A syntax-quoted template is data except for its unquoted forms, which the
/// macro evaluates; only those are visited.
fn visit_syntax_quote(
    builder: &mut ExtractionBuilder<'_, '_>,
    template: Node<'_>,
    depth: usize,
) -> Result<(), ExtractError> {
    let child_depth = depth.saturating_add(1);
    for node in descendants(template) {
        builder.context.ensure_active()?;
        if !UNQUOTE_KINDS.contains(&node.kind()) || nested_in_unquote(node, template) {
            continue;
        }
        for evaluated in named_children(node) {
            builder.visit(evaluated, child_depth)?;
        }
    }
    Ok(())
}

/// Whether an unquote lies inside another unquote of the same template, which
/// the outer unquote's own visit already covers, or inside a discarded form,
/// which is never read.
fn nested_in_unquote(unquote: Node<'_>, template: Node<'_>) -> bool {
    let mut current = unquote.parent();
    while let Some(ancestor) = current {
        if ancestor.id() == template.id() {
            return false;
        }
        if ancestor.kind() == "dis_expr" || UNQUOTE_KINDS.contains(&ancestor.kind()) {
            return true;
        }
        current = ancestor.parent();
    }
    false
}
