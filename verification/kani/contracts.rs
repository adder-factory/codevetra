//! Specifications are deliberately independent of the checked implementation formulas.
#![allow(dead_code)]

#[path = "../../crates/codevetra-config/src/numeric/predicate.rs"]
mod config;
#[path = "../../crates/codevetra-domain/src/source/coordinates.rs"]
mod coordinates;
#[path = "../../crates/codevetra-extract/src/budget/arithmetic.rs"]
mod extraction;
#[path = "../../crates/codevetra-db/src/rocks/engine/lifecycle.rs"]
mod lifecycle;
#[path = "../../crates/codevetra-db/src/native_database/retention/policy.rs"]
mod orphans;
#[path = "../../crates/codevetra-search/src/hybrid/rank.rs"]
mod ranks;
#[path = "../../crates/codevetra-llm/src/rerank/score.rs"]
mod scores;
#[path = "../../crates/codevetra-db/src/rocks/arithmetic.rs"]
mod storage;
#[path = "../../crates/codevetra-indexer/src/task_scope/admission.rs"]
mod tasks;
#[path = "../../crates/codevetra-cli/src/mcp_budget/arithmetic.rs"]
mod tokens;
#[path = "../../crates/codevetra-agent/src/source_context/window.rs"]
mod windows;
#[path = "../../crates/codevetra-scip/src/codec/wire.rs"]
mod wire;

#[kani::proof]
fn source_coordinates() {
    let (a, b): (u64, u64) = kani::any();
    let (al, bl, ac, bc): (u32, u32, u32, u32) = kani::any();
    assert_eq!(coordinates::valid_position(al), al > 0);
    let valid = coordinates::valid_span(a, b, al, bl, ac, bc);
    assert_eq!(
        valid,
        al > 0 && a < b && (al < bl || (al == bl && ac <= bc))
    );
    kani::cover!(valid);
    kani::cover!(!valid);
}

#[kani::proof]
fn configuration_integer() {
    let (value, maximum): (u64, u64) = kani::any();
    assert_eq!(
        config::positive_bounded(value, maximum),
        value >= 1 && value <= maximum
    );
}

#[kani::proof]
fn extraction_reservations() {
    let n: u64 = kani::any();
    let expected = (n as u128) * 64 + 1_048_576;
    assert_eq!(
        extraction::native_extraction_reservation(n).map(u128::from),
        (expected <= u64::MAX as u128).then_some(expected)
    );
    let expected = (n as u128) * 2 + 131_072;
    assert_eq!(
        extraction::native_read_reservation(n).map(u128::from),
        (expected <= u64::MAX as u128).then_some(expected)
    );
    let expected = (n as u128) * 32 + 262_144;
    assert_eq!(
        extraction::native_output_limit(n).map(u128::from),
        (expected <= u64::MAX as u128).then_some(expected)
    );
}

#[kani::proof]
fn storage_reservations() {
    let (a, b): (usize, usize) = kani::any();
    let expected = a as u128 * 2 + b as u128 + 4096;
    assert_eq!(
        storage::worker_reservation(a, b).map(u128::from),
        (expected <= u32::MAX as u128).then_some(expected)
    );
    let expected = a as u128 * 5 + b as u128 * 3 + 128;
    assert_eq!(
        storage::change_reservation(a, b).map(|n| n as u128),
        (expected <= usize::MAX as u128).then_some(expected)
    );
}

#[kani::proof]
fn retention_budget() {
    let (batch, counters, key, maximum): (usize, usize, usize, usize) = kani::any();
    let total = batch as u128 + counters as u128 * 96 + 131_072 + key as u128 * 2 + 256;
    assert_eq!(
        storage::retention_batch_fits(batch, counters, key, maximum),
        total <= maximum as u128
    );
}

#[kani::proof]
fn storage_policy() {
    let (cache, memtable, batch, rows): (usize, usize, usize, usize) = kani::any();
    assert_eq!(
        storage::valid_store_policy(cache, memtable, batch, rows),
        cache >= 8_388_608
            && cache <= 4_294_967_296
            && memtable >= 4_194_304
            && memtable <= cache
            && batch >= 1_048_576
            && batch <= 67_108_864
            && rows > 0
            && rows <= 100_000
    );
}

