//! Integer windows before the source owner checks UTF-8 and generation identity.

pub(super) fn byte_end(start: usize, length: usize, total: usize) -> Option<usize> {
    if start > total {
        None
    } else if length > total - start {
        Some(total)
    } else {
        Some(start + length)
    }
}

pub(super) fn context_start(line: u32, context: u32) -> u32 {
    if line <= context { 1 } else { line - context }
}

pub(super) fn context_end(line: u32, context: u32) -> u32 {
    if context > u32::MAX - line {
        u32::MAX
    } else {
        line + context
    }
}
