//! Canonicalization used by the validated source-path type.

const MAXIMUM_PATH_BYTES: usize = 4096;

pub(super) fn drive_prefix(first: u8, second: u8) -> bool {
    first.is_ascii_alphabetic() && second == b':'
}

pub(super) fn normalize_path(raw: &str) -> Option<String> {
    String::from_utf8(normalize_bytes(raw.as_bytes())?).ok()
}

pub(super) fn normalize_bytes(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.is_empty()
        || raw.len() > MAXIMUM_PATH_BYTES
        || matches!(raw[0], b'/' | b'\\')
        || has_drive_prefix(raw)
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
    if canonical.is_empty() || canonical.len() > MAXIMUM_PATH_BYTES || has_drive_prefix(&canonical)
    {
        None
    } else {
        Some(canonical)
    }
}

fn has_drive_prefix(path: &[u8]) -> bool {
    path.get(..2)
        .is_some_and(|prefix| drive_prefix(prefix[0], prefix[1]))
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
