//! Pure coordinate rules shared by construction and formal verification.

use super::SourcePosition;

pub(super) const fn valid_position(line: u32) -> bool {
    line != 0
}

pub(super) const fn valid_span(start: &SourcePosition, end: &SourcePosition) -> bool {
    start.line != 0
        && start.byte < end.byte
        && start.line <= end.line
        && (start.line != end.line || start.column <= end.column)
}
