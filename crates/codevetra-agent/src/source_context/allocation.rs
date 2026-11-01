//! Deterministic identity-based body allocation, using actual JSON sizes.

use std::collections::BTreeSet;

use codevetra_domain::{GenerationId, SymbolId};
use serde::Serialize;

use super::{
    SourceContextOptions, SourceContextRequest, SourceDetail, SourceRepresentation,
    SymbolSourceContext,
};
use crate::{ProjectCancellation, ProjectError, ProjectRuntime};

/// Body selection policy; compact/low-token modes still control whether source is requested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSelectionPolicy {
    /// Preserve selected identities; peripheral symbols receive explicit outlines.
    #[default]
    Adaptive,
    /// Attempt complete bodies for every selected identity within hard bounds.
    Full,
    /// Request bounded source prefixes and disclose all omitted body ranges.
    Focused,
    /// Emit only indexed identity/signature metadata.
    Outline,
}

/// A resolved symbol, with relevance supplied by the task/review owner.
pub struct SourceSelection {
    /// Exact identity, never a shared simple method name.
    pub symbol_id: SymbolId,
    /// Whether the task needs this implementation body.
    pub prioritized: bool,
}

/// Validated public allocation bounds.
#[derive(Clone, Copy)]
pub struct SourceAllocationOptions {
    policy: SourceSelectionPolicy,
    maximum_bytes: usize,
}

impl SourceAllocationOptions {
    /// Set a complete serialized source-allocation budget of 16–512 KiB.
    /// # Errors
    /// Rejects budgets outside the finite output range.
    pub const fn new(
        policy: SourceSelectionPolicy,
        maximum_bytes: usize,
    ) -> Result<Self, ProjectError> {
        if maximum_bytes < 16 * 1024 || maximum_bytes > 512 * 1024 {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            policy,
            maximum_bytes,
        })
    }
}

/// Source windows and accounting, independent of retrieval/evidence packet size.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceAllocation {
    /// Exact/focused source or labeled outlines, in deterministic priority order.
    pub windows: Vec<SymbolSourceContext>,
    /// Exact JSON bytes of this allocation including its metadata and outlines.
    pub encoded_bytes: usize,
    /// Hard allocation bound; enclosing response metadata is accounted separately.
    pub maximum_bytes: usize,
    /// Task-critical symbols whose complete bodies were not delivered.
    pub incomplete_priority: Vec<SymbolId>,
    /// The absence of findings cannot substitute for inspecting omitted bodies.
    pub required_bodies_complete: bool,
    /// Fresh complete bodies actually delivered, independently of priority.
    pub full_bodies: usize,
    /// Focused, truncated, outlined or unavailable bodies, never claimed as inspected.
    pub incomplete_bodies: usize,
    /// How to expand a body using the existing identity-preserving source service.
    pub expand: &'static str,
}

impl ProjectRuntime {
    /// Allocate source by exact selected identities under the existing batch capture/freshness boundary.
    /// # Errors
    /// Rejects excessive selections, source failures, cancellation and metadata that alone exceeds the budget.
    /// A changed generation returns `None`, never a mixture of publications.
    pub async fn allocate_source_with_cancellation(
        &self,
        generation: &GenerationId,
        mut selections: Vec<SourceSelection>,
        options: SourceAllocationOptions,
        cancellation: ProjectCancellation,
    ) -> Result<Option<SourceAllocation>, ProjectError> {
        if selections.len() > 100 {
            return Err(ProjectError::InvalidOptions);
        }
        let mut seen = BTreeSet::new();
        // Stable ordering retains the retrieval owner's relevance order within a priority tier.
        selections.sort_by_key(|selection| !selection.prioritized);
        selections.retain(|selection| seen.insert(selection.symbol_id.clone()));
        let priorities = selections
            .iter()
            .filter(|selection| selection.prioritized)
            .map(|selection| selection.symbol_id.clone())
            .collect::<BTreeSet<_>>();
        let requests = selections
            .into_iter()
            .map(|selection| {
                let detail = match options.policy {
                    SourceSelectionPolicy::Outline => SourceDetail::Outline,
                    SourceSelectionPolicy::Adaptive if !selection.prioritized => {
                        SourceDetail::Outline
                    }
                    SourceSelectionPolicy::Focused => SourceDetail::Focused,
                    SourceSelectionPolicy::Full | SourceSelectionPolicy::Adaptive => {
                        SourceDetail::Full
                    }
                };
                let maximum = if detail == SourceDetail::Focused {
                    8 * 1024
                } else {
                    256 * 1024
                };
                Ok(SourceContextRequest::new(
                    selection.symbol_id,
                    SourceContextOptions::new(0, maximum)?.with_detail(detail),
                ))
            })
            .collect::<Result<Vec<_>, ProjectError>>()?;
        let Some(contexts) = self
            .source_context_batch_if_current_with_cancellation(
                generation,
                requests,
                cancellation.clone(),
            )
            .await?
        else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || {
            allocate(contexts, &priorities, options.maximum_bytes, &cancellation)
        })
        .await
        .map_err(|_| ProjectError::SourceContextUnavailable)?
        .map(Some)
    }
}

fn allocate(
    contexts: Vec<SymbolSourceContext>,
    priorities: &BTreeSet<SymbolId>,
    maximum_bytes: usize,
    cancellation: &ProjectCancellation,
) -> Result<SourceAllocation, ProjectError> {
    let mut output = SourceAllocation {
        windows: contexts
            .iter()
            .cloned()
            .map(|mut context| {
                context.omit_body();
                context
            })
            .collect(),
        encoded_bytes: 0,
        maximum_bytes,
        incomplete_priority: priorities.iter().cloned().collect(),
        required_bodies_complete: false,
        full_bodies: 0,
        incomplete_bodies: 0,
        expand: "codevetra node --symbol-id <symbol.symbol_id> --detail full; use files --format read --file <symbol.path> --byte-offset <omitted_ranges.startByte> --byte-limit 65536 for exact missing bytes; continue from excerpt.endByte",
    };
    account(&mut output, priorities)?;
    if output.encoded_bytes > maximum_bytes {
        return Err(ProjectError::SourceContextUnavailable);
    }
    for (index, context) in contexts.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let outline = std::mem::replace(&mut output.windows[index], context);
        account(&mut output, priorities)?;
        if output.encoded_bytes > maximum_bytes {
            output.windows[index] = outline;
            account(&mut output, priorities)?;
        }
    }
    Ok(output)
}

fn account(
    output: &mut SourceAllocation,
    priorities: &BTreeSet<SymbolId>,
) -> Result<(), ProjectError> {
    output.incomplete_priority = priorities
        .iter()
        .filter(|id| {
            !output.windows.iter().any(|window| {
                window.symbol.symbol_id() == *id
                    && window.fresh
                    && window.representation == SourceRepresentation::Full
            })
        })
        .cloned()
        .collect();
    output.required_bodies_complete = output.incomplete_priority.is_empty();
    output.full_bodies = output
        .windows
        .iter()
        .filter(|window| window.fresh && window.representation == SourceRepresentation::Full)
        .count();
    output.incomplete_bodies = output.windows.len().saturating_sub(output.full_bodies);
    // The self-reported decimal byte count can change the encoding length once.
    for _ in 0..4 {
        let bytes = serde_json::to_vec(output)
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .len();
        if bytes == output.encoded_bytes {
            return Ok(());
        }
        output.encoded_bytes = bytes;
    }
    Err(ProjectError::SourceContextUnavailable)
}
