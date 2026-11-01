//! Generation-local scoped boundary joins. A matching literal alone never crosses scopes.
use super::{
    DerivedEdgeInput, EdgeKind, ResolutionCandidate, ResolutionIndex, ResolutionMutation,
    StageItemFailure, SymbolId, SymbolKind, append_derived_edge, framework_landmark_candidate,
    ordered_resolution_candidates,
};
use codevetra_domain::BoundarySite;
use std::collections::{BTreeMap, BTreeSet};

struct Site<'a> {
    candidate: &'a ResolutionCandidate,
    metadata: BoundarySite,
}
type GroupKey = (String, String, String);
#[derive(Default)]
struct Targets<'a> {
    exact: BTreeMap<&'a str, Vec<&'a Site<'a>>>,
    patterns: Vec<&'a Site<'a>>,
    patterns_overflow: bool,
}

pub(super) fn append<Cancel: FnMut() -> bool>(
    mut input: ResolutionMutation<'_, Cancel>,
) -> Result<(), StageItemFailure> {
    let (sites, callables) = collect(&mut input)?;
    if sites.is_empty() {
        return Ok(());
    }
    let blocked = blocked_next_scopes(&mut input)?;
    let positions = boundary_positions(&mut input)?;
    let registrations = sites
        .iter()
        .filter(|site| site.metadata.role == "registration")
        .map(|site| &site.candidate.symbol_id)
        .collect::<BTreeSet<_>>();
    let mut handlers = BTreeSet::new();
    for edge in &input.facts.edges {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        if edge.kind == EdgeKind::Calls && registrations.contains(&edge.source_symbol_id) {
            input.budget.charge(128)?;
            handlers.insert(edge.source_symbol_id.clone());
        }
    }
    let targets = target_index(&sites);
    for site in &sites {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        let kind = site.metadata.edge_kind().ok_or(StageItemFailure)?;
        let owner = owner(input.index, &callables, site.candidate, input.cancelled)?;
        match site.metadata.role.as_str() {
            "registration" => {
                append_registration(&mut input, site, owner, kind, &positions, &mut handlers)?;
            }
            "source" | "unknown" | "external" => append_source(
                &mut input, site, owner, kind, &positions, &targets, &blocked,
            )?,
            _ => return Err(StageItemFailure),
        }
    }
    Ok(())
}

fn boundary_positions<Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'_, Cancel>,
) -> Result<BTreeMap<SymbolId, usize>, StageItemFailure> {
    let mut positions = BTreeMap::new();
    for (position, symbol) in input.facts.symbols.iter().enumerate() {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        if symbol.symbol_kind == "resource" && symbol.qualified_name.contains("::boundary-v1::") {
            input.budget.charge(128)?;
            positions.insert(symbol.symbol_id.clone(), position);
        }
    }
    Ok(positions)
}

fn append_registration<Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'_, Cancel>,
    site: &Site<'_>,
    owner: Option<&ResolutionCandidate>,
    kind: EdgeKind,
    positions: &BTreeMap<SymbolId, usize>,
    handlers: &mut BTreeSet<SymbolId>,
) -> Result<(), StageItemFailure> {
    if let Some(owner) =
        owner.filter(|owner| owner.declaration_span == site.candidate.declaration_span)
    {
        link(
            input,
            &site.candidate.symbol_id,
            &owner.symbol_id,
            kind,
            "framework-boundary-registration-owner-v1",
            &site.metadata,
        )?;
        handlers.insert(site.candidate.symbol_id.clone());
    }
    annotate(
        input,
        positions,
        site,
        if handlers.contains(&site.candidate.symbol_id) {
            "registration_handler_evidenced"
        } else {
            "registration_handler_unresolved"
        },
    )?;
    Ok(())
}

fn append_source<Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'_, Cancel>,
    site: &Site<'_>,
    owner: Option<&ResolutionCandidate>,
    kind: EdgeKind,
    positions: &BTreeMap<SymbolId, usize>,
    targets: &BTreeMap<GroupKey, Targets<'_>>,
    blocked: &BTreeSet<String>,
) -> Result<(), StageItemFailure> {
    if let Some(owner) = owner {
        link(
            input,
            &owner.symbol_id,
            &site.candidate.symbol_id,
            kind,
            "framework-boundary-source-site-v1",
            &site.metadata,
        )?;
    }
    let (matches, state) = matches_for(site, targets, blocked);
    for target in matches {
        link(
            input,
            &site.candidate.symbol_id,
            &target.candidate.symbol_id,
            kind,
            "framework-boundary-scoped-match-v1",
            &site.metadata,
        )?;
    }
    annotate(input, positions, site, state)?;
    Ok(())
}

