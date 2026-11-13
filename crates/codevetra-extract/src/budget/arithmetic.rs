//! Checked reservation formulas, independent of parsing and allocation.

pub(super) fn scaled_reservation(source_bytes: u64, multiplier: u64, minimum: u64) -> Option<u64> {
    match source_bytes.checked_mul(multiplier) {
        Some(bytes) => bytes.checked_add(minimum),
        None => None,
    }
}

/// Conservative in-flight reservation for source, Tree-sitter, and bounded Rust facts.
#[must_use]
pub fn native_extraction_reservation(source_bytes: u64) -> Option<u64> {
    scaled_reservation(source_bytes, 64, 1024 * 1024)
}

/// Conservative in-flight reservation for streamed UTF-8 validation and hashing.
#[must_use]
pub fn native_read_reservation(source_bytes: u64) -> Option<u64> {
    scaled_reservation(source_bytes, 2, 128 * 1024)
}

/// Hard per-file limit for modeled Rust-owned extraction output.
#[must_use]
pub fn native_output_limit(source_bytes: u64) -> Option<u64> {
    scaled_reservation(source_bytes, 32, 256 * 1024)
}
