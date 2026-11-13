//! Canonicalization used by the validated source-path type.

pub(super) fn drive_prefix(first: u8, second: u8) -> bool {
    first.is_ascii_alphabetic() && second == b':'
}

pub(super) fn normalize_path(raw: &str) -> Option<String> {
    String::from_utf8(normalize_bytes(raw.as_bytes())?).ok()
}

pub(super) fn normalize_bytes(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.is_empty()
        || raw.len() > 4096
        || raw[0] == b'/'
        || raw[0] == b'\\'
        || (raw.len() >= 2 && drive_prefix(raw[0], raw[1]))
    {
        return None;
    }
    let mut canonical = Vec::new();
    canonical.try_reserve_exact(raw.len()).ok()?;
    let mut start = 0;
    for end in 0..=raw.len() {
        if end < raw.len() && raw[end] == 0 {
            return None;
        }
        if end == raw.len() || raw[end] == b'/' || raw[end] == b'\\' {
            append_component(&mut canonical, &raw[start..end])?;
            start = end + 1;
        }
    }
    if canonical.is_empty()
        || canonical.len() > 4096
        || (canonical.len() >= 2 && drive_prefix(canonical[0], canonical[1]))
    {
        None
    } else {
        Some(canonical)
    }
}

fn append_component(canonical: &mut Vec<u8>, component: &[u8]) -> Option<()> {
    if component == b".." {
        return None;
    }
    if !component.is_empty() && component != b"." {
        if !canonical.is_empty() {
            canonical.push(b'/');
        }
        canonical.extend_from_slice(component);
    }
    Some(())
}