fn collect<'a, Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'a, Cancel>,
) -> Result<
    (
        Vec<Site<'a>>,
        BTreeMap<&'a SymbolId, &'a ResolutionCandidate>,
    ),
    StageItemFailure,
> {
    let mut sites = Vec::new();
    let mut seen = BTreeSet::new();
    for (_, candidates) in ordered_resolution_candidates(input.index) {
        for candidate in candidates {
            if (input.cancelled)() {
                return Err(StageItemFailure);
            }
            if candidate.kind == SymbolKind::Resource
                && let Some(metadata) = BoundarySite::from_qualified_name(&candidate.qualified_name)
                && seen.insert(&candidate.symbol_id)
            {
                input.budget.charge(2048)?;
                sites.push(Site {
                    candidate,
                    metadata,
                });
            }
        }
    }
    let callables = collect_callables(input, &sites)?;
    sites.sort_by_key(|site| &site.candidate.symbol_id);
    Ok((sites, callables))
}
fn collect_callables<'a, Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'a, Cancel>,
    sites: &[Site<'_>],
) -> Result<BTreeMap<&'a SymbolId, &'a ResolutionCandidate>, StageItemFailure> {
    let mut callables = BTreeMap::new();
    if sites.is_empty() {
        return Ok(callables);
    }
    let files = sites
        .iter()
        .map(|site| &site.candidate.file_id)
        .collect::<BTreeSet<_>>();
    for (_, candidates) in ordered_resolution_candidates(input.index) {
        for candidate in candidates {
            if (input.cancelled)() {
                return Err(StageItemFailure);
            }
            if files.contains(&candidate.file_id)
                && matches!(
                    candidate.kind,
                    SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
                )
                && !framework_landmark_candidate(candidate)
                && !callables.contains_key(&candidate.symbol_id)
            {
                input.budget.charge(128)?;
                callables.insert(&candidate.symbol_id, candidate);
            }
        }
    }
    Ok(callables)
}

fn key(site: &Site<'_>) -> GroupKey {
    (
        site.metadata.channel.clone(),
        site.metadata.scope.clone(),
        if site.metadata.channel == "http" {
            site.metadata.operation.clone()
        } else {
            String::new()
        },
    )
}
fn target_index<'a>(sites: &'a [Site<'a>]) -> BTreeMap<GroupKey, Targets<'a>> {
    let mut groups = BTreeMap::<GroupKey, Targets<'a>>::new();
    for site in sites
        .iter()
        .filter(|site| site.metadata.role == "registration")
    {
        let group = groups.entry(key(site)).or_default();
        if matches!(site.metadata.channel.as_str(), "http" | "navigation")
            && site
                .metadata
                .destination
                .split('/')
                .any(|segment| segment.starts_with(':'))
        {
            if group.patterns.len() < 64 {
                group.patterns.push(site);
            } else {
                group.patterns_overflow = true;
            }
        } else {
            let matches = group.exact.entry(&site.metadata.destination).or_default();
            // One extra row proves overflow; do not retain/clone unbounded fanout.
            if matches.len() < 9 {
                matches.push(site);
            }
        }
    }
    groups
}
fn matches_for<'a>(
    site: &Site<'_>,
    groups: &BTreeMap<GroupKey, Targets<'a>>,
    blocked: &BTreeSet<String>,
) -> (Vec<&'a Site<'a>>, &'static str) {
    if site.metadata.role == "unknown" {
        return (Vec::new(), "dynamic_or_unproven_identity");
    }
    if site.metadata.role == "external" {
        return (Vec::new(), "external_destination");
    }
    if site.metadata.adapter == "next-app-v1" && blocked.contains(&site.metadata.scope) {
        return (Vec::new(), "routing_configuration_unverified");
    }
    let Some(group) = groups.get(&key(site)) else {
        return (Vec::new(), "no_supported_registration");
    };
    let found = match destination_matches(group, &site.metadata.destination) {
        Ok(found) => found,
        Err(reason) => return (Vec::new(), reason),
    };
    let fanout = if matches!(site.metadata.channel.as_str(), "event" | "observer") {
        8
    } else {
        1
    };
    if found.len() > fanout {
        return (
            Vec::new(),
            if fanout == 1 {
                "ambiguous_registrations"
            } else {
                "subscriber_fanout_limit"
            },
        );
    }
    let state = if found.is_empty() {
        "no_supported_registration"
    } else {
        "possible_scoped_destination"
    };
    (found, state)
}
fn destination_matches<'a>(
    group: &Targets<'a>,
    destination: &str,
) -> Result<Vec<&'a Site<'a>>, &'static str> {
    let mut found = group.exact.get(destination).cloned().unwrap_or_default();
    if found.is_empty() {
        if group.patterns_overflow {
            return Err("parameter_route_limit");
        }
        found.extend(
            group
                .patterns
                .iter()
                .filter(|target| route_matches(&target.metadata.destination, destination))
                .copied(),
        );
        if let Some(specificity) = found
            .iter()
            .map(|target| route_specificity(&target.metadata.destination))
            .max()
        {
            found.retain(|target| route_specificity(&target.metadata.destination) == specificity);
        }
    }
    Ok(found)
}

