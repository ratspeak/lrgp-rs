//! Per-session envelope replay-dedup cache.
//!
//! LRGP envelopes carry a required 8-byte `n` nonce (see [`crate::envelope`]).
//! The receiver keeps a bounded, TTL'd cache of recently-seen
//! `(receiving_identity_id, session_id, nonce)` tuples. If an inbound
//! envelope's nonce is already in that namespace, it is treated as a
//! retransmit and dropped; otherwise the nonce is recorded and the envelope is
//! dispatched normally.
//!
//! The normative cross-implementation contract is the repository's `SPEC.md`.
//!
//! Design constraints:
//!
//! * Scoped by receiving identity and session so cross-identity/session nonce
//!   reuse (negligible but free to isolate) cannot cause a false reject.
//! * Inner and outer LRU bounds prevent unbounded growth.
//! * TTL bound makes the cache forget nonces older than any realistic round
//!   trip, which limits memory for sessions that never reach terminal state.
//! * Terminal-session entries deliberately remain through their normal TTL so
//!   late transport retransmits cannot produce duplicate UI/state events.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::constants::{
    DEDUP_CACHE_PER_SESSION, DEDUP_CACHE_SESSIONS, DEDUP_TTL_SECONDS, KEY_NONCE, KEY_SESSION,
};
use crate::envelope::Envelope;

/// Verdict returned by [`ReplayDedup::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupVerdict {
    /// First time we've seen this envelope; dispatch it.
    Fresh,
    /// Duplicate of a prior arrival; caller should drop it silently.
    Replay,
}

/// Bounded LRU of `(receiving_identity_id, session_id, nonce)` observations.
pub struct ReplayDedup {
    max_per_session: usize,
    max_sessions: usize,
    ttl: Duration,
    by_session: HashMap<(String, String), SessionCache>,
}

struct SessionCache {
    // (nonce, first_seen). Kept in most-recently-used-at-end order so eviction
    // pops the front. Replay hits move an entry without changing first_seen,
    // preserving the absolute TTL. Lookup is linear in N, which is bounded by
    // max_per_session (512 by default) — cheap.
    entries: Vec<(Vec<u8>, Instant)>,
    last_touched: Instant,
}

impl SessionCache {
    fn new(now: Instant) -> Self {
        Self {
            entries: Vec::new(),
            last_touched: now,
        }
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
        Self::with_limits(max_per_session, ttl_seconds, DEDUP_CACHE_SESSIONS)
    }

    /// Build a cache with explicit per-session, TTL, and session-count bounds.
    pub fn with_limits(max_per_session: usize, ttl_seconds: u64, max_sessions: usize) -> Self {
        Self {
            max_per_session,
            max_sessions: max_sessions.max(1),
            ttl: Duration::from_secs(ttl_seconds),
            by_session: HashMap::new(),
        }
    }

    /// Decide whether `envelope` is a replay.
    ///
    /// Returns [`DedupVerdict::Replay`] if this envelope's `(session_id,
    /// nonce)` pair was already seen (caller should drop it). Otherwise
    /// the nonce is recorded and [`DedupVerdict::Fresh`] is returned.
    pub fn check(&mut self, envelope: &Envelope) -> DedupVerdict {
        self.check_scoped_at("", envelope, Instant::now())
    }

    /// Check a nonce in one receiving identity's namespace.
    pub fn check_scoped(&mut self, identity_id: &str, envelope: &Envelope) -> DedupVerdict {
        self.check_scoped_at(identity_id, envelope, Instant::now())
    }

    /// Probe one receiving identity's namespace without recording a fresh
    /// nonce or evicting any existing entry.
    ///
    /// Routers use this before participant authorization, then call
    /// [`check_scoped`](Self::check_scoped) after authorization succeeds. The
    /// second atomic check resolves concurrent duplicate races, while a stream
    /// of unauthenticated fresh nonces cannot consume or evict replay state.
    pub fn probe_scoped(&mut self, identity_id: &str, envelope: &Envelope) -> DedupVerdict {
        self.probe_scoped_at(identity_id, envelope, Instant::now())
    }

    /// [`check`](Self::check) with an injected clock for deterministic tests.
    ///
    /// Envelope MUST be post-`unpack_envelope` validated; missing/malformed
    /// fields here are a protocol violation and are dropped as `Replay`.
    pub fn check_at(&mut self, envelope: &Envelope, now: Instant) -> DedupVerdict {
        self.check_scoped_at("", envelope, now)
    }

