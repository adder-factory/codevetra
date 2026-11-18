//! Bounded protobuf arithmetic and byte decoding used by the actual codec.

pub(super) const VARINT_PAYLOAD_BITS: usize = 7;
pub(super) const VARINT_PAYLOAD_MASK: u8 = 0x7f;
pub(super) const VARINT_CONTINUATION_BIT: u8 = 0x80;

pub(super) const WIRE_VARINT: u8 = 0;
pub(super) const WIRE_I64: u8 = 1;
pub(super) const WIRE_LEN: u8 = 2;
pub(super) const WIRE_I32: u8 = 5;
pub(super) const PROTOBUF_TAG_SHIFT: u32 = 3;
pub(super) const PROTOBUF_WIRE_TYPE_MASK: u64 = 7;
const MAXIMUM_FIELD_NUMBER: u32 = 0x1fff_ffff;

pub(super) fn varint_length(value: u64) -> usize {
    let mut value = value;
    let mut length = 1;
    while value > u64::from(VARINT_PAYLOAD_MASK) {
        value >>= VARINT_PAYLOAD_BITS;
        length += 1;
    }
    length
}

pub(super) fn delimited_size(
    tag_bytes: usize,
    length_bytes: usize,
    payload: usize,
) -> Option<usize> {
    tag_bytes.checked_add(length_bytes)?.checked_add(payload)
}

pub(super) fn field_tag(field: u32, wire: u8) -> Option<u64> {
    if field == 0
        || field > MAXIMUM_FIELD_NUMBER
        || !matches!(wire, WIRE_VARINT | WIRE_I64 | WIRE_LEN | WIRE_I32)
    {
        return None;
    }
    Some((u64::from(field) << PROTOBUF_TAG_SHIFT) | u64::from(wire))
}

pub(super) fn cursor_end(position: usize, length: usize, total: usize) -> Option<usize> {
    if position > total || length > total - position {
        None
    } else {
        Some(position + length)
    }
}

pub(super) fn read_varint(bytes: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    let mut position = start;
    for shift in (0..u64::BITS).step_by(VARINT_PAYLOAD_BITS) {
        let byte = *bytes.get(position)?;
        position = position.checked_add(1)?;
        let payload = u64::from(byte & VARINT_PAYLOAD_MASK);
        if shift == u64::BITS - 1 && payload > 1 {
            return None;
        }
        value |= payload << shift;
        if byte & VARINT_CONTINUATION_BIT == 0 {
            return Some((value, position));
        }
    }
    None
}
