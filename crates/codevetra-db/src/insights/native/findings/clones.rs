//! Canonical clone classes suppress symmetric rows and preserve bounded member evidence.

mod groups;

use groups::Groups;

use super::{
    corrupt, documents::Documents, framework, inventory::Base, packed_tree_bytes,
    packed_tree_insertion_bytes, rules, workspace,
};
use crate::{
    insights::StructuralFinding,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction, packed_uuid, unpacked_uuid},
    semantic::native::materialize,
};
use codevetra_domain::SymbolId;
use num_traits::ToPrimitive as _;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
};

pub(super) struct Classes {
    representatives: BTreeMap<PackedSymbol, Class>,
    claimed: BTreeSet<PackedSymbol>,
    production: BTreeSet<PackedSymbol>,
}
struct Class {
    kind: &'static str,
    metric: &'static str,
    count: u64,
    members: Vec<PackedSymbol>,
    score: Option<f64>,
}
type PackedSymbol = [u8; 16];
type LinkRows = BTreeMap<PackedSymbol, BTreeMap<PackedSymbol, f64>>;

const MAXIMUM_CLONE_BYTES: usize = 16 * 1_048_576;
const MAXIMUM_SEMANTIC_BYTES: usize = 32 * 1_048_576;
const MINIMUM_EXACT_LINES: u32 = 6;
const MINIMUM_NEAR_LINES: u32 = 12;
const MAXIMUM_LITERAL_FRACTION: f64 = 0.6;
const SEMANTIC_THRESHOLD: f64 = 0.95;
const MAXIMUM_COMPONENT_PENDING: usize = 65_536;

struct Source<'a, 'view, 'store> {
    graph: &'a GraphSnapshot<'view, 'store>,
    documents: &'a Documents,
}

#[derive(Clone, Copy)]
enum ShapeKind {
    Exact,
    Near,
}

impl ShapeKind {
    const fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Exact => ("exact", "exact_copies"),
            Self::Near => ("near", "normalized_shape_copies"),
        }
    }
}

struct SemanticLinks {
    rows: LinkRows,
    bytes: usize,
}

struct Components<'a> {
    links: &'a LinkRows,
    visited: BTreeSet<PackedSymbol>,
}

