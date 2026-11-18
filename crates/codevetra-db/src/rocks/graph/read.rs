use std::mem::size_of;

use codevetra_domain::{
    ContentDigest, DocumentId, EdgeKind, FileId, GenerationId, NumericalSiteId, SymbolId,
};
use serde::de::DeserializeOwned;

use crate::{
    CanonicalSearchDocument, EdgeInput, FileInput, GraphDirection, NumericalSiteInput,
    ReferenceInput, SymbolInput,
};

use super::super::engine::PointReaders;
use super::super::{Collection, ReadBudget, RecordKey, Result, RevisionSnapshot, RocksError};
use super::{
    DOCUMENT, FILE, GraphInfo, INCOMING, MANIFEST, NUMERICAL, OUTGOING, REFERENCE, SYMBOL, codec,
    ingest::EdgeProperties, names,
};

/// Opaque cursor tied to one revision, vertex, direction, and packed block.
#[derive(Clone)]
pub struct GraphCursor {
    project: ContentDigest,
    generation: GenerationId,
    node: SymbolId,
    incoming: bool,
    block: RecordKey,
    offset: usize,
}

/// Topology-only relationship; properties are fetched separately on demand.
#[derive(Clone, Debug)]
pub struct GraphLink {
    project: ContentDigest,
    generation: GenerationId,
    source: SymbolId,
    target: SymbolId,
    kind: EdgeKind,
    entry: [u8; codec::ENTRY_BYTES],
}

impl GraphLink {
    /// Source symbol of this relationship.
    #[must_use]
    pub const fn source_symbol_id(&self) -> &SymbolId {
        &self.source
    }

    /// Target symbol of this relationship.
    #[must_use]
    pub const fn target_symbol_id(&self) -> &SymbolId {
        &self.target
    }

    /// Structural relationship category.
    #[must_use]
    pub const fn kind(&self) -> EdgeKind {
        self.kind
    }
}

/// Finite topology page in stable neighbor/kind/provenance-digest order.
pub struct GraphNeighborPage {
    /// Relationships; no source text or property documents are read.
    pub links: Vec<GraphLink>,
    /// Continue under the same immutable revision, including short pages.
    pub next: Option<GraphCursor>,
}

pub(crate) struct ReferenceSitePage {
    pub(crate) rows: Vec<(RecordKey, ReferenceInput)>,
    pub(crate) next: Option<RecordKey>,
}

/// Typed graph view borrowing exactly one complete native revision snapshot.
pub struct GraphSnapshot<'view, 'store> {
    pub(super) snapshot: &'view RevisionSnapshot<'store>,
    point_readers: PointReaders<'view, 'store>,
    pub(super) info: GraphInfo,
}

impl<'store> RevisionSnapshot<'store> {
    /// Validate the typed graph format and digest for this pinned revision.
    ///
    /// # Errors
    /// Rejects missing graph capability, format/digest mismatch, or read bounds.
    pub fn graph<'view>(
        &'view self,
        budget: &mut ReadBudget,
    ) -> Result<GraphSnapshot<'view, 'store>> {
        let bytes = self
            .get(Collection::Graph, &codec::key(MANIFEST, &[])?, budget)?
            .ok_or(RocksError::Format)?;
        let info: GraphInfo = codec::decode(&bytes)?;
        if info.format_version != 1 || info.name_index_version > 1 {
            return Err(RocksError::Format);
        }
        if self.revision().content_digest.as_ref() != Some(&info.content_digest) {
            return Err(codec::corrupt());
        }
        Ok(GraphSnapshot {
            snapshot: self,
            point_readers: self.point_readers(),
            info,
        })
    }
}

impl<'view, 'store> GraphSnapshot<'view, 'store> {
    /// Exact generation pinned by this immutable view.
    #[must_use]
    pub fn generation_id(&self) -> &GenerationId {
        &self.snapshot.revision().generation_id
    }

