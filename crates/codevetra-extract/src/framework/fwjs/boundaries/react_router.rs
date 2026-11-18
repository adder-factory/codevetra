//! JSX `BrowserRouter`: explicit router ownership, nested literal routes, and React.lazy modules.
use super::{
    ExtractError, FrameworkBuilder, Node, Site, State, argument, binding_scope, literal, scope_key,
    text, walk,
};
use std::collections::{BTreeMap, BTreeSet};

const MODULES: &[&str] = &["react-router", "react-router-dom"];
const MAX_BINDINGS: usize = 256;
const BINDING_WORKING_BYTES: u64 = 1024;
const MAX_LAZY_MODULE_BYTES: usize = 256;
struct Router<'tree> {
    node: Node<'tree>,
    scope: String,
    base: Option<String>,
}
struct Component {
    start: usize,
    end: usize,
    scopes: BTreeSet<usize>,
}
struct Navigate {
    start: usize,
    end: usize,
}

pub(super) fn scan(
    builder: &mut FrameworkBuilder<'_, '_>,
    root: Node<'_>,
    state: &mut State<'_>,
) -> Result<(), ExtractError> {
    if !state
        .imports
        .values()
        .flatten()
        .any(|(module, _)| MODULES.contains(&module.as_str()))
    {
        return Ok(());
    }
    let mut index = RouterIndex {
        root,
        routers: Vec::new(),
        components: BTreeMap::new(),
        lazy: BTreeMap::new(),
        navigators: BTreeMap::new(),
    };
    walk(builder, root, |builder, node| {
        index.collect_sites(builder, state, node)?;
        index.collect_bindings(builder, state, node)
    })?;
    walk(builder, root, |builder, node| {
        index.registration(builder, state, node)
    })?;
    walk(builder, root, |builder, node| {
        index.source_site(builder, state, node)
    })
}

struct RouterIndex<'tree> {
    root: Node<'tree>,
    routers: Vec<Router<'tree>>,
    components: BTreeMap<String, Component>,
    lazy: BTreeMap<String, String>,
    navigators: BTreeMap<String, Navigate>,
}
impl<'tree> RouterIndex<'tree> {
    fn collect_sites(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
        state: &State<'_>,
        node: Node<'tree>,
    ) -> Result<(), ExtractError> {
        let RouterIndex {
            routers,
            components,
            ..
        } = self;

        if node.kind() == "jsx_element"
            && let Some(open) = node.child_by_field_name("open_tag")
            && state.is_router_tag(builder, open, &["BrowserRouter"])
        {
            if routers.len() >= 64 {
                return Err(ExtractError::OutputLimit);
            }
            builder
                .bridge
                .reserve_working_bytes(BINDING_WORKING_BYTES)?;
            let base = match attribute(builder, open, "basename") {
                Ok(None) => Some(String::new()),
                Ok(Some(value)) => literal(builder, value)
                    .filter(|value| valid_path(value))
                    .map(|value| value.trim_end_matches('/').to_owned()),
                Err(()) => None,
            };
            routers.push(Router {
                node,
                scope: scope_key(
                    "react-router",
                    &format!("{}:{}", builder.path(), node.start_byte()),
                ),
                base,
            });
        }
        if node.kind() == "function_declaration"
            && node.parent().is_some_and(|parent| {
                parent.kind() == "program"
                    || parent.kind() == "export_statement"
                        && parent
                            .parent()
                            .is_some_and(|parent| parent.kind() == "program")
            })
            && let Some(name) = node.child_by_field_name("name")
            && state.names.get(text(builder, name)) == Some(&1)
        {
            if components.len() >= MAX_BINDINGS {
                return Err(ExtractError::OutputLimit);
            }
            builder
                .bridge
                .reserve_working_bytes(BINDING_WORKING_BYTES)?;
            components.insert(
                text(builder, name).to_owned(),
                Component {
                    start: node.start_byte(),
                    end: node.end_byte(),
                    scopes: BTreeSet::new(),
                },
            );
        }
        Ok(())
    }
}

