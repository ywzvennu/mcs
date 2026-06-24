//! Seek-pool matchmaker.
//!
//! This module implements the matchmaking layer that sits between the seek
//! persistence store and the game creation layer. It is the **only** place in
//! the server that decides whether two open seeks are compatible and which
//! player plays which colour.
//!
//! ## Lifecycle
//!
//! 1. A player calls [`Matchmaker::submit`] with a freshly constructed
//!    [`Seek`].
//! 2. The matchmaker scans the open seek pool for a compatible counterpart.
//!    Compatibility requires:
//!    - Same `variant_id`.
//!    - Equal `time_control` (exact match; flexible matching is a future
//!      concern).
//!    - Same `rated` flag — a rated seek never pairs with a casual seek, so both
//!      players always agree on whether the game counts towards their ratings.
//!    - Different `creator` (a player cannot match themselves).
//!    - Non-conflicting `color_preference` (see § Colour resolution below).
//! 3. If a match is found, both seeks are removed from the pool and a
//!    [`Pairing`] is returned.  The caller is responsible for spawning a
//!    game actor from the pairing.
//! 4. If no match is found, the incoming seek is persisted and
//!    [`SubmitOutcome::Queued`] is returned.
//!
//! ## Colour resolution
//!
//! | Incoming pref  | Existing pref  | Outcome               |
//! |----------------|----------------|-----------------------|
//! | `White`        | `Black`        | Paired (White/Black)  |
//! | `White`        | `Random`       | Paired (White/Black)  |
//! | `Black`        | `White`        | Paired (White/Black)  |
//! | `Black`        | `Random`       | Paired (Black/White)  |
//! | `Random`       | `White`        | Paired (White/Black)  |
//! | `Random`       | `Black`        | Paired (Black/White)  |
//! | `Random`       | `Random`       | Paired (deterministic tiebreak by seek ID) |
//! | `White`        | `White`        | **Incompatible** — skip |
//! | `Black`        | `Black`        | **Incompatible** — skip |
//!
//! The `Random`/`Random` tiebreak uses the lexicographic ordering of the two
//! [`SeekId`] UUIDs to assign colours without randomness, which keeps the
//! outcome deterministic and avoids requiring the `rand` crate.
//!
//! ## Concurrency safety
//!
//! [`Matchmaker`] wraps its find-and-remove critical section in a
//! [`tokio::sync::Mutex`]. This ensures that two simultaneous `submit` calls
//! cannot both match the same waiting seek; the second caller will observe the
//! pool after the first has already consumed the matching entry.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Mutex;
use tracing::instrument;

use mcs_domain::{ColorPreference, Seek, SeekId, TimeControl, UserId};
use mcs_storage::{SeekRepo, StorageError};

/// The result of successfully matching two seeks.
///
/// A [`Pairing`] contains everything needed to create a new game: the two
/// players' identities, the colour each will play, the variant, and the time
/// control agreed upon. Actual game-actor creation is the responsibility of
/// the caller (typically the API layer).
#[derive(Debug, Clone, PartialEq)]
pub struct Pairing {
    /// The user assigned to play white.
    pub white: UserId,
    /// The user assigned to play black.
    pub black: UserId,
    /// The variant both players agreed to play.
    pub variant_id: String,
    /// The time control both players agreed on.
    pub time_control: TimeControl,
    /// Whether the resulting game is rated.
    ///
    /// Both seeks agreed on this — a rated seek only pairs with another rated
    /// seek and a casual seek only with another casual seek — so this is simply
    /// the shared value of the two matched seeks.
    pub rated: bool,
}

/// The outcome of a [`Matchmaker::submit`] call.
#[derive(Debug)]
pub enum SubmitOutcome {
    /// The submitted seek was immediately matched with an existing open seek.
    ///
    /// The contained [`Pairing`] describes the game that should now be
    /// created. Neither seek remains in the pool.
    Paired(Pairing),

    /// No compatible seek was found; the seek was persisted and is now
    /// waiting in the pool.
    ///
    /// The contained [`SeekId`] is the ID of the newly queued seek (same as
    /// `seek.id` at the call site).
    Queued(SeekId),
}

