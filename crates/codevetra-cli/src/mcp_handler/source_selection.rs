//! Task relevance selects identities; shared simple names never select every sibling body.

use codevetra_agent::{SourceSelection, SourceSelectionPolicy};
use codevetra_mcp::ToolError;
use codevetra_search::{ContextPacket, EvidenceReason};
use serde_json::{Map, Value};

pub(super) fn refresh_projected_coverage(source: &mut Value) {
    if source.get("representation").and_then(Value::as_str) == Some("unavailable") {
        return;
    }
    let bounds = source
        .pointer("/symbol/start_byte")
        .and_then(Value::as_u64)
        .zip(source.pointer("/symbol/end_byte").and_then(Value::as_u64))
        .zip(
            source
                .pointer("/excerpt/start_byte")
                .and_then(Value::as_u64),
        )
        .zip(source.pointer("/excerpt/end_byte").and_then(Value::as_u64));
    let Some((((start, end), first), last)) = bounds.filter(|(((start, end), _), _)| start <= end)
    else {
        return;
    };
    let mut omitted = Vec::new();
    let first = first.clamp(start, end);
    let last = last.clamp(start, end);
    if start < first {
        omitted.push(serde_json::json!({"startByte":start,"endByte":first}));
    }
    if last < end {
        omitted.push(serde_json::json!({"startByte":last,"endByte":end}));
    }
    source["representation"] = serde_json::json!(if omitted.is_empty() {
        "full"
    } else {
        "focused"
    });
    source["omitted_ranges"] = Value::Array(omitted);
}

pub(super) async fn review_allocation(
    handler: &super::CodeVetraMcpHandler,
    packet: &codevetra_search::ReviewPacket,
    priorities: Option<&BTreeSet<codevetra_domain::SymbolId>>,
    policy: SourceSelectionPolicy,
    cancellation: codevetra_agent::ProjectCancellation,
) -> Result<Value, ToolError> {
    let Some(generation) = packet
        .generation()
        .filter(|_| packet.freshness() == codevetra_search::IndexFreshness::Current)
    else {
        return Ok(
            serde_json::json!({"state":"unavailable","reason":"source_not_current","requiredBodiesComplete":false}),
        );
    };
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for item in packet.evidence() {
        let Some(symbol) = item.symbol_id() else {
            continue;
        };
        if !seen.insert(symbol.clone()) {
            continue;
        }
        selected.push(SourceSelection {
            symbol_id: symbol.clone(),
            prioritized: priorities.map_or_else(
                || {
                    packet
                        .indexed_changed_files()
                        .iter()
                        .any(|path| path.as_str() == item.path())
                },
                |ids| ids.contains(symbol),
            ),
        });
    }
    // Preserve every required body's identity even if the general packet omitted it.
    if let Some(priorities) = priorities {
        for id in priorities {
            if seen.insert(id.clone()) {
                selected.push(SourceSelection {
                    symbol_id: id.clone(),
                    prioritized: true,
                });
            }
        }
    }
    if selected.len() > 100 {
        return Ok(
            serde_json::json!({"state":"unavailable","reason":"source_selection_limit","requiredBodiesComplete":false,
            "selectedSymbols":selected.len(),"maximumSymbols":100,"nextAction":"Narrow pathFilter or maxChangedFiles"}),
        );
    }
    let options = codevetra_agent::SourceAllocationOptions::new(policy, 512 * 1024)
        .map_err(|_| super::invalid_arguments())?;
    let allocation = handler
        .runtime
        .allocate_source_with_cancellation(
            generation.generation_id(),
            selected,
            options,
            cancellation,
        )
        .await
        .map_err(|error| super::project_error(&error))?;
    allocation.map_or_else(
        || Ok(serde_json::json!({"state":"unavailable","reason":"generation_changed","requiredBodiesComplete":false})),
        |allocation| serde_json::to_value(allocation).map_err(|_| ToolError::internal()),
    )
}
use std::collections::BTreeSet;

pub(super) fn parse_policy(
    arguments: &Map<String, Value>,
) -> Result<SourceSelectionPolicy, ToolError> {
    match super::optional_text(arguments, "sourceDetail")?.unwrap_or("adaptive") {
        "adaptive" => Ok(SourceSelectionPolicy::Adaptive),
        "full" => Ok(SourceSelectionPolicy::Full),
        "focused" => Ok(SourceSelectionPolicy::Focused),
        "outline" => Ok(SourceSelectionPolicy::Outline),
        _ => Err(super::invalid_arguments()),
    }
}

pub(super) fn select(
    packet: &ContextPacket,
    task: &str,
    maximum_symbols: usize,
    maximum_files: usize,
) -> Vec<SourceSelection> {
    let mut seen = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut selected = Vec::new();
    let mut priorities = priority_symbols(packet, task);
    include_call_ancestors(packet, &mut priorities);
    let primary_paths = packet
        .edit_candidates()
        .candidates()
        .iter()
        .map(codevetra_search::EditCandidate::path)
        .collect::<BTreeSet<_>>();
    let ranked = packet
        .evidence()
        .iter()
        .filter(|item| primary_paths.contains(item.path()))
        .chain(
            packet
                .evidence()
                .iter()
                .filter(|item| !primary_paths.contains(item.path())),
        );
    for item in ranked {
        if selected.len() >= maximum_symbols {
            break;
        }
        let Some(symbol) = item.symbol_id() else {
            continue;
        };
        if (!paths.contains(item.path()) && paths.len() >= maximum_files)
            || !seen.insert(symbol.clone())
        {
            continue;
        }
        paths.insert(item.path().to_owned());
        let prioritized = selected.is_empty() || priorities.contains(symbol);
        selected.push(SourceSelection {
            symbol_id: symbol.clone(),
            prioritized,
        });
    }
    selected
}

fn priority_symbols(packet: &ContextPacket, task: &str) -> BTreeSet<codevetra_domain::SymbolId> {
    let exact = packet
        .evidence()
        .iter()
        .filter(|item| item.reasons().contains(&EvidenceReason::ExactName))
        .filter_map(codevetra_search::EvidenceItem::symbol_id)
        .collect::<BTreeSet<_>>();
    let mut priorities = BTreeSet::new();
    if exact.len() == 1 {
        priorities.extend(exact.iter().copied().cloned());
    }
    for word in task.split(|character: char| {
        !character.is_alphanumeric() && !matches!(character, '_' | ':' | '.')
    }) {
        if word.is_empty() {
            continue;
        }
        let matches = packet
            .evidence()
            .iter()
            .filter(|item| {
                item.qualified_name() == word
                    || item.qualified_name().rsplit("::").next() == Some(word)
            })
            .filter_map(codevetra_search::EvidenceItem::symbol_id)
            .collect::<BTreeSet<_>>();
        if matches.len() == 1 {
            priorities.extend(matches.into_iter().cloned());
        }
    }
    priorities
}

fn include_call_ancestors(
    packet: &ContextPacket,
    priorities: &mut BTreeSet<codevetra_domain::SymbolId>,
) {
    // Preserve the indexed call-path ancestors of selected identities, not every neighbor.
    for _ in 0..packet.evidence().len() {
        let before = priorities.len();
        for item in packet.evidence() {
            if let Some(edge) = item.graph()
                && edge.edge_kind() == "calls"
                && priorities.contains(edge.to_symbol_id())
            {
                priorities.insert(edge.from_symbol_id().clone());
            }
        }
        if priorities.len() == before {
            break;
        }
    }
}