impl RouterIndex<'_> {
    fn collect_bindings(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
        state: &State<'_>,
        node: Node<'_>,
    ) -> Result<(), ExtractError> {
        if node.kind() != "variable_declarator"
            || !node
                .parent()
                .is_some_and(|parent| text(builder, parent).starts_with("const "))
        {
            return Ok(());
        }
        let Some(name) = node.child_by_field_name("name").filter(|node| {
            node.kind() == "identifier" && state.names.get(text(builder, *node)) == Some(&1)
        }) else {
            return Ok(());
        };
        let Some(value) = node
            .child_by_field_name("value")
            .filter(|value| value.kind() == "call_expression")
        else {
            return Ok(());
        };
        let Some(function) = value.child_by_field_name("function") else {
            return Ok(());
        };
        let (start, end) = binding_scope(builder, node)?;
        if function.kind() == "identifier"
            && state.imported(text(builder, function), MODULES, &["useNavigate"])
        {
            if self.navigators.len() >= MAX_BINDINGS {
                return Err(ExtractError::OutputLimit);
            }
            builder
                .bridge
                .reserve_working_bytes(BINDING_WORKING_BYTES)?;
            self.navigators
                .insert(text(builder, name).to_owned(), Navigate { start, end });
        }
        if (start..end) == self.root.byte_range()
            && function.kind() == "identifier"
            && state.imported(text(builder, function), &["react"], &["lazy"])
            && let Some(module) = lazy_module(builder, value)
        {
            if self.lazy.len() >= MAX_BINDINGS {
                return Err(ExtractError::OutputLimit);
            }
            builder
                .bridge
                .reserve_working_bytes(BINDING_WORKING_BYTES)?;
            self.lazy
                .insert(text(builder, name).to_owned(), module.to_owned());
        }
        Ok(())
    }
}

impl RouterIndex<'_> {
    fn registration(
        &mut self,
        builder: &mut FrameworkBuilder<'_, '_>,
        state: &mut State<'_>,
        node: Node<'_>,
    ) -> Result<(), ExtractError> {
        let RouterIndex {
            routers,
            components,
            lazy,
            ..
        } = self;

        if !state.is_router_tag(builder, node, &["Route"]) {
            return Ok(());
        }
        let Some(router_index) = physical_scope(node, routers) else {
            return Ok(());
        };
        let router = &routers[router_index];
        let route = router.route_path(builder, state, node)?;
        let target = attribute(builder, node, "element")
            .ok()
            .flatten()
            .and_then(element_name);
        let target_name = target.map(|node| text(builder, node).to_owned());
        if route.is_some()
            && let Some(component) = target_name
                .as_ref()
                .and_then(|name| components.get_mut(name))
        {
            component.scopes.insert(router_index);
        }
        let resolution = target_name
            .as_ref()
            .and_then(|name| lazy.get(name))
            .map(|module| format!("framework-react-lazy::{module}"));
        // Only a unique local declaration or an explicit import/lazy binding is usable.
        let bound = target_name.as_ref().is_some_and(|name| {
            components.contains_key(name)
                || lazy.contains_key(name)
                || state.names.get(name).copied().unwrap_or(0) == 0
                    && state.imports.get(name).is_some_and(Option::is_some)
        });
        let target = target
            .zip(target_name.as_deref())
            .filter(|_| bound)
            .map(|(node, name)| {
                (
                    name,
                    resolution.as_deref(),
                    node.start_byte(),
                    node.end_byte(),
                )
            });
        state.add_resolved(
            builder,
            Site {
                channel: "navigation",
                role: if route.is_some() {
                    "registration"
                } else {
                    "unknown"
                },
                scope: &router.scope,
                operation: "route",
                adapter: "react-router-jsx-v1",
                destination: route.as_deref().unwrap_or("<unsupported-route>"),
                start: node.start_byte(),
                end: node.end_byte(),
                target: None,
            },
            target,
        )
    }
}

