//! Record/session identity: ULIDs.
//!
//! ULID over content hash by design: the time-ordered ID is the stable anchor
//! for mutable metadata (Perkeep's "content hash ≠ object identity"); content
//! hashes live in the artifact list for integrity only.
//!
//! A record's ULID is generated when its capture's *first frame* arrives, so
//! the embedded time bits approximate the trigger time. The migration tool
//! uses [`UlidGen::next_at_ms`] with the legacy file's mtime — the filename
//! `t0_ns` is CLOCK_MONOTONIC and must NOT be used as a ULID timestamp.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Monotonic ULID generator (one per producer; not thread-safe by design —
/// the sink owns one, the migration tool owns one).
pub struct UlidGen {
    inner: ulid::Generator,
    /// Last millisecond handed to the generator. `ulid::Generator` silently
    /// *clamps* a request earlier than its previous one to keep monotonic
    /// order — which would falsify the time bits. We detect that case and
    /// bypass the generator instead (see [`UlidGen::next_at_ms`]).
    last_ms: u64,
}

impl UlidGen {
    pub fn new() -> Self {
        UlidGen { inner: ulid::Generator::new(), last_ms: 0 }
    }

    /// ULID for "now". Monotonic within this generator while the clock does
    /// not step backwards; on a backwards step the time bits stay faithful
    /// to the requested instant (see `next_at_ms`).
    pub fn next_now(&mut self) -> String {
        self.next_at(SystemTime::now())
    }

    /// ULID with explicit unix-epoch-millisecond time bits (migration path).
    ///
    /// Time-bit fidelity beats cross-call monotonicity: for an out-of-order
    /// request (unsorted migration input, clock step) the generator is
    /// bypassed and a fresh random ULID is built at the requested time —
    /// sort inputs by mtime if strict output ordering matters. A record's
    /// ULID is permanent; silently wrong time bits would be unfixable.
    pub fn next_at_ms(&mut self, unix_ms: u64) -> String {
        self.next_at(UNIX_EPOCH + Duration::from_millis(unix_ms))
    }

    fn next_at(&mut self, t: SystemTime) -> String {
        let ms = t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        if ms < self.last_ms {
            // ulid::Generator would clamp this to last_ms; keep the time
            // bits honest instead (uniqueness still holds: 80 random bits).
            return ulid::Ulid::from_datetime(t).to_string();
        }
        // MonotonicError means the 80-bit random space overflowed within one
        // millisecond — practically unreachable, but never panic on an ID
        // path: step forward 1 ms and retry.
        let mut t = t;
        loop {
            match self.inner.generate_from_datetime(t) {
                Ok(id) => {
                    self.last_ms = id.timestamp_ms();
                    return id.to_string();
                }
                Err(_) => t += Duration::from_millis(1),
            }
        }
    }
}

impl Default for UlidGen {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical ULID check: the string must decode AND re-encode to itself.
/// The round-trip rejects wrong length, lowercase, and — crucially —
/// overflowed encodings (26 chars carry 130 bits; the crate silently drops
/// the top 2, so e.g. `ZZZZ…` decodes but re-encodes differently, which
/// would give one record two different ids across parse/re-encode paths).
pub fn is_valid_ulid(s: &str) -> bool {
    ulid::Ulid::from_string(s).map(|u| u.to_string() == s).unwrap_or(false)
}

/// Shard bucket for the records tree: the first two ULID characters
/// (coarse-time buckets, since the leading bits encode the timestamp).
pub fn ulid_shard(ulid: &str) -> &str {
    &ulid[..2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_and_strictly_increasing_ids() {
        let mut g = UlidGen::new();
        let mut prev = String::new();
        for _ in 0..100 {
            let id = g.next_now();
            assert!(is_valid_ulid(&id), "{id}");
            assert!(id > prev, "{id} !> {prev}");
            prev = id;
        }
    }

    #[test]
    fn explicit_ms_sets_time_bits_and_stays_monotonic() {
        let mut g = UlidGen::new();
        let a = g.next_at_ms(1_750_000_000_000);
        let b = g.next_at_ms(1_750_000_000_000); // same ms → increment
        let c = g.next_at_ms(1_750_000_000_001);
        assert!(a < b && b < c, "{a} {b} {c}");
        let ts = ulid::Ulid::from_string(&a).unwrap().timestamp_ms();
        assert_eq!(ts, 1_750_000_000_000);
    }

    #[test]
    fn out_of_order_ms_keeps_faithful_time_bits() {
        let mut g = UlidGen::new();
        let _ = g.next_at_ms(2_000_000_000_000);
        // Earlier than the previous request: the generator would clamp the
        // time bits to 2_000_000_000_000 — we must not let it.
        let b = g.next_at_ms(1_000_000_000_000);
        assert_eq!(
            ulid::Ulid::from_string(&b).unwrap().timestamp_ms(),
            1_000_000_000_000
        );
        // And the generator path keeps working afterwards.
        let c = g.next_at_ms(2_000_000_000_001);
        assert_eq!(
            ulid::Ulid::from_string(&c).unwrap().timestamp_ms(),
            2_000_000_000_001
        );
    }

    #[test]
    fn validity_rejects_garbage() {
        assert!(!is_valid_ulid(""));
        assert!(!is_valid_ulid("not-a-ulid"));
        assert!(!is_valid_ulid("01jxab3c4d5e6f7g8h9jkmnpqr")); // lowercase
        assert!(!is_valid_ulid("01JXAB3C4D5E6F7G8H9JKMNPQ")); // 25 chars
        // Overflowed non-canonical encoding: decodes, but re-encodes as
        // "7ZZZ…" ≠ input. Must be rejected (it would double-identify a
        // record across parse/re-encode paths).
        assert!(!is_valid_ulid("ZZZZZZZZZZZZZZZZZZZZZZZZZZ"));
        assert!(is_valid_ulid("01JXAB3C4D5E6F7G8H9JKMNPQR"));
        assert!(is_valid_ulid("7ZZZZZZZZZZZZZZZZZZZZZZZZZ")); // max canonical
    }

    #[test]
    fn shard_is_first_two_chars() {
        assert_eq!(ulid_shard("01JXAB3C4D5E6F7G8H9JKMNPQR"), "01");
    }
}
