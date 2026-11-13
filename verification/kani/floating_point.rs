//! Diagnostic lemmas for the unresolved normalization result. These are not
//! substitutes for the production-bound normalization harness.

#[path = "../../crates/codevetra-llm/src/rerank/score.rs"]
mod scores;

#[kani::proof]
fn widening_preserves_finite_order() {
    let (low, high): (f32, f32) = kani::any();
    kani::assume(low.is_finite() && high.is_finite() && low < high);
    let (wide_low, wide_high) = (f64::from(low), f64::from(high));
    assert!(wide_low.is_finite() && wide_high.is_finite() && wide_low < wide_high);
}

#[kani::proof]
fn widened_differences_are_positive_and_ordered() {
    let (low, middle, high): (f32, f32, f32) = kani::any();
    kani::assume(low.is_finite() && middle.is_finite() && high.is_finite());
    kani::assume(low < middle && middle < high);
    let numerator = f64::from(middle) - f64::from(low);
    let denominator = f64::from(high) - f64::from(low);
    assert!(numerator.is_finite() && denominator.is_finite());
    assert!(numerator > 0.0 && denominator > 0.0 && numerator <= denominator);
}

#[kani::proof]
fn positive_ordered_division_stays_in_unit_interval() {
    let (numerator, denominator): (f64, f64) = kani::any();
    kani::assume(numerator.is_finite() && denominator.is_finite());
    kani::assume(numerator >= 0.0 && denominator > 0.0 && numerator <= denominator);
    let quotient = numerator / denominator;
    assert!(quotient.is_finite() && quotient >= 0.0 && quotient <= 1.0);
}

#[kani::proof]
fn widened_difference_is_positive_and_finite() {
    let (low, high): (f32, f32) = kani::any();
    kani::assume(low.is_finite() && high.is_finite() && low < high);
    let difference = f64::from(high) - f64::from(low);
    assert!(difference.is_finite() && difference > 0.0);
}

#[kani::proof]
fn widened_subtraction_is_monotone() {
    let (low, middle, high): (f32, f32, f32) = kani::any();
    kani::assume(low.is_finite() && middle.is_finite() && high.is_finite());
    kani::assume(low < middle && middle < high);
    assert!(f64::from(middle) - f64::from(low) <= f64::from(high) - f64::from(low));
}

#[kani::proof]
fn binary64_rounding_carry_reproducer() {
    let tiny: f32 = kani::any();
    kani::assume(tiny.to_bits() == 0x2480_0000); // 2^-54, exactly representable in f32.
    // 1 - 2^-54 is halfway between the two adjacent binary64 values below/at
    // one. Round-to-nearest, ties-to-even must produce exactly one.
    assert_eq!(1.0_f64 - f64::from(tiny), 1.0);
}

#[kani::proof]
fn production_rounding_carry_reproducer() {
    let tiny: f32 = kani::any();
    kani::assume(tiny.to_bits() == 0x2480_0000);
    let value = scores::normalize_score(0.5, tiny, 1.0);
    assert!(value.is_some());
    if let Some(value) = value {
        assert!(value.is_finite() && value >= 0.0 && value <= 1.0);
    }
}
