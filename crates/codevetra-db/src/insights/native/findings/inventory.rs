//! Detector inputs stream from canonical metadata and compact topology.

use super::file_counts::{FileCounts, reserve};

use super::{clones, corrupt, documents::Documents, framework, row_bytes, rules, workspace};
use crate::{
    FileInput, GraphDirection, SymbolInput,
    insights::StructuralFinding,
    retrieval::native::simple_name,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction, StoredRevision, packed_uuid},
};
use codevetra_domain::{EdgeKind, SourceLanguage, SymbolId, Visibility};
use num_traits::ToPrimitive as _;
use std::collections::{BTreeMap, BTreeSet};

const RECENT_PUBLICATION_SECONDS: i64 = 30 * 86_400;
const SYMBOLS_PER_BATCH: usize = 4096;
const MAXIMUM_FINDING_BYTES: usize = 16 * 1_048_576;
const MAXIMUM_REFERENCE_BYTES: usize = 8 * 1_048_576;
const MAXIMUM_PARTIAL_CLONE_PEERS: usize = 128;
const MAXIMUM_CLONE_SHAPE_BYTES: usize = 4096;
const MAXIMUM_OPERATION_SITES: usize = 16 * 25;
const MAXIMUM_SITES_PER_METRIC: u16 = 16;

const FIELDS: &[&str] = &[
    "code_lines",
    "parameter_count",
    "cyclomatic",
    "max_nesting",
    "max_conditional_operands",
    "magic_numbers",
    "hardcoded_urls",
    "hardcoded_url_requests",
    "hardcoded_url_configuration",
    "hardcoded_url_presentation_abstentions",
    "secrets_score",
    "stale_doc_numbers",
    "accidental_quadratic",
    "empty_catches",
    "sync_io_in_async",
    "sequential_await_loops",
    "serial_await_dependency_loops",
    "serial_await_control_flow_loops",
    "serial_await_intent_loops",
    "facade_factory_delegates",
    "ts_any_casts",
    "ts_suppressions",
    "debug_logs",
    "incomplete_markers",
    "dynamic_eval",
    "insecure_hash",
    "insecure_random",
    "http_without_timeout",
    "sql_string_concatenation",
    "unsafe_json_parse",
    "unvalidated_env",
    "empty_body",
    "literal_bytes",
];

pub(super) struct Document {
    pub(super) analyzed_language: Option<codevetra_domain::SourceLanguage>,
    pub(super) test: bool,
    health: Vec<(u8, f64)>,
    pub(super) operation_sites: Vec<codevetra_domain::biomarkers::HealthOperationSite>,
    pub(super) clones: ClonePolicy,
    pub(super) shape: Option<String>,
    pub(super) partial: Partial,
    pub(super) secrets_actionable: bool,
    pub(super) syntax: SyntaxEvidence,
}

pub(super) struct SyntaxEvidence {
    pub(super) facade_factory: bool,
    pub(super) flat_value_mapping: bool,
}

pub(super) struct ClonePolicy {
    pub(super) enabled: bool,
    pub(super) near_compatible: bool,
}
#[derive(Default)]
pub(super) struct Partial {
    pub(super) peers: u64,
    pub(super) component: u64,
    pub(super) representative: Option<SymbolId>,
    pub(super) maximum: f64,
    pub(super) minimum: f64,
    pub(super) listed: Vec<SymbolId>,
}
impl Document {
    pub(super) fn health(&self, key: &str) -> f64 {
        self.health_or(key, 0.0)
    }
    pub(super) fn health_or(&self, key: &str, default: f64) -> f64 {
        FIELDS
            .iter()
            .position(|name| *name == key)
            .and_then(|index| self.health.iter().find(|(id, _)| usize::from(*id) == index))
            .map_or(default, |(_, value)| *value)
    }
}
pub(super) struct Base<'a> {
    pub(super) symbol: &'a SymbolInput,
    pub(super) file: &'a FileInput,
    pub(super) document: &'a Document,
    pub(super) topology: Topology,
    pub(super) unresolved: u64,
    pub(super) external: u64,
    pub(super) centrality: f64,
    pub(super) envy: Option<&'a Envy>,
    pub(super) coverage: Option<(f64, f64)>,
    pub(super) prior: Option<u32>,
    pub(super) recent_prior: bool,
}
#[derive(Default)]
pub(super) struct Topology {
    pub(super) outgoing: u64,
    pub(super) groups: [u64; 5],
    pub(super) incoming: u64,
    pub(super) external: u64,
    pub(super) exported_api: u64,
    pub(super) function_local: bool,
    pub(super) framework: bool,
    pub(super) behavioral: u64,
    pub(super) complex: u64,
}
#[derive(Default)]
pub(super) struct Envy {
    pub(super) fields: BTreeSet<[u8; 16]>,
    pub(super) classes: BTreeSet<[u8; 16]>,
    pub(super) own: u64,
    pub(super) foreign: u64,
}
#[derive(Default)]
struct References {
    dynamic: BTreeMap<String, FileCounts>,
    envy: BTreeMap<[u8; 16], Envy>,
    bytes: usize,
}

