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

use mcs_core::{Color, Outcome};
use mcs_domain::{Game, Rating, UserId};
use mcs_game::GameCompletionHook;
use mcs_rating::{update_single, Glicko2Rating, Score, DEFAULT_TAU};
use mcs_storage::RatingRepo;

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
}

impl RatingUpdateHook {
    /// Builds a hook that reads and writes ratings through `ratings`.
    #[must_use]
    pub fn new(ratings: Arc<dyn RatingRepo>) -> Self {
        Self { ratings }
    }

    /// Loads `user`'s current rating for `variant_id`, falling back to the
    /// Glicko-2 seed for an unrated player. A storage error is propagated so the
    /// caller can abort the whole update rather than persist a partial result.
    async fn current_rating(&self, user: UserId, variant_id: &str) -> Result<Rating, ()> {
        match self.ratings.get(user, variant_id).await {
            Ok(Some(rating)) => Ok(rating),
            // No row yet: a player's first rated game in this variant starts from
            // the standard seed.
            Ok(None) => Ok(Rating::default()),
            Err(error) => {
                tracing::error!(
                    %user,
                    variant_id,
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
        let (white_score, black_score) = scores_for(outcome);

        // Read both players' pre-game ratings. If either read fails we abort the
        // whole update so we never persist a one-sided change.
        let (Ok(white_rating), Ok(black_rating)) = (
            self.current_rating(game.white, variant_id).await,
            self.current_rating(game.black, variant_id).await,
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

        // Persist both. A write failure is logged but not retried here; the
        // durable game record is already the source of truth and a follow-up
        // reconciliation could replay it.
        if let Err(error) = self
            .ratings
            .upsert(game.white, variant_id, &white_post)
            .await
        {
            tracing::error!(
                user = %game.white,
                variant_id,
                %error,
                "failed to persist updated rating for White",
            );
        }
        if let Err(error) = self
            .ratings
            .upsert(game.black, variant_id, &black_post)
            .await
        {
            tracing::error!(
                user = %game.black,
                variant_id,
                %error,
                "failed to persist updated rating for Black",
            );
        }

        // Count the rating update (#88): one per finished *rated* game that
        // reached this point (a casual game returned early above).
        crate::metrics::record_rating_update();

        tracing::info!(
            game_id = %game.id,
            variant_id,
            "applied post-game Glicko-2 rating update",
        );
    }
}
