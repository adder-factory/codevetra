//! Scoped application boundaries reuse framework landmarks and lexical target resolution.
mod channels;
mod next;
mod react_router;

use super::{lexical, literal, text, walk};
use crate::{
    ExtractError,
    framework::{FrameworkBuilder, LandmarkInput},
};
use codevetra_domain::SymbolKind;
use std::collections::BTreeMap;
use tree_sitter::Node;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiverKind {
    Event,
    Queue,
    Observer,
    Router,
    Axios,
}
#[derive(Clone)]
struct Receiver {
    kind: ReceiverKind,
    scope: String,
    lexical_start: usize,
    lexical_end: usize,
    observer_dispatch_proven: bool,
}
struct State {
    names: BTreeMap<String, usize>,
    imports: BTreeMap<String, Option<(String, String)>>,
    receivers: BTreeMap<String, Receiver>,
    next_scope: Option<String>,
    sites: usize,
    event_import: bool,
    queue_import: bool,
}
#[derive(Clone, Copy)]
struct Site<'a, 'tree> {
    channel: &'a str,
    role: &'a str,
    scope: &'a str,
    operation: &'a str,
    adapter: &'a str,
    destination: &'a str,
    start: usize,
    end: usize,
    target: Option<Node<'tree>>,
}

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    // This is only a rejection prefilter; every admitted site is still proved by AST.
    // Avoid spending the optional pass budget on unrelated large registration tables.
    if next::scope_for_path(builder.path()).is_none()
        && !builder.import_bindings().iter().any(|binding| {
            matches!(
                binding.module_specifier.as_str(),
                "events" | "node:events" | "bull" | "react-router" | "react-router-dom"
            )
        })
        && !builder.source().contains("Set")
    {
        return Ok(());
    }
    let checkpoint = builder.bridge.checkpoint();
    match scan_inner(builder, root, lexical) {
        Ok(()) if builder.bridge.retained_output_fits() => Ok(()),
        Ok(()) | Err(ExtractError::OutputLimit) => {
            builder.bridge.omit_facts(checkpoint);
            Ok(())
        }
        Err(error) => Err(error),
    }
}
fn scan_inner(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let mut state = State {
        names: BTreeMap::new(),
        imports: BTreeMap::new(),
        receivers: BTreeMap::new(),
        next_scope: next::scope_for_path(builder.path()),
        sites: 0,
        event_import: false,
        queue_import: false,
    };
    for position in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        if let Some(symbol) = builder
            .original_symbol(position)
            .filter(|symbol| symbol.kind != SymbolKind::Import)
        {
            let name = symbol.name.clone();
            builder
                .bridge
                .reserve_working_bytes(name.len() as u64 + 64)?;
            *state.names.entry(name).or_default() += 1;
        }
    }
    for position in 0..builder.import_bindings().len() {
        builder.bridge.charge_work(1)?;
        let binding = &builder.import_bindings()[position];
        let (name, module, export) = (
            binding.local_name.clone(),
            binding.module_specifier.clone(),
            binding.imported_name.clone(),
        );
        builder
            .bridge
            .reserve_working_bytes((name.len() + module.len() + export.len() + 128) as u64)?;
        state.event_import |= matches!(module.as_str(), "events" | "node:events");
        state.queue_import |= module == "bull";
        state
            .imports
            .entry(name)
            .and_modify(|binding| *binding = None)
            .or_insert(Some((module, export)));
    }
    walk(builder, root, |builder, node| {
        collect_receiver(builder, node, &mut state)
    })?;
    walk(builder, root, |builder, node| {
        channels::prove_observer(builder, node, &mut state);
        Ok(())
    })?;
    next::registrations(builder, &mut state, lexical)?;
    react_router::scan(builder, root, &mut state, lexical)?;
    walk(builder, root, |builder, node| {
        if node.kind() == "call_expression" {
            channels::call(builder, node, &mut state, lexical)?;
            next::call(builder, node, &mut state, lexical)?;
        } else if matches!(
            node.kind(),
            "jsx_opening_element" | "jsx_self_closing_element"
        ) {
            next::markup(builder, node, &mut state, lexical)?;
        }
        Ok(())
    })
}

fn imported(
    _builder: &FrameworkBuilder<'_, '_>,
    state: &State,
    local: &str,
    modules: &[&str],
    exports: &[&str],
) -> bool {
    if state.names.get(local).copied().unwrap_or(0) != 0 {
        return false;
    }
    let Some(Some((module, export))) = state.imports.get(local) else {
        return false;
    };
    modules.contains(&module.as_str()) && exports.contains(&export.as_str())
}

