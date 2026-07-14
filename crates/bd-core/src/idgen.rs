//! Issue identity.
//!
//! Beads ids look like `bd-a3f2dd`: a workspace prefix and a base36-encoded
//! slice of a SHA-256 over the issue's content. They are content-addressed
//! rather than sequential so that two clones working offline can both mint ids
//! without coordinating.
//!
//! The id length adapts to table size via the birthday bound, so a small
//! workspace gets short, readable ids and a large one automatically widens
//! before collisions become likely. Collision *checking* is still local-only —
//! two disconnected clones genuinely can mint the same id, and reconciling that
//! is the merge layer's problem, not ours.

use sha2::{Digest, Sha256};

pub const BASE36_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

pub const MIN_ID_LENGTH: usize = 3;
pub const MAX_ID_LENGTH: usize = 8;
/// Widen the id once the birthday-paradox collision probability for the current
/// table size would exceed this.
pub const MAX_COLLISION_PROBABILITY: f64 = 0.25;
/// Nonce attempts per length before widening.
pub const MAX_NONCE_ATTEMPTS: u32 = 10;

/// How many bytes of digest we need to fill `length` base36 characters.
fn bytes_for_length(length: usize) -> usize {
    match length {
        0..=2 => 2,
        3..=4 => 3,
        5..=6 => 4,
        _ => 5,
    }
}

/// Encode bytes as exactly `length` base36 chars, left-padding with '0' and
/// truncating from the left if the value is too wide.
fn base36_encode(bytes: &[u8], length: usize) -> String {
    let mut value: u128 = 0;
    for &b in bytes {
        value = (value << 8) | b as u128;
    }
    let mut out = Vec::with_capacity(length);
    if value == 0 {
        out.push(b'0');
    }
    while value > 0 {
        out.push(BASE36_ALPHABET[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();

    let mut s = String::from_utf8(out).expect("base36 alphabet is ascii");
    if s.len() < length {
        s = "0".repeat(length - s.len()) + &s;
    } else if s.len() > length {
        // Keep the low-order end: it has the most entropy.
        s = s[s.len() - length..].to_string();
    }
    s
}

/// Mint a candidate id. Deterministic in all its inputs — the caller varies
/// `nonce` to retry after a local collision.
pub fn generate_hash_id(
    prefix: &str,
    title: &str,
    description: &str,
    creator: &str,
    timestamp_nanos: i64,
    length: usize,
    nonce: u32,
) -> String {
    let content = format!("{title}|{description}|{creator}|{timestamp_nanos}|{nonce}");
    let digest = Sha256::digest(content.as_bytes());
    let n = bytes_for_length(length);
    let encoded = base36_encode(&digest[..n], length);
    format!("{prefix}-{encoded}")
}

/// Smallest id length whose collision probability at `count` existing issues
/// stays under [`MAX_COLLISION_PROBABILITY`].
///
/// Birthday bound: p ≈ 1 - exp(-n² / (2 · 36^len)).
pub fn compute_adaptive_length(count: u64) -> usize {
    let n = count as f64;
    for length in MIN_ID_LENGTH..=MAX_ID_LENGTH {
        let space = 36f64.powi(length as i32);
        let p = 1.0 - (-(n * n) / (2.0 * space)).exp();
        if p <= MAX_COLLISION_PROBABILITY {
            return length;
        }
    }
    MAX_ID_LENGTH
}

/// The (length, nonce) candidates to try, in order, for a workspace holding
/// `count` issues. The caller checks each against the database and takes the
/// first that isn't taken.
pub fn candidate_sequence(count: u64) -> impl Iterator<Item = (usize, u32)> {
    let base = compute_adaptive_length(count);
    (base..=MAX_ID_LENGTH).flat_map(|len| (0..MAX_NONCE_ATTEMPTS).map(move |nonce| (len, nonce)))
}

// ---------------------------------------------------------------------------
// Hierarchical child ids: bd-af78e9a2.1.2
// ---------------------------------------------------------------------------

pub const MAX_HIERARCHY_DEPTH: usize = 3;

/// `bd-abc` + child 2 => `bd-abc.2`
pub fn child_id(parent_id: &str, n: u32) -> String {
    format!("{parent_id}.{n}")
}

/// The parent of `bd-abc.1.2` is `bd-abc.1`. A root id has no parent.
pub fn parent_of(id: &str) -> Option<&str> {
    id.rsplit_once('.').map(|(head, _)| head)
}

/// Depth 0 = root (`bd-abc`), depth 1 = `bd-abc.1`, and so on.
pub fn depth(id: &str) -> usize {
    id.matches('.').count()
}

pub fn is_child_id(id: &str) -> bool {
    id.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_deterministic_in_its_inputs() {
        let a = generate_hash_id("bd", "title", "desc", "me", 1_700_000_000, 6, 0);
        let b = generate_hash_id("bd", "title", "desc", "me", 1_700_000_000, 6, 0);
        assert_eq!(a, b);
        // The nonce is what lets a caller retry past a local collision.
        let c = generate_hash_id("bd", "title", "desc", "me", 1_700_000_000, 6, 1);
        assert_ne!(a, c);
    }

    #[test]
    fn id_has_prefix_and_exact_length() {
        for len in MIN_ID_LENGTH..=MAX_ID_LENGTH {
            let id = generate_hash_id("bd", "t", "d", "c", 1, len, 0);
            let suffix = id.strip_prefix("bd-").expect("prefix");
            assert_eq!(suffix.len(), len, "id {id} wrong length");
            assert!(
                suffix.bytes().all(|b| BASE36_ALPHABET.contains(&b)),
                "id {id} not base36"
            );
        }
    }

    #[test]
    fn length_widens_as_the_table_grows() {
        // A fresh workspace gets short, readable ids...
        assert_eq!(compute_adaptive_length(0), MIN_ID_LENGTH);
        assert_eq!(compute_adaptive_length(100), MIN_ID_LENGTH);
        // ...and a big one widens before collisions get likely.
        let big = compute_adaptive_length(1_000_000);
        assert!(big > MIN_ID_LENGTH, "expected widening, got {big}");
        assert!(big <= MAX_ID_LENGTH);
        // Monotonic: more issues never yields a shorter id.
        let mut prev = 0;
        for n in [0u64, 1_000, 100_000, 10_000_000, 1_000_000_000] {
            let l = compute_adaptive_length(n);
            assert!(l >= prev, "length shrank at n={n}");
            prev = l;
        }
    }

    #[test]
    fn candidates_start_at_adaptive_length_then_widen() {
        let c: Vec<_> = candidate_sequence(0).take(12).collect();
        assert_eq!(c[0], (3, 0));
        assert_eq!(c[9], (3, 9));
        // Exhausting nonces at one length moves to the next.
        assert_eq!(c[10], (4, 0));
    }

    #[test]
    fn hierarchy_navigation() {
        assert_eq!(child_id("bd-abc", 2), "bd-abc.2");
        assert_eq!(parent_of("bd-abc.1.2"), Some("bd-abc.1"));
        assert_eq!(parent_of("bd-abc"), None);
        assert_eq!(depth("bd-abc"), 0);
        assert_eq!(depth("bd-abc.1.2"), 2);
        assert!(is_child_id("bd-abc.1"));
        assert!(!is_child_id("bd-abc"));
    }
}
