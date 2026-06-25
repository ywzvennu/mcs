//! The Glicko-2 rating-update completion hook.
//!
//! [`RatingUpdateHook`] is the API layer's implementation of
//! [`GameCompletionHook`]: when a game finishes, the actor invokes it (after
//! persisting the result), and it recomputes both players' Glicko-2 ratings for
//! the game's variant and writes them back.
//!
//! Keeping this in `mcs-api` — rather than `mcs-game` — is deliberate. The game
//! actor stays free of any rating dependency; it knows only the abstract
//! [`GameCompletionHook`] trait. The concrete wiring of "a finished game updates
//! ratings" lives here, where the storage and rating crates already meet.

use std::sync::Arc;

use async_trait::async_trait;

use time::OffsetDateTime;

use mcs_core::{Color, Outcome};
use mcs_domain::{Game, Rating, RatingHistoryEntry, TimeClass, UserId};
use mcs_game::GameCompletionHook;
use mcs_rating::{update_single, Glicko2Rating, Score, DEFAULT_TAU};
use mcs_storage::{RatingHistoryRepo, RatingRepo};

/// A [`GameCompletionHook`] that applies a Glicko-2 rating update to both
/// players when a game finishes.
///
/// A **casual** game ([`Game::rated`]` == false`) is exempt: the hook returns
/// immediately without reading or writing any rating, leaving both players'
/// ratings — and the leaderboard — untouched.
///
/// On a decisive or drawn result for a **rated** game it:
///
/// 1. Reads each player's current [`Rating`] for the game's `variant_id`,
///    seeding an unrated player with [`Rating::default`].
/// 2. Computes the Glicko-2 update for each player against the other, using the
///    opponent's *pre-game* rating (so the pairing is symmetric).
/// 3. Persists both new ratings via [`RatingRepo::upsert`].
/// 4. Appends a [`RatingHistoryEntry`] snapshot for each player via
///    [`RatingHistoryRepo::record`], so a rated game leaves two new history rows.
///
/// # Robustness
///
/// The hook never panics — a requirement of the [`GameCompletionHook`] contract,
/// since it runs on the game's actor task. Every failure mode is handled as a
/// log-and-skip:
///
/// - A non-decisive, non-drawn end (no [`Outcome::winner`] and not explicitly a
///   draw is impossible here — any finished `Outcome` is decisive or a draw —
///   but an *ongoing*/aborted game never reaches this hook at all).
/// - A storage error while reading or writing a rating logs and aborts the
///   update without disturbing the game.
#[derive(Clone)]
pub struct RatingUpdateHook {
    ratings: Arc<dyn RatingRepo>,
    history: Arc<dyn RatingHistoryRepo>,
}

impl RatingUpdateHook {
    /// Builds a hook that reads and writes ratings through `ratings` and appends
    /// a per-player snapshot to `history` after each rated game.
    #[must_use]
    pub fn new(ratings: Arc<dyn RatingRepo>, history: Arc<dyn RatingHistoryRepo>) -> Self {
        Self { ratings, history }
    }

    /// Appends a rating-history snapshot for `user` in `variant_id` after their
    /// new rating has been persisted. A failure is logged and swallowed — the
    /// durable current rating is already written, and the history log is a
    /// best-effort audit trail whose loss must never disturb the game.
    async fn record_history(
        &self,
        user: UserId,
        variant_id: &str,
        time_class: TimeClass,
        rating: &Rating,
        game_id: mcs_domain::GameId,
        now: OffsetDateTime,
    ) {
        let entry = RatingHistoryEntry {
            user_id: user,
            variant_id: variant_id.to_owned(),
            time_class,
            value: rating.value,
            deviation: rating.deviation,
            game_id,
            created_at: now,
        };
        if let Err(error) = self.history.record(&entry).await {
            tracing::error!(
                %user,
                variant_id,
                %time_class,
                %error,
                "failed to append rating-history snapshot",
            );
        }
    }

