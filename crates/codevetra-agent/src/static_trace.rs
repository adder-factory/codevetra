//! Static source structure and exact indexed call occurrences share one generation.
use crate::{
    ProjectCancellation, ProjectError, ProjectRuntime, SourceContextOptions, SourceContextRequest,
    SourceRepresentation,
};
use codevetra_db::{
    CurrentReferenceSite, CurrentSymbolRecord, ReferencePageRequest, ReferenceSelector,
    ReferenceSpanPrecision,
};
use codevetra_domain::{GenerationId, SymbolId};
use codevetra_extract::{StaticTraceInput, SyntaxTrace, static_syntax_trace};
use serde::Serialize;
use std::{
    collections::{BTreeSet, VecDeque},
    time::Duration,
};

#[cfg(test)]
mod tests;

/// Finite trace request. Expansion is a definition table, not a flattened execution list.
pub struct StaticTraceRequest {
    /// Exact entry declaration.
    pub symbol_id: SymbolId,
    /// Optional exact changed/target identity, annotated without dropping guards.
    pub focus: Option<SymbolId>,
    /// Maximum retained syntax steps across all definitions, 1..=512.
    pub maximum_steps: usize,
    /// Maximum call expansion depth, 0..=4.
    pub maximum_depth: usize,
    /// Maximum JSON report bytes, 16..=512 KiB.
    pub maximum_bytes: usize,
}

/// Call identity evidence attached to one local syntax step.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticTraceCall {
    /// Syntax step index inside the containing function.
    pub step: usize,
    /// Unique resolved identity; None means unresolved, ambiguous or coarse.
    pub target: Option<SymbolId>,
    /// Explicit identity/expansion disposition.
    pub state: &'static str,
    /// Reference occurrence precision, resolver confidence and provenance.
    pub evidence: Vec<CurrentReferenceSite>,
    /// The target's source structure is present in the report definition table.
    pub expanded: bool,
    /// Whether this exact target matches the optional request focus.
    pub focused: bool,
}

/// One selected body; call expansion links point into the same report's function table.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticTraceFunction {
    /// Exact declaration identity, path and generation.
    pub symbol: CurrentSymbolRecord,
    /// Shortest discovered call expansion depth; never a timestamp or execution order.
    pub expansion_depth: usize,
    /// Nested source structure. Parent indices preserve branch and cleanup grouping.
    pub syntax: SyntaxTrace,
    /// Evidenced call relations, separate from source ordering.
    pub calls: Vec<StaticTraceCall>,
    /// Exact generation-owned event/queue/HTTP/navigation sites in this body. These
    /// retain their own uncertainty; their position does not order downstream work.
    pub boundaries: Vec<CurrentSymbolRecord>,
}

/// Possible static paths with bounded ordered syntax and separate resolution evidence.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticTraceReport {
    /// Explicit interpretation boundary.
    pub evidence_kind: &'static str,
    /// Immutable graph publication shared by all included bodies/references.
    pub generation: GenerationId,
    /// All returned source was freshly validated against this generation.
    pub fresh: bool,
    /// Root exact declaration identity.
    pub entry: SymbolId,
    /// Optional focus; does not hide surrounding guards or cleanup.
    pub focus: Option<SymbolId>,
    /// Bounded definition table, with nested ordered steps and explicit call links.
    pub functions: Vec<StaticTraceFunction>,
    /// Truncation, unsupported syntax or unresolved evidence exists.
    pub partial: bool,
    /// Stable limits/unknowns; an empty list is not proof of path feasibility.
    pub unknowns: Vec<&'static str>,
    /// Actual serialized report JSON bytes.
    pub encoded_bytes: usize,
    /// Supported slice and boundaries visible even for empty results.
    pub coverage: &'static str,
}

impl StaticTraceReport {
    fn new(generation: GenerationId, request: &StaticTraceRequest) -> Self {
        Self {
            evidence_kind: "possible_static_execution_paths_not_runtime_observation_or_feasibility_proof",
            generation,
            fresh: true,
            entry: request.symbol_id.clone(),
            focus: request.focus.clone(),
            functions: Vec::new(),
            partial: false,
            unknowns: Vec::new(),
            encoded_bytes: 0,
            coverage: "JS/JSX/TS/TSX callable bodies: source order with nested guards, switch cases, loops, abrupt exits and try/catch/finally. Exact indexed call occurrences only; table links do not imply runtime or concurrent ordering. Callback bodies are deferred; evidenced boundary sites are attached with scope and resolution state, without downstream ordering. Scheduler binding and dynamic dispatch remain unknown. Other languages explicitly unsupported.",
        }
    }

