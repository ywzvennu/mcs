//! Player online-presence tracking.
//!
//! [`PresenceTracker`] is the public trait: mark a user seen, query when they
//! were last seen, and ask whether they are still online within a given TTL.
//! The default implementation is [`InProcessPresence`] — a concurrency-safe,
//! in-memory `UserId → last_seen` map backed by a `std::sync::Mutex<HashMap>`.
//!
//! # Per-node semantics
//!
//! **Presence is per-node and in-process today.** A user is "online" from the
//! perspective of the node that handled their last authenticated request. In a
//! multi-node deployment, if a user connects to node A and queries their status
//! from node B, node B will report them as offline — it has never seen them.
//!
//! The intended evolution is a Redis-backed `PresenceTracker`: `mark_seen`
//! writes a key with a TTL to a shared Redis instance; `last_seen` reads it
//! back. The trait is intentionally designed so that dropping in a
//! `RedisPresence` type requires no changes to any call site.
//!
//! # Injectable clock
//!
//! The low-level [`mark_seen_at`](InProcessPresence::mark_seen_at) and
//! [`is_online_at`](InProcessPresence::is_online_at) methods accept an explicit
//! `now` parameter, enabling deterministic tests without wall-clock sleeps.
//! The ergonomic [`PresenceTracker`] trait methods call through to these with
//! [`OffsetDateTime::now_utc()`].

use std::collections::HashMap;
use std::sync::Mutex;

use mcs_domain::UserId;
use time::{Duration, OffsetDateTime};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Tracks the last-seen instant for each user, enabling online-status queries.
///
/// Implementations must be `Send + Sync` (axum clones the state across threads)
/// and cheap to clone (share their data behind an [`Arc`](std::sync::Arc)
/// internally — see [`InProcessPresence`]).
///
/// # Multi-node note
///
/// In a multi-node deployment, `mark_seen` only records the activity on the
/// current node. A user is online from a given node's perspective only if that
/// node has served one of their requests within the TTL window. A Redis-backed
/// implementation is the natural cross-node upgrade path.
pub trait PresenceTracker: Send + Sync {
    /// Records `user_id` as having been seen right now (wall-clock UTC).
    fn mark_seen(&self, user_id: UserId);

    /// Returns the most recent instant `user_id` was seen, or `None` if never.
    fn last_seen(&self, user_id: UserId) -> Option<OffsetDateTime>;