#[kani::proof]
#[kani::unwind(33)]
fn writer_fences() {
    let (active, requested): (u64, u64) = kani::any();
    let a: [u8; 32] = kani::any();
    let b: [u8; 32] = kani::any();
    let unexpired: bool = kani::any();
    let owned = lifecycle::owns_writer(active, &a, requested, &b);
    let mut bytes_match = true;
    for i in 0..32 {
        bytes_match &= a[i] == b[i];
    }
    assert_eq!(owned, active == requested && bytes_match);
    assert_eq!(
        lifecycle::can_mutate_writer(owned, unexpired),
        active == requested && bytes_match && unexpired
    );
    kani::cover!(owned && unexpired);
    kani::cover!(!owned);
}

#[kani::proof]
fn publication_base() {
    let current: Option<u64> = kani::any();
    let base: Option<u64> = kani::any();
    assert_eq!(
        lifecycle::same_base(current, base),
        match (current, base) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    );
}

#[kani::proof]
fn retention_decisions() {
    let (sequence, floor): (u64, u64) = kani::any();
    let (retained, keep): (u8, u8) = kani::any();
    let (published, anchor): (bool, bool) = kani::any();
    assert_eq!(
        lifecycle::keep_superseded(sequence, floor, retained, keep),
        !(sequence < floor || retained >= keep)
    );
    let remove = lifecycle::remove_version(published, sequence, floor, anchor);
    assert_eq!(!remove, published && (sequence >= floor || !anchor));
    assert!(!published || sequence < floor || !remove);
}

#[kani::proof]
fn orphan_grace() {
    let (now, first): (i64, i64) = kani::any();
    assert_eq!(
        orphans::grace_elapsed(now, first),
        now as i128 - first as i128 >= 600
    );
}

#[kani::proof]
fn task_byte_admission() {
    let (current, additional, maximum): (u64, u64, u64) = kani::any();
    let sum = current as u128 + additional as u128;
    let result = tasks::reserve(current, additional, maximum);
    assert_eq!(
        result.map(u128::from),
        (additional > 0 && sum <= maximum as u128).then_some(sum)
    );
    kani::cover!(result.is_some());
    kani::cover!(result.is_none());
}

#[kani::proof]
fn source_windows() {
    let (start, length, total): (usize, usize, usize) = kani::any();
    let result = windows::byte_end(start, length, total);
    assert_eq!(
        result.map(|n| n as u128),
        (start <= total).then_some((start as u128 + length as u128).min(total as u128))
    );
    let (line, context): (u32, u32) = kani::any();
    assert_eq!(
        windows::context_start(line, context) as i64,
        (line as i64 - context as i64).max(1)
    );
    assert_eq!(
        windows::context_end(line, context) as u64,
        (line as u64 + context as u64).min(u32::MAX as u64)
    );
}

#[kani::proof]
fn token_rounding() {
    let chars: usize = kani::any();
    assert_eq!(
        tokens::estimate_tokens(chars) as u128,
        (chars as u128 + 3) / 4
    );
}

#[kani::proof]
fn protobuf_admission() {
    let (current, tag, length, payload, maximum): (usize, usize, usize, usize, usize) = kani::any();
    let total = current as u128 + tag as u128 + length as u128 + payload as u128;
    assert_eq!(
        wire::delimited_end(current, tag, length, payload, maximum).map(|n| n as u128),
        (total <= maximum as u128).then_some(total)
    );
    let end = current as u128 + payload as u128;
    assert_eq!(
        wire::cursor_end(current, payload, maximum).map(|n| n as u128),
        (end <= maximum as u128).then_some(end)
    );
}

#[kani::proof]
#[kani::unwind(11)]
fn protobuf_varint_length() {
    let value: u64 = kani::any();
    let length = wire::varint_length(value);
    assert!(length >= 1 && length <= 10);
    assert!((value as u128) < (1_u128 << (7 * length)));
    assert!(length == 1 || (value as u128) >= (1_u128 << (7 * (length - 1))));
}