pub(super) struct ComputedInventory {
    pub(super) rows: Vec<StructuralFinding>,
    pub(super) analyzed: u64,
    pub(super) coverage: Vec<codevetra_domain::biomarkers::DetectorCoverage>,
}

pub(super) struct Inputs {
    documents: Documents,
    population: f64,
}

impl Inputs {
    pub(super) fn read(
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
    ) -> Result<Self, RocksError> {
        let snapshot = transaction.graph_snapshot()?;
        let graph = snapshot.graph(budget)?;
        let documents = Documents::read(&graph, budget)?;
        let population = population(&graph, &documents, budget)?;
        Ok(Self {
            documents,
            population,
        })
    }

    pub(super) fn references(
        mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
    ) -> Result<Referenced, RocksError> {
        let snapshot = transaction.graph_snapshot()?;
        let graph = snapshot.graph(budget)?;
        Ok(Referenced {
            references: references(&graph, &mut self.documents, budget)?,
            coverage: coverage(transaction, &graph, budget)?,
            inputs: self,
        })
    }
}

pub(super) struct Referenced {
    inputs: Inputs,
    references: References,
    coverage: BTreeMap<SymbolId, (f64, f64)>,
}

impl Referenced {
    pub(super) fn clones(
        self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
    ) -> Result<Inventory, RocksError> {
        let clone_classes = clones::Classes::new(transaction, &self.inputs.documents, budget)?;
        Ok(Inventory {
            inputs: self,
            clone_classes,
            output: FindingResults {
                rows: Vec::new(),
                bytes: 0,
                analyzed: 0,
                coverage: super::coverage::CoverageAccumulator::new(),
            },
            position: 0,
        })
    }
}

pub(super) struct Inventory {
    inputs: Referenced,
    clone_classes: clones::Classes,
    output: FindingResults,
    position: usize,
}

struct FindingResults {
    rows: Vec<StructuralFinding>,
    bytes: usize,
    analyzed: u64,
    coverage: super::coverage::CoverageAccumulator,
}

impl Inventory {
    pub(super) fn batch(
        &mut self,
        transaction: &StateTransaction<'_>,
        budget: &mut ReadBudget,
        previous: Option<&StoredRevision>,
    ) -> Result<bool, RocksError> {
        let snapshot = transaction.graph_snapshot()?;
        let graph = snapshot.graph(budget)?;
        let documents = &self.inputs.inputs.documents;
        let population = self.inputs.inputs.population;
        let references = &self.inputs.references;
        let coverage = &self.inputs.coverage;
        let clone_classes = &self.clone_classes;
        let prior = previous
            .map(|previous| transaction.retained_graph_snapshot(&previous.generation_id))
            .transpose()?;
        let prior_graph = prior
            .as_ref()
            .map(|snapshot| snapshot.graph(budget))
            .transpose()?;
        let recent_prior = recent_publication(previous)?;
        for handle in documents.iter().skip(self.position).take(SYMBOLS_PER_BATCH) {
            self.position += 1;
            budget.checkpoint()?;
            let id = handle.symbol()?;
            let symbol = graph.symbol(&id, budget)?.ok_or(corrupt())?;
            let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
            let document = handle.load(&graph, budget)?;
            if document.test
                || matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter")
            {
                self.output
                    .coverage
                    .observe(super::coverage::Subject::unscored(
                        &file, &symbol, &document,
                    ));
                continue;
            }
            if document
                .analyzed_language
                .is_some_and(|language| !language_matches_host(language, &file.language))
            {
                return Err(corrupt());
            }
            let topology = TopologyReader {
                graph: &graph,
                documents,
            }
            .read(&symbol, &file, budget)?;
            let potential = dynamic_external_references(references, &symbol)?;
            let external = topology
                .external
                .checked_add(topology.exported_api)
                .and_then(|sum| sum.checked_add(potential))
                .ok_or(corrupt())?;
            let centrality = (topology.incoming.to_f64().ok_or(corrupt())? / population).min(1.0);
            let prior = prior_graph
                .as_ref()
                .map(|graph| graph.symbol(&id, budget))
                .transpose()?
                .flatten()
                .map(|symbol| symbol.end_line - symbol.start_line + 1);
            let base = Base {
                symbol: &symbol,
                file: &file,
                document: &document,
                topology,
                external,
                centrality,
                unresolved: handle.unresolved,
                envy: references.envy.get(&packed_uuid(id.as_str())?),
                coverage: coverage.get(&id).copied(),
                prior,
                recent_prior,
            };
            self.output
                .coverage
                .observe(super::coverage::Subject::scored(&base));
            validate_operation_sites(&base)?;
            let mut findings = rules::evaluate(&base)?;
            if let Some(finding) = clone_classes.finding(&base, &graph, budget)? {
                findings.push(finding);
            }
            self.output.record(&file.language, findings)?;
        }
        Ok(self.position == documents.len())
    }