impl RouterIndex<'_> {
    fn source_shape(
        &self,
        builder: &FrameworkBuilder<'_, '_>,
        state: &State<'_>,
        node: Node<'_>,
    ) -> Option<(&'static str, Option<String>)> {
        if state.is_router_tag(builder, node, &["Link", "NavLink", "Navigate"]) {
            Some((
                if state.is_router_tag(builder, node, &["Navigate"]) {
                    "redirect"
                } else {
                    "link"
                },
                attribute(builder, node, "to")
                    .ok()
                    .flatten()
                    .and_then(|node| literal(builder, node))
                    .map(str::to_owned),
            ))
        } else if node.kind() == "call_expression"
            && let Some(function) = node
                .child_by_field_name("function")
                .filter(|node| node.kind() == "identifier")
            && self
                .navigators
                .get(text(builder, function))
                .is_some_and(|binding| {
                    binding.start <= node.start_byte() && node.end_byte() <= binding.end
                })
        {
            Some((
                "navigate",
                argument(node, 0)
                    .and_then(|node| literal(builder, node))
                    .map(str::to_owned),
            ))
        } else {
            None
        }
    }
}

impl RouterIndex<'_> {
    fn source_site(
        &self,
        builder: &mut FrameworkBuilder<'_, '_>,
        state: &mut State<'_>,
        node: Node<'_>,
    ) -> Result<(), ExtractError> {
        let RouterIndex {
            routers,
            components,
            ..
        } = self;
        let Some((operation, destination)) = self.source_shape(builder, state, node) else {
            return Ok(());
        };
        let mut scopes = BTreeSet::new();
        if let Some(scope) = physical_scope(node, routers) {
            scopes.insert(scope);
        }
        for component in components.values() {
            builder.bridge.charge_work(1)?;
            if component.start <= node.start_byte() && node.end_byte() <= component.end {
                scopes.extend(&component.scopes);
            }
        }
        let router = if scopes.len() == 1 {
            scopes.first().and_then(|index| routers.get(*index))
        } else {
            None
        };
        let scope = router.map_or_else(
            || scope_key("unknown-react-router", builder.path()),
            |router| router.scope.clone(),
        );
        let (role, path) = match destination.as_deref() {
            Some(value)
                if value.starts_with("//") || value.contains(':') && !value.starts_with('/') =>
            {
                ("external", "<external>".to_owned())
            }
            Some(value)
                if valid_path(value) && router.is_some_and(|router| router.base.is_some()) =>
            {
                (
                    "source",
                    format!(
                        "{}{}",
                        router
                            .and_then(|router| router.base.as_deref())
                            .unwrap_or_default(),
                        value.split(['?', '#']).next().unwrap_or(value)
                    ),
                )
            }
            _ => ("unknown", "<dynamic-or-unproven-router>".to_owned()),
        };
        state.add(
            builder,
            Site {
                channel: "navigation",
                role,
                scope: &scope,
                operation,
                adapter: "react-router-jsx-v1",
                destination: &path,
                start: node.start_byte(),
                end: node.end_byte(),
                target: None,
            },
        )
    }
}

