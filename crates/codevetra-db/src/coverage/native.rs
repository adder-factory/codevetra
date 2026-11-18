//! Source-labelled LCOV snapshots joined to the complete native graph.

use super::{
    CoverageLoadReport, CoverageLoadRequest, CoverageSourceRecord, CoverageStats, CoverageTarget,
    SymbolCoverageFact, SymbolCoverageQuery, SymbolCoverageRecord, validate_source,
    validate_source_and_limit,
};
use crate::{
    GraphDirection, NativeDatabase, StorageError,
    native_database::{dataset, now},
    retrieval::native::{test_path, visit_files, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateNamespace, StateTransaction},
};
use codevetra_domain::{FileId, NormalizedPath, ProjectId, SymbolId};
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const NAMESPACE: StateNamespace = StateNamespace::Coverage;
const SELECTED_FACT_OVERHEAD: usize = 256;
const MAXIMUM_SELECTED_FACT_BYTES: usize = 32 * 1_048_576;

#[derive(Clone, Serialize, Deserialize)]
struct Metadata {
    id: String,
    label: String,
    digest: String,
    loaded: String,
    provenance: serde_json::Value,
}
struct Selected {
    fact: SymbolCoverageFact,
    source: String,
    id: String,
}

impl NativeDatabase {
    /// Return complete current spans for the existing deterministic LCOV join.
    /// # Errors
    /// Rejects project mismatch, corrupt spans, or the bounded target workspace.
    pub async fn current_coverage_targets(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<CoverageTarget>, StorageError> {
        self.require_project(project)?;
        let Some(current) = self.current_generation_record(project).await? else {
            return Ok(Vec::new());
        };
        let generation = current.generation_id().clone();
        self.graph(
            current.generation_id(),
            super::COVERAGE_TIMEOUT,
            move |graph, budget| {
                let mut output = Vec::new();
                let mut bytes = 0_usize;
                visit_symbols(graph, budget, |symbol, budget| {
                    if matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter") {
                        return Ok(());
                    }
                    let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                    bytes = bytes.saturating_add(
                        file.normalized_path.len()
                            + symbol.qualified_name.len()
                            + symbol.symbol_kind.len()
                            + 256,
                    );
                    if bytes > 48 * 1_048_576 || output.len() >= 1_000_000 {
                        return Err(workspace());
                    }
                    output.push(CoverageTarget {
                        generation_id: generation.clone(),
                        symbol_id: symbol.symbol_id,
                        path: NormalizedPath::parse(&file.normalized_path)
                            .map_err(|_| corrupt())?,
                        symbol_kind: symbol.symbol_kind,
                        qualified_name: symbol.qualified_name,
                        start_line: symbol.start_line,
                        end_line: symbol.end_line,
                    });
                    Ok(())
                })?;
                output.sort_by(|a, b| {
                    a.path
                        .cmp(&b.path)
                        .then(a.start_line.cmp(&b.start_line))
                        .then(a.symbol_id.cmp(&b.symbol_id))
                });
                Ok(output)
            },
        )
        .await
    }

    /// Prepare an exact LCOV source snapshot and atomically bind it to its generation.
    /// # Errors
    /// Rejects missing/duplicate symbols, changed publication, or native storage limits.
    pub async fn replace_current_symbol_coverage(
        &self,
        request: CoverageLoadRequest,
    ) -> Result<CoverageLoadReport, StorageError> {
        self.require_project(&request.project_id)?;
        let mut ids = request
            .facts
            .iter()
            .map(|fact| fact.symbol_id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(StorageError::InvalidInput {
                field: "coverage_duplicate_symbol",
            });
        }
        self.graph(
            &request.generation_id,
            super::COVERAGE_TIMEOUT,
            move |graph, budget| {
                for id in ids {
                    if graph.symbol(&id, budget)?.is_none() {
                        return Err(RocksError::InvalidInput {
                            field: "coverage_symbol_id",
                        });
                    }
                }
                Ok(())
            },
        )
        .await?;
        let id = source_id(&request.project_id, &request.source_label);
        let metadata = Metadata {
            id: id.clone(),
            label: request.source_label.clone(),
            digest: request.report_digest.as_str().to_owned(),
            loaded: now(),
            provenance: request.report_metadata,
        };
        let report = CoverageLoadReport {
            generation_id: request.generation_id.clone(),
            source_id: id,
            source_label: request.source_label.clone(),
            symbols_written: request.facts.len() as u64,
        };
        self.replace_dataset(
            crate::native_database::dataset::DatasetReplacement {
                namespace: NAMESPACE,
                name: source_name(&request.source_label),
                expected: Some(request.generation_id),
                metadata: &metadata,
                guard: crate::native_database::dataset::CommitGuard::default(),
            },
            request.facts.into_iter(),
        )
        .await?;
        Ok(report)
    }