/// Errors that can arise during matchmaking operations.
#[derive(Debug, Error)]
pub enum MatchmakingError {
    /// The underlying seek repository returned an error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// The seek-pool matchmaker.
///
/// [`Matchmaker`] is cheaply clonable via its internal `Arc`s and is safe to
/// share across tasks and threads.
///
/// ```no_run
/// use std::sync::Arc;
/// use mcs_game::matchmaking::Matchmaker;
/// use mcs_storage::SeekRepo;
///
/// # async fn example(repo: Arc<dyn SeekRepo>) {
/// let matchmaker = Matchmaker::new(repo);
/// # let _ = matchmaker;
/// # }
/// ```
pub struct Matchmaker {
    /// The backing seek store — shared with other callers.
    repo: Arc<dyn SeekRepo>,
    /// Serialises the find-and-remove critical section to prevent
    /// double-pairing under concurrent `submit` calls.
    lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for Matchmaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Matchmaker")
            .field("repo", &"Arc<dyn SeekRepo>")
            .field("lock", &self.lock)
            .finish()
    }
}

impl Clone for Matchmaker {
    fn clone(&self) -> Self {
        Self {
            repo: Arc::clone(&self.repo),
            lock: Arc::clone(&self.lock),
        }
    }
}

impl Matchmaker {
    /// Creates a new [`Matchmaker`] backed by the given [`SeekRepo`].
    #[must_use]
    pub fn new(repo: Arc<dyn SeekRepo>) -> Self {
        Self {
            repo,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Submits a seek to the matchmaking pool.
    ///
    /// Atomically scans the open pool for a compatible counterpart. If one is
    /// found, both seeks are removed and a [`SubmitOutcome::Paired`] is
    /// returned. Otherwise the seek is persisted and
    /// [`SubmitOutcome::Queued`] is returned.
    ///
    /// # Errors
    ///
    /// Returns [`MatchmakingError::Storage`] if any repository operation fails.
    #[instrument(skip(self), fields(seek_id = %seek.id, creator = %seek.creator))]
    pub async fn submit(&self, seek: Seek) -> Result<SubmitOutcome, MatchmakingError> {
        // Hold the lock for the entire find-and-remove critical section.
        let _guard = self.lock.lock().await;

        let open = self.repo.list_open().await?;

        for candidate in &open {
            if let Some(pairing) = try_pair(&seek, candidate) {
                // Remove both seeks from the pool.
                self.repo.remove(candidate.id).await?;
                // The incoming seek was never persisted, so we only remove
                // the candidate. (If it was already persisted by a prior
                // concurrent attempt that raced here, idempotent remove is
                // fine — SeekRepo::remove is documented as idempotent.)
                return Ok(SubmitOutcome::Paired(pairing));
            }
        }

        // No match found: persist the seek and report it as queued.
        self.repo.create(&seek).await?;
        Ok(SubmitOutcome::Queued(seek.id))
    }

    /// Cancels an open seek by removing it from the pool.
    ///
    /// This operation is idempotent: cancelling a seek that has already been
    /// matched or cancelled is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`MatchmakingError::Storage`] if the repository removal fails.
    #[instrument(skip(self), fields(%id))]
    pub async fn cancel(&self, id: SeekId) -> Result<(), MatchmakingError> {
        self.repo.remove(id).await?;
        Ok(())
    }

    /// Returns all seeks currently open in the pool.
    ///
    /// # Errors
    ///
    /// Returns [`MatchmakingError::Storage`] if the repository query fails.
    pub async fn open_seeks(&self) -> Result<Vec<Seek>, MatchmakingError> {
        let seeks = self.repo.list_open().await?;
        Ok(seeks)
    }
}

// ---------------------------------------------------------------------------
// Compatibility helpers
// ---------------------------------------------------------------------------

/// Attempts to pair `incoming` with `existing`.
///
/// Returns `Some(Pairing)` if the two seeks are compatible, or `None` if they
/// are incompatible and the caller should keep looking.
fn try_pair(incoming: &Seek, existing: &Seek) -> Option<Pairing> {
    // A player may not match themselves.
    if incoming.creator == existing.creator {
        return None;
    }

    // Both seeks must be for the same variant.
    if incoming.variant_id != existing.variant_id {
        return None;
    }

    // Time controls must be identical.
    if incoming.time_control != existing.time_control {
        return None;
    }

    // Both seeks must agree on rated vs. casual: a rated seek never pairs with a
    // casual one. The agreed value then carries onto the resulting `Pairing`.
    if incoming.rated != existing.rated {
        return None;
    }

    // Resolve colours; returns None when preferences conflict.
    let (white, black) = resolve_colors(
        incoming.creator,
        incoming.color_preference,
        existing.creator,
        existing.color_preference,
    )?;

    Some(Pairing {
        white,
        black,
        variant_id: incoming.variant_id.clone(),
        time_control: incoming.time_control.clone(),
        rated: incoming.rated,
    })
}

/// Resolves which player gets white and which gets black, given their
/// preferences.
///
/// Returns `None` when the preferences conflict (both want the same
/// deterministic colour) and the pair must be skipped.
fn resolve_colors(
    a_id: UserId,
    a_pref: ColorPreference,
    b_id: UserId,
    b_pref: ColorPreference,
) -> Option<(UserId, UserId)> {
    // (white, black)
    match (a_pref, b_pref) {
        // Explicit opposite preferences — straightforward.
        (ColorPreference::White, ColorPreference::Black) => Some((a_id, b_id)),
        (ColorPreference::Black, ColorPreference::White) => Some((b_id, a_id)),

        // One explicit, one flexible.
        (ColorPreference::White, ColorPreference::Random) => Some((a_id, b_id)),
        (ColorPreference::Black, ColorPreference::Random) => Some((b_id, a_id)),
        (ColorPreference::Random, ColorPreference::White) => Some((b_id, a_id)),
        (ColorPreference::Random, ColorPreference::Black) => Some((a_id, b_id)),

        // Both flexible — use a deterministic tiebreak so tests are stable.
        // We compare the UUID strings lexicographically; the lower one plays
        // white. This is arbitrary but consistent and avoids `rand`.
        (ColorPreference::Random, ColorPreference::Random) => {
            if a_id.to_string() <= b_id.to_string() {
                Some((a_id, b_id))
            } else {
                Some((b_id, a_id))
            }
        }

        // Conflicting hard preferences — incompatible.
        (ColorPreference::White, ColorPreference::White)
        | (ColorPreference::Black, ColorPreference::Black) => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use time::OffsetDateTime;
    use tokio::sync::Mutex as AsyncMutex;

    use mcs_domain::{ColorPreference, Seek, SeekId, TimeControl, UserId};
    use mcs_storage::{SeekRepo, StorageError, StorageResult};

    use super::{Matchmaker, SubmitOutcome};

    // -----------------------------------------------------------------------
    // In-memory SeekRepo mock
    // -----------------------------------------------------------------------

    #[derive(Debug, Default)]
    struct MemSeekRepo {
        inner: AsyncMutex<HashMap<SeekId, Seek>>,
    }

    #[async_trait]
    impl SeekRepo for MemSeekRepo {
        async fn create(&self, seek: &Seek) -> StorageResult<()> {
            let mut map = self.inner.lock().await;
            if map.contains_key(&seek.id) {
                return Err(StorageError::Conflict(format!(
                    "seek {} already exists",
                    seek.id
                )));
            }
            map.insert(seek.id, seek.clone());
            Ok(())
        }

        async fn get(&self, id: SeekId) -> StorageResult<Option<Seek>> {
            let map = self.inner.lock().await;
            Ok(map.get(&id).cloned())
        }

        async fn remove(&self, id: SeekId) -> StorageResult<()> {
            let mut map = self.inner.lock().await;
            map.remove(&id);
            Ok(())
        }

        async fn list_open(&self) -> StorageResult<Vec<Seek>> {
            let map = self.inner.lock().await;
            Ok(map.values().cloned().collect())
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn blitz() -> TimeControl {
        TimeControl::RealTime {
            initial: Duration::from_secs(180),
            increment: Duration::from_secs(2),
        }
    }

    fn rapid() -> TimeControl {
        TimeControl::RealTime {
            initial: Duration::from_secs(600),
            increment: Duration::from_secs(5),
        }
    }

    /// Builds a **rated** seek. Tests that exercise the rated/casual rule use
    /// [`make_seek_rated`] to set the flag explicitly.
    fn make_seek(creator: UserId, variant: &str, tc: TimeControl, pref: ColorPreference) -> Seek {
        make_seek_rated(creator, variant, tc, pref, true)
    }

    /// Builds a seek with an explicit `rated` flag.
    fn make_seek_rated(
        creator: UserId,
        variant: &str,
        tc: TimeControl,
        pref: ColorPreference,
        rated: bool,
    ) -> Seek {
        Seek::new(
            creator,
            variant.to_owned(),
            tc,
            pref,
            rated,
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    fn new_matchmaker() -> Matchmaker {
        let repo = Arc::new(MemSeekRepo::default());
        Matchmaker::new(repo)
    }

    // -----------------------------------------------------------------------
    // Basic pairing — opposite explicit preferences
    // -----------------------------------------------------------------------

    /// Two compatible seeks with opposite colour preferences must be paired,
    /// and the resulting `Pairing` must have exactly one white and one black
    /// player, each being one of the two seekers.
    #[tokio::test]
    async fn opposite_prefs_pair_correctly() {
        let mm = new_matchmaker();

        let user_a = UserId::new();
        let user_b = UserId::new();

        let seek_a = make_seek(user_a, "standard", blitz(), ColorPreference::White);
        let seek_b = make_seek(user_b, "standard", blitz(), ColorPreference::Black);

        // First submit: no pool entry yet → queued.
        let out_a = mm.submit(seek_a).await.unwrap();
        assert!(matches!(out_a, SubmitOutcome::Queued(_)));

        // Second submit: should match the first.
        let out_b = mm.submit(seek_b).await.unwrap();
        let pairing = match out_b {
            SubmitOutcome::Paired(p) => p,
            SubmitOutcome::Queued(_) => panic!("expected Paired, got Queued"),
        };

        // Structural invariants — do not assert which specific user plays white.
        let players = [pairing.white, pairing.black];
        assert!(
            players.contains(&user_a),
            "user_a must appear in the pairing"
        );
        assert!(
            players.contains(&user_b),
            "user_b must appear in the pairing"
        );
        assert_ne!(pairing.white, pairing.black, "white and black must differ");
        assert_eq!(pairing.variant_id, "standard");
        assert_eq!(pairing.time_control, blitz());

        // Pool must now be empty.
        assert!(mm.open_seeks().await.unwrap().is_empty());
    }

    /// For the White/Black pair, the explicit colour preferences must be
    /// honoured: the White-preferring user must be assigned white and the
    /// Black-preferring user must be assigned black.
    #[tokio::test]
    async fn explicit_prefs_honoured() {
        let mm = new_matchmaker();

        let user_white = UserId::new();
        let user_black = UserId::new();

        let seek_w = make_seek(user_white, "standard", blitz(), ColorPreference::White);
        let seek_b = make_seek(user_black, "standard", blitz(), ColorPreference::Black);

        mm.submit(seek_w).await.unwrap();
        let out = mm.submit(seek_b).await.unwrap();

        let pairing = match out {
            SubmitOutcome::Paired(p) => p,
            SubmitOutcome::Queued(_) => panic!("expected Paired"),
        };

        assert_eq!(pairing.white, user_white);
        assert_eq!(pairing.black, user_black);
    }

    // -----------------------------------------------------------------------
    // Incompatible time controls → both queued
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn incompatible_time_controls_both_queued() {
        let mm = new_matchmaker();

        let user_a = UserId::new();
        let user_b = UserId::new();

        let seek_a = make_seek(user_a, "standard", blitz(), ColorPreference::Random);
        let seek_b = make_seek(user_b, "standard", rapid(), ColorPreference::Random);

        let out_a = mm.submit(seek_a.clone()).await.unwrap();
        let out_b = mm.submit(seek_b.clone()).await.unwrap();

        assert!(
            matches!(out_a, SubmitOutcome::Queued(id) if id == seek_a.id),
            "seek_a should be queued"
        );
        assert!(
            matches!(out_b, SubmitOutcome::Queued(id) if id == seek_b.id),
            "seek_b should be queued"
        );

        // Both seeks remain open.
        let open = mm.open_seeks().await.unwrap();
        assert_eq!(open.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Conflicting colour preferences → not paired
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn conflicting_same_color_prefs_not_paired() {
        let mm = new_matchmaker();

        let user_a = UserId::new();
        let user_b = UserId::new();

        // Both want white — incompatible.
        let seek_a = make_seek(user_a, "standard", blitz(), ColorPreference::White);
        let seek_b = make_seek(user_b, "standard", blitz(), ColorPreference::White);

        let out_a = mm.submit(seek_a.clone()).await.unwrap();
        let out_b = mm.submit(seek_b.clone()).await.unwrap();

        assert!(matches!(out_a, SubmitOutcome::Queued(_)));
        assert!(matches!(out_b, SubmitOutcome::Queued(_)));

        let open = mm.open_seeks().await.unwrap();
        assert_eq!(open.len(), 2);
    }

    #[tokio::test]
    async fn conflicting_black_prefs_not_paired() {
        let mm = new_matchmaker();

        let user_a = UserId::new();
        let user_b = UserId::new();

        let seek_a = make_seek(user_a, "standard", blitz(), ColorPreference::Black);
        let seek_b = make_seek(user_b, "standard", blitz(), ColorPreference::Black);

        mm.submit(seek_a).await.unwrap();
        let out_b = mm.submit(seek_b).await.unwrap();

        assert!(matches!(out_b, SubmitOutcome::Queued(_)));
        let open = mm.open_seeks().await.unwrap();
        assert_eq!(open.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Random/Random → paired with a deterministic colour assignment
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn random_random_pair_assigns_distinct_colors() {
        let mm = new_matchmaker();

        let user_a = UserId::new();
        let user_b = UserId::new();

        let seek_a = make_seek(user_a, "standard", blitz(), ColorPreference::Random);
        let seek_b = make_seek(user_b, "standard", blitz(), ColorPreference::Random);

        mm.submit(seek_a).await.unwrap();
        let out = mm.submit(seek_b).await.unwrap();

        let pairing = match out {
            SubmitOutcome::Paired(p) => p,
            SubmitOutcome::Queued(_) => panic!("expected Paired"),
        };

        // Structural invariant: the two players are assigned different colours.
        assert_ne!(pairing.white, pairing.black);
        let players = [pairing.white, pairing.black];
        assert!(players.contains(&user_a));
        assert!(players.contains(&user_b));
    }

    // -----------------------------------------------------------------------
    // Cancel removes seek from pool
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cancel_removes_seek() {
        let mm = new_matchmaker();
        let user = UserId::new();
        let seek = make_seek(user, "standard", blitz(), ColorPreference::Random);
        let id = seek.id;

        mm.submit(seek).await.unwrap();
        assert_eq!(mm.open_seeks().await.unwrap().len(), 1);

        mm.cancel(id).await.unwrap();
        assert!(mm.open_seeks().await.unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Different variant_ids → not paired
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn different_variants_not_paired() {
        let mm = new_matchmaker();

        let user_a = UserId::new();
        let user_b = UserId::new();

        let seek_a = make_seek(user_a, "standard", blitz(), ColorPreference::Random);
        let seek_b = make_seek(user_b, "960", blitz(), ColorPreference::Random);

        mm.submit(seek_a).await.unwrap();
        let out_b = mm.submit(seek_b).await.unwrap();

        assert!(matches!(out_b, SubmitOutcome::Queued(_)));
        assert_eq!(mm.open_seeks().await.unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // Same creator cannot match themselves
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn same_creator_not_paired() {
        let mm = new_matchmaker();
        let user = UserId::new();

        let seek_a = make_seek(user, "standard", blitz(), ColorPreference::White);
        let seek_b = make_seek(user, "standard", blitz(), ColorPreference::Black);

        mm.submit(seek_a).await.unwrap();
        let out_b = mm.submit(seek_b).await.unwrap();

        assert!(matches!(out_b, SubmitOutcome::Queued(_)));
        assert_eq!(mm.open_seeks().await.unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // Rated vs. casual: only seeks that agree on `rated` may pair
    // -----------------------------------------------------------------------

    /// Two rated seeks with otherwise-compatible criteria pair, and the
    /// resulting `Pairing` is rated.
    #[tokio::test]
    async fn two_rated_seeks_pair_rated() {
        let mm = new_matchmaker();

        let seek_a = make_seek_rated(
            UserId::new(),
            "standard",
            blitz(),
            ColorPreference::White,
            true,
        );
        let seek_b = make_seek_rated(
            UserId::new(),
            "standard",
            blitz(),
            ColorPreference::Black,
            true,
        );

        mm.submit(seek_a).await.unwrap();
        let pairing = match mm.submit(seek_b).await.unwrap() {
            SubmitOutcome::Paired(p) => p,
            SubmitOutcome::Queued(_) => panic!("expected Paired"),
        };
        assert!(pairing.rated, "two rated seeks must produce a rated game");
    }

    /// Two casual seeks pair, and the resulting `Pairing` is casual.
    #[tokio::test]
    async fn two_casual_seeks_pair_casual() {
        let mm = new_matchmaker();

        let seek_a = make_seek_rated(
            UserId::new(),
            "standard",
            blitz(),
            ColorPreference::White,
            false,
        );
        let seek_b = make_seek_rated(
            UserId::new(),
            "standard",
            blitz(),
            ColorPreference::Black,
            false,
        );

        mm.submit(seek_a).await.unwrap();
        let pairing = match mm.submit(seek_b).await.unwrap() {
            SubmitOutcome::Paired(p) => p,
            SubmitOutcome::Queued(_) => panic!("expected Paired"),
        };
        assert!(
            !pairing.rated,
            "two casual seeks must produce a casual game"
        );
    }

    /// A rated seek and a casual seek must never pair, even when every other
    /// criterion is compatible.
    #[tokio::test]
    async fn rated_and_casual_seeks_do_not_pair() {
        let mm = new_matchmaker();

        let rated = make_seek_rated(
            UserId::new(),
            "standard",
            blitz(),
            ColorPreference::White,
            true,
        );
        let casual = make_seek_rated(
            UserId::new(),
            "standard",
            blitz(),
            ColorPreference::Black,
            false,
        );

        mm.submit(rated).await.unwrap();
        let out = mm.submit(casual).await.unwrap();

        assert!(
            matches!(out, SubmitOutcome::Queued(_)),
            "a rated seek must not pair with a casual seek"
        );
        assert_eq!(mm.open_seeks().await.unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // Concurrency: many simultaneous submits → no user double-booked
    // -----------------------------------------------------------------------

    /// Spawns `n` tasks each submitting one seek. After all tasks complete,
    /// every user must appear in at most one pairing, and every pairing must
    /// have distinct white and black players.
    #[tokio::test]
    async fn concurrent_submits_no_double_booking() {
        use std::collections::HashSet;

        let repo = Arc::new(MemSeekRepo::default());
        let mm = Matchmaker::new(repo);

        let n = 40_usize;
        let mut handles = Vec::with_capacity(n);

        for _ in 0..n {
            let mm_clone = mm.clone();
            let user = UserId::new();
            let seek = make_seek(user, "standard", blitz(), ColorPreference::Random);
            handles.push(tokio::spawn(async move {
                (user, mm_clone.submit(seek).await.unwrap())
            }));
        }

        let mut pairings = Vec::new();
        let mut queued_count = 0usize;

        for h in handles {
            let (_, outcome) = h.await.unwrap();
            match outcome {
                SubmitOutcome::Paired(p) => pairings.push(p),
                SubmitOutcome::Queued(_) => queued_count += 1,
            }
        }

        // Every player appears in at most one pairing.
        let mut seen: HashSet<UserId> = HashSet::new();
        for p in &pairings {
            assert!(
                seen.insert(p.white),
                "user {:?} appears in more than one pairing (white)",
                p.white
            );
            assert!(
                seen.insert(p.black),
                "user {:?} appears in more than one pairing (black)",
                p.black
            );
            assert_ne!(
                p.white, p.black,
                "white and black must differ within a pairing"
            );
        }

        // Each of the n submitted seeks produces exactly one outcome: either
        // `Queued` (seek persisted) or `Paired` (seek immediately matched an
        // existing entry). The first member of a successfully matched pair
        // received `Queued`; the second received `Paired`. Therefore the total
        // number of outcomes must equal n.
        let total_outcomes = pairings.len() + queued_count;
        assert_eq!(
            total_outcomes, n,
            "every submitted seek must produce exactly one outcome (paired or queued)"
        );

        // Each pairing consumes exactly two seeks: one from the pool (which
        // was Queued) and one incoming. So the players named in pairings come
        // from 2 * pairings.len() of the n seekers.
        assert_eq!(
            pairings.len() * 2 + mm.open_seeks().await.unwrap().len(),
            n,
            "paired players + open seeks must account for all n submissions"
        );
    }
}
