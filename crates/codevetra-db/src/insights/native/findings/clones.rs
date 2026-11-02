//! Canonical clone classes suppress symmetric rows and preserve bounded member evidence.

mod groups;

use groups::Groups;

use super::{
    corrupt, documents::Documents, framework, inventory::Base, packed_tree_bytes,
    packed_tree_insertion_bytes, rules, workspace,
};
use crate::{
    insights::StructuralFinding,
    retrieval::native::retain,
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
type SemanticLinks = BTreeMap<PackedSymbol, BTreeMap<PackedSymbol, f64>>;

impl Classes {
    pub(super) fn new(
        transaction: &StateTransaction<'_>,
        graph: &GraphSnapshot<'_, '_>,
        documents: &Documents,
        budget: &mut ReadBudget,
    ) -> Result<Self, RocksError> {
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
            let document = handle.load(graph, budget)?;
            if !document.clones.enabled {
                continue;
            }
            let packed = packed_uuid(id.as_str())?;
            let node = packed_tree_insertion_bytes(output.production.len());
            output.production.insert(packed);
            bytes = bytes.checked_add(node).ok_or_else(workspace)?;
            if symbol.end_line - symbol.start_line + 1 >= 6 {
                let hash = blake3::Hash::from_hex(symbol.structural_digest.as_str())
                    .map_err(|_| corrupt())?;
                exact.push(*hash.as_bytes(), packed, &mut bytes)?;
            }
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
        }
        output.absorb(exact, "exact", "exact_copies", budget)?;
        let near = output.near_groups(graph, documents, budget)?;
        output.absorb(near, "near", "normalized_shape_copies", budget)?;
        output.semantic(transaction, graph, documents, budget)?;
        Ok(output)
    }
    fn near_groups(
        &self,
        graph: &GraphSnapshot<'_, '_>,
        documents: &Documents,
        budget: &mut ReadBudget,
    ) -> Result<Groups, RocksError> {
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
            if symbol.end_line - symbol.start_line + 1 < 12
                || document.health("literal_bytes") / size > 0.6
            {
                continue;
            }
            groups.push(*blake3::hash(shape.as_bytes()).as_bytes(), *id, &mut bytes)?;
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
        }
        Ok(groups)
    }
    fn absorb(
        &mut self,
        groups: Groups,
        kind: &'static str,
        metric: &'static str,
        budget: &ReadBudget,
    ) -> Result<(), RocksError> {
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
        if self.bytes() > 16 * 1_048_576 {
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
        graph: &GraphSnapshot<'_, '_>,
        documents: &Documents,
        budget: &mut ReadBudget,
    ) -> Result<(), RocksError> {
        let mut eligible = BTreeSet::new();
        for id in self.production.difference(&self.claimed) {
            let document = documents
                .get(&symbol_id(id)?)?
                .ok_or(corrupt())?
                .load(graph, budget)?;
            let symbol = graph.symbol(&symbol_id(id)?, budget)?.ok_or(corrupt())?;
            if document.partial.peers == 0 && symbol.end_line - symbol.start_line + 1 >= 6 {
                eligible.insert(*id);
            }
        }
        let (links, bytes) = semantic_links(
            transaction,
            budget,
            &eligible,
            self.bytes() + packed_tree_bytes(eligible.len()),
        )?;
        let mut visited = BTreeSet::new();
        for start in links.keys() {
            budget.checkpoint()?;
            if visited.contains(start) {
                continue;
            }
            let members = connected_members(&links, start, &mut visited, budget)?;
            let representative = *members.first().ok_or(corrupt())?;
            let peers = links.get(&representative).ok_or(corrupt())?;
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
            if self.bytes() + bytes > 32 * 1_048_576 {
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
                detail["semanticThreshold"] = json!(0.95);
            }
            return Ok(Some(rules::finding(
                base,
                "duplicate_code",
                class.metric,
                class.count.to_f64().ok_or(corrupt())?,
                if class.kind == "exact" {
                    "warning"
                } else {
                    "info"
                },
                Some(detail),
            )));
        }
        let partial = &base.document.partial;
        if self.claimed.contains(id)
            || !self.production.contains(id)
            || partial.peers == 0
            || partial.representative.as_ref() != Some(&base.symbol.symbol_id)
            || base.symbol.end_line - base.symbol.start_line + 1 < 12
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
        Ok(Some(rules::finding(
            base,
            "duplicate_code",
            "partial_clone_peers",
            partial.peers.to_f64().ok_or(corrupt())?,
            "info",
            Some(detail),
        )))
    }
}

