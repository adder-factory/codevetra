use super::{Result, RocksError};

mod encoding;

pub(super) fn escape(bytes: &[u8]) -> Vec<u8> {
    encoding::escape(bytes)
}

pub(super) fn successor(bytes: Vec<u8>) -> Option<Vec<u8>> {
    encoding::successor(bytes)
}

const MAXIMUM_KEY_BYTES: usize = 4_096;

/// Versioned record namespaces with independent `RocksDB` compaction policies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Collection {
    /// Files, symbols, edges, references, numerical sites, and graph indexes.
    Graph,
    /// Search documents, term statistics, and field-aware postings.
    Search,
    /// Model-scoped vectors and vector-index state.
    Embeddings,
    /// Content-fenced summaries, coverage, history, and other derived evidence.
    Derived,
}

impl Collection {
    pub(super) const ALL: [Self; 4] = [Self::Graph, Self::Search, Self::Embeddings, Self::Derived];

    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Search => "search",
            Self::Embeddings => "embeddings",
            Self::Derived => "derived",
        }
    }
}

/// A bounded logical record key, never rendered in debug/error messages.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordKey(Vec<u8>);

impl std::fmt::Debug for RecordKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordKey")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl RecordKey {
    /// Admit one nonempty logical key, including binary composite keys.
    ///
    /// # Errors
    /// Returns a limit error for empty or oversized keys.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > MAXIMUM_KEY_BYTES {
            return Err(RocksError::Limit { resource: "key" });
        }
        Ok(Self(bytes))
    }

    /// Logical bytes for typed decoding inside the storage boundary.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(super) fn prefix(&self) -> Vec<u8> {
        let mut encoded = escape(&self.0);
        encoded.extend_from_slice(&[0, 0]);
        encoded
    }

    pub(super) fn at_sequence(&self, sequence: u64) -> Vec<u8> {
        let mut encoded = self.prefix();
        encoded.extend_from_slice(&(!sequence).to_be_bytes());
        encoded
    }
}

pub(super) fn decode(encoded: &[u8]) -> Result<(RecordKey, u64)> {
    let invalid = || RocksError::Corrupt {
        field: "record_key",
    };
    let (bytes, sequence) = encoding::decode(encoded).ok_or_else(invalid)?;
    let key = RecordKey::new(bytes).map_err(|_| invalid())?;
    Ok((key, sequence))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_keys_and_sequences_have_independent_order()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let first = RecordKey::new(b"edge\0a\0b".to_vec())?;
        let next = RecordKey::new(b"edge\0a\0c".to_vec())?;
        assert!(first.at_sequence(9) < first.at_sequence(8));
        assert!(first.at_sequence(0) < next.at_sequence(u64::MAX));
        assert_eq!(decode(&first.at_sequence(42))?, (first, 42));
        assert!(decode(b"unterminated").is_err());
        assert!(decode(&[0, 0, 1]).is_err());
        Ok(())
    }
}
