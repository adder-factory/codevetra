//! Bounded protobuf arithmetic and byte decoding used by the actual codec.

pub(super) fn varint_length(value: u64) -> usize {
    if value < (1 << 7) {
        1
    } else if value < (1 << 14) {
        2
    } else if value < (1 << 21) {
        3
    } else if value < (1 << 28) {
        4
    } else if value < (1 << 35) {
        5
    } else if value < (1 << 42) {
        6
    } else if value < (1 << 49) {
        7
    } else if value < (1 << 56) {
        8
    } else if value < (1 << 63) {
        9
    } else {
        10
    }
}

pub(super) fn delimited_end(
    current: usize,
    tag_bytes: usize,
    length_bytes: usize,
    payload: usize,
    maximum: usize,
) -> Option<usize> {
    if current > maximum || tag_bytes > maximum - current {
        return None;
    }
    let after_tag = current + tag_bytes;
    if length_bytes > maximum - after_tag {
        return None;
    }
    let after_length = after_tag + length_bytes;
    if payload > maximum - after_length {
        return None;
    }
    Some(after_length + payload)
}

pub(super) fn field_tag(field: u32, wire: u8) -> Option<u64> {
    if field == 0 || field > 0x1fff_ffff || wire > 5 || wire == 3 || wire == 4 {
        return None;
    }
    Some((u64::from(field) << 3) | u64::from(wire))
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
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(position)?;
        position = position.checked_add(1)?;
        let payload = u64::from(byte & 0x7f);
        if shift == 63 && payload > 1 {
            return None;
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            return Some((value, position));
        }
    }
    None
}
