//! Bounded byte/string proofs. Bounds are explicit and unwinding assertions stay enabled.
#![allow(dead_code)]

#[path = "../../crates/codevetra-db/src/rocks/key/encoding.rs"]
mod keys;
#[path = "../../crates/codevetra-domain/src/source/path.rs"]
mod paths;

#[kani::proof]
fn windows_drive_prefix() {
    let (first, second): (u8, u8) = kani::any();
    assert_eq!(
        paths::drive_prefix(first, second),
        ((65..=90).contains(&first) || (97..=122).contains(&first)) && second == 58
    );
}

#[kani::proof]
#[kani::unwind(11)]
fn canonical_path_is_relative_and_idempotent() {
    let bytes: [u8; 8] = kani::any();
    let length: usize = kani::any();
    kani::assume(length <= bytes.len());
    let raw = &bytes[..length];
    let result = paths::normalize_bytes(raw);
    if let Some(path) = result.as_ref() {
        assert!(!path.is_empty() && path.len() <= raw.len());
        assert!(path[0] != b'/' && path[0] != b'\\');
        assert!(!path.contains(&0) && !path.contains(&b'\\'));
        for component in path.split(|byte| *byte == b'/') {
            assert!(!component.is_empty() && component != b"." && component != b"..");
        }
        assert_eq!(paths::normalize_bytes(path).as_ref(), Some(path));
    }
    kani::cover!(result.is_some());
    kani::cover!(result.is_none());
}

#[kani::proof]
#[kani::unwind(11)]
fn storage_key_round_trip() {
    let bytes: [u8; 4] = kani::any();
    let length: usize = kani::any();
    kani::assume(length > 0 && length <= 4);
    let sequence: u64 = kani::any();
    let key = &bytes[..length];
    let escaped = keys::escape(key);
    assert!(escaped.len() >= length && escaped.len() <= length * 2);
    // Fixed proof-only assembly avoids modelling Vec growth for the framing.
    // The production encoder and decoder, input domain and assertions remain.
    let mut encoded = [0_u8; 18];
    encoded[..escaped.len()].copy_from_slice(&escaped);
    let end = escaped.len() + 10;
    encoded[escaped.len() + 2..end].copy_from_slice(&(!sequence).to_be_bytes());
    let decoded = keys::decode(&encoded[..end]);
    assert!(decoded.is_some());
    if let Some((decoded_key, decoded_sequence)) = decoded {
        assert_eq!(decoded_key.as_slice(), key);
        assert_eq!(decoded_sequence, sequence);
    }
}

#[kani::proof]
#[kani::unwind(13)]
fn storage_decoder_rejects_malformed_keys_safely() {
    let bytes: [u8; 12] = kani::any();
    let length: usize = kani::any();
    kani::assume(length <= 12);
    if let Some((decoded, sequence)) = keys::decode(&bytes[..length]) {
        assert!(!decoded.is_empty());
        let mut encoded = keys::escape(&decoded);
        encoded.extend_from_slice(&[0, 0]);
        encoded.extend_from_slice(&(!sequence).to_be_bytes());
        assert_eq!(encoded, bytes[..length]);
    }
}

#[kani::proof]
#[kani::unwind(9)]
fn storage_prefix_successor() {
    let bytes: [u8; 4] = kani::any();
    let length: usize = kani::any();
    kani::assume(length <= 4);
    let prefix = &bytes[..length];
    let result = keys::successor(prefix.to_vec());
    let all_maximum = prefix.iter().all(|byte| *byte == 255);
    assert_eq!(result.is_none(), all_maximum);
    if let Some(next) = result {
        assert!(!next.is_empty() && next.len() <= prefix.len());
        assert!(next.as_slice() > prefix);
        let suffix: [u8; 4] = kani::any();
        let mut extended = prefix.to_vec();
        extended.extend_from_slice(&suffix);
        assert!(extended < next);
    }
}