    pub(super) fn finish(self) -> ComputedInventory {
        self.output.finish()
    }
}

impl FindingResults {
    fn record(
        &mut self,
        language: &str,
        findings: Vec<StructuralFinding>,
    ) -> Result<(), RocksError> {
        for finding in findings {
            self.coverage.finding(language, &finding.finding);
            self.bytes = self
                .bytes
                .checked_add(row_bytes(&finding))
                .ok_or_else(workspace)?;
            if self.bytes > MAXIMUM_FINDING_BYTES {
                return Err(workspace());
            }
            self.rows.push(finding);
        }
        self.analyzed += 1;
        Ok(())
    }

    fn finish(self) -> ComputedInventory {
        ComputedInventory {
            rows: self.rows,
            analyzed: self.analyzed,
            coverage: self.coverage.finish(),
        }
    }
}

fn recent_publication(previous: Option<&StoredRevision>) -> Result<bool, RocksError> {
    Ok(previous
        .and_then(|previous| previous.published_at.as_ref())
        .map(|time| time.parse::<jiff::Timestamp>().map_err(|_| corrupt()))
        .transpose()?
        .is_some_and(|time| {
            time.as_second()
                >= jiff::Timestamp::now()
                    .as_second()
                    .saturating_sub(RECENT_PUBLICATION_SECONDS)
        }))
}

fn language_matches_host(language: SourceLanguage, host: &str) -> bool {
    language.as_str() == host
        || (matches!(host, "svelte" | "vue" | "astro")
            && matches!(
                language,
                SourceLanguage::JavaScript | SourceLanguage::TypeScript | SourceLanguage::Tsx
            ))
}

fn validate_operation_sites(base: &Base<'_>) -> Result<(), RocksError> {
    let mut counts = BTreeMap::<&str, u16>::new();
    let mut unique = BTreeSet::new();
    for site in &base.document.operation_sites {
        let field = site.metric.field();
        let count = counts.entry(field).or_default();
        *count = count.checked_add(1).ok_or(corrupt())?;
        if *count > MAXIMUM_SITES_PER_METRIC
            || f64::from(*count) > base.document.health(field)
            || !operation_site_within_symbol(site, base.symbol)
            || !unique.insert((field, site.span))
        {
            return Err(corrupt());
        }
    }
    Ok(())
}

fn operation_site_within_symbol(
    site: &codevetra_domain::biomarkers::HealthOperationSite,
    symbol: &SymbolInput,
) -> bool {
    site.span.start_byte() >= symbol.start_byte
        && site.span.end_byte() <= symbol.end_byte
        && site.span.start_line() >= symbol.start_line
        && site.span.end_line() <= symbol.end_line
}

fn dynamic_external_references(
    references: &References,
    symbol: &SymbolInput,
) -> Result<u64, RocksError> {
    if symbol.symbol_kind != "method"
        || !symbol.export.exported
        || symbol.visibility == Some(Visibility::Private)
    {
        return Ok(0);
    }
    let file_id = packed_uuid(symbol.file_id.as_str())?;
    references
        .dynamic
        .get(simple_name(&symbol.qualified_name))
        .map_or(Ok(0), |files| files.external(file_id))
}