#[kani::proof]
fn protobuf_field_tags() {
    let (field, kind): (u32, u8) = kani::any();
    let result = wire::field_tag(field, kind);
    let valid = field >= 1 && field <= 536_870_911 && matches!(kind, 0 | 1 | 2 | 5);
    assert_eq!(result, valid.then_some(field as u64 * 8 + kind as u64));
}

#[kani::proof]
#[kani::unwind(12)]
fn protobuf_decoder() {
    let bytes: [u8; 10] = kani::any();
    let length: usize = kani::any();
    kani::assume(length <= 10);
    let start: usize = kani::any();
    let result = wire::read_varint(&bytes[..length], start);
    // Independent base-128 oracle, including every rejection case.
    let mut expected = None;
    let mut mathematical = 0_u128;
    if start < length {
        for index in start..length {
            mathematical += (bytes[index] % 128) as u128 * (1_u128 << (7 * (index - start)));
            if bytes[index] < 128 {
                if mathematical <= u64::MAX as u128 {
                    expected = Some((mathematical as u64, index + 1));
                }
                break;
            }
        }
    }
    assert_eq!(result, expected);
    if let Some((value, end)) = result {
        assert!(start < end && end <= length && end - start <= 10);
        assert!(bytes[end - 1] < 128);
        let mut sum = 0_u128;
        for i in start..end {
            assert!(i == end - 1 || bytes[i] >= 128);
            sum += (bytes[i] % 128) as u128 * (1_u128 << (7 * (i - start)));
        }
        assert_eq!(sum, value as u128);
    }
    kani::cover!(result.is_some());
    kani::cover!(result.is_none());
}

#[kani::proof]
fn retrieval_rank_bounds() {
    let (rank, maximum): (u16, u16) = kani::any();
    assert_eq!(
        ranks::valid_rank(rank, maximum),
        rank >= 1 && rank <= maximum
    );
    let score = ranks::reciprocal_rank(rank);
    assert!(score.is_finite() && score > 0.0 && score <= 1.0 / 60.0);
}

#[kani::proof]
fn finite_score_normalization() {
    let (score, minimum, maximum): (f32, f32, f32) = kani::any();
    let result = scores::normalize_score(score, minimum, maximum);
    let valid = score.is_finite()
        && minimum.is_finite()
        && maximum.is_finite()
        && minimum <= score
        && score <= maximum;
    assert_eq!(result.is_some(), valid);
    if let Some(value) = result {
        assert!(value.is_finite() && value >= 0.0 && value <= 1.0);
        assert!(minimum != maximum || value == 1.0);
        assert!(score != maximum || value == 1.0);
        assert!(score != minimum || minimum == maximum || value == 0.0);
    }
    kani::cover!(result.is_some());
    kani::cover!(result.is_none());
}

// This exhaustive partition keeps rejection and endpoint cases independent
// from the expensive IEEE-754 division query. Together the three cases have
// exactly the same input domain and assertions as finite_score_normalization.
#[kani::proof]
fn finite_score_rejection() {
    let (score, minimum, maximum): (f32, f32, f32) = kani::any();
    let valid = score.is_finite()
        && minimum.is_finite()
        && maximum.is_finite()
        && minimum <= score
        && score <= maximum;
    kani::assume(!valid);
    assert_eq!(scores::normalize_score(score, minimum, maximum), None);
}

#[kani::proof]
fn finite_score_endpoints() {
    let (score, minimum, maximum): (f32, f32, f32) = kani::any();
    kani::assume(
        score.is_finite()
            && minimum.is_finite()
            && maximum.is_finite()
            && minimum <= score
            && score <= maximum,
    );
    kani::assume(score == minimum || score == maximum);
    let expected = if score == maximum { 1.0 } else { 0.0 };
    assert_eq!(
        scores::normalize_score(score, minimum, maximum),
        Some(expected)
    );
}

#[kani::proof]
fn finite_score_interior() {
    let (score, minimum, maximum): (f32, f32, f32) = kani::any();
    kani::assume(
        score.is_finite()
            && minimum.is_finite()
            && maximum.is_finite()
            && minimum < score
            && score < maximum,
    );
    let result = scores::normalize_score(score, minimum, maximum);
    assert!(result.is_some());
    if let Some(value) = result {
        assert!(value.is_finite() && value >= 0.0 && value <= 1.0);
    }
}