    /// Read a finite canonical file page from this revision.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn files(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<FileInput>, Option<RecordKey>)> {
        self.table(Collection::Graph, FILE)
            .page(after, limit, budget)
    }

    /// Read a finite canonical symbol page from this revision.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn symbols(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<SymbolInput>, Option<RecordKey>)> {
        self.table(Collection::Graph, SYMBOL)
            .page(after, limit, budget)
    }

    /// Bind this lookup to one immutable graph and its query identity.
    #[must_use]
    pub fn symbols_by_name<'query>(
        &'query self,
        name: &'query str,
    ) -> NamedSymbols<'query, 'view, 'store> {
        NamedSymbols { graph: self, name }
    }

    /// Read a finite canonical document page for bounded native retrieval.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn documents(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<CanonicalSearchDocument>, Option<RecordKey>)> {
        self.table(Collection::Search, DOCUMENT)
            .page(after, limit, budget)
    }

    pub(crate) fn document_cursor(id: &DocumentId) -> Result<RecordKey> {
        codec::key(DOCUMENT, &codec::uuid(id.as_str())?)
    }

    /// Read a finite canonical reference page, including unresolved references.
    ///
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn reference_records(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<ReferenceInput>, Option<RecordKey>)> {
        self.table(Collection::Graph, REFERENCE)
            .page(after, limit, budget)
    }

    /// Bind this lookup to one immutable graph and its query identity.
    #[must_use]
    pub(crate) fn reference_sites<'query>(
        &'query self,
        file: Option<&'query FileId>,
    ) -> ReferenceSites<'query, 'view, 'store> {
        ReferenceSites { graph: self, file }
    }

    /// Read a finite canonical numerical-site page from this revision.
    /// # Errors
    /// Returns cursor, decoding, budget, or storage failures.
    pub fn numerical_records(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<NumericalSiteInput>, Option<RecordKey>)> {
        self.table(Collection::Graph, NUMERICAL)
            .page(after, limit, budget)
    }

    /// Exact row counts and physical graph format for the pinned revision.
    #[must_use]
    pub const fn info(&self) -> &GraphInfo {
        &self.info
    }

    /// Read one canonical source-file record.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn file(&self, id: &FileId, budget: &mut ReadBudget) -> Result<Option<FileInput>> {
        self.table(Collection::Graph, FILE)
            .get(&codec::uuid(id.as_str())?, budget)
    }

    /// Read one canonical declaration without traversing its relationships.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn symbol(&self, id: &SymbolId, budget: &mut ReadBudget) -> Result<Option<SymbolInput>> {
        self.table(Collection::Graph, SYMBOL)
            .get(&codec::uuid(id.as_str())?, budget)
    }

    /// Read one static numerical evidence record.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn numerical_site(
        &self,
        id: &NumericalSiteId,
        budget: &mut ReadBudget,
    ) -> Result<Option<NumericalSiteInput>> {
        self.table(Collection::Graph, NUMERICAL)
            .get(&codec::uuid(id.as_str())?, budget)
    }

    /// Read a canonical durable search document; this does not imply BM25 readiness.
    ///
    /// # Errors
    /// Returns read-budget, decoding, or storage failures.
    pub fn document(
        &self,
        id: &DocumentId,
        budget: &mut ReadBudget,
    ) -> Result<Option<CanonicalSearchDocument>> {
        self.table(Collection::Search, DOCUMENT)
            .get(&codec::uuid(id.as_str())?, budget)
    }

    /// Bind this lookup to one immutable graph and its query identity.
    #[must_use]
    pub fn references<'query>(
        &'query self,
        id: &'query FileId,
    ) -> FileReferences<'query, 'view, 'store> {
        FileReferences { graph: self, id }
    }

    /// Bind topology reads to one snapshot, vertex and direction.
    #[must_use]
    pub fn adjacency<'query>(
        &'query self,
        node: &'query SymbolId,
        direction: GraphDirection,
    ) -> GraphAdjacency<'query, 'store> {
        GraphAdjacency {
            snapshot: self.snapshot,
            node,
            incoming: matches!(direction, GraphDirection::Incoming),
        }
    }

    /// Hydrate confidence, provenance, and site multiplicity for one topology link.
    ///
    /// # Errors
    /// Rejects links from another revision or mismatching/missing properties.
    pub fn edge(&self, link: &GraphLink, budget: &mut ReadBudget) -> Result<EdgeInput> {
        if link.project != *self.snapshot.project_identity()
            || link.generation != self.snapshot.revision().generation_id
        {
            return Err(RocksError::GenerationChanged);
        }
        let key = codec::edge_key(&link.source, &link.target, &link.entry)?;
        let bytes = self
            .point_readers
            .get(Collection::Graph, &key, budget)?
            .ok_or_else(codec::corrupt)?;
        let properties: EdgeProperties = codec::decode(&bytes)?;
        // The inclusive range also rejects NaN and either infinity.
        if !(0.0..=1.0).contains(&properties.confidence)
            || !(1..=100_000_000).contains(&properties.site_count)
            || properties.provenance.len() > 256
            || blake3::hash(properties.provenance.as_bytes())
                .as_bytes()
                .as_slice()
                != &link.entry[codec::ID_BYTES + 1..]
        {
            return Err(codec::corrupt());
        }
        Ok(EdgeInput {
            source_symbol_id: link.source.clone(),
            target_symbol_id: link.target.clone(),
            kind: link.kind,
            confidence: properties.confidence,
            provenance: properties.provenance,
            site_count: properties.site_count,
        })
    }

    fn table<'query>(
        &'query self,
        collection: Collection,
        prefix: &'query [u8],
    ) -> GraphTable<'query, 'view, 'store> {
        GraphTable {
            snapshot: self.snapshot,
            readers: &self.point_readers,
            collection,
            prefix,
        }
    }
}

