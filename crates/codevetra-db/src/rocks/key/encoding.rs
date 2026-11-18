//! Logical-key encoding independent of the `RocksDB` execution boundary.

pub(super) const MAXIMUM_KEY_BYTES: usize = 4_096;
const ESCAPED_ZERO: u8 = 255;
const SEQUENCE_BYTES: usize = std::mem::size_of::<u64>();

pub(super) fn escape(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(bytes.len().saturating_add(2));
    for &byte in bytes {
        encoded.push(byte);
        if byte == 0 {
            encoded.push(ESCAPED_ZERO);
        }
    }
    encoded
}

pub(super) fn decode(encoded: &[u8]) -> Option<(Vec<u8>, u64)> {
    let mut decoded = Vec::new();
    let mut index = 0_usize;
    while let Some(&byte) = encoded.get(index) {
        index += 1;
        if byte != 0 {
            decoded.push(byte);
        } else {
            match encoded.get(index) {
                Some(&ESCAPED_ZERO) => decoded.push(0),
                Some(0) => {
                    index += 1;
                    let sequence: [u8; SEQUENCE_BYTES] = encoded.get(index..)?.try_into().ok()?;
                    if decoded.is_empty() || decoded.len() > MAXIMUM_KEY_BYTES {
                        return None;
                    }
                    return Some((decoded, !u64::from_be_bytes(sequence)));
                }
                _ => return None,
            }
            index += 1;
        }
        if decoded.len() > MAXIMUM_KEY_BYTES {
            break;
        }
    }
    None
}

pub(super) fn successor(mut bytes: Vec<u8>) -> Option<Vec<u8>> {
    while let Some(last) = bytes.pop() {
        if let Some(next) = last.checked_add(1) {
            bytes.push(next);
            return Some(bytes);
        }
    }
    None
}