    /// Select the best source row per current symbol before filters and ranking.
    /// # Errors
    /// Rejects invalid queries, corrupt source facts, or exhausted bounded graph reads.
    pub async fn current_symbol_coverage(
        &self,
        project: &ProjectId,
        request: &SymbolCoverageQuery,
    ) -> Result<Vec<SymbolCoverageRecord>, StorageError> {
        self.require_project(project)?;
        validate_source_and_limit(request.source.as_deref(), request.limit)?;
        let request = request.clone();
        self.state(
            crate::rocks::StateAccess::read_only(None),
            move |transaction, budget| {
                if transaction.current()?.is_none() {
                    return Ok(Vec::new());
                }
                let selected = SelectedFacts::new(false).read(
                    transaction,
                    budget,
                    request.source.as_deref(),
                )?;
                let snapshot = transaction.graph_snapshot()?;
                let graph = snapshot.graph(budget)?;
                let tests = test_files(&graph, budget)?;
                let mut population = 0_u64;
                visit_symbols(&graph, budget, |symbol, _| {
                    if !matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter") {
                        population += 1;
                    }
                    Ok(())
                })?;
                let reader = CoverageReader {
                    graph: &graph,
                    tests: &tests,
                    population,
                };
                let mut output = crate::retrieval::native::RankedRows::new(
                    usize::from(request.limit),
                    |a: &SymbolCoverageRecord, b| {
                        a.coverage_fraction
                            .partial_cmp(&b.coverage_fraction)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(b.degree_centrality.total_cmp(&a.degree_centrality))
                            .then(b.incoming_edges.cmp(&a.incoming_edges))
                            .then(a.path.cmp(&b.path))
                            .then(a.qualified_name.cmp(&b.qualified_name))
                            .then(a.symbol_id.cmp(&b.symbol_id))
                    },
                );
                for (id, row) in selected {
                    if request
                        .symbol_id
                        .as_ref()
                        .is_some_and(|expected| expected != &id)
                    {
                        continue;
                    }
                    let Some(record) = reader.record(budget, row)? else {
                        continue;
                    };
                    if !matches_record(&record, &request) {
                        continue;
                    }
                    output.insert(record);
                }
                Ok(output.into_vec())
            },
        )
        .await
    }

    /// Aggregate the best current coverage fraction per symbol across selected sources.
    /// # Errors
    /// Rejects invalid source filters, corrupt counts, or bounded native read failures.
    pub async fn current_coverage_stats(
        &self,
        project: &ProjectId,
        source: Option<&str>,
    ) -> Result<CoverageStats, StorageError> {
        self.require_project(project)?;
        validate_source(source)?;
        let source = source.map(ToOwned::to_owned);
        self.state(
            crate::rocks::StateAccess::read_only(None),
            move |transaction, budget| {
                let selected =
                    SelectedFacts::new(true).read(transaction, budget, source.as_deref())?;
                let mut stats = CoverageStats {
                    symbols: 0,
                    lines_found: 0,
                    lines_hit: 0,
                    coverage_fraction: None,
                    sources: 0,
                };
                let mut sources = BTreeSet::new();
                for (_, row) in selected {
                    stats.symbols += 1;
                    stats.lines_found = stats
                        .lines_found
                        .checked_add(row.fact.lines_found)
                        .ok_or(corrupt())?;
                    stats.lines_hit = stats
                        .lines_hit
                        .checked_add(row.fact.lines_hit)
                        .ok_or(corrupt())?;
                    sources.insert(row.id);
                }
                stats.sources = sources.len() as u64;
                stats.coverage_fraction = fraction(stats.lines_found, stats.lines_hit)?;
                Ok(stats)
            },
        )
        .await
    }

    /// List loaded sources and exact counts still bound to the current graph.
    /// # Errors
    /// Rejects project mismatch or corrupt source metadata.
    pub async fn coverage_sources(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<CoverageSourceRecord>, StorageError> {
        self.require_project(project)?;
        self.state(
            crate::rocks::StateAccess::read_only(None),
            |transaction, budget| {
                let current = transaction
                    .current()?
                    .map(|revision| revision.generation_id);
                let mut output = Vec::new();
                for (name, manifest) in
                    dataset::Reader::new(transaction, NAMESPACE).list("source/", budget)?
                {
                    let metadata = metadata(&manifest)?;
                    let mut count = 0_u64;
                    if manifest.generation == current && current.is_some() {
                        dataset::Reader::new(transaction, NAMESPACE)
                            .at(&name)
                            .visit::<SymbolCoverageFact>(&manifest, budget, |_, _| {
                                count += 1;
                                Ok(())
                            })?;
                    }
                    output.push(CoverageSourceRecord {
                        source_id: metadata.id,
                        label: metadata.label,
                        report_digest: metadata.digest,
                        loaded_at: metadata.loaded,
                        current_symbols: count,
                    });
                }
                output.sort_by(|a, b| a.label.cmp(&b.label));
                Ok(output)
            },
        )
        .await
    }

    /// Remove one named coverage source atomically from the visible source set.
    /// # Errors
    /// Rejects project/source mismatch or failed native writes.
    pub async fn drop_coverage_source(
        &self,
        project: &ProjectId,
        source: &str,
    ) -> Result<bool, StorageError> {
        self.require_project(project)?;
        validate_source(Some(source))?;
        self.clear_dataset(NAMESPACE, &source_name(source)).await
    }
}

