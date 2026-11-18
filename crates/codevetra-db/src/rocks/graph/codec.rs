use std::io;

mod arithmetic;
use arithmetic::nibble;

use codevetra_domain::{EdgeKind, SymbolId};
use serde::{Serialize, de::DeserializeOwned};

use super::super::{RecordKey, Result, RocksError};

pub(super) const ID_BYTES: usize = 16;
pub(super) const ENTRY_BYTES: usize = ID_BYTES + 1 + 32;
pub(super) const BLOCK_CAPACITY: usize = 128;
pub(super) const BLOCK_HEADER: &[u8] = b"CGA1";
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const NIBBLE_BITS: u32 = 4;
const NIBBLE_MASK: u8 = 0x0f;
const UUID_GROUP_STARTS: [usize; 4] = [4, 6, 8, 10];
const UUID_TEXT_BYTES: usize = ID_BYTES * 2 + UUID_GROUP_STARTS.len();

pub(super) fn radix_prefix(entry: &[u8; ENTRY_BYTES], depth: usize) -> Result<[u8; ENTRY_BYTES]> {
    let (bytes, remainder) = arithmetic::radix_geometry(depth, ENTRY_BYTES).ok_or_else(corrupt)?;
    let mut prefix = [0; ENTRY_BYTES];
    prefix[..bytes].copy_from_slice(&entry[..bytes]);
    if remainder != 0 {
        prefix[bytes] = arithmetic::prefix_byte(entry[bytes], remainder).ok_or_else(corrupt)?;
    }
    Ok(prefix)
}

pub(super) fn corrupt() -> RocksError {
    RocksError::Corrupt {
        field: "graph_record",
    }
}

pub(crate) fn uuid(raw: &str) -> Result<[u8; ID_BYTES]> {
    let mut bytes = [0; ID_BYTES];
    let mut hex = raw.bytes().filter(|&byte| byte != b'-');
    for byte in &mut bytes {
        let upper = hex.next().and_then(nibble).ok_or_else(corrupt)?;
        let lower = hex.next().and_then(nibble).ok_or_else(corrupt)?;
        *byte = (upper << NIBBLE_BITS) | lower;
    }
    if hex.next().is_some() {
        return Err(corrupt());
    }
    Ok(bytes)
}

pub(super) fn symbol(bytes: &[u8]) -> Result<SymbolId> {
    SymbolId::parse(&uuid_text(bytes)?).map_err(|_| corrupt())
}

pub(crate) fn uuid_text(bytes: &[u8]) -> Result<String> {
    if bytes.len() != ID_BYTES {
        return Err(corrupt());
    }
    let mut raw = String::with_capacity(UUID_TEXT_BYTES);
    for (index, byte) in bytes.iter().enumerate() {
        if UUID_GROUP_STARTS.contains(&index) {
            raw.push('-');
        }
        raw.push(char::from(HEX_DIGITS[usize::from(byte >> NIBBLE_BITS)]));
        raw.push(char::from(HEX_DIGITS[usize::from(byte & NIBBLE_MASK)]));
    }
    Ok(raw)
}

pub(super) fn key(prefix: &[u8], suffix: &[u8]) -> Result<RecordKey> {
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(suffix);
    RecordKey::new(bytes)
}

pub(super) fn entry(
    neighbor: &SymbolId,
    kind: EdgeKind,
    provenance: &str,
) -> Result<[u8; ENTRY_BYTES]> {
    let mut bytes = [0; ENTRY_BYTES];
    bytes[..ID_BYTES].copy_from_slice(&uuid(neighbor.as_str())?);
    bytes[ID_BYTES] = kind as u8;
    bytes[ID_BYTES + 1..].copy_from_slice(blake3::hash(provenance.as_bytes()).as_bytes());
    Ok(bytes)
}

pub(super) fn edge_key(
    source: &SymbolId,
    target: &SymbolId,
    entry: &[u8; ENTRY_BYTES],
) -> Result<RecordKey> {
    let mut suffix = uuid(source.as_str())?.to_vec();
    suffix.extend_from_slice(&uuid(target.as_str())?);
    suffix.extend_from_slice(&entry[ID_BYTES..]);
    key(super::EDGE, &suffix)
}

pub(super) fn kind(byte: u8) -> Result<EdgeKind> {
    EdgeKind::ALL
        .get(usize::from(byte))
        .copied()
        .ok_or_else(corrupt)
}

pub(super) fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut writer = BoundedJson(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(|_| corrupt())?;
    Ok(writer.0)
}

struct BoundedJson(Vec<u8>);

impl io::Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > super::super::engine::MAXIMUM_RECORD_BYTES {
            return Err(io::Error::other("graph record limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| corrupt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_codec_preserves_canonical_grouping_and_both_nibbles() -> Result<()> {
        let bytes = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba,
            0xdc, 0xfe,
        ];
        let canonical = "01234567-89ab-cdef-1032-547698badcfe";
        assert_eq!(uuid_text(&bytes)?, canonical);
        assert_eq!(uuid(canonical)?, bytes);
        assert!(uuid_text(&bytes[..15]).is_err());
        assert!(uuid_text(&[0; 17]).is_err());
        Ok(())
    }
}