fn population(
    graph: &GraphSnapshot<'_, '_>,
    documents: &Documents,
    budget: &mut ReadBudget,
) -> Result<f64, RocksError> {
    let mut count = 0_u64;
    for document in documents.iter() {
        if !document.test
            && graph
                .symbol(&document.symbol()?, budget)?
                .is_some_and(|symbol| {
                    !matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter")
                })
        {
            count += 1;
        }
    }
    count.saturating_sub(1).max(1).to_f64().ok_or(corrupt())
}

pub(super) fn parse_document(
    value: &serde_json::Value,
    test: bool,
) -> Result<Document, RocksError> {
    let health_metadata = value.get("health");
    let mut health = Vec::new();
    for (index, &key) in FIELDS.iter().enumerate() {
        let field = nested_field(health_metadata, key);
        let number = number(field)?;
        if number != 0.0 || field.is_some_and(|value| !value.is_null()) {
            health.push((u8::try_from(index).map_err(|_| corrupt())?, number));
        }
    }
    let partial = value.get("partial_clone");
    let representative = nested_field(partial, "representative_symbol_id")
        .and_then(serde_json::Value::as_str)
        .map(SymbolId::parse)
        .transpose()
        .map_err(|_| corrupt())?;
    let mut listed = Vec::new();
    if let Some(values) = nested_field(partial, "listed_peer_symbol_ids") {
        let values = values.as_array().ok_or(corrupt())?;
        if values.len() > MAXIMUM_PARTIAL_CLONE_PEERS {
            return Err(workspace());
        }
        for value in values {
            listed.push(SymbolId::parse(value.as_str().ok_or(corrupt())?).map_err(|_| corrupt())?);
        }
    }
    let shape = value
        .get("clone_shape_digest")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if shape
        .as_ref()
        .is_some_and(|shape| shape.len() > MAXIMUM_CLONE_SHAPE_BYTES)
    {
        return Err(workspace());
    }
    let operation_sites = nested_field(health_metadata, "operation_sites")
        .map(|sites| {
            if sites
                .as_array()
                .is_none_or(|sites| sites.len() > MAXIMUM_OPERATION_SITES)
            {
                return Err(corrupt());
            }
            serde_json::from_value(sites.clone()).map_err(|_| corrupt())
        })
        .transpose()?
        .unwrap_or_default();
    let analyzed_language = nested_field(health_metadata, "analyzed_language")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()).map_err(|_| corrupt()))
        .transpose()?;
    Ok(Document {
        analyzed_language,
        test,
        operation_sites,
        health,
        shape,
        clones: ClonePolicy {
            enabled: boolean(value.get("duplicate_detection_enabled"), true)?,
            near_compatible: boolean(value.get("near_clone_compatible"), false)?,
        },
        secrets_actionable: boolean(nested_field(health_metadata, "secrets_actionable"), false)?,
        syntax: SyntaxEvidence {
            facade_factory: boolean(nested_field(health_metadata, "facade_factory"), false)?,
            flat_value_mapping: boolean(
                nested_field(health_metadata, "flat_value_mapping"),
                false,
            )?,
        },
        partial: Partial {
            peers: integer(nested_field(partial, "peer_count"))?,
            component: integer(nested_field(partial, "component_size"))?,
            representative,
            maximum: number(nested_field(partial, "maximum_overlap_ppm"))? / 1_000_000.0,
            minimum: number(nested_field(partial, "minimum_overlap_ppm"))? / 1_000_000.0,
            listed,
        },
    })
}
fn nested_field<'a>(
    parent: Option<&'a serde_json::Value>,
    key: &str,
) -> Option<&'a serde_json::Value> {
    parent.and_then(|value| value.get(key))
}

fn integer(value: Option<&serde_json::Value>) -> Result<u64, RocksError> {
    number(value)?.to_u64().ok_or(corrupt())
}

fn number(value: Option<&serde_json::Value>) -> Result<f64, RocksError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(0.0);
    };
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or(corrupt())?;
    if !number.is_finite() || number < 0.0 {
        return Err(corrupt());
    }
    Ok(number)
}
fn boolean(value: Option<&serde_json::Value>, default: bool) -> Result<bool, RocksError> {
    value
        .filter(|value| !value.is_null())
        .map_or(Ok(default), |value| {
            value
                .as_bool()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                .ok_or(corrupt())
        })
}

