//! Bit and index geometry of packed graph records.

pub(super) fn radix_geometry(depth: usize, bytes: usize) -> Option<(usize, usize)> {
    let whole = depth / 8;
    let remainder = depth % 8;
    if whole > bytes || (whole == bytes && remainder != 0) {
        None
    } else {
        Some((whole, remainder))
    }
}

pub(super) fn prefix_byte(byte: u8, bits: usize) -> Option<u8> {
    if bits > 8 {
        None
    } else if bits == 0 {
        Some(0)
    } else {
        Some(byte & (u8::MAX << (8 - bits)))
    }
}

pub(super) fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
