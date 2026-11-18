use std::collections::{BTreeSet, VecDeque};

use codevetra_db::{
    EdgeInput, FileInput, GraphDirection, SymbolInput,
    rocks::{
        GraphLink, GraphSnapshot, ReadBudget, RecordKey, RevisionSnapshot, RocksError,
        StorageRequest,
    },
};
use codevetra_domain::{EdgeKind, NormalizedPath, SymbolId};
use serde::Serialize;
use serde_json::{Value, json};

use super::{RocksProjectError, RocksProjectRuntime};

const MAXIMUM_QUERY_BYTES: usize = 4096;
const MAXIMUM_SEARCH_HITS: u16 = 100;
const MAXIMUM_PATH_SYMBOLS: u16 = 200;
const MAXIMUM_FILES: u16 = 500;
const MAXIMUM_GRAPH_DEPTH: u8 = 8;
const MAXIMUM_GRAPH_NODES: u16 = 500;
const MAXIMUM_GRAPH_EDGES: usize = 5000;
const QUERY_READ_ROWS: usize = 2_000_000;
const QUERY_READ_BYTES: usize = 192 * 1_048_576;
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const MAXIMUM_OUTPUT_BYTES: usize = 2 * 1_048_576;

/// Bounded native query, independent of CLI or MCP JSON syntax.
#[derive(Clone)]
pub enum RocksQuery {
    /// Exact simple or qualified declaration name.
    Name {
        /// Exact name.
        query: String,
        /// Maximum rows.
        limit: u16,
    },
    /// Exact source file and its declarations.
    Path {
        /// Project-relative source path.
        path: String,
        /// Maximum declarations.
        limit: u16,
    },
    /// Exact resolved or unresolved reference text.
    Reference {
        /// Exact reference text.
        query: String,
        /// Maximum rows.
        limit: u16,
    },
    /// Deterministic native BM25 over canonical document fields.
    Lexical {
        /// Bounded query terms.
        query: String,
        /// Maximum ranked hits.
        limit: u16,
    },
    /// Source-file inventory.
    Files {
        /// Optional project-relative subtree.
        directory: Option<String>,
        /// Maximum files.
        limit: u16,
    },
    /// Directed traversal of compact topology, with optional edge filtering.
    Graph {
        /// Exact root symbol.
        symbol: SymbolId,
        /// Traversal orientation.
        direction: GraphDirection,
        /// Optional relationship kind.
        kind: Option<EdgeKind>,
        /// Maximum depth.
        depth: u8,
        /// Maximum non-root nodes.
        maximum_nodes: u16,
    },
}

/// Generation-fenced query evidence with explicit live-source freshness.
#[derive(Clone, Debug, Serialize)]
pub struct RocksQueryResult {
    /// Native storage selection.
    pub storage: &'static str,
    /// Complete pinned generation.
    pub generation_id: codevetra_domain::GenerationId,
    /// Source and policy identity of that generation.
    pub source_revision: codevetra_domain::ContentDigest,
    /// Source was checked before and after the query.
    pub fresh: bool,
    /// Bounded operation-specific evidence.
    #[serde(flatten)]
    pub evidence: Value,
}

