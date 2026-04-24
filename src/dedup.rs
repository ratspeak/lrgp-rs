//! Per-session envelope replay-dedup cache.
//!
//! LRGP envelopes carry an optional 8-byte `n` nonce (see [`crate::envelope`]).
//! The receiver keeps a bounded, TTL'd cache of recently-seen
//! `(session_id, nonce)` pairs. If an inbound envelope's nonce is already in
//! the cache for that session, it is treated as a retransmit and dropped;
//! otherwise the nonce is recorded and the envelope is dispatched normally.
//!
//! See `rs/docs/lrgp-nonce-design.md` for the cross-implementation contract.
//! Mirrors the Python `lrgp.dedup.ReplayDedup` class.
//!
//! Design constraints:
//!
//! * Keyed by session id so cross-session reuse of a nonce value (negligible
//!   but free to isolate) cannot cause a false reject.
//! * LRU bound prevents unbounded growth inside a single long-running session.
//! * TTL bound makes the cache forget nonces older than any realistic round
//!   trip, which limits memory for sessions that never reach terminal state.
//! * Caller is responsible for [`ReplayDedup::drop_session`] on session close;
//!   the cache does not inspect session state on its own.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::constants::{DEDUP_CACHE_PER_SESSION, DEDUP_TTL_SECONDS, KEY_NONCE, KEY_SESSION};
use crate::envelope::Envelope;

/// Verdict returned by [`ReplayDedup::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupVerdict {
    /// First time we've seen this envelope; dispatch it.
    Fresh,
    /// Duplicate of a prior arrival; caller should drop it silently.
    Replay,
    /// No nonce in the envelope (legacy pre-nonce peer). Dispatch it, but
    /// the caller may want to log the peer so stale-fleet sightings are
    /// observable.
    LegacyNoNonce,
}

/// Per-session bounded LRU of `(session_id, nonce)` → last-seen time.
pub struct ReplayDedup {
    max_per_session: usize,
    ttl: Duration,
    by_session: HashMap<String, SessionCache>,
}

struct SessionCache {
    // (nonce, last_seen). Kept in most-recent-at-end order so eviction pops
    // the front. Lookup is linear in N, which is bounded by max_per_session
    // (512 by default) — cheap.
    entries: Vec<(Vec<u8>, Instant)>,
}

impl SessionCache {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    fn prune_expired(&mut self, now: Instant, ttl: Duration) {
        let cutoff = now.checked_sub(ttl);
        if let Some(cutoff) = cutoff {
            self.entries.retain(|(_, seen_at)| *seen_at >= cutoff);
        }
    }

    fn position(&self, nonce: &[u8]) -> Option<usize> {
        self.entries.iter().position(|(n, _)| n.as_slice() == nonce)
    }
}

impl ReplayDedup {
    /// Build a cache sized per the protocol defaults
    /// (`DEDUP_CACHE_PER_SESSION` entries, `DEDUP_TTL_SECONDS` TTL).
    pub fn new() -> Self {
        Self::with_bounds(DEDUP_CACHE_PER_SESSION, DEDUP_TTL_SECONDS)
    }

    pub fn with_bounds(max_per_session: usize, ttl_seconds: u64) -> Self {
        Self {
            max_per_session,
            ttl: Duration::from_secs(ttl_seconds),
            by_session: HashMap::new(),
        }
    }

    /// Decide whether `envelope` is a replay.
    ///
    /// Returns [`DedupVerdict::Replay`] if this envelope's `(session_id,
    /// nonce)` pair was already seen (caller should drop it). Otherwise
    /// the nonce is recorded and [`DedupVerdict::Fresh`] is returned.
    /// Envelopes without a `KEY_NONCE` field return [`DedupVerdict::LegacyNoNonce`]
    /// without touching the cache.
    pub fn check(&mut self, envelope: &Envelope) -> DedupVerdict {
        self.check_at(envelope, Instant::now())
    }

    /// [`check`](Self::check) with an injected clock for deterministic tests.
    pub fn check_at(&mut self, envelope: &Envelope, now: Instant) -> DedupVerdict {
        let nonce = match envelope.get(KEY_NONCE) {
            Some(rmpv::Value::Binary(b)) if b.len() == crate::constants::NONCE_BYTES => b.clone(),
            Some(_) => return DedupVerdict::LegacyNoNonce, // malformed — envelope::unpack should have caught this
            None => return DedupVerdict::LegacyNoNonce,
        };
        let session_id = match envelope.get(KEY_SESSION) {
            Some(rmpv::Value::String(s)) => match s.as_str() {
                Some(s) => s.to_string(),
                None => return DedupVerdict::LegacyNoNonce,
            },
            _ => return DedupVerdict::LegacyNoNonce,
        };

        let cache = self
            .by_session
            .entry(session_id)
            .or_insert_with(SessionCache::new);
        cache.prune_expired(now, self.ttl);

        if let Some(pos) = cache.position(&nonce) {
            // Refresh recency so an active duplicate burst doesn't evict
            // the canonical entry mid-stream.
            let entry = cache.entries.remove(pos);
            cache.entries.push((entry.0, now));
            return DedupVerdict::Replay;
        }

        cache.entries.push((nonce, now));
        // Trim from the front until we're within the cap.
        while cache.entries.len() > self.max_per_session {
            cache.entries.remove(0);
        }
        DedupVerdict::Fresh
    }