struct SelectedFacts {
    rows: BTreeMap<SymbolId, Selected>,
    bytes: usize,
    tie_id: bool,
}
impl SelectedFacts {
    fn new(tie_id: bool) -> Self {
        Self {
            rows: BTreeMap::new(),
            bytes: 0,
            tie_id,
        }
    }
    fn read(
        mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        source: Option<&str>,
    ) -> Result<BTreeMap<SymbolId, Selected>, RocksError> {
        let Some(current) = transaction.current()? else {
            return Ok(self.rows);
        };
        for (name, manifest) in
            dataset::Reader::new(transaction, NAMESPACE).list("source/", budget)?
        {
            if manifest.generation.as_ref() != Some(&current.generation_id) {
                continue;
            }
            let metadata = metadata(&manifest)?;
            if source.is_some_and(|source| source != metadata.label) {
                continue;
            }
            dataset::Reader::new(transaction, NAMESPACE)
                .at(&name)
                .visit::<SymbolCoverageFact>(&manifest, budget, |fact, _| {
                    self.select_fact(&metadata, fact)
                })?;
        }
        Ok(self.rows)
    }
}

impl SelectedFacts {
    fn select_fact(
        &mut self,
        metadata: &Metadata,
        fact: SymbolCoverageFact,
    ) -> Result<(), RocksError> {
        let tie_id = self.tie_id;

        if fact.lines_hit > fact.lines_found || fact.functions_hit > fact.functions_found {
            return Err(corrupt());
        }
        let score = fraction(fact.lines_found, fact.lines_hit)?;
        if let Some(existing) = self.rows.get(&fact.symbol_id) {
            let old = fraction(existing.fact.lines_found, existing.fact.lines_hit)?;
            let earlier = if tie_id {
                metadata.id < existing.id
            } else {
                metadata.label < existing.source
            };
            if !(score > old || (score == old && earlier)) {
                return Ok(());
            }
        } else {
            self.bytes = self
                .bytes
                .saturating_add(metadata.label.len() + metadata.id.len() + SELECTED_FACT_OVERHEAD);
            if self.bytes > MAXIMUM_SELECTED_FACT_BYTES {
                return Err(workspace());
            }
        }
        self.rows.insert(
            fact.symbol_id.clone(),
            Selected {
                fact,
                source: metadata.label.clone(),
                id: metadata.id.clone(),
            },
        );
        Ok(())
    }
}

pub(crate) fn fractions(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<SymbolId, f64>, RocksError> {
    let mut output = BTreeMap::new();
    for (id, selected) in SelectedFacts::new(true).read(transaction, budget, None)? {
        if let Some(value) = fraction(selected.fact.lines_found, selected.fact.lines_hit)? {
            output.insert(id, value);
        }
    }
    Ok(output)
}

struct CoverageReader<'a, 'view, 'store> {
    graph: &'a GraphSnapshot<'view, 'store>,
    tests: &'a BTreeSet<FileId>,
    population: u64,
}
impl CoverageReader<'_, '_, '_> {
    fn record(
        &self,
        budget: &mut ReadBudget,
        selected: Selected,
    ) -> Result<Option<SymbolCoverageRecord>, RocksError> {
        let graph = self.graph;
        let population = self.population;
        let id = &selected.fact.symbol_id;

        let Some(symbol) = graph.symbol(id, budget)? else {
            return Ok(None);
        };
        let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
        let (incoming, direct_tests) = self.pressure(budget, id)?;
        Ok(Some(SymbolCoverageRecord {
            symbol_id: id.as_str().to_owned(),
            path: file.normalized_path,
            language: file.language,
            symbol_kind: symbol.symbol_kind,
            qualified_name: symbol.qualified_name,
            source: selected.source,
            lines_found: selected.fact.lines_found,
            lines_hit: selected.fact.lines_hit,
            coverage_fraction: fraction(selected.fact.lines_found, selected.fact.lines_hit)?,
            incoming_edges: incoming,
            direct_test_files: direct_tests,
            degree_centrality: (incoming.to_f64().ok_or(corrupt())?
                / population
                    .saturating_sub(1)
                    .max(1)
                    .to_f64()
                    .ok_or(corrupt())?)
            .min(1.0),
        }))
    }
}