fn route_specificity(route: &str) -> usize {
    route
        .split('/')
        .filter(|segment| !segment.starts_with(':'))
        .count()
}
fn route_matches(route: &str, destination: &str) -> bool {
    let route = route.split('/').collect::<Vec<_>>();
    let destination = destination.split('/').collect::<Vec<_>>();
    route.len() == destination.len()
        && route.iter().zip(destination).all(|(pattern, value)| {
            !value.is_empty() && pattern.starts_with(':') || *pattern == value
        })
}
fn owner<'a, Cancel: FnMut() -> bool>(
    index: &'a ResolutionIndex,
    callables: &BTreeMap<&SymbolId, &'a ResolutionCandidate>,
    site: &ResolutionCandidate,
    cancelled: &mut Cancel,
) -> Result<Option<&'a ResolutionCandidate>, StageItemFailure> {
    let mut id = &site.symbol_id;
    for _ in 0..64 {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if let Some(owner) = callables.get(id) {
            return Ok((owner.file_id == site.file_id
                && owner.declaration_span.0 <= site.declaration_span.0
                && owner.declaration_span.1 >= site.declaration_span.1)
                .then_some(*owner));
        }
        let Some(parent) = index.parents.get(id) else {
            return Ok(None);
        };
        id = parent;
    }
    Ok(None)
}
fn link<Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'_, Cancel>,
    source: &SymbolId,
    target: &SymbolId,
    kind: EdgeKind,
    provenance: &'static str,
    site: &BoundarySite,
) -> Result<(), StageItemFailure> {
    append_derived_edge(
        input.facts,
        input.budget,
        DerivedEdgeInput {
            source_symbol_id: source,
            target_symbol_id: target,
            kind,
            confidence: if site.adapter == "next-app-v1" {
                0.8
            } else {
                0.9
            },
            provenance,
        },
    )
}
fn annotate<Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'_, Cancel>,
    positions: &BTreeMap<SymbolId, usize>,
    site: &Site<'_>,
    state: &str,
) -> Result<(), StageItemFailure> {
    input.budget.charge(128)?;
    let position = positions
        .get(&site.candidate.symbol_id)
        .ok_or(StageItemFailure)?;
    input
        .facts
        .symbols
        .get_mut(*position)
        .ok_or(StageItemFailure)?
        .signature = format!("boundary-state:{state}");
    Ok(())
}
fn blocked_next_scopes<Cancel: FnMut() -> bool>(
    input: &mut ResolutionMutation<'_, Cancel>,
) -> Result<BTreeSet<String>, StageItemFailure> {
    let mut scopes = BTreeSet::new();
    for file in input.index.modules.files.values() {
        if (input.cancelled)() {
            return Err(StageItemFailure);
        }
        let (directory, name) = file.path.rsplit_once('/').unwrap_or(("", &file.path));
        if matches!(
            name,
            "next.config.js" | "next.config.ts" | "next.config.mjs" | "next.config.cjs"
        ) {
            input.budget.charge(128)?;
            scopes.insert(
                blake3::hash(format!("boundary-scope-v1:next-application:{directory}").as_bytes())
                    .to_hex()[..32]
                    .to_owned(),
            );
        }
    }
    Ok(scopes)
}