    /// Loads `user`'s current rating for `(variant_id, time_class)`, falling back
    /// to the Glicko-2 seed for an unrated player. A storage error is propagated
    /// so the caller can abort the whole update rather than persist a partial
    /// result.
    async fn current_rating(
        &self,
        user: UserId,
        variant_id: &str,
        time_class: TimeClass,
    ) -> Result<Rating, ()> {
        match self.ratings.get(user, variant_id, time_class).await {
            Ok(Some(rating)) => Ok(rating),
            // No row yet: a player's first rated game in this (variant, time
            // class) starts from the standard seed.
            Ok(None) => Ok(Rating::default()),
            Err(error) => {
                tracing::error!(
                    %user,
                    variant_id,
                    %time_class,
                    %error,
                    "failed to read rating for post-game update; skipping",
                );
                Err(())
            }
        }
    }
}

impl std::fmt::Debug for RatingUpdateHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RatingUpdateHook")
            .field("ratings", &"<dyn RatingRepo>")
            .field("history", &"<dyn RatingHistoryRepo>")
            .finish()
    }
}

/// The two [`Score`]s of a finished game, from White's and Black's perspective.
///
/// A drawn outcome scores `Draw`/`Draw`; a decisive one scores `Win` for the
/// winner and `Loss` for the loser.
fn scores_for(outcome: &Outcome) -> (Score, Score) {
    match outcome.winner {
        Some(Color::White) => (Score::Win, Score::Loss),
        Some(Color::Black) => (Score::Loss, Score::Win),
        None => (Score::Draw, Score::Draw),
    }
}

#[async_trait]
impl GameCompletionHook for RatingUpdateHook {
    async fn on_finished(&self, game: &Game, outcome: &Outcome) {
        // A casual game never affects ratings: skip all rating reads and writes.
        if !game.rated {
            tracing::debug!(
                game_id = %game.id,
                "casual game finished; skipping rating update",
            );
            return;
        }

        let variant_id = game.variant_id.as_str();
        // Ratings are keyed per (variant, time_class): a bullet game updates the
        // player's bullet rating, a classical game their classical rating, etc.
        let time_class = game.time_control.time_class();
        let (white_score, black_score) = scores_for(outcome);

        // Read both players' pre-game ratings. If either read fails we abort the
        // whole update so we never persist a one-sided change.
        let (Ok(white_rating), Ok(black_rating)) = (
            self.current_rating(game.white, variant_id, time_class)
                .await,
            self.current_rating(game.black, variant_id, time_class)
                .await,
        ) else {
            return;
        };

        // Compute each player's update against the opponent's *pre-game* rating,
        // so the result is symmetric and order-independent.
        let white_pre: Glicko2Rating = white_rating.into();
        let black_pre: Glicko2Rating = black_rating.into();

        let white_post: Rating =
            update_single(white_pre, black_pre, white_score, DEFAULT_TAU).into();
        let black_post: Rating =
            update_single(black_pre, white_pre, black_score, DEFAULT_TAU).into();

        // A single timestamp for both snapshots, so the two rows a rated game
        // appends share one recorded instant.
        let now = OffsetDateTime::now_utc();

        // Persist both. A write failure is logged but not retried here; the
        // durable game record is already the source of truth and a follow-up
        // reconciliation could replay it. A history snapshot is appended only
        // after the player's new rating was durably written, so the log never
        // records a rating the current-rating row does not also reflect.
        match self
            .ratings
            .upsert(game.white, variant_id, time_class, &white_post)
            .await
        {
            Ok(()) => {
                self.record_history(
                    game.white,
                    variant_id,
                    time_class,
                    &white_post,
                    game.id,
                    now,
                )
                .await;
            }
            Err(error) => {
                tracing::error!(
                    user = %game.white,
                    variant_id,
                    %time_class,
                    %error,
                    "failed to persist updated rating for White",
                );
            }
        }
        match self
            .ratings
            .upsert(game.black, variant_id, time_class, &black_post)
            .await
        {
            Ok(()) => {
                self.record_history(
                    game.black,
                    variant_id,
                    time_class,
                    &black_post,
                    game.id,
                    now,
                )
                .await;
            }
            Err(error) => {
                tracing::error!(
                    user = %game.black,
                    variant_id,
                    %time_class,
                    %error,
                    "failed to persist updated rating for Black",
                );
            }
        }

        // Count the rating update (#88): one per finished *rated* game that
        // reached this point (a casual game returned early above).
        crate::metrics::record_rating_update();

        tracing::info!(
            game_id = %game.id,
            variant_id,
            %time_class,
            "applied post-game Glicko-2 rating update",
        );
    }
}