fn physical_scope(node: Node<'_>, routers: &[Router<'_>]) -> Option<usize> {
    let mut found = None;
    for (index, router) in routers.iter().enumerate() {
        if router.node.start_byte() <= node.start_byte()
            && node.end_byte() <= router.node.end_byte()
        {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}
impl State<'_> {
    fn is_router_tag(
        &self,
        builder: &FrameworkBuilder<'_, '_>,
        node: Node<'_>,
        exports: &[&str],
    ) -> bool {
        matches!(
            node.kind(),
            "jsx_opening_element" | "jsx_self_closing_element"
        ) && node
            .child_by_field_name("name")
            .is_some_and(|name| self.imported(text(builder, name), MODULES, exports))
    }
}

fn attribute<'tree>(
    builder: &FrameworkBuilder<'_, '_>,
    node: Node<'tree>,
    key: &str,
) -> Result<Option<Node<'tree>>, ()> {
    if node.named_child_count() > 128 {
        return Err(());
    }
    let mut cursor = node.walk();
    let mut found = None;
    for child in node.named_children(&mut cursor) {
        if child.kind() == "jsx_expression" {
            return Err(());
        }
        if child.kind() == "jsx_attribute"
            && child
                .named_child(0)
                .is_some_and(|name| text(builder, name) == key)
        {
            if found.is_some() {
                return Err(());
            }
            let value = child.named_child(1).ok_or(())?;
            found = Some(if value.kind() == "jsx_expression" {
                value.named_child(0).ok_or(())?
            } else {
                value
            });
        }
    }
    Ok(found)
}
fn element_name(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "jsx_element" {
        node.child_by_field_name("open_tag")?
            .child_by_field_name("name")
    } else if node.kind() == "jsx_self_closing_element" {
        node.child_by_field_name("name")
    } else {
        None
    }
    .filter(|node| node.kind() == "identifier")
}
fn valid_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.starts_with("//")
        && value.len() <= 512
        && !value.contains(['\\', '*', '%'])
        && !value
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
}
impl Router<'_> {
    fn route_path(
        &self,
        builder: &mut FrameworkBuilder<'_, '_>,
        state: &State<'_>,
        node: Node<'_>,
    ) -> Result<Option<String>, ExtractError> {
        let Some(base) = &self.base else {
            return Ok(None);
        };
        let mut parts = Vec::new();
        let mut ancestor = Some(node);
        for _ in 0..64 {
            builder.bridge.charge_work(1)?;
            let Some(current) = ancestor else {
                return Ok(None);
            };
            if current == self.node {
                parts.reverse();
                let path = format!(
                    "{base}/{}",
                    parts
                        .iter()
                        .map(|part: &String| part.trim_matches('/'))
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("/")
                );
                return Ok(valid_path(&path).then_some(path));
            }
            let open = route_opening(current, node);
            if let Some(open) = open.filter(|open| state.is_router_tag(builder, *open, &["Route"]))
            {
                // The starting opening tag is encountered again through its element parent.
                if current != node && open == node {
                    ancestor = current.parent();
                    continue;
                }
                if !append_route_part(builder, open, &mut parts) {
                    return Ok(None);
                }
            }
            ancestor = current.parent();
        }
        Err(ExtractError::OutputLimit)
    }
}

fn route_opening<'tree>(current: Node<'tree>, start: Node<'tree>) -> Option<Node<'tree>> {
    if current.kind() == "jsx_element" {
        current.child_by_field_name("open_tag")
    } else if current == start {
        Some(current)
    } else {
        None
    }
}

fn append_route_part(
    builder: &FrameworkBuilder<'_, '_>,
    open: Node<'_>,
    parts: &mut Vec<String>,
) -> bool {
    match attribute(builder, open, "path") {
        Ok(Some(path)) => {
            let Some(path) = literal(builder, path)
                .filter(|path| path.len() <= 512 && !path.contains(['*', '?', '#', '\\', '%']))
            else {
                return false;
            };
            if !parts.is_empty()
                && parts
                    .last()
                    .is_some_and(|part: &String| part.starts_with('/'))
            {
                return false;
            }
            parts.push(path.to_owned());
        }
        Ok(None) => (),
        Err(()) => return false,
    }
    true
}

fn lazy_module<'source>(
    builder: &FrameworkBuilder<'source, '_>,
    value: Node<'_>,
) -> Option<&'source str> {
    let callback = argument(value, 0).filter(|node| node.kind() == "arrow_function")?;
    let body = callback
        .child_by_field_name("body")
        .filter(|node| node.kind() == "call_expression")?;
    if body
        .child_by_field_name("function")
        .is_none_or(|function| function.kind() != "import")
    {
        return None;
    }
    argument(body, 0)
        .and_then(|node| literal(builder, node))
        .filter(|module| {
            module.starts_with('.')
                && module.len() <= MAX_LAZY_MODULE_BYTES
                && !module.contains('#')
        })
}