fn semantic_links(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    eligible: &BTreeSet<PackedSymbol>,
    mut bytes: usize,
) -> Result<(SemanticLinks, usize), RocksError> {
    let mut links = SemanticLinks::new();
    materialize::visit_current(transaction, budget, |edge, _| {
        let source = packed_uuid(edge.source.as_str())?;
        let target = packed_uuid(edge.target.as_str())?;
        if edge.score >= 0.95 && eligible.contains(&source) && eligible.contains(&target) {
            for (source, target) in [(source, target), (target, source)] {
                add_semantic_link(&mut links, &mut bytes, source, target, edge.score);
            }
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
        }
        Ok(())
    })?;
    Ok((links, bytes))
}

fn add_semantic_link(
    links: &mut SemanticLinks,
    bytes: &mut usize,
    source: PackedSymbol,
    target: PackedSymbol,
    score: f64,
) {
    if !links.contains_key(&source) {
        *bytes += if links.is_empty() { 512 } else { 128 };
    }
    let peers = links.entry(source).or_default();
    if !peers.contains_key(&target) {
        *bytes += packed_tree_insertion_bytes(peers.len());
    }
    peers
        .entry(target)
        .and_modify(|old| *old = old.max(score))
        .or_insert(score);
}

fn connected_members(
    links: &SemanticLinks,
    start: &PackedSymbol,
    visited: &mut BTreeSet<PackedSymbol>,
    budget: &mut ReadBudget,
) -> Result<Vec<PackedSymbol>, RocksError> {
    let mut pending = vec![*start];
    let mut members = Vec::new();
    while let Some(id) = pending.pop() {
        budget.checkpoint()?;
        if !visited.insert(id) {
            continue;
        }
        members.push(id);
        if let Some(peers) = links.get(&id) {
            pending.extend(peers.keys().filter(|id| !visited.contains(*id)).copied());
        }
        if pending.len() > 65_536 {
            return Err(workspace());
        }
    }
    members.sort_unstable();
    Ok(members)
}
fn symbol_id(id: &PackedSymbol) -> Result<SymbolId, RocksError> {
    SymbolId::parse(&unpacked_uuid(id)?).map_err(|_| corrupt())
}
fn members<'a>(
    ids: impl Iterator<Item = &'a PackedSymbol>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<Vec<Value>, RocksError> {
    let mut output = Vec::new();
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
        retain(&mut output, row, 10, |a, b| {
            a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
        });
    }
    Ok(output.into_iter().map(|(path, line, id, name)|json!({ "symbolId":id, "qualifiedName":name, "location":format!("{path}:{line}") })).collect())
}

#[cfg(test)]
mod allocation_tests {
    use super::*;

    #[test]
    fn sparse_semantic_pairs_charge_each_independent_leaf() {
        let mut links = SemanticLinks::new();
        let mut bytes = 0;
        add_semantic_link(&mut links, &mut bytes, [1; 16], [2; 16], 0.96);
        assert_eq!(bytes, 896);
        add_semantic_link(&mut links, &mut bytes, [1; 16], [2; 16], 0.99);
        assert_eq!(bytes, 896);
        let mut exceeded = false;
        for index in 1_u64..17_000 {
            let mut source = [0; 16];
            source[8..].copy_from_slice(&(index * 2).to_be_bytes());
            let mut target = [0; 16];
            target[8..].copy_from_slice(&(index * 2 + 1).to_be_bytes());
            add_semantic_link(&mut links, &mut bytes, source, target, 0.96);
            add_semantic_link(&mut links, &mut bytes, target, source, 0.96);
            if bytes > 16 * 1_048_576 {
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