impl RocksProjectRuntime {
    /// Query one immutable revision and reject live drift unless explicitly allowed.
    ///
    /// # Errors
    /// Returns stale, malformed, missing-entity, storage, or finite-budget errors.
    pub(super) async fn query_inner(
        &self,
        query: RocksQuery,
        allow_stale: bool,
        request: &StorageRequest,
    ) -> Result<RocksQueryResult, RocksProjectError> {
        query.validate()?;
        let status = self.status_inner(request).await?;
        if !allow_stale && !status.fresh {
            return Err(RocksProjectError::SourceChanged);
        }
        let current = status.current.ok_or(RocksProjectError::NotIndexed)?;
        let result = self
            .store
            .with_request(request)
            .read(
                Some(current.generation_id),
                ReadBudget::new(QUERY_READ_ROWS, QUERY_READ_BYTES, QUERY_TIMEOUT)?,
                move |snapshot, budget| {
                    let evidence = execute(snapshot, budget, query)?;
                    if serde_json::to_vec(&evidence).map_err(|_| invalid())?.len()
                        > MAXIMUM_OUTPUT_BYTES
                    {
                        return Err(RocksError::Limit {
                            resource: "query_output_bytes",
                        });
                    }
                    Ok(RocksQueryResult {
                        storage: "rocksdb",
                        generation_id: snapshot.revision().generation_id.clone(),
                        source_revision: snapshot.revision().source_revision.clone(),
                        fresh: status.fresh,
                        evidence,
                    })
                },
            )
            .await?;
        let (after, _) = self.scan(request, false).await?;
        if after.digest != status.live_source_revision {
            return Err(RocksProjectError::SourceChanged);
        }
        Ok(result)
    }
}

impl RocksQuery {
    fn validate(&self) -> Result<(), RocksProjectError> {
        let valid = match self {
            Self::Name { query, limit }
            | Self::Reference { query, limit }
            | Self::Lexical { query, limit } => {
                !query.is_empty()
                    && query.len() <= MAXIMUM_QUERY_BYTES
                    && (1..=MAXIMUM_SEARCH_HITS).contains(limit)
            }
            Self::Path { path, limit } => {
                NormalizedPath::parse(path).is_ok() && (1..=MAXIMUM_PATH_SYMBOLS).contains(limit)
            }
            Self::Files { directory, limit } => {
                directory
                    .as_ref()
                    .is_none_or(|path| NormalizedPath::parse(path).is_ok())
                    && (1..=MAXIMUM_FILES).contains(limit)
            }
            Self::Graph {
                depth,
                maximum_nodes,
                ..
            } => {
                (1..=MAXIMUM_GRAPH_DEPTH).contains(depth)
                    && (1..=MAXIMUM_GRAPH_NODES).contains(maximum_nodes)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(RocksProjectError::InvalidQuery)
        }
    }
}

fn invalid() -> RocksError {
    RocksError::Corrupt {
        field: "native_query",
    }
}

fn execute(
    snapshot: &RevisionSnapshot<'_>,
    budget: &mut ReadBudget,
    query: RocksQuery,
) -> Result<Value, RocksError> {
    let mut reader = NativeQueryReader {
        graph: snapshot.graph(budget)?,
        budget,
    };
    match query {
        RocksQuery::Name { query, limit } => reader.symbols(None, Some(&query), limit),
        RocksQuery::Path { path, limit } => reader.symbols(Some(&path), None, limit),
        RocksQuery::Files { directory, limit } => reader.files(directory.as_deref(), limit),
        RocksQuery::Reference { query, limit } => reader.references(&query, limit),
        RocksQuery::Lexical { query, limit } => reader.lexical(&query, limit),
        RocksQuery::Graph {
            symbol,
            direction,
            kind,
            depth,
            maximum_nodes,
        } => reader.traversal(&Traversal {
            symbol,
            direction,
            kind,
            depth,
            maximum_nodes,
        }),
    }
}

fn named(symbol: &SymbolInput, query: &str) -> bool {
    symbol.qualified_name == query
        || symbol.qualified_name.rsplit([':', '.', '#', '/']).next() == Some(query)
}

fn matches_symbol(symbol: &SymbolInput, name: Option<&str>, file: Option<&FileInput>) -> bool {
    name.is_none_or(|name| named(symbol, name))
        && file.is_none_or(|file| file.file_id == symbol.file_id)
}

struct Traversal {
    symbol: SymbolId,
    direction: GraphDirection,
    kind: Option<EdgeKind>,
    depth: u8,
    maximum_nodes: u16,
}

struct TraversalWalk<'query> {
    query: &'query Traversal,
    nodes: Vec<Value>,
    edges: Vec<EdgeInput>,
    seen: BTreeSet<SymbolId>,
    queue: VecDeque<(SymbolId, u8)>,
}

