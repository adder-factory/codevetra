//! Logical-key encoding independent of the `RocksDB` execution boundary.

pub(super) fn escape(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(bytes.len().saturating_add(2));
    for &byte in bytes {
        encoded.push(byte);
        if byte == 0 {
            encoded.push(255);
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
                Some(255) => decoded.push(0),
                Some(0) => {
                    index += 1;
                    let sequence: [u8; 8] = encoded.get(index..)?.try_into().ok()?;
                    if decoded.is_empty() || decoded.len() > 4096 {
                        return None;
                    }
                    return Some((decoded, !u64::from_be_bytes(sequence)));
                }
                _ => return None,
            }
            index += 1;
        }
        if decoded.len() > 4096 {
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