struct TopologyReader<'input, 'snapshot, 'store> {
    graph: &'input GraphSnapshot<'snapshot, 'store>,
    documents: &'input Documents,
}

impl TopologyReader<'_, '_, '_> {
    fn read(
        &self,
        symbol: &SymbolInput,
        file: &FileInput,
        budget: &mut ReadBudget,
    ) -> Result<Topology, RocksError> {
        let mut output = Topology::default();
        let contains = self.contained_symbols(symbol, budget, &mut output)?;
        (output.outgoing, output.groups) = self.dependency_topology(symbol, &contains, budget)?;
        let mut parent_default = false;
        self.graph
            .adjacency(&symbol.symbol_id, GraphDirection::Incoming)
            .visit(budget, |link, budget| {
                let source = self
                    .graph
                    .symbol(link.source_symbol_id(), budget)?
                    .ok_or(corrupt())?;
                if link.kind() == EdgeKind::Contains {
                    output.function_local |= matches!(
                        source.symbol_kind.as_str(),
                        "function" | "method" | "component"
                    );
                    parent_default |= source.export.default_export;
                } else {
                    let count = u64::from(self.graph.edge(&link, budget)?.site_count);
                    output.incoming = output.incoming.checked_add(count).ok_or(corrupt())?;
                    if source.file_id != symbol.file_id {
                        output.external += 1;
                    }
                    if source.file_id == symbol.file_id
                        && source.symbol_id != symbol.symbol_id
                        && source.export.exported
                        && matches!(
                            link.kind(),
                            EdgeKind::Calls
                                | EdgeKind::References
                                | EdgeKind::TypeOf
                                | EdgeKind::Returns
                                | EdgeKind::Instantiates
                                | EdgeKind::DefUse
                        )
                    {
                        output.exported_api =
                            output.exported_api.checked_add(count).ok_or(corrupt())?;
                    }
                }
                Ok(())
            })?;
        output.framework = framework::convention(symbol, &file.normalized_path, parent_default)?;
        Ok(output)
    }

    fn contained_symbols(
        &self,
        symbol: &SymbolInput,
        budget: &mut ReadBudget,
        output: &mut Topology,
    ) -> Result<BTreeSet<SymbolId>, RocksError> {
        let mut contains = BTreeSet::new();
        self.graph
            .adjacency(&symbol.symbol_id, GraphDirection::Outgoing)
            .visit(budget, |link, budget| {
                if link.kind() == EdgeKind::Contains {
                    contains.insert(link.target_symbol_id().clone());
                    if contains.len() > 16_384 {
                        return Err(workspace());
                    }
                    let child = self
                        .graph
                        .symbol(link.target_symbol_id(), budget)?
                        .ok_or(corrupt())?;
                    if matches!(child.symbol_kind.as_str(), "function" | "method")
                        && let Some(document) = self
                            .documents
                            .get(&child.symbol_id)?
                            .filter(|document| !document.test)
                    {
                        output.behavioral += u64::from(document.code_lines >= 5.0);
                        output.complex += u64::from(document.cyclomatic >= 3.0);
                    }
                }
                Ok(())
            })?;
        Ok(contains)
    }

    fn dependency_topology(
        &self,
        symbol: &SymbolInput,
        contains: &BTreeSet<SymbolId>,
        budget: &mut ReadBudget,
    ) -> Result<(u64, [u64; 5]), RocksError> {
        let mut dependencies = BTreeSet::new();
        let mut groups: [BTreeSet<SymbolId>; 5] = std::array::from_fn(|_| BTreeSet::new());
        self.graph
            .adjacency(&symbol.symbol_id, GraphDirection::Outgoing)
            .visit(budget, |link, _| {
                if link.kind() != EdgeKind::Contains
                    && link.target_symbol_id() != &symbol.symbol_id
                    && !contains.contains(link.target_symbol_id())
                {
                    dependencies.insert(link.target_symbol_id().clone());
                    if let Some(index) = dependency_group(link.kind()) {
                        groups[index].insert(link.target_symbol_id().clone());
                    }
                    if dependencies.len() > 16_384 {
                        return Err(workspace());
                    }
                }
                Ok(())
            })?;
        Ok((
            dependencies.len() as u64,
            groups.map(|group| group.len() as u64),
        ))
    }
}

