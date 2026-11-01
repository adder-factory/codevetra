//! Initial same-origin Next App Router slice. Unknown base/configuration stays explicit.
use super::{
    ExtractError, FrameworkBuilder, Node, ReceiverKind, Site, State, SymbolKind, add, argument,
    imported, lexical, literal, member, scope_key, text,
};

pub(super) fn scope_for_path(path: &str) -> Option<String> {
    let normalized = format!("/{path}");
    let index = normalized
        .find("/app/")
        .or_else(|| normalized.find("/pages/"))?;
    let prefix = normalized[..index].trim_start_matches('/');
    let prefix = if prefix == "src" {
        ""
    } else {
        prefix.strip_suffix("/src").unwrap_or(prefix)
    };
    Some(scope_key("next-application", prefix))
}

pub(super) fn registrations(
    builder: &mut FrameworkBuilder<'_, '_>,
    state: &mut State,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let Some(scope) = state.next_scope.clone() else {
        return Ok(());
    };
    let path = format!("/{}", builder.path());
    let Some((_, remainder)) = path.split_once("/app/") else {
        return Ok(());
    };
    let (directory, filename) = remainder.rsplit_once('/').unwrap_or(("", remainder));
    let http = filename.starts_with("route.");
    if !http && !filename.starts_with("page.") {
        return Ok(());
    }
    let route = route_path(directory);
    let channel = if http { "http" } else { "navigation" };
    let role = if route.is_some() {
        "registration"
    } else {
        "unknown"
    };
    for position in 0..builder.original_symbol_count() {
        builder.bridge.charge_work(1)?;
        let Some((kind, export, name, start, end)) =
            builder.original_symbol(position).map(|symbol| {
                (
                    symbol.kind,
                    symbol.export,
                    symbol.name.clone(),
                    symbol.span.start_byte(),
                    symbol.span.end_byte(),
                )
            })
        else {
            continue;
        };
        if !matches!(
            kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
        ) {
            continue;
        }
        let eligible = registration_export(http, export.exported, export.default_export, &name);
        if !eligible {
            continue;
        }
        add(
            builder,
            state,
            lexical,
            Site {
                channel,
                role,
                scope: &scope,
                operation: if http { &name } else { "route" },
                adapter: "next-app-v1",
                destination: route.as_deref().unwrap_or("<unsupported-route-pattern>"),
                start: usize::try_from(start).map_err(|_| ExtractError::InvalidSpan)?,
                end: usize::try_from(end).map_err(|_| ExtractError::InvalidSpan)?,
                target: None,
            },
        )?;
    }
    Ok(())
}
fn registration_export(http: bool, exported: bool, default_export: bool, name: &str) -> bool {
    if http {
        exported
            && matches!(
                name,
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
            )
    } else {
        default_export
    }
}

fn route_path(directory: &str) -> Option<String> {
    let mut segments = Vec::new();
    for segment in directory.split('/').filter(|segment| !segment.is_empty()) {
        if segment.starts_with('(') && segment.ends_with(')') {
            continue;
        }
        if segment.starts_with('@') || segment.contains("...") {
            return None;
        }
        if let Some(parameter) = segment
            .strip_prefix('[')
            .and_then(|part| part.strip_suffix(']'))
        {
            if parameter.is_empty() || parameter.contains(['[', ']']) {
                return None;
            }
            segments.push(format!(":{parameter}"));
        } else {
            segments.push(segment.to_owned());
        }
    }
    Some(format!("/{}", segments.join("/")))
}

pub(super) fn receiver(
    builder: &FrameworkBuilder<'_, '_>,
    value: Node<'_>,
    state: &State,
) -> Option<ReceiverKind> {
    if value.kind() != "call_expression" {
        return None;
    }
    let function = value.child_by_field_name("function")?;
    if function.kind() == "identifier"
        && imported(
            builder,
            state,
            text(builder, function),
            &["next/navigation", "next/router"],
            &["useRouter"],
        )
    {
        return Some(ReceiverKind::Router);
    }
    let (owner, method) = member(builder, value)?;
    if method != "create" || !imported(builder, state, owner, &["axios"], &["default"]) {
        return None;
    }
    let options = argument(value, 0)?;
    (options.named_child_count() == 1
        && field(builder, options, "baseURL")
            .ok()
            .flatten()
            .and_then(|node| literal(builder, node))
            == Some("/"))
    .then_some(ReceiverKind::Axios)
}