    fn unknown(&mut self, reason: &'static str) {
        self.partial = true;
        if !self.unknowns.contains(&reason) {
            self.unknowns.push(reason);
        }
    }
    fn record_unknowns(&mut self, reasons: impl IntoIterator<Item = &'static str>) {
        for reason in reasons {
            self.unknown(reason);
        }
    }

    fn has_capacity(&mut self, steps: usize, maximum_steps: usize) -> bool {
        if self.functions.len() >= 16 {
            self.unknown("function_limit");
            return false;
        }
        if steps >= maximum_steps {
            self.unknown("step_limit");
            return false;
        }
        true
    }

    fn account(&mut self) -> Result<(), ProjectError> {
        for _ in 0..4 {
            let bytes = serde_json::to_vec(&self)
                .map_err(|_| ProjectError::SourceContextUnavailable)?
                .len();
            if bytes == self.encoded_bytes {
                return Ok(());
            }
            self.encoded_bytes = bytes;
        }
        Err(ProjectError::SourceContextUnavailable)
    }
}

impl ProjectRuntime {
    /// Trace bounded possible static paths without executing project code.
    /// # Errors
    /// Rejects invalid bounds, cancellation, unavailable source and generation changes.
    pub async fn static_trace(
        &self,
        request: StaticTraceRequest,
        cancellation: ProjectCancellation,
    ) -> Result<StaticTraceReport, ProjectError> {
        if request.maximum_steps == 0
            || request.maximum_steps > 512
            || request.maximum_depth > 4
            || !(16 * 1024..=512 * 1024).contains(&request.maximum_bytes)
        {
            return Err(ProjectError::InvalidOptions);
        }
        let control = codevetra_db::CodeVetraDatabase::request_control(Duration::from_secs(30))
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        let mut progress = None;
        let (outcome, cancelled) = {
            let operation = codevetra_db::CodeVetraDatabase::with_request(
                control.clone(),
                self.static_trace_inner(&request, cancellation.clone(), &mut progress),
            );
            tokio::pin!(operation);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    control.cancel();
                    (operation.await, true)
                }
                outcome = &mut operation => (outcome, cancellation.is_cancelled()),
            }
        };
        if cancelled {
            let mut report = progress.ok_or(ProjectError::RequestCancelled)?;
            report.fresh = false;
            report.unknown("cancelled_generation_freshness_not_rechecked");
            reconcile_expanded_calls(&mut report);
            report.account()?;
            return Ok(report);
        }
        let project = outcome?;
        let report = progress.ok_or(ProjectError::SourceContextUnavailable)?;
        codevetra_db::CodeVetraDatabase::with_request(
            control,
            self.finish_static_trace(&project, report, request.maximum_bytes),
        )
        .await
    }

    async fn static_trace_inner(
        &self,
        request: &StaticTraceRequest,
        cancellation: ProjectCancellation,
        progress: &mut Option<StaticTraceReport>,
    ) -> Result<codevetra_domain::ProjectId, ProjectError> {
        let (project_id, generation_id) = self.trace_identity().await?;
        *progress = Some(StaticTraceReport::new(generation_id, request));
        let report = progress
            .as_mut()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let mut pending = VecDeque::from([(request.symbol_id.clone(), 0_usize)]);
        let mut seen = BTreeSet::from([request.symbol_id.clone()]);
        let mut steps = 0;
        while let Some((id, depth)) = pending.pop_front() {
            if cancellation.is_cancelled() {
                return Err(ProjectError::RequestCancelled);
            }
            if !report.has_capacity(steps, request.maximum_steps) {
                break;
            }
            let Some(mut contexts) = self
                .source_context_batch_if_current_with_cancellation(
                    &report.generation,
                    vec![SourceContextRequest::new(
                        id,
                        SourceContextOptions::new(0, 256 * 1024)?,
                    )],
                    cancellation.clone(),
                )
                .await?
            else {
                return Err(ProjectError::SourceContextUnavailable);
            };
            let Some(context) = contexts.pop() else {
                report.unknown("missing_target_definition");
                continue;
            };
            if !context.fresh() {
                report.fresh = false;
                report.functions.clear();
                report.unknown("stale_source_no_trace_returned");
                break;
            }
            if context.representation() != SourceRepresentation::Full {
                report.unknown("body_exceeds_source_window_or_unavailable");
                continue;
            }
            let symbol = context.symbol().clone();
            let boundaries = self.trace_boundaries(&project_id, &symbol, report).await?;
            let remaining = request.maximum_steps - steps;
            let syntax = trace_syntax(context, remaining, cancellation.clone()).await?;
            steps += syntax.steps.len();
            if syntax.partial {
                report.unknown("syntax_incomplete_see_function_unknowns");
            }
            let (calls, unknowns) = self
                .trace_calls(
                    CallInput {
                        project: &project_id,
                        generation: &report.generation,
                        symbol: &symbol,
                        syntax: &syntax,
                        focus: request.focus.as_ref(),
                        depth,
                        maximum_depth: request.maximum_depth,
                    },
                    &mut seen,
                    &mut pending,
                )
                .await?;
            report.record_unknowns(unknowns);
            report.functions.push(StaticTraceFunction {
                symbol,
                expansion_depth: depth,
                syntax,
                calls,
                boundaries,
            });
            report.account()?;
            if report.encoded_bytes.saturating_add(1024) > request.maximum_bytes {
                report.functions.pop();
                report.unknown("byte_limit");
                break;
            }
            #[cfg(test)]
            if cancellation.trace_cancel_after == Some(report.functions.len()) {
                cancellation.cancel();
            }
        }
        Ok(project_id)
    }

    async fn trace_identity(
        &self,
    ) -> Result<(codevetra_domain::ProjectId, GenerationId), ProjectError> {
        let snapshot = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let current = snapshot
            .current
            .ok_or(ProjectError::SourceContextUnavailable)?;
        Ok((snapshot.project_id, current.generation_id))
    }

    async fn trace_boundaries(
        &self,
        project: &codevetra_domain::ProjectId,
        symbol: &CurrentSymbolRecord,
        report: &mut StaticTraceReport,
    ) -> Result<Vec<CurrentSymbolRecord>, ProjectError> {
        let sites = self
            .database()
            .current_symbols_at_range(codevetra_db::CurrentSourceRangeLookup::new(
                codevetra_db::CurrentGenerationLookup::new(project, symbol.generation_id()),
                codevetra_db::SourceLineRange::new(
                    symbol.path(),
                    symbol.start_line(),
                    symbol.end_line(),
                ),
                500,
            ))
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        if sites.len() == 500 {
            report.unknown("boundary_site_scan_limit");
        }
        Ok(sites
            .into_iter()
            .filter(|site| {
                site.boundary().is_some()
                    && symbol.start_byte() <= site.start_byte()
                    && site.end_byte() <= symbol.end_byte()
            })
            .collect())
    }

    async fn finish_static_trace(
        &self,
        project: &codevetra_domain::ProjectId,
        mut report: StaticTraceReport,
        maximum_bytes: usize,
    ) -> Result<StaticTraceReport, ProjectError> {
        reconcile_expanded_calls(&mut report);
        let latest = self
            .database()
            .current_generation_record(project)
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        if latest
            .as_ref()
            .is_none_or(|generation| generation.generation_id() != &report.generation)
        {
            return Err(ProjectError::SourceContextUnavailable);
        }
        report.account()?;
        if report.encoded_bytes > maximum_bytes {
            return Err(ProjectError::SourceContextUnavailable);
        }
        Ok(report)
    }
    async fn trace_calls(
        &self,
        input: CallInput<'_>,
        seen: &mut BTreeSet<SymbolId>,
        pending: &mut VecDeque<(SymbolId, usize)>,
    ) -> Result<(Vec<StaticTraceCall>, Vec<&'static str>), ProjectError> {
        let mut calls = Vec::new();
        let mut unknowns = Vec::new();
        for step in &input.syntax.steps {
            let Some(byte) = step.call_anchor else {
                continue;
            };
            if calls.len() >= 64 {
                unknowns.push("call_resolution_limit");
                break;
            }
            let page = self
                .database()
                .current_reference_page(ReferencePageRequest {
                    project: input.project,
                    generation: input.generation,
                    selector: ReferenceSelector::Occurrence {
                        file: input.symbol.file_id().clone(),
                        byte,
                    },
                    limit: 20,
                    after: None,
                })
                .await
                .map_err(|_| ProjectError::SourceContextUnavailable)?;
            let evidence = page
                .sites
                .into_iter()
                .filter(|site| {
                    matches!(
                        site.occurrence.reference_kind.as_str(),
                        "calls" | "instantiates"
                    )
                })
                .collect::<Vec<_>>();
            let targets = evidence
                .iter()
                .filter_map(|site| site.occurrence.target_symbol_id.clone())
                .collect::<BTreeSet<_>>();
            let exact = !page.truncated
                && !evidence.is_empty()
                && evidence.iter().all(|site| {
                    site.occurrence.span_precision == ReferenceSpanPrecision::Exact
                        && site.occurrence.target_symbol_id.is_some()
                });
            let target = (exact && targets.len() == 1)
                .then(|| targets.into_iter().next())
                .flatten();
            let state = admit_call_target(&input, target.as_ref(), seen, pending, &mut unknowns);
            calls.push(StaticTraceCall {
                step: step.id,
                focused: target.is_some() && target.as_ref() == input.focus,
                target,
                state,
                evidence,
                expanded: false,
            });
        }
        Ok((calls, unknowns))
    }
}