impl CoverageReader<'_, '_, '_> {
    fn pressure(&self, budget: &mut ReadBudget, id: &SymbolId) -> Result<(u64, u64), RocksError> {
        let graph = self.graph;
        let tests = self.tests;

        let mut incoming = 0_u64;
        let mut test_files = BTreeSet::new();
        let mut cursor = None;
        loop {
            let page =
                graph
                    .adjacency(id, GraphDirection::Incoming)
                    .page(cursor.as_ref(), 256, budget)?;
            for link in page.links {
                if link.kind() != codevetra_domain::EdgeKind::Contains {
                    incoming = incoming
                        .checked_add(u64::from(graph.edge(&link, budget)?.site_count))
                        .ok_or(corrupt())?;
                }
                let source = graph
                    .symbol(link.source_symbol_id(), budget)?
                    .ok_or(corrupt())?;
                if tests.contains(&source.file_id) {
                    test_files.insert(source.file_id);
                }
            }
            cursor = page.next;
            if cursor.is_none() {
                return Ok((incoming, test_files.len() as u64));
            }
        }
    }
}

fn test_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeSet<FileId>, RocksError> {
    let mut files = path_test_files(graph, budget)?;
    let mut cursor = None;
    loop {
        let (documents, next) = graph.documents(cursor.as_ref(), 64, budget)?;
        for document in documents {
            if document.kind == codevetra_domain::DocumentKind::Test
                && let Some(file) = document.file_id
            {
                files.insert(file);
            }
            if files.len() > 65_536 {
                return Err(workspace());
            }
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(files);
        }
    }
}

fn path_test_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeSet<FileId>, RocksError> {
    let mut files = BTreeSet::new();
    visit_files(graph, budget, |file, _| {
        if test_path(&file.normalized_path) {
            files.insert(file.file_id);
            if files.len() > 65_536 {
                return Err(workspace());
            }
        }
        Ok(())
    })?;
    Ok(files)
}

fn matches_record(record: &SymbolCoverageRecord, query: &SymbolCoverageQuery) -> bool {
    (query.include_tests || !test_path(&record.path))
        && query
            .maximum_fraction
            .is_none_or(|value| record.coverage_fraction.unwrap_or(0.0) <= value)
        && query
            .minimum_centrality
            .is_none_or(|value| record.degree_centrality >= value)
        && (query.symbol_kinds.is_empty() || query.symbol_kinds.contains(&record.symbol_kind))
        && query
            .path_prefix
            .as_ref()
            .is_none_or(|prefix| record.path.starts_with(prefix))
}

fn metadata(manifest: &dataset::Manifest) -> Result<Metadata, RocksError> {
    let metadata: Metadata =
        serde_json::from_value(manifest.metadata.clone()).map_err(|_| corrupt())?;
    if metadata.loaded.parse::<jiff::Timestamp>().is_err()
        || metadata.label.is_empty()
        || metadata.label.len() > 256
        || codevetra_domain::ContentDigest::parse(&metadata.digest).is_err()
    {
        return Err(corrupt());
    }
    Ok(metadata)
}

fn source_name(label: &str) -> String {
    format!("source/{}", blake3::hash(label.as_bytes()).to_hex())
}
fn source_id(project: &ProjectId, label: &str) -> String {
    let digest = blake3::derive_key(
        "cartograph-native-coverage-source-v1",
        format!("{}:{label}", project.as_str()).as_bytes(),
    );
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    codevetra_domain::CoverageSourceId::from_uuid_v8(bytes)
        .as_str()
        .to_owned()
}
fn fraction(found: u64, hit: u64) -> Result<Option<f64>, RocksError> {
    if hit > found {
        return Err(corrupt());
    }
    Ok(if found == 0 {
        None
    } else {
        Some(hit.to_f64().ok_or(corrupt())? / found.to_f64().ok_or(corrupt())?)
    })
}
fn corrupt() -> RocksError {
    RocksError::Corrupt { field: "coverage" }
}
fn workspace() -> RocksError {
    RocksError::Limit {
        resource: "coverage_workspace",
    }
}