fn dependency_group(kind: EdgeKind) -> Option<usize> {
    match kind {
        EdgeKind::Calls => Some(0),
        EdgeKind::Imports | EdgeKind::Exports => Some(1),
        EdgeKind::Implements | EdgeKind::Extends | EdgeKind::TypeOf | EdgeKind::Returns => Some(2),
        EdgeKind::Instantiates | EdgeKind::Decorates => Some(3),
        EdgeKind::FieldAccess | EdgeKind::DefUse => Some(4),
        _ => None,
    }
}

fn references(
    graph: &GraphSnapshot<'_, '_>,
    documents: &mut Documents,
    budget: &mut ReadBudget,
) -> Result<References, RocksError> {
    let mut collector = ReferenceCollector {
        graph,
        output: References::default(),
        parents: Parents::default(),
    };
    super::super::visit_references(graph, budget, |reference, budget| {
        if reference.target_symbol_id.is_none() {
            record_unresolved_reference(&mut collector.output, documents, reference)?;
        } else if reference.reference_kind == "field_access" {
            collector.record_envy(&reference, budget)?;
        }
        if collector.output.bytes > MAXIMUM_REFERENCE_BYTES {
            return Err(workspace());
        }
        Ok(())
    })?;
    Ok(collector.output)
}

fn record_unresolved_reference(
    output: &mut References,
    documents: &mut Documents,
    reference: crate::ReferenceInput,
) -> Result<(), RocksError> {
    let count = u64::from(reference.site_count);
    if reference.reference_kind == "calls"
        && reference.resolution_provenance == "native-dynamic-unresolved"
    {
        let name = dynamic_name(&reference.reference_name);
        if !output.dynamic.contains_key(name) {
            let node = if output.dynamic.is_empty() { 768 } else { 160 };
            reserve(&mut output.bytes, name.len() + node)?;
        }
        let files = output.dynamic.entry(name.to_owned()).or_default();
        files.add(
            packed_uuid(reference.file_id.as_str())?,
            count,
            &mut output.bytes,
        )?;
    } else if !excluded_unresolved(&reference.resolution_provenance)
        && let Some(owner) = reference.owner_symbol_id
    {
        documents.record_unresolved(&owner, count)?;
    }
    Ok(())
}

fn dynamic_name(name: &str) -> &str {
    let Some((_, tail)) = name.rsplit_once('.') else {
        return name;
    };
    let candidate = tail.split("::").next().unwrap_or(tail);
    if candidate
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        candidate
    } else {
        name
    }
}
fn excluded_unresolved(provenance: &str) -> bool {
    matches!(
        provenance,
        "native-dynamic-unresolved"
            | "native-member-unresolved"
            | "native-rust-external"
            | "native-rust-intrinsic"
            | "native-rust-macro-unexpanded"
            | "native-external-reference"
            | "native-javascript-intrinsic"
            | "native-python-intrinsic"
            | "native-shell-command"
            | "native-manifest-reference"
    ) || (provenance.starts_with("native-embedded-sql-") && provenance.ends_with("-unresolved"))
}
#[derive(Default)]
struct Parents {
    values: BTreeMap<SymbolId, Vec<(SymbolId, bool)>>,
    bytes: usize,
}
impl Parents {
    const MAXIMUM_PARENTS: usize = 32;
    const MAXIMUM_BYTES: usize = 8 * 1_048_576;
    const ENTRY_BYTES: usize = 128;
    const PARENT_BYTES: usize = 96;

    fn get(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
        id: &SymbolId,
    ) -> Result<Vec<(SymbolId, bool)>, RocksError> {
        if let Some(parents) = self.values.get(id) {
            return Ok(parents.clone());
        }
        let mut parents = BTreeMap::new();
        graph
            .adjacency(id, GraphDirection::Incoming)
            .visit(budget, |link, budget| {
                if link.kind() == EdgeKind::Contains {
                    let parent = graph
                        .symbol(link.source_symbol_id(), budget)?
                        .ok_or(corrupt())?;
                    if matches!(
                        parent.symbol_kind.as_str(),
                        "class" | "struct" | "trait" | "interface"
                    ) {
                        parents.insert(parent.symbol_id, parent.symbol_kind == "interface");
                        if parents.len() > Self::MAXIMUM_PARENTS {
                            return Err(workspace());
                        }
                    }
                }
                Ok(())
            })?;
        let parents = parents.into_iter().collect::<Vec<_>>();
        self.bytes = self
            .bytes
            .checked_add(Self::ENTRY_BYTES + parents.len() * Self::PARENT_BYTES)
            .ok_or_else(workspace)?;
        if self.bytes > Self::MAXIMUM_BYTES {
            return Err(workspace());
        }
        self.values.insert(id.clone(), parents.clone());
        Ok(parents)
    }
}

