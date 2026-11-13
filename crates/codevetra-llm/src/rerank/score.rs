//! Finite score arithmetic before conversion to the public f32 score type.

pub(super) fn normalize_score(score: f32, minimum: f32, maximum: f32) -> Option<f64> {
    if !score.is_finite()
        || !minimum.is_finite()
        || !maximum.is_finite()
        || score < minimum
        || score > maximum
    {
        return None;
    }
    // The range guard makes these endpoint comparisons exact, including
    // signed zero; approximate equality would collapse distinct tiny scores.
    if score >= maximum {
        return Some(1.0);
    }
    if score <= minimum {
        return Some(0.0);
    }
    // Every finite f32 difference fits in f64, including opposite extremes.
    Some((f64::from(score) - f64::from(minimum)) / (f64::from(maximum) - f64::from(minimum)))
}