impl Classes {
    pub(super) fn new(
        transaction: &StateTransaction<'_>,
        documents: &Documents,
        budget: &mut ReadBudget,
    ) -> Result<Self, RocksError> {
        let snapshot = transaction.graph_snapshot()?;
        let graph = snapshot.graph(budget)?;
        let source = Source {
            graph: &graph,
            documents,
        };
        let mut output = Self {
            representatives: BTreeMap::new(),
            claimed: BTreeSet::new(),
            production: BTreeSet::new(),
        };
        let mut exact = Groups::default();
        let mut bytes = 0_usize;
        for handle in documents.iter() {
            let id = handle.symbol()?;
            budget.checkpoint()?;
            if handle.test {
                continue;
            }
            let symbol = graph.symbol(&id, budget)?.ok_or(corrupt())?;
            if !matches!(
                symbol.symbol_kind.as_str(),
                "function" | "method" | "component"
            ) {
                continue;
            }
            let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
            if !framework::production(&file.normalized_path)? {
                continue;
            }
            let document = handle.load(&graph, budget)?;
            if !document.clones.enabled {
                continue;
            }
            let packed = packed_uuid(id.as_str())?;
            let node = packed_tree_insertion_bytes(output.production.len());
            output.production.insert(packed);
            bytes = bytes.checked_add(node).ok_or_else(workspace)?;
            if symbol.end_line - symbol.start_line + 1 >= MINIMUM_EXACT_LINES {
                let hash = blake3::Hash::from_hex(symbol.structural_digest.as_str())
                    .map_err(|_| corrupt())?;
                exact.push(*hash.as_bytes(), packed, &mut bytes)?;
            }
            if bytes > MAXIMUM_CLONE_BYTES {
                return Err(workspace());
            }
        }
        output.absorb(exact, ShapeKind::Exact, budget)?;
        let near = output.near_groups(&source, budget)?;
        output.absorb(near, ShapeKind::Near, budget)?;
        output.semantic(transaction, &source, budget)?;
        Ok(output)
    }
    fn near_groups(
        &self,
        source: &Source<'_, '_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<Groups, RocksError> {
        let Source { graph, documents } = source;
        let mut groups = Groups::default();
        let mut bytes = self.bytes();
        for id in self.production.difference(&self.claimed) {
            budget.checkpoint()?;
            let document = documents
                .get(&symbol_id(id)?)?
                .ok_or(corrupt())?
                .load(graph, budget)?;
            let Some(shape) = document
                .shape
                .as_ref()
                .filter(|_| document.clones.near_compatible)
            else {
                continue;
            };
            let symbol = graph.symbol(&symbol_id(id)?, budget)?.ok_or(corrupt())?;
            let size = symbol
                .end_byte
                .saturating_sub(symbol.start_byte)
                .max(1)
                .to_f64()
                .ok_or(corrupt())?;
            if symbol.end_line - symbol.start_line + 1 < MINIMUM_NEAR_LINES
                || document.health("literal_bytes") / size > MAXIMUM_LITERAL_FRACTION
            {
                continue;
            }
            groups.push(*blake3::hash(shape.as_bytes()).as_bytes(), *id, &mut bytes)?;
            if bytes > MAXIMUM_CLONE_BYTES {
                return Err(workspace());
            }
        }
        Ok(groups)
    }
    fn absorb(
        &mut self,
        groups: Groups,
        shape: ShapeKind,
        budget: &ReadBudget,
    ) -> Result<(), RocksError> {
        let (kind, metric) = shape.labels();
        let mut groups = groups.sorted(budget)?.peekable();
        while let Some((hash, first)) = groups.next() {
            budget.checkpoint()?;
            if groups.peek().is_none_or(|(next, _)| *next != hash) {
                continue;
            }
            let mut members = vec![first];
            while groups.peek().is_some_and(|(next, _)| *next == hash) {
                budget.checkpoint()?;
                members.push(groups.next().ok_or(corrupt())?.1);
            }
            let representative = first;
            self.claimed.extend(members.iter().copied());
            self.representatives.insert(
                representative,
                Class {
                    kind,
                    metric,
                    count: members.len() as u64,
                    members,
                    score: None,
                },
            );
        }
        if self.bytes() > MAXIMUM_CLONE_BYTES {
            return Err(workspace());
        }
        Ok(())
    }
    fn bytes(&self) -> usize {
        packed_tree_bytes(self.production.len())
            + packed_tree_bytes(self.claimed.len())
            + if self.representatives.is_empty() {
                0
            } else {
                2048
            }
            + self
                .representatives
                .values()
                .map(|class| 256 + class.members.capacity() * size_of::<PackedSymbol>())
                .sum::<usize>()
    }
    fn semantic(
        &mut self,
        transaction: &StateTransaction<'_>,
        source: &Source<'_, '_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<(), RocksError> {
        let Source { graph, documents } = source;
        let mut eligible = BTreeSet::new();
        for id in self.production.difference(&self.claimed) {
            let document = documents
                .get(&symbol_id(id)?)?
                .ok_or(corrupt())?
                .load(graph, budget)?;
            let symbol = graph.symbol(&symbol_id(id)?, budget)?.ok_or(corrupt())?;
            if document.partial.peers == 0
                && symbol.end_line - symbol.start_line + 1 >= MINIMUM_EXACT_LINES
            {
                eligible.insert(*id);
            }
        }
        let mut links = SemanticLinks {
            rows: LinkRows::new(),
            bytes: self.bytes() + packed_tree_bytes(eligible.len()),
        };
        links.collect(transaction, budget, &eligible)?;
        let mut components = Components {
            links: &links.rows,
            visited: BTreeSet::new(),
        };
        for start in links.rows.keys() {
            budget.checkpoint()?;
            if components.visited.contains(start) {
                continue;
            }
            let members = components.connected_members(start, budget)?;
            let representative = *members.first().ok_or(corrupt())?;
            let peers = links.rows.get(&representative).ok_or(corrupt())?;
            let score = peers.values().copied().reduce(f64::max).ok_or(corrupt())?;
            self.claimed.extend(members.iter().copied());
            self.representatives.insert(
                representative,
                Class {
                    kind: "semantic",
                    metric: "semantic_clone_peers",
                    count: peers.len() as u64,
                    members,
                    score: Some(score),
                },
            );
            if self.bytes() + links.bytes > MAXIMUM_SEMANTIC_BYTES {
                return Err(workspace());
            }
        }
        Ok(())
    }
    pub(super) fn finding(
        &self,
        base: &Base<'_>,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
    ) -> Result<Option<StructuralFinding>, RocksError> {
        let id = &packed_uuid(base.symbol.symbol_id.as_str())?;
        if let Some(class) = self.representatives.get(id) {
            let mut detail = json!({ "cloneType":class.kind, "classSize":class.members.len(), "recordScope":"clone_class",
                "symmetricMemberRowsSuppressed":true, "loc":base.symbol.end_line - base.symbol.start_line + 1,
                "members":members(class.members.iter().filter(|peer| *peer != id), graph, budget)? });
            if let Some(score) = class.score {
                detail["maximumSemanticScore"] = json!(score);
                detail["semanticThreshold"] = json!(SEMANTIC_THRESHOLD);
            }
            return Ok(Some(
                rules::FindingEvidence {
                    name: "duplicate_code",
                    metric: class.metric,
                    value: class.count.to_f64().ok_or(corrupt())?,
                    severity: if class.kind == "exact" {
                        "warning"
                    } else {
                        "info"
                    },
                    detail: Some(detail),
                }
                .project(base),
            ));
        }
        let partial = &base.document.partial;
        if self.claimed.contains(id)
            || !self.production.contains(id)
            || partial.peers == 0
            || partial.representative.as_ref() != Some(&base.symbol.symbol_id)
            || base.symbol.end_line - base.symbol.start_line + 1 < MINIMUM_NEAR_LINES
        {
            return Ok(None);
        }
        let listed = partial
            .listed
            .iter()
            .map(|id| packed_uuid(id.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let detail = json!({ "cloneType":"partial", "classSize":partial.component, "recordScope":"clone_class",
            "symmetricMemberRowsSuppressed":true, "loc":base.symbol.end_line - base.symbol.start_line + 1,
            "maximumOverlap":partial.maximum, "minimumOverlap":partial.minimum,
            "members":members(listed.iter().filter(|id| self.production.contains(*id)), graph, budget)? });
        Ok(Some(
            rules::FindingEvidence {
                name: "duplicate_code",
                metric: "partial_clone_peers",
                value: partial.peers.to_f64().ok_or(corrupt())?,
                severity: "info",
                detail: Some(detail),
            }
            .project(base),
        ))
    }
}

impl SemanticLinks {
    fn collect(
        &mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        eligible: &BTreeSet<PackedSymbol>,
    ) -> Result<(), RocksError> {
        materialize::visit_current(transaction, budget, |edge, _| {
            let source = packed_uuid(edge.source.as_str())?;
            let target = packed_uuid(edge.target.as_str())?;
            if edge.score >= SEMANTIC_THRESHOLD
                && eligible.contains(&source)
                && eligible.contains(&target)
            {
                for (source, target) in [(source, target), (target, source)] {
                    self.add(source, target, edge.score);
                }
                if self.bytes > MAXIMUM_CLONE_BYTES {
                    return Err(workspace());
                }
            }
            Ok(())
        })
    }

    fn add(&mut self, source: PackedSymbol, target: PackedSymbol, score: f64) {
        if !self.rows.contains_key(&source) {
            self.bytes += if self.rows.is_empty() { 512 } else { 128 };
        }
        let peers = self.rows.entry(source).or_default();
        if !peers.contains_key(&target) {
            self.bytes += packed_tree_insertion_bytes(peers.len());
        }
        peers
            .entry(target)
            .and_modify(|old| *old = old.max(score))
            .or_insert(score);
    }
}

impl Components<'_> {
    fn connected_members(
        &mut self,
        start: &PackedSymbol,
        budget: &mut ReadBudget,
    ) -> Result<Vec<PackedSymbol>, RocksError> {
        let mut pending = vec![*start];
        let mut members = Vec::new();
        while let Some(id) = pending.pop() {
            budget.checkpoint()?;
            if !self.visited.insert(id) {
                continue;
            }
            members.push(id);
            if let Some(peers) = self.links.get(&id) {
                pending.extend(
                    peers
                        .keys()
                        .filter(|id| !self.visited.contains(*id))
                        .copied(),
                );
            }
            if pending.len() > MAXIMUM_COMPONENT_PENDING {
                return Err(workspace());
            }
        }
        members.sort_unstable();
        Ok(members)
    }
}

fn symbol_id(id: &PackedSymbol) -> Result<SymbolId, RocksError> {
    SymbolId::parse(&unpacked_uuid(id)?).map_err(|_| corrupt())
}
fn members<'a>(
    ids: impl Iterator<Item = &'a PackedSymbol>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<Vec<Value>, RocksError> {
    let mut output =
        crate::retrieval::native::RankedRows::new(10, |a: &(String, u32, SymbolId, String), b| {
            a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
        });
    for id in ids {
        budget.checkpoint()?;
        let symbol = graph.symbol(&symbol_id(id)?, budget)?.ok_or(corrupt())?;
        let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
        let row = (
            file.normalized_path,
            symbol.start_line,
            symbol.symbol_id,
            symbol.qualified_name,
        );
        output.insert(row);
    }
    Ok(output.into_iter().map(|(path, line, id, name)|json!({ "symbolId":id, "qualifiedName":name, "location":format!("{path}:{line}") })).collect())
}

#[cfg(test)]
mod allocation_tests {
    use super::*;

    #[test]
    fn sparse_semantic_pairs_charge_each_independent_leaf() {
        let mut links = SemanticLinks {
            rows: LinkRows::new(),
            bytes: 0,
        };
        links.add([1; 16], [2; 16], 0.96);
        assert_eq!(links.bytes, 896);
        links.add([1; 16], [2; 16], 0.99);
        assert_eq!(links.bytes, 896);
        let mut exceeded = false;
        for index in 1_u64..17_000 {
            let mut source = [0; 16];
            source[8..].copy_from_slice(&(index * 2).to_be_bytes());
            let mut target = [0; 16];
            target[8..].copy_from_slice(&(index * 2 + 1).to_be_bytes());
            links.add(source, target, 0.96);
            links.add(target, source, 0.96);
            if links.bytes > 16 * 1_048_576 {
                exceeded = true;
                break;
            }
        }
        assert!(
            exceeded,
            "singleton peer trees were amortized across unrelated roots"
        );
    }
}
