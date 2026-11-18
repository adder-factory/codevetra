//! Conservative syntactic recognition of a whole-body Rust value mapping.

use tree_sitter::Node;

use crate::ExtractError;

const MAX_MAPPING_NODES: usize = 4096;
const MAX_VALUE_DEPTH: usize = 16;

/// Preserve syntactic pattern domains without making changed outputs evade near clones.
pub(super) fn clone_discriminator(
    body: Node<'_>,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<codevetra_domain::ContentDigest>, ExtractError> {
    if !is_flat_mapping(body, cancelled)? {
        return Ok(None);
    }
    let mut syntax = MappingSyntax {
        remaining: MAX_MAPPING_NODES,
        cancelled,
    };
    let Some(expression) = syntax.single_expression(body)? else {
        return Ok(None);
    };
    let mut hasher = blake3::Hasher::new_derive_key("codevetra.v4.mapping-patterns.v1");
    if let Some(callee) = expression.child_by_field_name("function") {
        let digest = super::structural_digest(callee, source, syntax.cancelled)?;
        super::hash_field(&mut hasher, digest.as_str().as_bytes());
    }
    let Some(mapping) = syntax.mapping_expression(expression)? else {
        return Ok(None);
    };
    let Some(arms) = mapping.child_by_field_name("body") else {
        return Ok(None);
    };
    for arm in super::children(arms).filter(tree_sitter::Node::is_named) {
        if !syntax.admit(arm)? {
            return Ok(None);
        }
        if matches!(arm.kind(), "line_comment" | "block_comment") {
            continue;
        }
        let Some(pattern) = arm.child_by_field_name("pattern") else {
            return Ok(None);
        };
        let digest = super::structural_digest(pattern, source, syntax.cancelled)?;
        super::hash_field(&mut hasher, digest.as_str().as_bytes());
    }
    Ok(Some(codevetra_domain::ContentDigest::from_bytes(
        *hasher.finalize().as_bytes(),
    )))
}

pub(super) fn is_flat_mapping(
    body: Node<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<bool, ExtractError> {
    if body.has_error() {
        return Ok(false);
    }
    let mut syntax = MappingSyntax {
        remaining: MAX_MAPPING_NODES,
        cancelled,
    };
    let Some(expression) = syntax.single_expression(body)? else {
        return Ok(false);
    };
    let Some(expression) = syntax.mapping_expression(expression)? else {
        return Ok(false);
    };
    let Some(value) = expression.child_by_field_name("value") else {
        return Ok(false);
    };
    if !syntax.value(value, 0)? {
        return Ok(false);
    }
    let Some(arms) = expression.child_by_field_name("body") else {
        return Ok(false);
    };
    let mut count = 0;
    for arm in super::children(arms).filter(tree_sitter::Node::is_named) {
        if !syntax.admit(arm)? {
            return Ok(false);
        }
        if matches!(arm.kind(), "line_comment" | "block_comment") {
            continue;
        }
        if !syntax.arm(arm)? {
            return Ok(false);
        }
        count += 1;
    }
    Ok(count > 0)
}

struct MappingSyntax<'cancel> {
    remaining: usize,
    cancelled: &'cancel mut dyn FnMut() -> bool,
}

