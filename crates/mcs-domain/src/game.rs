//! Game aggregate and lifecycle types.
//!
//! [`Game`] is the persistence-facing aggregate that tracks a game from
//! creation through to its final result. It deliberately does not hold
//! in-memory game state (the board, legal moves, etc.) — that responsibility
//! belongs to `mcs-game`. This type exists to record what the storage and API
//! layers need to know about a game.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mcs_core::Outcome;

use crate::ids::{GameId, UserId};
use crate::time_control::TimeControl;

/// Where a game is in its server-side lifecycle.
///
/// This mirrors, at a coarser granularity, the game-engine status.
///
/// - [`GameLifecycle::Created`] — the game record exists but play has not
///   started (e.g. waiting for both players to confirm).
/// - [`GameLifecycle::Active`] — moves are being played.
/// - [`GameLifecycle::Finished`] — the game has ended; `outcome` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameLifecycle {
    /// The game record has been created but play has not yet started.
    Created,
    /// Play is in progress.
    Active,
    /// The game has ended.
    Finished,
}

/// A game record as stored and served by the server.
///
/// Holds the identities of both players, the variant, the time control, and
/// the current lifecycle state. The transactional `finish` method atomically
/// transitions the game to [`GameLifecycle::Finished`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Game {
    /// Stable identifier for this game.
    pub id: GameId,
    /// Which chess variant is being played (e.g. `"standard"`).
    pub variant_id: String,
    /// The user playing White.
    pub white: UserId,
    /// The user playing Black.
    pub black: UserId,
    /// Current lifecycle state.
    pub lifecycle: GameLifecycle,
    /// The final outcome, set once `lifecycle` is [`GameLifecycle::Finished`].
    pub outcome: Option<Outcome>,
    /// Time control for this game.
    pub time_control: TimeControl,
    /// When this game record was created (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When this game record was last modified (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl Game {
    /// Creates a new [`Game`] record in the [`GameLifecycle::Created`] state.
    ///
    /// # Arguments
    ///
    /// * `variant_id` – the variant string identifier.
    /// * `white` – the user assigned the white pieces.
    /// * `black` – the user assigned the black pieces.
    /// * `time_control` – the agreed time control.
    /// * `now` – the creation/update timestamp; pass `OffsetDateTime::now_utc()`
    ///   in application code.
    #[must_use]
    pub fn new(
        variant_id: String,
        white: UserId,
        black: UserId,
        time_control: TimeControl,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            id: GameId::new(),
            variant_id,
            white,
            black,
            lifecycle: GameLifecycle::Created,
            outcome: None,
            time_control,
            created_at: now,
            updated_at: now,
        }
    }

    /// Transitions the game to [`GameLifecycle::Finished`], recording the
    /// outcome and update timestamp.
    ///
    /// Calling this method on an already-finished game is idempotent with
    /// respect to the lifecycle field, but it will overwrite the outcome and
    /// `updated_at`. Callers should guard against double-finish if needed.
    pub fn finish(&mut self, outcome: Outcome, now: OffsetDateTime) {
        self.lifecycle = GameLifecycle::Finished;
        self.outcome = Some(outcome);
        self.updated_at = now;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mcs_core::{EndReason, Outcome};
    use time::OffsetDateTime;

    use super::*;
    use crate::time_control::TimeControl;

    fn sample_game() -> Game {
        Game::new(
            "standard".to_owned(),
            UserId::new(),
            UserId::new(),
            TimeControl::RealTime {
                initial: Duration::from_secs(300),
                increment: Duration::from_secs(0),
            },
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn new_starts_in_created_state() {
        let game = sample_game();
        assert_eq!(game.lifecycle, GameLifecycle::Created);
        assert!(game.outcome.is_none());
        assert_eq!(game.created_at, game.updated_at);
    }

    #[test]
    fn finish_sets_lifecycle_and_outcome() {
        let mut game = sample_game();
        let outcome = Outcome::win(mcs_core::Color::White, EndReason::Checkmate);
        let later = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(3600);
        game.finish(outcome.clone(), later);

        assert_eq!(game.lifecycle, GameLifecycle::Finished);
        assert_eq!(game.outcome, Some(outcome));
        assert_eq!(game.updated_at, later);
        assert_eq!(game.created_at, OffsetDateTime::UNIX_EPOCH);
    }

    #[test]
    fn finish_draw() {
        let mut game = sample_game();
        let outcome = Outcome::draw(EndReason::Stalemate);
        let later = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1800);
        game.finish(outcome.clone(), later);
        assert!(game.outcome.as_ref().unwrap().winner.is_none());
    }

    #[test]
    fn serde_round_trip_active() {
        let mut game = sample_game();
        game.lifecycle = GameLifecycle::Active;
        let json = serde_json::to_string(&game).unwrap();
        let back: Game = serde_json::from_str(&json).unwrap();
        assert_eq!(game, back);
    }

    #[test]
    fn serde_round_trip_finished() {
        let mut game = sample_game();
        let later = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(7200);
        game.finish(
            Outcome::win(mcs_core::Color::Black, EndReason::Resignation),
            later,
        );
        let json = serde_json::to_string(&game).unwrap();
        let back: Game = serde_json::from_str(&json).unwrap();
        assert_eq!(game, back);
    }

    #[test]
    fn new_generates_unique_ids() {
        let a = sample_game();
        let b = sample_game();
        assert_ne!(a.id, b.id);
    }
}