/// Borrowed adjacency view with fixed project, generation, vertex and direction.
pub struct GraphAdjacency<'view, 'store> {
    snapshot: &'view RevisionSnapshot<'store>,
    node: &'view SymbolId,
    incoming: bool,
}

impl GraphAdjacency<'_, '_> {
    /// Traverse compact adjacency without loading confidence/provenance properties.
    ///
    /// Each block contains at most 128 binary relationships. Large vertices
    /// use radix-partitioned blocks, so a local edit rewrites a bounded leaf
    /// instead of shifting every subsequent adjacency page.
    ///
    /// # Errors
    /// Rejects foreign/stale cursors, malformed blocks, or exhausted read bounds.
    pub fn page(
        &self,
        after: Option<&GraphCursor>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<GraphNeighborPage> {
        if limit == 0 || limit > 4_096 {
            return Err(RocksError::Limit {
                resource: "graph_neighbors",
            });
        }
        let incoming = self.incoming;
        let node = self.node;
        if after.is_some_and(|cursor| {
            cursor.project != *self.snapshot.project_identity()
                || cursor.generation != self.snapshot.revision().generation_id
                || cursor.node != *node
                || cursor.incoming != incoming
        }) {
            return Err(RocksError::GenerationChanged);
        }
        let prefix = codec::key(
            if incoming { INCOMING } else { OUTGOING },
            &codec::uuid(node.as_str())?,
        )?;
        Neighborhood {
            snapshot: self.snapshot,
            node,
            incoming,
            prefix,
            cursor: after.cloned(),
            resume_block: after.is_some(),
            links: Vec::new(),
            limit: usize::from(limit),
        }
        .read(budget)
    }

    pub(crate) fn visit(
        &self,
        budget: &mut ReadBudget,
        mut visit: impl FnMut(GraphLink, &mut ReadBudget) -> Result<()>,
    ) -> Result<()> {
        let mut cursor = None;
        loop {
            let page = self.page(cursor.as_ref(), 256, budget)?;
            for link in page.links {
                budget.checkpoint()?;
                visit(link, budget)?;
            }
            cursor = page.next;
            if cursor.is_none() {
                return Ok(());
            }
        }
    }
}

struct Neighborhood<'view, 'store> {
    snapshot: &'view RevisionSnapshot<'store>,
    node: &'view SymbolId,
    incoming: bool,
    prefix: RecordKey,
    cursor: Option<GraphCursor>,
    resume_block: bool,
    links: Vec<GraphLink>,
    limit: usize,
}

impl Neighborhood<'_, '_> {
    fn read(mut self, budget: &mut ReadBudget) -> Result<GraphNeighborPage> {
        while let Some((key, bytes)) = self.next_block(budget)? {
            let block = validate_block(&key, self.prefix.as_bytes(), &bytes)?;
            let offset = self
                .cursor
                .as_ref()
                .filter(|cursor| cursor.block == key)
                .map_or(0, |cursor| cursor.offset);
            if offset > block.entries.len() {
                return Err(codec::corrupt());
            }
            self.append(&block, offset, budget)?;
            if self.links.len() == self.limit {
                return Ok(GraphNeighborPage {
                    links: self.links,
                    next: self.cursor,
                });
            }
            let next_offset = block.entries.len();
            self.cursor = Some(self.cursor_at(key, next_offset));
        }
        Ok(GraphNeighborPage {
            links: self.links,
            next: None,
        })
    }