    /// [`check_scoped`](Self::check_scoped) with an injected clock.
    pub fn check_scoped_at(
        &mut self,
        identity_id: &str,
        envelope: &Envelope,
        now: Instant,
    ) -> DedupVerdict {
        let nonce = match envelope.get(KEY_NONCE) {
            Some(rmpv::Value::Binary(b)) if b.len() == crate::constants::NONCE_BYTES => b.clone(),
            _ => return DedupVerdict::Replay,
        };
        let session_id = match envelope.get(KEY_SESSION) {
            Some(rmpv::Value::String(s)) => match s.as_str() {
                Some(s) => s.to_string(),
                None => return DedupVerdict::Replay,
            },
            _ => return DedupVerdict::Replay,
        };

        self.prune_expired_sessions(now);
        self.make_room_for_session(identity_id, &session_id);

        let cache = self
            .by_session
            .entry((identity_id.to_string(), session_id))
            .or_insert_with(|| SessionCache::new(now));
        cache.last_touched = now;
        cache.prune_expired(now, self.ttl);

        if let Some(pos) = cache.position(&nonce) {
            // Refresh LRU ordering, but deliberately retain the first-seen
            // timestamp. Replays do not extend the protocol TTL window.
            let entry = cache.entries.remove(pos);
            cache.entries.push(entry);
            return DedupVerdict::Replay;
        }

        cache.entries.push((nonce, now));
        // Trim from the front until we're within the cap.
        while cache.entries.len() > self.max_per_session {
            cache.entries.remove(0);
        }
        DedupVerdict::Fresh
    }

    /// [`probe_scoped`](Self::probe_scoped) with an injected clock.
    pub fn probe_scoped_at(
        &mut self,
        identity_id: &str,
        envelope: &Envelope,
        now: Instant,
    ) -> DedupVerdict {
        let nonce = match envelope.get(KEY_NONCE) {
            Some(rmpv::Value::Binary(b)) if b.len() == crate::constants::NONCE_BYTES => b,
            _ => return DedupVerdict::Replay,
        };
        let session_id = match envelope.get(KEY_SESSION) {
            Some(rmpv::Value::String(s)) => match s.as_str() {
                Some(s) => s,
                None => return DedupVerdict::Replay,
            },
            _ => return DedupVerdict::Replay,
        };

        self.prune_expired_sessions(now);
        let key = (identity_id.to_string(), session_id.to_string());
        let Some(cache) = self.by_session.get_mut(&key) else {
            return DedupVerdict::Fresh;
        };
        cache.last_touched = now;
        if let Some(position) = cache.position(nonce) {
            // Retain the original first-seen timestamp while refreshing only
            // bounded LRU order, just like an ordinary replay check.
            let entry = cache.entries.remove(position);
            cache.entries.push(entry);
            DedupVerdict::Replay
        } else {
            DedupVerdict::Fresh
        }
    }

    /// Forget a session in the legacy unscoped namespace used by [`Self::check`].
    ///
    /// Scoped integrations must use [`drop_scoped_session`](Self::drop_scoped_session)
    /// so deleting one identity's session never affects another identity.
    /// Do not call either helper merely because a session became terminal;
    /// terminal replay entries remain useful through their normal TTL.
    pub fn drop_session(&mut self, session_id: &str) {
        self.drop_scoped_session("", session_id);
    }

    /// Forget a session cache only for one receiving identity.
    pub fn drop_scoped_session(&mut self, identity_id: &str, session_id: &str) {
        self.by_session
            .remove(&(identity_id.to_string(), session_id.to_string()));
    }

    /// Remove one previously-recorded nonce in the legacy unscoped namespace.
    /// This is a low-level transaction-recovery primitive; router integrations
    /// should use the scoped recovery APIs on [`crate::router::LrgpRouter`].
    /// Ordinary authorization failures never record a nonce.
    pub fn forget_nonce(&mut self, session_id: &str, nonce: &[u8]) {
        self.forget_scoped_nonce("", session_id, nonce);
    }

