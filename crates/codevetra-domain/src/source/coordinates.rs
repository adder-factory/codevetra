//! Pure coordinate rules shared by construction and formal verification.

pub(super) const fn valid_position(line: u32) -> bool {
    line != 0
}

pub(super) const fn valid_span(
    start_byte: u64,
    end_byte: u64,
    start_line: u32,
    end_line: u32,
    start_column: u32,
    end_column: u32,
) -> bool {
    start_line != 0
        && start_byte < end_byte
        && start_line <= end_line
        && (start_line != end_line || start_column <= end_column)
}
