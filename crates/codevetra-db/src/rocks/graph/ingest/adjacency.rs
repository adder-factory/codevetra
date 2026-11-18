//! Index-only sorting keeps both directions within the publication workspace.

use std::cmp::Ordering;

use codevetra_domain::SymbolId;

use super::{EdgeInput, RecordKey, Result, StorageRequest, codec, count};
use crate::rocks::GraphPolicy;

#[derive(Default)]
pub(in crate::rocks::graph) struct AdjacencyReport {
    pub(in crate::rocks::graph) blocks: u64,
    pub(in crate::rocks::graph) maximum_degree: u64,
}

// One adjacency writer owns the cancellation scope, sink and block accounting.
pub(in crate::rocks::graph) struct AdjacencyWriter<'a, Emit> {
    request: &'a StorageRequest,
    emit: Emit,
    report: AdjacencyReport,
}

impl<'a, Emit> AdjacencyWriter<'a, Emit>
where
    Emit: FnMut(RecordKey, Vec<u8>) -> Result<()>,
{
    pub(in crate::rocks::graph) fn new(request: &'a StorageRequest, emit: Emit) -> Self {
        Self {
            request,
            emit,
            report: AdjacencyReport::default(),
        }
    }
    pub(in crate::rocks::graph) fn visit(
        mut self,
        edges: &[EdgeInput],
        incoming: bool,
        policy: GraphPolicy,
    ) -> Result<AdjacencyReport> {
        super::check_workspace::<usize>(edges.len(), 0, policy)?;
        let mut order = Vec::new();
        order
            .try_reserve_exact(edges.len())
            .map_err(|_| super::super::limit())?;
        super::check_workspace::<usize>(order.capacity(), 0, policy)?;
        for index in 0..edges.len() {
            self.request.check()?;
            order.push(index);
        }
        order.sort_unstable_by(|&left, &right| compare(&edges[left], &edges[right], incoming));
        self.request.check()?;
        let prefix = if incoming {
            super::INCOMING
        } else {
            super::OUTGOING
        };
        let mut offset = 0;
        while offset < order.len() {
            self.request.check()?;
            let node = parts(&edges[order[offset]], incoming).0;
            let end = offset
                + order[offset..]
                    .partition_point(|&index| parts(&edges[index], incoming).0 == node);
            self.report.maximum_degree = self.report.maximum_degree.max(count(end - offset)?);
            let node_key = codec::key(prefix, &codec::uuid(node.as_str())?)?;
            self.partition(
                &PackedPartition {
                    rows: &order[offset..end],
                    edges,
                    incoming,
                    node: node_key.as_bytes(),
                },
                0,
            )?;
            offset = end;
        }
        Ok(self.report)
    }
}

fn parts(edge: &EdgeInput, incoming: bool) -> (&SymbolId, &SymbolId) {
    if incoming {
        (&edge.target_symbol_id, &edge.source_symbol_id)
    } else {
        (&edge.source_symbol_id, &edge.target_symbol_id)
    }
}

fn compare(left: &EdgeInput, right: &EdgeInput, incoming: bool) -> Ordering {
    let (left_node, left_neighbor) = parts(left, incoming);
    let (right_node, right_neighbor) = parts(right, incoming);
    (left_node, left_neighbor, left.kind as u8)
        .cmp(&(right_node, right_neighbor, right.kind as u8))
        .then_with(|| {
            blake3::hash(left.provenance.as_bytes())
                .as_bytes()
                .cmp(blake3::hash(right.provenance.as_bytes()).as_bytes())
        })
}

fn entry(edge: &EdgeInput, incoming: bool) -> Result<[u8; codec::ENTRY_BYTES]> {
    codec::entry(parts(edge, incoming).1, edge.kind, &edge.provenance)
}

struct PackedPartition<'a> {
    rows: &'a [usize],
    edges: &'a [EdgeInput],
    incoming: bool,
    node: &'a [u8],
}

impl<Emit> AdjacencyWriter<'_, Emit>
where
    Emit: FnMut(RecordKey, Vec<u8>) -> Result<()>,
{
    fn partition(&mut self, partition: &PackedPartition<'_>, depth: usize) -> Result<()> {
        self.request.check()?;
        let first = entry(
            &partition.edges[*partition.rows.first().ok_or_else(codec::corrupt)?],
            partition.incoming,
        )?;
        if partition.rows.len() <= codec::BLOCK_CAPACITY {
            let suffix = codec::radix_prefix(&first, depth)?;
            let key = codec::key(partition.node, if depth == 0 { &[] } else { &suffix })?;
            let mut value = codec::BLOCK_HEADER.to_vec();
            value.extend_from_slice(
                &u16::try_from(depth)
                    .map_err(|_| codec::corrupt())?
                    .to_be_bytes(),
            );
            for &index in partition.rows {
                self.request.check()?;
                value.extend_from_slice(&entry(&partition.edges[index], partition.incoming)?);
            }
            (self.emit)(key, value)?;
            self.report.blocks = self
                .report
                .blocks
                .checked_add(1)
                .ok_or_else(super::super::limit)?;
            return Ok(());
        }
        let last = entry(
            &partition.edges[*partition.rows.last().ok_or_else(codec::corrupt)?],
            partition.incoming,
        )?;
        let byte = first
            .iter()
            .zip(last)
            .position(|(&first, last)| first != last)
            .ok_or_else(codec::corrupt)?;
        let bit = usize::try_from((first[byte] ^ last[byte]).leading_zeros())
            .map_err(|_| codec::corrupt())?;
        let split = byte * 8 + bit;
        if split < depth {
            return Err(codec::corrupt());
        }
        // The comparator used precisely these binary entry fields. Search with
        // checked entry decoding so an invalid identity cannot become a key.
        let mut middle = 0;
        for &index in partition.rows {
            self.request.check()?;
            if entry(&partition.edges[index], partition.incoming)?[byte] & (0x80 >> bit) != 0 {
                break;
            }
            middle += 1;
        }
        if middle == 0 || middle == partition.rows.len() {
            return Err(codec::corrupt());
        }
        for rows in [&partition.rows[..middle], &partition.rows[middle..]] {
            self.partition(
                &PackedPartition {
                    rows,
                    edges: partition.edges,
                    incoming: partition.incoming,
                    node: partition.node,
                },
                split + 1,
            )?;
        }
        Ok(())
    }
}