fn admit_call_target(
    input: &CallInput<'_>,
    target: Option<&SymbolId>,
    seen: &mut BTreeSet<SymbolId>,
    pending: &mut VecDeque<(SymbolId, usize)>,
    unknowns: &mut Vec<&'static str>,
) -> &'static str {
    let Some(target) = target else {
        unknowns.push("unresolved_calls");
        return "unresolved_ambiguous_or_coarse_call";
    };
    if seen.contains(target) {
        return "definition_reuse_or_recursion_boundary";
    }
    if input.depth >= input.maximum_depth {
        unknowns.push("call_depth_limit");
        return "call_depth_limit";
    }
    if seen.len() >= 16 {
        unknowns.push("function_limit");
        return "function_limit";
    }
    seen.insert(target.clone());
    pending.push_back((target.clone(), input.depth + 1));
    "resolved_static_call"
}

fn reconcile_expanded_calls(report: &mut StaticTraceReport) {
    let ids = report
        .functions
        .iter()
        .map(|function| function.symbol.symbol_id().clone())
        .collect::<BTreeSet<_>>();
    for function in &mut report.functions {
        for call in &mut function.calls {
            call.expanded = call.target.as_ref().is_some_and(|id| ids.contains(id));
        }
    }
}

struct CallInput<'a> {
    project: &'a codevetra_domain::ProjectId,
    generation: &'a GenerationId,
    symbol: &'a CurrentSymbolRecord,
    syntax: &'a SyntaxTrace,
    focus: Option<&'a SymbolId>,
    depth: usize,
    maximum_depth: usize,
}

async fn trace_syntax(
    context: crate::SymbolSourceContext,
    remaining: usize,
    token: ProjectCancellation,
) -> Result<SyntaxTrace, ProjectError> {
    tokio::task::spawn_blocking(move || {
        let excerpt = context
            .excerpt()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        static_syntax_trace(
            StaticTraceInput {
                source: excerpt.text(),
                language: codevetra_domain::SourceLanguage::ALL
                    .into_iter()
                    .find(|language| language.as_str() == context.symbol().language())
                    .ok_or(ProjectError::SourceContextUnavailable)?,
                byte_offset: excerpt.start_byte(),
                start_line: excerpt.start_line(),
                symbol_start: context.symbol().start_byte(),
                symbol_end: context.symbol().end_byte(),
                maximum_steps: remaining,
                maximum_depth: 24,
            },
            || token.is_cancelled(),
        )
        .map_err(|error| {
            if matches!(error, codevetra_extract::ExtractError::Cancelled) {
                ProjectError::RequestCancelled
            } else {
                ProjectError::SourceContextUnavailable
            }
        })
    })
    .await
    .map_err(|_| ProjectError::SourceContextUnavailable)?
}