    fn next_block(&mut self, budget: &mut ReadBudget) -> Result<Option<(RecordKey, Vec<u8>)>> {
        if self.resume_block {
            self.resume_block = false;
            let key = &self.cursor.as_ref().ok_or_else(codec::corrupt)?.block;
            let bytes = self
                .snapshot
                .get(Collection::Graph, key, budget)?
                .ok_or_else(codec::corrupt)?;
            return Ok(Some((key.clone(), bytes)));
        }
        let mut after = self.cursor.as_ref().map(|cursor| cursor.block.clone());
        loop {
            let page = self
                .snapshot
                .scan_prefix(Collection::Graph, self.prefix.as_bytes())
                .page(after.as_ref(), 1, budget)?;
            if let Some(row) = page.rows.into_iter().next() {
                return Ok(Some(row));
            }
            after = page.next_after;
            if after.is_none() {
                return Ok(None);
            }
        }
    }

    fn append(
        &mut self,
        block: &AdjacencyBlock<'_>,
        offset: usize,
        budget: &mut ReadBudget,
    ) -> Result<()> {
        // One 64-byte project digest and three 36-byte textual graph IDs.
        const LINK_IDENTITY_BYTES: usize = 64 + 3 * 36;
        let initial_length = self.links.len();
        for entry in block.entries.iter().skip(offset) {
            budget.charge(size_of::<GraphLink>() + LINK_IDENTITY_BYTES)?;
            self.links.push(self.link(*entry)?);
            if self.links.len() == self.limit {
                break;
            }
        }
        if self.links.len() > initial_length {
            self.cursor = Some(self.cursor_at(
                block.key.clone(),
                offset + self.links.len() - initial_length,
            ));
        }
        Ok(())
    }

    fn link(&self, entry: [u8; codec::ENTRY_BYTES]) -> Result<GraphLink> {
        let neighbor = codec::symbol(&entry[..codec::ID_BYTES])?;
        let kind = codec::kind(entry[codec::ID_BYTES])?;
        let (source, target) = if self.incoming {
            (neighbor, self.node.clone())
        } else {
            (self.node.clone(), neighbor)
        };
        Ok(GraphLink {
            project: self.snapshot.project_identity().clone(),
            generation: self.snapshot.revision().generation_id.clone(),
            source,
            target,
            kind,
            entry,
        })
    }

    fn cursor_at(&self, block: RecordKey, offset: usize) -> GraphCursor {
        GraphCursor {
            project: self.snapshot.project_identity().clone(),
            generation: self.snapshot.revision().generation_id.clone(),
            node: self.node.clone(),
            incoming: self.incoming,
            block,
            offset,
        }
    }
}

struct AdjacencyBlock<'a> {
    key: &'a RecordKey,
    entries: &'a [[u8; codec::ENTRY_BYTES]],
}

fn validate_block<'a>(
    key: &'a RecordKey,
    prefix: &[u8],
    bytes: &'a [u8],
) -> Result<AdjacencyBlock<'a>> {
    let (depth, entries) = block_payload(bytes)?;
    let suffix = key
        .as_bytes()
        .strip_prefix(prefix)
        .ok_or_else(codec::corrupt)?;
    let suffix_bytes = if depth == 0 { 0 } else { codec::ENTRY_BYTES };
    if suffix.len() != suffix_bytes {
        return Err(codec::corrupt());
    }
    let mut previous = None;
    for entry in entries {
        if (depth > 0 && codec::radix_prefix(entry, depth)?.as_slice() != suffix)
            || previous.is_some_and(|previous| previous >= entry)
        {
            return Err(codec::corrupt());
        }
        codec::kind(entry[codec::ID_BYTES])?;
        previous = Some(entry);
    }
    Ok(AdjacencyBlock { key, entries })
}