pub(super) fn call(
    builder: &mut FrameworkBuilder<'_, '_>,
    call: Node<'_>,
    state: &mut State,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(());
    };
    let direct = text(builder, function);
    let unbound_fetch = direct == "fetch"
        && !state.names.contains_key("fetch")
        && !state.imports.contains_key("fetch");
    let redirect = function.kind() == "identifier"
        && imported(
            builder,
            state,
            direct,
            &["next/navigation"],
            &["redirect", "permanentRedirect"],
        );
    let receiver = member(builder, call).and_then(|(owner, operation)| {
        state
            .receivers
            .get(owner)
            .filter(|binding| {
                binding.lexical_start <= call.start_byte() && call.end_byte() <= binding.lexical_end
            })
            .cloned()
            .map(|binding| (binding, operation.to_owned()))
    });
    let (channel, operation, eligible) = if unbound_fetch {
        ("http", fetch_method(builder, call), client_module(builder))
    } else if redirect {
        ("navigation", Some("redirect".to_owned()), true)
    } else if let Some((binding, operation)) = receiver {
        match binding.kind {
            ReceiverKind::Router if matches!(operation.as_str(), "push" | "replace") => {
                ("navigation", Some(operation), true)
            }
            ReceiverKind::Axios
                if matches!(
                    operation.as_str(),
                    "get" | "post" | "put" | "patch" | "delete" | "head" | "options"
                ) =>
            {
                (
                    "http",
                    Some(operation.to_ascii_uppercase()),
                    client_module(builder)
                        && argument(
                            call,
                            if matches!(operation.as_str(), "post" | "put" | "patch") {
                                2
                            } else {
                                1
                            },
                        )
                        .is_none(),
                )
            }
            _ => return Ok(()),
        }
    } else {
        return Ok(());
    };
    let destination = argument(call, 0)
        .and_then(|node| literal(builder, node))
        .map(str::to_owned);
    source_site(
        builder,
        state,
        lexical,
        SourceSite {
            channel,
            operation: operation.as_deref().unwrap_or("UNKNOWN"),
            destination: destination.as_deref(),
            node: call,
            eligible: eligible && operation.is_some(),
        },
    )
}

#[derive(Clone, Copy)]
struct SourceSite<'a, 'tree> {
    channel: &'a str,
    operation: &'a str,
    destination: Option<&'a str>,
    node: Node<'tree>,
    eligible: bool,
}
fn source_site(
    builder: &mut FrameworkBuilder<'_, '_>,
    state: &mut State,
    lexical: &lexical::Index,
    input: SourceSite<'_, '_>,
) -> Result<(), ExtractError> {
    let scope = state
        .next_scope
        .clone()
        .unwrap_or_else(|| scope_key("unknown-next-scope", builder.path()));
    let (destination, role) = match input.destination {
        Some(value) if value.starts_with("//") || value.contains("://") => {
            ("<external>", "external")
        }
        Some(value) if value.starts_with('/') && input.eligible && state.next_scope.is_some() => {
            (value.split(['?', '#']).next().unwrap_or(value), "source")
        }
        _ => ("<dynamic-or-unproven-base>", "unknown"),
    };
    add(
        builder,
        state,
        lexical,
        Site {
            channel: input.channel,
            role,
            scope: &scope,
            operation: input.operation,
            adapter: "next-app-v1",
            destination,
            start: input.node.start_byte(),
            end: input.node.end_byte(),
            target: None,
        },
    )
}

fn fetch_method(builder: &FrameworkBuilder<'_, '_>, call: Node<'_>) -> Option<String> {
    let Some(options) = argument(call, 1) else {
        return Some("GET".to_owned());
    };
    match field(builder, options, "method") {
        Ok(None) => Some("GET".to_owned()),
        Ok(Some(method)) => literal(builder, method)
            .map(str::to_ascii_uppercase)
            .filter(|method| {
                matches!(
                    method.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
                )
            }),
        Err(()) => None,
    }
}
fn field<'tree>(
    builder: &FrameworkBuilder<'_, '_>,
    object: Node<'tree>,
    key: &str,
) -> Result<Option<Node<'tree>>, ()> {
    if object.kind() != "object" || object.named_child_count() > 64 {
        return Err(());
    }
    let mut found = None;
    let mut cursor = object.walk();
    for pair in object.named_children(&mut cursor) {
        if pair.kind() != "pair" {
            return Err(());
        }
        let name = pair.child_by_field_name("key").ok_or(())?;
        let name = if name.kind() == "property_identifier" {
            text(builder, name)
        } else {
            literal(builder, name).ok_or(())?
        };
        if name == key {
            if found.is_some() {
                return Err(());
            }
            found = pair.child_by_field_name("value");
        }
    }
    Ok(found)
}
fn client_module(builder: &FrameworkBuilder<'_, '_>) -> bool {
    builder
        .syntax_root()
        .and_then(|root| root.named_child(0))
        .filter(|node| node.kind() == "expression_statement")
        .and_then(|node| node.named_child(0))
        .and_then(|node| literal(builder, node))
        == Some("use client")
}

pub(super) fn markup(
    builder: &mut FrameworkBuilder<'_, '_>,
    node: Node<'_>,
    state: &mut State,
    lexical: &lexical::Index,
) -> Result<(), ExtractError> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let tag = text(builder, name);
    if tag != "a" && !imported(builder, state, tag, &["next/link"], &["default"]) {
        return Ok(());
    }
    let mut destination = None;
    let mut uncertain = node.named_child_count() > 128;
    let mut cursor = node.walk();
    for attribute in node.named_children(&mut cursor).take(128) {
        if attribute.kind() == "jsx_expression" {
            uncertain = true;
        }
        if attribute.kind() != "jsx_attribute" {
            continue;
        }
        if attribute
            .named_child(0)
            .is_some_and(|name| text(builder, name) == "href")
        {
            if destination.is_some() {
                uncertain = true;
            }
            destination = attribute.named_child(1).and_then(|value| {
                literal(builder, value).or_else(|| {
                    value
                        .named_child(0)
                        .and_then(|child| literal(builder, child))
                })
            });
        }
    }
    let destination = destination.map(str::to_owned);
    source_site(
        builder,
        state,
        lexical,
        SourceSite {
            channel: "navigation",
            operation: "link",
            destination: destination.as_deref(),
            node,
            eligible: !uncertain,
        },
    )
}