struct ReferenceCollector<'graph, 'snapshot, 'store> {
    graph: &'graph GraphSnapshot<'snapshot, 'store>,
    output: References,
    parents: Parents,
}

#[derive(Clone, Copy)]
struct FieldAccess<'a> {
    owner: &'a SymbolId,
    target: &'a SymbolId,
    count: u64,
}

impl ReferenceCollector<'_, '_, '_> {
    fn record_envy(
        &mut self,
        reference: &crate::ReferenceInput,
        budget: &mut ReadBudget,
    ) -> Result<(), RocksError> {
        let (Some(owner), Some(target)) = (&reference.owner_symbol_id, &reference.target_symbol_id)
        else {
            return Ok(());
        };
        let source = self.graph.symbol(owner, budget)?.ok_or(corrupt())?;
        let field = self.graph.symbol(target, budget)?.ok_or(corrupt())?;
        if source.symbol_kind != "method"
            || !matches!(field.symbol_kind.as_str(), "field" | "property" | "method")
        {
            return Ok(());
        }
        let sources = self.parents.get(self.graph, budget, owner)?;
        let targets = self.parents.get(self.graph, budget, target)?;
        let site = FieldAccess {
            owner,
            target,
            count: u64::from(reference.site_count),
        };
        for (source, interface) in sources {
            if interface {
                continue;
            }
            for (target_class, _) in &targets {
                self.record_envy_pair(site, &source, target_class)?;
            }
        }
        Ok(())
    }
    fn record_envy_pair(
        &mut self,
        site: FieldAccess<'_>,
        source_class: &SymbolId,
        target_class: &SymbolId,
    ) -> Result<(), RocksError> {
        let owner = packed_uuid(site.owner.as_str())?;
        if !self.output.envy.contains_key(&owner) {
            let node = if self.output.envy.is_empty() {
                1024
            } else {
                256
            };
            reserve(&mut self.output.bytes, node)?;
        }
        let envy = self.output.envy.entry(owner).or_default();
        if source_class == target_class {
            envy.own = envy.own.checked_add(site.count).ok_or(corrupt())?;
        } else {
            envy.foreign = envy.foreign.checked_add(site.count).ok_or(corrupt())?;
            for (set, id) in [
                (&mut envy.fields, site.target),
                (&mut envy.classes, target_class),
            ] {
                let id = packed_uuid(id.as_str())?;
                if !set.contains(&id) {
                    reserve(
                        &mut self.output.bytes,
                        super::packed_tree_insertion_bytes(set.len()),
                    )?;
                    set.insert(id);
                }
            }
        }
        Ok(())
    }
}

fn coverage(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<SymbolId, (f64, f64)>, RocksError> {
    let mut rows = Vec::new();
    for (id, fraction) in crate::coverage::native::fractions(transaction, budget)? {
        let mut incoming = 0_u64;
        graph
            .adjacency(&id, GraphDirection::Incoming)
            .visit(budget, |link, budget| {
                if link.kind() != EdgeKind::Contains {
                    incoming = incoming
                        .checked_add(u64::from(graph.edge(&link, budget)?.site_count))
                        .ok_or(corrupt())?;
                }
                Ok(())
            })?;
        rows.push((incoming, id, fraction));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let denominator = rows
        .len()
        .saturating_sub(1)
        .max(1)
        .to_f64()
        .ok_or(corrupt())?;
    let mut output = BTreeMap::new();
    let mut rank = 0_usize;
    let mut previous = None;
    for (index, (incoming, id, fraction)) in rows.into_iter().enumerate() {
        if previous != Some(incoming) {
            rank = index;
            previous = Some(incoming);
        }
        output.insert(
            id,
            (fraction, rank.to_f64().ok_or(corrupt())? / denominator),
        );
    }
    Ok(output)
}
