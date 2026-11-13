//! Bit-precise verification of the production MCP frame admission function.

#[path = "../../crates/codevetra-mcp/src/byte_budget.rs"]
mod byte_budget;

#[kani::proof]
fn admission_is_exact_and_cannot_overflow() {
    let current: usize = kani::any();
    let additional: usize = kani::any();
    let maximum: usize = kani::any();
    let mathematical_sum = current as u128 + additional as u128;
    let result = byte_budget::admit_bytes(current, additional, maximum);
    match result {
        Some(total) => {
            assert_eq!(total as u128, mathematical_sum);
            assert!(total <= maximum);
        }
        None => assert!(mathematical_sum > maximum as u128),
    }
    kani::cover!(result.is_some());
    kani::cover!(result.is_none());
}