impl MappingSyntax<'_> {
    /// One uniform call after selecting data adds no arm-specific call site.
    /// This is structural evidence, not a claim that the callee is pure.
    fn mapping_expression<'tree>(
        &mut self,
        expression: Node<'tree>,
    ) -> Result<Option<Node<'tree>>, ExtractError> {
        if expression.kind() == "match_expression" {
            return Ok(Some(expression));
        }
        if expression.kind() != "call_expression" || !self.admit(expression)? {
            return Ok(None);
        }
        let Some(callee) = expression.child_by_field_name("function") else {
            return Ok(None);
        };
        if !matches!(callee.kind(), "identifier" | "scoped_identifier") || !self.value(callee, 0)? {
            return Ok(None);
        }
        let Some(arguments) = expression.child_by_field_name("arguments") else {
            return Ok(None);
        };
        if !self.admit(arguments)? {
            return Ok(None);
        }
        let mut mapping = None;
        for argument in super::children(arguments).filter(tree_sitter::Node::is_named) {
            if !self.admit(argument)? {
                return Ok(None);
            }
            if matches!(argument.kind(), "line_comment" | "block_comment") {
                continue;
            }
            if argument.kind() != "match_expression" || mapping.replace(argument).is_some() {
                return Ok(None);
            }
        }
        Ok(mapping)
    }

    fn single_expression<'tree>(
        &mut self,
        body: Node<'tree>,
    ) -> Result<Option<Node<'tree>>, ExtractError> {
        let mut expression = None;
        for child in super::children(body).filter(tree_sitter::Node::is_named) {
            if !self.admit(child)? {
                return Ok(None);
            }
            if matches!(
                child.kind(),
                "line_comment" | "block_comment" | "use_declaration"
            ) {
                continue;
            }
            if expression.replace(child).is_some() {
                return Ok(None);
            }
        }
        Ok(expression.and_then(|node| {
            if node.kind() == "expression_statement" && node.named_child_count() == 1 {
                node.named_child(0)
            } else {
                Some(node)
            }
        }))
    }

    fn admit(&mut self, node: Node<'_>) -> Result<bool, ExtractError> {
        if (self.cancelled)() {
            return Err(ExtractError::Cancelled);
        }
        if self.remaining == 0 || node.is_missing() || node.has_error() {
            return Ok(false);
        }
        self.remaining -= 1;
        Ok(true)
    }

    fn arm(&mut self, arm: Node<'_>) -> Result<bool, ExtractError> {
        if arm.kind() != "match_arm" || arm.named_child_count() != 2 {
            return Ok(false);
        }
        let Some(pattern) = arm.child_by_field_name("pattern") else {
            return Ok(false);
        };
        if pattern.child_by_field_name("condition").is_some() || pattern.named_child_count() != 1 {
            return Ok(false);
        }
        let Some(pattern) = pattern.named_child(0) else {
            return Ok(false);
        };
        // No bindings, destructuring, range/or patterns, guards or wildcard.
        if pattern.kind() != "scoped_identifier" || !self.value(pattern, 0)? {
            return Ok(false);
        }
        match arm.child_by_field_name("value") {
            Some(value) => self.value(value, 0),
            None => Ok(false),
        }
    }

    fn value(&mut self, node: Node<'_>, depth: usize) -> Result<bool, ExtractError> {
        if depth > MAX_VALUE_DEPTH || !self.admit(node)? {
            return Ok(false);
        }
        match node.kind() {
            "identifier" | "self" | "super" | "crate" | "field_identifier" | "primitive_type"
            | "integer_literal" | "float_literal" | "boolean_literal" | "char_literal"
            | "string_literal" | "raw_string_literal" => Ok(true),
            "scoped_identifier" | "field_expression" | "type_cast_expression" => {
                for child in super::children(node).filter(tree_sitter::Node::is_named) {
                    if !self.value(child, depth + 1)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            // Calls, macros, guards, arithmetic, indexing, blocks and all
            // control flow remain ordinary complexity evidence.
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::{clone_discriminator, is_flat_mapping};
    use crate::ExtractError;

    #[test]
    fn uniform_call_checks_cancellation_and_keeps_the_mapping_node_budget() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap_or_else(|_| panic!("Rust grammar"));
        for (arms, expected) in [(20, true), (1000, false)] {
            let mut entries = String::new();
            for index in 0..arms {
                assert!(write!(entries, "Kind::V{index} => data.value, ").is_ok());
            }
            let source = format!(
                "fn mapping(kind: Kind, data: Data) {{ convert(match kind {{ {entries} }}) }}"
            );
            let tree = parser
                .parse(&source, None)
                .unwrap_or_else(|| panic!("mapping tree"));
            let body = tree
                .root_node()
                .named_child(0)
                .and_then(|function| function.child_by_field_name("body"))
                .unwrap_or_else(|| panic!("mapping body"));
            let mut polls = 0;
            assert_eq!(
                is_flat_mapping(body, &mut || {
                    polls += 1;
                    false
                }),
                Ok(expected)
            );
            assert!(polls <= super::MAX_MAPPING_NODES + 1);
            if expected {
                for stop in 0..polls {
                    let mut reached = 0;
                    let result = is_flat_mapping(body, &mut || {
                        reached += 1;
                        reached > stop
                    });
                    assert_eq!(result, Err(ExtractError::Cancelled));
                    assert_eq!(reached, stop + 1);
                }
                let mut digest_polls = 0;
                let digest = clone_discriminator(body, &source, &mut || {
                    digest_polls += 1;
                    false
                });
                assert!(matches!(digest, Ok(Some(_))));
                for stop in 0..digest_polls {
                    let mut reached = 0;
                    let result = clone_discriminator(body, &source, &mut || {
                        reached += 1;
                        reached > stop
                    });
                    assert_eq!(result, Err(ExtractError::Cancelled));
                    assert_eq!(reached, stop + 1);
                }
            } else {
                assert_eq!(clone_discriminator(body, &source, &mut || false), Ok(None));
            }
        }
    }
}