impl TraversalWalk<'_> {
    fn visit_node(
        &mut self,
        reader: &mut NativeQueryReader<'_, '_, '_>,
        node: &SymbolId,
        depth: u8,
    ) -> Result<bool, RocksError> {
        let mut cursor = None;
        loop {
            let page = reader.graph.adjacency(node, self.query.direction).page(
                cursor.as_ref(),
                128,
                reader.budget,
            )?;
            for link in page.links {
                if self.query.kind.is_some_and(|kind| link.kind() != kind) {
                    continue;
                }
                if !self.visit_link(reader, (node, depth), &link)? {
                    return Ok(false);
                }
            }
            cursor = page.next;
            if cursor.is_none() {
                return Ok(true);
            }
        }
    }

    fn visit_link(
        &mut self,
        reader: &mut NativeQueryReader<'_, '_, '_>,
        current: (&SymbolId, u8),
        link: &GraphLink,
    ) -> Result<bool, RocksError> {
        let (node, depth) = current;
        let neighbor = if link.source_symbol_id() == node {
            link.target_symbol_id()
        } else {
            link.source_symbol_id()
        };
        if !self.seen.contains(neighbor) {
            if self.seen.len() > usize::from(self.query.maximum_nodes) {
                return Ok(false);
            }
            self.seen.insert(neighbor.clone());
            self.nodes.push(json!(
                reader
                    .graph
                    .symbol(neighbor, reader.budget)?
                    .ok_or_else(invalid)?
            ));
            self.queue.push_back((neighbor.clone(), depth + 1));
        }
        if self.edges.len() == MAXIMUM_GRAPH_EDGES {
            return Ok(false);
        }
        self.edges.push(reader.graph.edge(link, reader.budget)?);
        Ok(true)
    }
}

struct NativeQueryReader<'snapshot, 'store, 'budget> {
    graph: GraphSnapshot<'snapshot, 'store>,
    budget: &'budget mut ReadBudget,
}