    /// Returns `true` when `user_id` was seen within the last `ttl`.
    ///
    /// A user is considered online when
    /// `now_utc() - last_seen(user_id) <= ttl`. A user who was never seen
    /// returns `false`.
    fn is_online(&self, user_id: UserId, ttl: Duration) -> bool {
        match self.last_seen(user_id) {
            Some(ts) => {
                let elapsed = OffsetDateTime::now_utc() - ts;
                elapsed <= ttl
            }
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Default implementation
// ---------------------------------------------------------------------------

/// The default, single-node presence tracker: a `Mutex<HashMap<UserId, OffsetDateTime>>`.
///
/// All clones share the same underlying map through an [`Arc`](std::sync::Arc),
/// so every clone of [`AppState`](crate::state::AppState) that holds one
/// observes the same activity. The `Mutex` is a `std::sync::Mutex` (not Tokio's)
/// because the critical section is short (a single map insert or lookup) and is
/// never held across an `.await`.
///
/// # Clock injection for tests
///
/// The ergonomic methods ([`mark_seen`](PresenceTracker::mark_seen) /
/// [`is_online`](PresenceTracker::is_online)) use the real wall clock.
/// Tests can use the lower-level [`mark_seen_at`] and [`is_online_at`] methods,
/// which accept an explicit `now` — so TTL-expiry assertions never require a
/// real `sleep`.
///
/// [`mark_seen_at`]: InProcessPresence::mark_seen_at
/// [`is_online_at`]: InProcessPresence::is_online_at
#[derive(Debug, Clone)]
pub struct InProcessPresence {
    inner: std::sync::Arc<Mutex<HashMap<UserId, OffsetDateTime>>>,
}

impl Default for InProcessPresence {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcessPresence {
    /// Creates a new, empty presence map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Records `user_id` as having been seen at `now`.
    ///
    /// Prefer this over [`PresenceTracker::mark_seen`] when you need to inject
    /// a specific instant — e.g. in tests that verify TTL-expiry behaviour
    /// without real sleeps.
    pub fn mark_seen_at(&self, user_id: UserId, now: OffsetDateTime) {
        let mut map = self.inner.lock().expect("presence lock poisoned");
        map.insert(user_id, now);
    }

    /// Returns `true` when `user_id` was last seen within `ttl` of `now`.
    ///
    /// Prefer this over [`PresenceTracker::is_online`] in tests so the TTL
    /// expiry check uses your controlled clock rather than the real wall clock.
    #[must_use]
    pub fn is_online_at(&self, user_id: UserId, ttl: Duration, now: OffsetDateTime) -> bool {
        let map = self.inner.lock().expect("presence lock poisoned");
        match map.get(&user_id) {
            Some(&ts) => {
                let elapsed = now - ts;
                elapsed <= ttl
            }
            None => false,
        }
    }
}

impl PresenceTracker for InProcessPresence {
    fn mark_seen(&self, user_id: UserId) {
        self.mark_seen_at(user_id, OffsetDateTime::now_utc());
    }

    fn last_seen(&self, user_id: UserId) -> Option<OffsetDateTime> {
        let map = self.inner.lock().expect("presence lock poisoned");
        map.get(&user_id).copied()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn uid() -> UserId {
        UserId::new()
    }

    #[test]
    fn never_seen_user_is_offline() {
        let tracker = InProcessPresence::new();
        let user = uid();
        assert!(tracker.last_seen(user).is_none());
        assert!(!tracker.is_online(user, Duration::seconds(30)));
    }

    #[test]
    fn mark_seen_at_records_exact_instant() {
        let tracker = InProcessPresence::new();
        let user = uid();
        let t = datetime!(2025-01-01 12:00:00 UTC);
        tracker.mark_seen_at(user, t);
        assert_eq!(tracker.last_seen(user), Some(t));
    }

    #[test]
    fn is_online_at_within_ttl() {
        let tracker = InProcessPresence::new();
        let user = uid();
        let seen_at = datetime!(2025-01-01 12:00:00 UTC);
        tracker.mark_seen_at(user, seen_at);

        let ttl = Duration::seconds(30);
        // 10 seconds after — still online.
        let now = seen_at + Duration::seconds(10);
        assert!(tracker.is_online_at(user, ttl, now));
    }

    #[test]
    fn is_online_at_exactly_at_ttl_boundary_is_online() {
        let tracker = InProcessPresence::new();
        let user = uid();
        let seen_at = datetime!(2025-01-01 12:00:00 UTC);
        tracker.mark_seen_at(user, seen_at);

        let ttl = Duration::seconds(30);
        // Exactly at the TTL boundary — still online (elapsed == ttl).
        let now = seen_at + ttl;
        assert!(tracker.is_online_at(user, ttl, now));
    }

    #[test]
    fn is_online_at_after_ttl_expiry_is_offline() {
        let tracker = InProcessPresence::new();
        let user = uid();
        let seen_at = datetime!(2025-01-01 12:00:00 UTC);
        tracker.mark_seen_at(user, seen_at);

        let ttl = Duration::seconds(30);
        // One millisecond past the TTL — now offline.  No real sleep required.
        let now = seen_at + ttl + Duration::milliseconds(1);
        assert!(!tracker.is_online_at(user, ttl, now));
    }

    #[test]
    fn mark_seen_updates_existing_entry() {
        let tracker = InProcessPresence::new();
        let user = uid();
        let t1 = datetime!(2025-01-01 12:00:00 UTC);
        let t2 = datetime!(2025-01-01 12:01:00 UTC);
        tracker.mark_seen_at(user, t1);
        tracker.mark_seen_at(user, t2);
        assert_eq!(tracker.last_seen(user), Some(t2));
    }

    #[test]
    fn clones_share_the_same_map() {
        let tracker = InProcessPresence::new();
        let clone = tracker.clone();
        let user = uid();
        let t = datetime!(2025-01-01 12:00:00 UTC);
        tracker.mark_seen_at(user, t);
        // The clone must observe the same entry.
        assert_eq!(clone.last_seen(user), Some(t));
    }
}