/// Decode the block framing before interpreting entries or a partition key.
fn block_payload(bytes: &[u8]) -> Result<(usize, &[[u8; codec::ENTRY_BYTES]])> {
    let payload = bytes
        .strip_prefix(codec::BLOCK_HEADER)
        .ok_or_else(codec::corrupt)?;
    let (depth, entries) = payload
        .split_at_checked(size_of::<u16>())
        .ok_or_else(codec::corrupt)?;
    let depth = usize::from(u16::from_be_bytes(
        depth.try_into().map_err(|_| codec::corrupt())?,
    ));
    let (entries, remainder) = entries.as_chunks::<{ codec::ENTRY_BYTES }>();
    if !remainder.is_empty()
        || !(1..=codec::BLOCK_CAPACITY).contains(&entries.len())
        || depth > codec::ENTRY_BYTES * u8::BITS as usize
    {
        return Err(codec::corrupt());
    }
    Ok((depth, entries))
}

struct GraphTable<'query, 'view, 'store> {
    snapshot: &'view RevisionSnapshot<'store>,
    readers: &'query PointReaders<'view, 'store>,
    collection: Collection,
    prefix: &'query [u8],
}

impl GraphTable<'_, '_, '_> {
    fn page<T: DeserializeOwned>(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<T>, Option<RecordKey>)> {
        let page = self
            .snapshot
            .scan_prefix(self.collection, self.prefix)
            .page(after, limit, budget)?;
        let rows = page
            .rows
            .into_iter()
            .map(|(_, bytes)| codec::decode(&bytes))
            .collect::<Result<Vec<_>>>()?;
        Ok((rows, page.next_after))
    }

    fn get<T: DeserializeOwned>(
        &self,
        suffix: &[u8],
        budget: &mut ReadBudget,
    ) -> Result<Option<T>> {
        self.readers
            .get(self.collection, &codec::key(self.prefix, suffix)?, budget)?
            .map(|bytes| codec::decode(&bytes))
            .transpose()
    }
}

/// A graph lookup whose query identity remains fixed across pages.
pub struct NamedSymbols<'query, 'view, 'store> {
    graph: &'query GraphSnapshot<'view, 'store>,
    name: &'query str,
}

impl NamedSymbols<'_, '_, '_> {
    /// Page exact simple or qualified names, matching Unicode lowercase keys.
    /// Callers retain their original case-sensitive or qualified predicates and
    /// result ordering. Inspect `info().name_index_version` before using this
    /// optional index; zero requires the existing symbol scan.
    /// # Errors
    /// Rejects unavailable indexes, invalid names, cursors, corrupt postings,
    /// changed/missing symbols, and exhausted read budgets.
    pub fn page(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<SymbolInput>, Option<RecordKey>)> {
        let name = self.name;

        if self.graph.info.name_index_version != 1 || name.is_empty() || name.len() > 4096 {
            return Err(RocksError::InvalidInput {
                field: "name_index",
            });
        }
        let mut prefix = names::PREFIX.to_vec();
        prefix.extend_from_slice(&names::hash(name));
        let page = self
            .graph
            .snapshot
            .scan_prefix(Collection::Graph, &prefix)
            .page(after, limit, budget)?;
        let expected = name.to_lowercase();
        let mut rows = Vec::new();
        for (key, marker) in page.rows {
            if marker != [1] {
                return Err(codec::corrupt());
            }
            let raw = key
                .as_bytes()
                .strip_prefix(prefix.as_slice())
                .ok_or_else(codec::corrupt)?;
            let id = codec::symbol(raw)?;
            let symbol = self.graph.symbol(&id, budget)?.ok_or_else(codec::corrupt)?;
            // Hash collisions cannot admit an unrelated declaration.
            if symbol.qualified_name.to_lowercase() == expected
                || names::simple(&symbol.qualified_name).to_lowercase() == expected
            {
                rows.push(symbol);
            }
        }
        Ok((rows, page.next_after))
    }
}

/// A graph lookup whose query identity remains fixed across pages.
pub(crate) struct ReferenceSites<'query, 'view, 'store> {
    graph: &'query GraphSnapshot<'view, 'store>,
    file: Option<&'query FileId>,
}