impl NativeQueryReader<'_, '_, '_> {
    fn symbols(
        &mut self,
        path: Option<&str>,
        name: Option<&str>,
        limit: u16,
    ) -> Result<Value, RocksError> {
        let mut cursor = None;
        let mut hits = Vec::new();
        let matching_file = path.map(|path| self.find_file(path)).transpose()?.flatten();
        if path.is_some() && matching_file.is_none() {
            return Ok(json!({"results": [], "file": null, "truncated": false}));
        }
        let mut truncated = false;
        'pages: loop {
            let (rows, next) = self.symbol_candidates(cursor.as_ref(), name)?;
            for symbol in rows {
                if !matches_symbol(&symbol, name, matching_file.as_ref()) {
                    continue;
                }
                if hits.len() == usize::from(limit) {
                    truncated = true;
                    break 'pages;
                }
                let file = self
                    .graph
                    .file(&symbol.file_id, self.budget)?
                    .ok_or_else(invalid)?;
                hits.push(json!({"symbol_id": symbol.symbol_id, "qualified_name": symbol.qualified_name, "symbol_kind": symbol.symbol_kind, "path": file.normalized_path, "language": file.language, "start_line": symbol.start_line, "end_line": symbol.end_line, "signature": symbol.signature}));
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(
            json!({"results": hits, "file": matching_file, "truncated": truncated, "channel": "exact"}),
        )
    }
    fn symbol_candidates(
        &mut self,
        cursor: Option<&RecordKey>,
        name: Option<&str>,
    ) -> Result<(Vec<SymbolInput>, Option<RecordKey>), RocksError> {
        if let Some(name) = name.filter(|_| self.graph.info().name_index_version == 1) {
            self.graph
                .symbols_by_name(name)
                .page(cursor, 256, self.budget)
        } else {
            self.graph.symbols(cursor, 256, self.budget)
        }
    }
    fn find_file(&mut self, path: &str) -> Result<Option<FileInput>, RocksError> {
        let mut cursor = None;
        loop {
            let (rows, next) = self.graph.files(cursor.as_ref(), 256, self.budget)?;
            if let Some(file) = rows.into_iter().find(|file| file.normalized_path == path) {
                return Ok(Some(file));
            }
            cursor = next;
            if cursor.is_none() {
                return Ok(None);
            }
        }
    }
    fn files(&mut self, directory: Option<&str>, limit: u16) -> Result<Value, RocksError> {
        let mut cursor = None;
        let mut found = Vec::new();
        let mut truncated = false;
        'pages: loop {
            let (rows, next) = self.graph.files(cursor.as_ref(), 256, self.budget)?;
            for file in rows {
                if directory.is_some_and(|dir| {
                    file.normalized_path != dir
                        && !file
                            .normalized_path
                            .strip_prefix(dir)
                            .is_some_and(|tail| tail.starts_with('/'))
                }) {
                    continue;
                }
                if found.len() == usize::from(limit) {
                    truncated = true;
                    break 'pages;
                }
                found.push(file);
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        found.sort_unstable_by(|left, right| left.normalized_path.cmp(&right.normalized_path));
        Ok(json!({"files": found, "truncated": truncated}))
    }
    fn references(&mut self, query: &str, limit: u16) -> Result<Value, RocksError> {
        let mut cursor = None;
        let mut hits = Vec::new();
        let mut truncated = false;
        'pages: loop {
            let (rows, next) = self
                .graph
                .reference_records(cursor.as_ref(), 256, self.budget)?;
            for reference in rows {
                if reference.reference_name != query {
                    continue;
                }
                if hits.len() == usize::from(limit) {
                    truncated = true;
                    break 'pages;
                }
                let file = self
                    .graph
                    .file(&reference.file_id, self.budget)?
                    .ok_or_else(invalid)?;
                hits.push(json!({"reference": reference, "path": file.normalized_path}));
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(json!({"results": hits, "truncated": truncated, "channel": "reference"}))
    }
    fn traversal(&mut self, query: &Traversal) -> Result<Value, RocksError> {
        let root = self
            .graph
            .symbol(&query.symbol, self.budget)?
            .ok_or(RocksError::GenerationUnavailable)?;
        let mut walk = TraversalWalk {
            query,
            nodes: vec![json!(root)],
            edges: Vec::new(),
            seen: BTreeSet::from([query.symbol.clone()]),
            queue: VecDeque::from([(query.symbol.clone(), 0_u8)]),
        };
        let mut truncated = false;
        while let Some((node, depth)) = walk.queue.pop_front() {
            if depth == query.depth {
                continue;
            }
            if !walk.visit_node(self, &node, depth)? {
                truncated = true;
                break;
            }
        }
        Ok(json!({"root": root, "nodes": walk.nodes, "edges": walk.edges, "truncated": truncated}))
    }
    fn lexical(&mut self, query: &str, limit: u16) -> Result<Value, RocksError> {
        let indexed = self
            .graph
            .lexical_statistics(self.budget)?
            .is_some_and(|statistics| statistics.scan_documents == 0);
        let (hits, truncated) = self.graph.search_code(query, limit, self.budget)?;
        let hits = hits
            .into_iter()
            .map(|hit| {
                json!({
                    "document_id": hit.document_id(), "symbol_id": hit.symbol_id(),
                    "file_id": hit.file_id(), "qualified_name": hit.qualified_name(),
                    "path": hit.path(), "language": hit.language(),
                    "kind": hit.document_kind(), "score": hit.score(),
                })
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"results": hits, "truncated": truncated, "channel": "bm25",
        "implementation": if indexed { "native_field_postings_v1" } else { "native_bounded_document_scan_v1" },
        "semantic": "not_available"}),
        )
    }
}
