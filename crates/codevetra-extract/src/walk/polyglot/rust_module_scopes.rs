//! Exact conventional file-module declarations inside inline module blocks.
use crate::{ExtractError, ExtractedImportBinding, ImportBindingKind, walk::ExtractionBuilder};
use tree_sitter::Node;

const MAX_ADJACENT_ATTRIBUTE_NODES: usize = 128;

/// A single literal path on a top-level external module. Unsupported adjacent
/// attributes invalidate the whole binding; comments never terminate the scan.
pub(super) fn explicit_file(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<Option<String>, ExtractError> {
    let mut previous = node.prev_named_sibling();
    let mut path = None;
    for _ in 0..MAX_ADJACENT_ATTRIBUTE_NODES {
        builder.context.ensure_active()?;
        let Some(sibling) = previous else {
            return Ok(path);
        };
        previous = sibling.prev_named_sibling();
        if matches!(sibling.kind(), "line_comment" | "block_comment") {
            continue;
        }
        if sibling.kind() != "attribute_item" {
            return Ok(path);
        }
        if sibling.has_error() || path.is_some() || sibling.byte_range().len() > 1_100 {
            return Ok(None);
        }
        let text = builder.context.text(sibling);
        let Some(value) = literal_path(text) else {
            return Ok(None);
        };
        builder.context.budget.ensure_string_length(value.len())?;
        path = Some(value.to_owned());
    }
    Ok(None)
}

fn literal_path(attribute: &str) -> Option<&str> {
    let value = attribute
        .strip_prefix("#[")?
        .strip_suffix(']')?
        .trim()
        .strip_prefix("path")?
        .trim_start()
        .strip_prefix('=')?
        .trim()
        .strip_prefix('"')?
        .strip_suffix('"')?;
    (!value.is_empty()
        && value.len() <= 1_024
        && !value.starts_with('/')
        && !value.contains(['\\', '"', ':'])
        && !value.chars().any(char::is_control))
    .then_some(value)
}

pub(super) fn file_module(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "mod_item"
        || node.has_error()
        || node.child_by_field_name("body").is_some()
        || !conventional_attributes(builder, node, false)?
        || !inline_scope(builder, node)?
    {
        return Ok(());
    }
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let imported_name = builder.context.owned_text(name)?;
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-inline-file-module>".to_owned(),
        imported_name,
        local_name: builder.qualifiers.join("::"),
        span: crate::walk::syntax::span_for(node)?,
    })
}

pub(super) fn module_glob(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(), ExtractError> {
    if node.kind() != "use_declaration" || !inline_scope(builder, node)? {
        return Ok(());
    }
    let Some(argument) = node.child_by_field_name("argument") else {
        return Ok(());
    };
    let mut path = builder.context.owned_text(argument)?;
    if !path.contains('*') {
        return Ok(());
    }
    if node.has_error() || !conventional_attributes(builder, node, false)? {
        "*".clone_into(&mut path);
    }
    builder.emit_import_binding(ExtractedImportBinding {
        kind: ImportBindingKind::Namespace,
        module_specifier: "<rust-scoped-module-glob>".to_owned(),
        imported_name: path,
        local_name: "*".to_owned(),
        span: crate::walk::syntax::span_for(node)?,
    })
}

fn inline_scope(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<bool, ExtractError> {
    let mut parent = node.parent();
    let mut inline = false;
    while let Some(scope) = parent {
        builder.context.ensure_active()?;
        match scope.kind() {
            "source_file" => return Ok(inline),
            "declaration_list" => {}
            "mod_item" => {
                if scope.has_error() || !conventional_attributes(builder, scope, true)? {
                    return Ok(false);
                }
                inline = true;
            }
            _ => return Ok(false),
        }
        parent = scope.parent();
    }
    Ok(false)
}

pub(super) fn conventional_attributes(
    builder: &mut ExtractionBuilder<'_, '_>,
    node: Node<'_>,
    allow_cfg: bool,
) -> Result<bool, ExtractError> {
    let mut previous = node.prev_named_sibling();
    let mut remaining = MAX_ADJACENT_ATTRIBUTE_NODES;
    while let Some(sibling) = previous {
        builder.context.ensure_active()?;
        if remaining == 0 {
            return Ok(false);
        }
        remaining -= 1;
        if matches!(sibling.kind(), "line_comment" | "block_comment") {
            previous = sibling.prev_named_sibling();
            continue;
        }
        if sibling.kind() != "attribute_item" {
            return Ok(true);
        }
        if !allow_cfg
            || !crate::walk::rust_test_attributes::well_formed_cfg(
                sibling,
                builder.context.source(),
            )
        {
            return Ok(false);
        }
        previous = sibling.prev_named_sibling();
    }
    Ok(true)
}
