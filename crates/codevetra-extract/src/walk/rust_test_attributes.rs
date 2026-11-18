//! Test ownership needs a recognized attribute, not a substring in a cfg expression.

use tree_sitter::Node;

use crate::ExtractError;

const MAX_CFG_DEPTH: usize = 16;
const MAX_CFG_TOKENS: u32 = 128;
const MAX_CFG_NODES: usize = 4096;

/// Validate a bounded ordinary cfg without evaluating the selected build target.
pub(super) fn well_formed_cfg(node: Node<'_>, source: &str) -> bool {
    if node.has_error() || node.descendant_count() > MAX_CFG_NODES {
        return false;
    }
    let Some(attribute) = node.named_child(0) else {
        return false;
    };
    let Some(path) = attribute.named_child(0) else {
        return false;
    };
    path.kind() == "identifier"
        && source.get(path.start_byte()..path.end_byte()) == Some("cfg")
        && attribute
            .child_by_field_name("arguments")
            .is_some_and(|group| {
                CfgParser { source }
                    .requires_test(group, CfgMode::Single, 0)
                    .is_some()
            })
}

pub(super) fn marks_test(node: Node<'_>, source: &str) -> Result<bool, ExtractError> {
    if node.has_error() {
        return Ok(false);
    }
    let compact = super::compact_rust_attribute(source, node)?;
    let Some(attribute) = node.named_child(0) else {
        return Ok(false);
    };
    if compact.starts_with("#[cfg(") {
        return Ok(attribute
            .child_by_field_name("arguments")
            .and_then(|group| CfgParser { source }.requires_test(group, CfgMode::Single, 0))
            .unwrap_or(false));
    }
    if matches!(compact.as_str(), "#[test]" | "#[rstest]" | "#[kani::proof]") {
        return Ok(true);
    }
    Ok([
        "tokio::test",
        "async_std::test",
        "actix_rt::test",
        "test_case",
        "rstest",
        "kani::proof_for_contract",
    ]
    .iter()
    .any(|name| {
        compact
            .strip_prefix("#[")
            .and_then(|value| value.strip_prefix(name))
            .is_some_and(|suffix| suffix == "]" || suffix.starts_with('('))
    }))
}

#[derive(Clone, Copy)]
enum CfgMode {
    Single,
    All,
    Any,
    Not,
}

impl CfgMode {
    fn combine(self, previous: bool, next: bool) -> bool {
        match self {
            Self::Single | Self::All => previous || next,
            Self::Any => previous && next,
            Self::Not => false,
        }
    }
}

struct CfgParser<'source> {
    source: &'source str,
}

impl CfgParser<'_> {
    fn requires_test(&self, group: Node<'_>, mode: CfgMode, depth: usize) -> Option<bool> {
        let count = group.child_count();
        if depth > MAX_CFG_DEPTH || !(2..=MAX_CFG_TOKENS).contains(&count) {
            return None;
        }
        if group.child(0)?.kind() != "(" || group.child(count - 1)?.kind() != ")" {
            return None;
        }
        let mut first = 1;
        let mut predicates = 0;
        let mut required = matches!(mode, CfgMode::Any);
        for end in 1..count {
            let token = group.child(end)?;
            // Nested token trees and string literals are atomic AST nodes. A comma
            // inside either one cannot turn an ordinary feature into a test guard.
            if token.kind() != "," && end != count - 1 {
                continue;
            }
            if first < end {
                let next = self.predicate_requires_test(group, first..end, depth)?;
                required = mode.combine(required, next);
                predicates += 1;
            } else if end != count - 1 {
                return None;
            }
            first = end + 1;
        }
        if matches!(mode, CfgMode::Single | CfgMode::Not) && predicates != 1 {
            return None;
        }
        Some(required && predicates > 0)
    }

    fn predicate_requires_test(
        &self,
        group: Node<'_>,
        range: std::ops::Range<u32>,
        depth: usize,
    ) -> Option<bool> {
        let name = group.child(range.start)?;
        let text = self
            .source
            .get(name.start_byte()..name.end_byte())
            .unwrap_or_default();
        if name.kind() != "identifier" {
            return None;
        }
        if range.len() == 1 {
            return Some(text == "test");
        }
        if range.len() == 3 && group.child(range.start + 1)?.kind() == "=" {
            return matches!(
                group.child(range.start + 2)?.kind(),
                "string_literal" | "raw_string_literal"
            )
            .then_some(false);
        }
        if range.len() != 2 {
            return None;
        }
        let mode = match text {
            "all" => CfgMode::All,
            "any" => CfgMode::Any,
            // Validate negation syntax, but never infer test ownership from it.
            "not" => CfgMode::Not,
            _ => return None,
        };
        let arguments = group.child(range.start + 1)?;
        if arguments.kind() != "token_tree" {
            return None;
        }
        self.requires_test(arguments, mode, depth + 1)
    }
}