impl ReferenceSites<'_, '_, '_> {
    /// Page references with exact continuation keys, optionally restricted to one file.
    /// # Errors
    /// Rejects malformed file identities, cursors, decoding and read-budget failures.
    pub(crate) fn page(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<ReferenceSitePage> {
        let file = self.file;

        let mut prefix = REFERENCE.to_vec();
        if let Some(file) = file {
            prefix.extend_from_slice(&codec::uuid(file.as_str())?);
        }
        let page = self
            .graph
            .snapshot
            .scan_prefix(Collection::Graph, &prefix)
            .page(after, limit, budget)?;
        let rows = page
            .rows
            .into_iter()
            .map(|(key, bytes)| Ok((key, codec::decode(&bytes)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(ReferenceSitePage {
            rows,
            next: page.next_after,
        })
    }
}

/// A graph lookup whose query identity remains fixed across pages.
pub struct FileReferences<'query, 'view, 'store> {
    graph: &'query GraphSnapshot<'view, 'store>,
    id: &'query FileId,
}

impl FileReferences<'_, '_, '_> {
    /// Read a bounded file-reference page with the native logical-key cursor.
    ///
    /// # Errors
    /// Returns cursor, read-budget, decoding, or storage failures.
    pub fn page(
        &self,
        after: Option<&RecordKey>,
        limit: u16,
        budget: &mut ReadBudget,
    ) -> Result<(Vec<ReferenceInput>, Option<RecordKey>)> {
        let id = self.id;

        let prefix = codec::key(REFERENCE, &codec::uuid(id.as_str())?)?;
        let page = self
            .graph
            .snapshot
            .scan_prefix(Collection::Graph, prefix.as_bytes())
            .page(after, limit, budget)?;
        let mut rows = Vec::with_capacity(page.rows.len());
        for (_, bytes) in page.rows {
            rows.push(codec::decode(&bytes)?);
        }
        Ok((rows, page.next_after))
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    fn block(depth: u16, entries: &[[u8; 49]]) -> Vec<u8> {
        let mut bytes = b"CGA1".to_vec();
        bytes.extend_from_slice(&depth.to_be_bytes());
        bytes.extend_from_slice(entries.as_flattened());
        bytes
    }

    #[test]
    fn adjacency_geometry_preserves_capacity_and_radix_boundaries() -> Result<()> {
        let prefix = b"cg1/o/fixture";
        let root = RecordKey::new(prefix.to_vec())?;
        let entries: Vec<[u8; 49]> = (1..=129)
            .map(|index| {
                let mut entry = [0; 49];
                entry[0] = index;
                entry
            })
            .collect();
        assert!(validate_block(&root, prefix, &block(0, &[])).is_err());
        assert_eq!(
            validate_block(&root, prefix, &block(0, &entries[..128]))?
                .entries
                .as_flattened()
                .len(),
            128 * 49
        );
        assert!(validate_block(&root, prefix, &block(0, &entries)).is_err());
        assert!(validate_block(&root, b"foreign", &block(0, &entries[..1])).is_err());
        let leaf = RecordKey::new([prefix.as_slice(), &entries[0]].concat())?;
        assert!(validate_block(&leaf, prefix, &block(0, &entries[..1])).is_err());
        assert!(validate_block(&root, prefix, &block(392, &entries[..1])).is_err());
        assert!(validate_block(&leaf, prefix, &block(392, &entries[..1])).is_ok());
        assert!(validate_block(&leaf, prefix, &block(393, &entries[..1])).is_err());
        assert!(validate_block(&leaf, prefix, &block(392, &entries[1..2])).is_err());
        assert!(validate_block(&root, prefix, &block(0, &[entries[0], entries[0]])).is_err());
        assert!(validate_block(&root, prefix, &block(0, &[entries[1], entries[0]])).is_err());
        let mut invalid_kind = entries[0];
        invalid_kind[16] = 255;
        assert!(validate_block(&root, prefix, &block(0, &[invalid_kind])).is_err());
        let bytes = block(0, &entries[..1]);
        for end in 0..bytes.len() {
            assert!(validate_block(&root, prefix, &bytes[..end]).is_err());
        }
        Ok(())
    }
}
