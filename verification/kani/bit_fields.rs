//! Exhaustive bit-field properties; inputs retain their complete machine domains.
#![allow(dead_code)]

#[path = "../../crates/codevetra-db/src/rocks/graph/codec/arithmetic.rs"]
mod graph;
#[path = "../../crates/codevetra-mcp/src/profile/bits.rs"]
mod profiles;

#[kani::proof]
fn graph_prefix_geometry() {
    let (depth, bytes): (usize, usize) = kani::any();
    let result = graph::radix_geometry(depth, bytes);
    assert_eq!(result.is_some(), depth as u128 <= bytes as u128 * 8);
    if let Some((whole, remainder)) = result {
        assert!(remainder < 8 && whole <= bytes);
        assert!(remainder == 0 || whole < bytes);
        assert_eq!(whole as u128 * 8 + remainder as u128, depth as u128);
    }
}

#[kani::proof]
fn graph_prefix_bits() {
    let byte: u8 = kani::any();
    let bits: usize = kani::any();
    let result = graph::prefix_byte(byte, bits);
    assert_eq!(result.is_some(), bits <= 8);
    if let Some(prefix) = result {
        let divisor = 1_u16 << (8 - bits);
        assert_eq!(prefix as u16, (byte as u16 / divisor) * divisor);
    }
}

#[kani::proof]
fn graph_hex_digits() {
    let byte: u8 = kani::any();
    let value = graph::nibble(byte);
    let alphabet = b"0123456789abcdef";
    if let Some(nibble) = value {
        assert!(nibble < 16);
        assert_eq!(alphabet[nibble as usize], byte);
    }
    assert_eq!(
        value.is_some(),
        (48..=57).contains(&byte) || (97..=102).contains(&byte)
    );
}

#[kani::proof]
#[kani::unwind(9)]
fn explicit_profile_membership() {
    let (a, b): (u8, u8) = kani::any();
    let combined = profiles::union(a, b);
    for index in 0..8 {
        let profile = 1_u8 << index;
        assert_eq!(
            profiles::includes(combined, profile),
            profiles::includes(a, profile) || profiles::includes(b, profile)
        );
        assert_eq!(profiles::includes(a, profile), (a / profile) % 2 == 1);
    }
    assert_eq!(profiles::union(a, 0), a);
    assert_eq!(profiles::union(a, a), a);
}