    pub fn forget_scoped_nonce(&mut self, identity_id: &str, session_id: &str, nonce: &[u8]) {
        let key = (identity_id.to_string(), session_id.to_string());
        if let Some(cache) = self.by_session.get_mut(&key)
            && let Some(position) = cache.position(nonce)
        {
            cache.entries.remove(position);
        }
        if self
            .by_session
            .get(&key)
            .is_some_and(|cache| cache.entries.is_empty())
        {
            self.by_session.remove(&key);
        }
    }

    fn prune_expired_sessions(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.by_session.retain(|_, cache| {
            cache.prune_expired(now, ttl);
            !cache.entries.is_empty()
        });
    }

    fn make_room_for_session(&mut self, identity_id: &str, incoming_session: &str) {
        let incoming_key = (identity_id.to_string(), incoming_session.to_string());
        if self.max_sessions == 0 || self.by_session.contains_key(&incoming_key) {
            return;
        }
        while self.by_session.len() >= self.max_sessions {
            let Some(oldest) = self
                .by_session
                .iter()
                .min_by_key(|(_, cache)| cache.last_touched)
                .map(|(session_key, _)| session_key.clone())
            else {
                break;
            };
            self.by_session.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn session_count(&self) -> usize {
        self.by_session.len()
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
        let mut envelope =
            pack_envelope("ttt", 1, "move", "0000000000000000", None, Some(nonce)).unwrap();
        // ReplayDedup intentionally operates after canonical validation; its
        // unit tests use short labels to make cache-scope assertions legible.
        envelope.insert(KEY_SESSION.into(), rmpv::Value::String(session.into()));
        envelope
    }

    #[test]
    fn fresh_nonce_is_not_a_replay() {
        let mut d = ReplayDedup::new();
        assert_eq!(d.check(&env("s1", [0; 8])), DedupVerdict::Fresh);
    }

    #[test]
    fn fresh_probe_does_not_record_or_evict_existing_nonce() {
        let mut d = ReplayDedup::with_limits(1, 600, 4);
        let recorded = env("s1", [1; 8]);
        let untrusted = env("s1", [2; 8]);
        assert_eq!(d.check_scoped("local", &recorded), DedupVerdict::Fresh);

        assert_eq!(d.probe_scoped("local", &untrusted), DedupVerdict::Fresh);
        assert_eq!(d.probe_scoped("local", &recorded), DedupVerdict::Replay);
        assert_eq!(d.check_scoped("local", &recorded), DedupVerdict::Replay);
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
    fn oldest_nonce_evicted_once_cap_exceeded() {
        let mut d = ReplayDedup::with_bounds(4, 3600);
        for i in 0..4 {
            let mut n = [0u8; 8];
            n[0] = i;
            assert_eq!(d.check(&env("s1", n)), DedupVerdict::Fresh);
        }
        assert_eq!(
            d.check(&env("s1", [9, 0, 0, 0, 0, 0, 0, 0])),
            DedupVerdict::Fresh
        );
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

    #[test]
    fn duplicate_does_not_extend_absolute_ttl() {
        let start = Instant::now();
        let mut d = ReplayDedup::with_bounds(4, 10);
        let e = env("0000000000000001", [0x66; 8]);
        assert_eq!(d.check_at(&e, start), DedupVerdict::Fresh);
        assert_eq!(
            d.check_at(&e, start + Duration::from_secs(9)),
            DedupVerdict::Replay
        );
        assert_eq!(
            d.check_at(&e, start + Duration::from_secs(11)),
            DedupVerdict::Fresh
        );
    }

    #[test]
    fn outer_session_map_is_bounded() {
        let mut d = ReplayDedup::with_limits(4, 3600, 2);
        assert_eq!(
            d.check(&env("0000000000000001", [1; 8])),
            DedupVerdict::Fresh
        );
        assert_eq!(
            d.check(&env("0000000000000002", [2; 8])),
            DedupVerdict::Fresh
        );
        assert_eq!(
            d.check(&env("0000000000000003", [3; 8])),
            DedupVerdict::Fresh
        );
        assert_eq!(d.session_count(), 2);
    }

    #[test]
    fn receiving_identities_have_independent_replay_namespaces() {
        let mut d = ReplayDedup::new();
        let envelope = env("0000000000000001", [7; 8]);
        assert_eq!(d.check_scoped("alice", &envelope), DedupVerdict::Fresh);
        assert_eq!(d.check_scoped("alice", &envelope), DedupVerdict::Replay);
        assert_eq!(d.check_scoped("bob", &envelope), DedupVerdict::Fresh);
    }
}