    /// Forget every nonce for `session_id`. Called on session terminal states
    /// so a long-lived node doesn't accumulate dead session caches forever.
    pub fn drop_session(&mut self, session_id: &str) {
        self.by_session.remove(session_id);
    }
}

impl Default for ReplayDedup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::pack_envelope;

    fn env(session: &str, nonce: [u8; 8]) -> Envelope {
        pack_envelope("ttt", 1, "move", session, None, Some(nonce))
    }

    #[test]
    fn fresh_nonce_is_not_a_replay() {
        let mut d = ReplayDedup::new();
        assert_eq!(d.check(&env("s1", [0; 8])), DedupVerdict::Fresh);
    }

    #[test]
    fn same_nonce_same_session_is_replay() {
        let mut d = ReplayDedup::new();
        let e = env("s1", [0x11; 8]);
        assert_eq!(d.check(&e), DedupVerdict::Fresh);
        assert_eq!(d.check(&e), DedupVerdict::Replay);
        assert_eq!(d.check(&e), DedupVerdict::Replay);
    }

    #[test]
    fn same_nonce_different_session_is_fresh() {
        let mut d = ReplayDedup::new();
        let n = [0x22; 8];
        assert_eq!(d.check(&env("s1", n)), DedupVerdict::Fresh);
        assert_eq!(d.check(&env("s2", n)), DedupVerdict::Fresh);
    }

    #[test]
    fn different_nonce_same_session_is_fresh() {
        let mut d = ReplayDedup::new();
        assert_eq!(d.check(&env("s1", [0x33; 8])), DedupVerdict::Fresh);
        assert_eq!(d.check(&env("s1", [0x44; 8])), DedupVerdict::Fresh);
    }

    #[test]
    fn legacy_envelope_without_nonce_not_replay() {
        use crate::envelope::{Envelope, value_from_map};
        let _ = value_from_map;
        let mut d = ReplayDedup::new();
        let mut legacy = Envelope::new();
        legacy.insert(
            crate::constants::KEY_APP.into(),
            rmpv::Value::String("ttt.1".into()),
        );
        legacy.insert(
            crate::constants::KEY_COMMAND.into(),
            rmpv::Value::String("move".into()),
        );
        legacy.insert(
            crate::constants::KEY_SESSION.into(),
            rmpv::Value::String("s1".into()),
        );
        legacy.insert(
            crate::constants::KEY_PAYLOAD.into(),
            rmpv::Value::Map(vec![]),
        );
        assert_eq!(d.check(&legacy), DedupVerdict::LegacyNoNonce);
        assert_eq!(d.check(&legacy), DedupVerdict::LegacyNoNonce);
    }

    #[test]
    fn oldest_nonce_evicted_once_cap_exceeded() {
        let mut d = ReplayDedup::with_bounds(4, 3600);
        for i in 0..4 {
            let mut n = [0u8; 8];
            n[0] = i;
            assert_eq!(d.check(&env("s1", n)), DedupVerdict::Fresh);
        }
        assert_eq!(d.check(&env("s1", [9, 0, 0, 0, 0, 0, 0, 0])), DedupVerdict::Fresh);
        // Nonce 0 was the oldest; it should have been evicted and now be
        // treated as fresh on retransmit.
        assert_eq!(d.check(&env("s1", [0; 8])), DedupVerdict::Fresh);
        // Nonce 3 is still in the cache — replay.
        assert_eq!(
            d.check(&env("s1", [3, 0, 0, 0, 0, 0, 0, 0])),
            DedupVerdict::Replay
        );
    }

    #[test]
    fn drop_session_clears_cache() {
        let mut d = ReplayDedup::new();
        let n = [0x55; 8];
        assert_eq!(d.check(&env("s1", n)), DedupVerdict::Fresh);
        d.drop_session("s1");
        assert_eq!(d.check(&env("s1", n)), DedupVerdict::Fresh);
    }

    #[test]
    fn drop_session_unknown_is_a_noop() {
        let mut d = ReplayDedup::new();
        d.drop_session("never-seen");
    }
}