fn collect_receiver(
    builder: &mut FrameworkBuilder<'_, '_>,
    node: Node<'_>,
    state: &mut State,
) -> Result<(), ExtractError> {
    if node.kind() != "variable_declarator"
        || !node
            .parent()
            .is_some_and(|parent| text(builder, parent).starts_with("const "))
    {
        return Ok(());
    }
    let Some(name) = node
        .child_by_field_name("name")
        .filter(|node| node.kind() == "identifier")
    else {
        return Ok(());
    };
    let name = text(builder, name);
    if state.names.get(name) != Some(&1) {
        return Ok(());
    }
    let Some(value) = node.child_by_field_name("value") else {
        return Ok(());
    };
    let kind = receiver_kind(builder, value, state);
    if let Some(kind) = kind {
        if state.receivers.len() >= 256 {
            return Err(ExtractError::OutputLimit);
        }
        let scope = if matches!(kind, ReceiverKind::Router | ReceiverKind::Axios) {
            state
                .next_scope
                .clone()
                .unwrap_or_else(|| scope_key("unknown", builder.path()))
        } else {
            scope_key(
                "instance",
                &format!("{}:{}:{}", builder.path(), name, node.start_byte()),
            )
        };
        builder
            .bridge
            .reserve_working_bytes((name.len() + scope.len() + 128) as u64)?;
        let (lexical_start, lexical_end) = binding_scope(builder, node)?;
        state.receivers.insert(
            name.to_owned(),
            Receiver {
                kind,
                observer_dispatch_proven: false,
                scope,
                lexical_start,
                lexical_end,
            },
        );
    }
    Ok(())
}

fn receiver_kind(
    builder: &FrameworkBuilder<'_, '_>,
    value: Node<'_>,
    state: &State,
) -> Option<ReceiverKind> {
    if value.kind() == "new_expression" {
        let constructor = value.child_by_field_name("constructor")?;
        let constructor = text(builder, constructor);
        if imported(
            builder,
            state,
            constructor,
            &["events", "node:events"],
            &["EventEmitter", "default"],
        ) {
            Some(ReceiverKind::Event)
        } else if imported(
            builder,
            state,
            constructor,
            &["bull"],
            &["default", "Queue"],
        ) {
            Some(ReceiverKind::Queue)
        } else if constructor == "Set"
            && !state.names.contains_key("Set")
            && !state.imports.contains_key("Set")
        {
            Some(ReceiverKind::Observer)
        } else {
            None
        }
    } else {
        next::receiver(builder, value, state)
    }
}

fn binding_scope(
    builder: &mut FrameworkBuilder<'_, '_>,
    node: Node<'_>,
) -> Result<(usize, usize), ExtractError> {
    let mut ancestor = node.parent();
    for _ in 0..64 {
        builder.bridge.charge_work(1)?;
        let Some(scope) = ancestor else {
            return Ok((node.start_byte(), node.end_byte()));
        };
        if matches!(
            scope.kind(),
            "statement_block" | "program" | "for_statement" | "for_in_statement" | "switch_body"
        ) {
            return Ok((scope.start_byte(), scope.end_byte()));
        }
        ancestor = scope.parent();
    }
    Err(ExtractError::OutputLimit)
}

fn scope_key(kind: &str, value: &str) -> String {
    blake3::hash(format!("boundary-scope-v1:{kind}:{value}").as_bytes()).to_hex()[..32].to_owned()
}
fn argument(call: Node<'_>, index: u32) -> Option<Node<'_>> {
    call.child_by_field_name("arguments")?.named_child(index)
}
fn member<'a>(builder: &'a FrameworkBuilder<'_, '_>, call: Node<'_>) -> Option<(&'a str, &'a str)> {
    let function = call.child_by_field_name("function")?;
    let object = function.child_by_field_name("object")?;
    let property = function.child_by_field_name("property")?;
    (object.kind() == "identifier" && property.kind() == "property_identifier")
        .then(|| (text(builder, object), text(builder, property)))
}
fn add(
    builder: &mut FrameworkBuilder<'_, '_>,
    state: &mut State,
    lexical: &lexical::Index,
    site: Site<'_, '_>,
) -> Result<(), ExtractError> {
    let target = site
        .target
        .filter(|node| site.role == "registration" && node.kind() == "identifier");
    let binding = match target {
        Some(node) => lexical.resolution(builder, node)?,
        None => None,
    };
    let target_name = target.map(|node| text(builder, node).to_owned());
    add_resolved(
        builder,
        state,
        site,
        target
            .zip(binding.as_ref())
            .zip(target_name.as_deref())
            .map(|((node, binding), name)| {
                (
                    name,
                    binding.resolution_name.as_deref(),
                    node.start_byte(),
                    node.end_byte(),
                )
            }),
    )
}

fn add_resolved(
    builder: &mut FrameworkBuilder<'_, '_>,
    state: &mut State,
    mut site: Site<'_, '_>,
    target: Option<(&str, Option<&str>, usize, usize)>,
) -> Result<(), ExtractError> {
    if site.destination.len() > 512 || site.destination.contains(['\n', '\r', '#']) {
        site.role = "unknown";
        site.destination = "<unsupported-literal>";
    }
    if state.sites >= 512 {
        return Err(ExtractError::OutputLimit);
    }
    let identity = format!(
        "boundary-v1::{}::{}::{}::{}::{}::{}",
        site.channel, site.role, site.scope, site.operation, site.adapter, site.destination
    );
    builder.add_contained_landmark(LandmarkInput {
        kind: SymbolKind::Resource,
        name: site.destination.to_owned(),
        identity,
        start: site.start,
        end: site.end,
        body_search_text: format!(
            "{} {} {} boundary {} {}; possible static relationship, not runtime execution",
            site.adapter, site.channel, site.operation, site.role, site.destination
        ),
        target,
    })?;
    state.sites += 1;
    Ok(())
}
