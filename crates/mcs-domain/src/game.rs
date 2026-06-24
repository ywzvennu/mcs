//! Game aggregate and lifecycle types.
//!
//! [`Game`] is the persistence-facing aggregate that tracks a game from
//! creation through to its final result. It deliberately does not hold
//! in-memory game state (the board, legal moves, etc.) — that responsibility
//! belongs to `mcs-game`. This type exists to record what the storage and API
//! layers need to know about a game.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mcs_core::{Color, Outcome, VariantOptions};

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
///
/// # Reconstructing the live game
///
/// The record carries everything needed to rebuild the playable session after
/// a restart. [`variant_id`](Self::variant_id) plus
/// [`variant_options`](Self::variant_options) feed
/// `VariantRegistry::new_game(variant_id, &variant_options)` to recreate a
/// fresh session, and the **live snapshot** fields
/// ([`ply`](Self::ply), [`clock_white_ms`](Self::clock_white_ms),
/// [`clock_black_ms`](Self::clock_black_ms),
/// [`side_to_move`](Self::side_to_move)) record the latest observed in-progress
/// state so a recovering server can present an accurate view without replaying
/// the move history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Game {
    /// Stable identifier for this game.
    pub id: GameId,
    /// Which chess variant is being played (e.g. `"standard"`).
    pub variant_id: String,
    /// The options the variant was created with.
    ///
    /// Combined with [`variant_id`](Self::variant_id), this lets the server
    /// re-create the playable session via
    /// `VariantRegistry::new_game(variant_id, &variant_options)`. Defaults to
    /// [`VariantOptions::default`] (the variant's own defaults).
    #[serde(default)]
    pub variant_options: VariantOptions,
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
    /// Number of plies (half-moves) played so far in the live snapshot.
    ///
    /// `0` for a freshly created game. Updated via
    /// [`update_snapshot`](Self::update_snapshot).
    #[serde(default)]
    pub ply: u32,
    /// White's remaining clock in milliseconds, if the game is timed and a
    /// snapshot has been recorded; `None` otherwise.
    #[serde(default)]
    pub clock_white_ms: Option<u64>,
    /// Black's remaining clock in milliseconds, if the game is timed and a
    /// snapshot has been recorded; `None` otherwise.
    #[serde(default)]
    pub clock_black_ms: Option<u64>,
    /// Whose turn it is in the live snapshot, or `None` before the first
    /// snapshot (or once the game is finished).
    #[serde(default)]
    pub side_to_move: Option<Color>,
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
    /// The live snapshot fields ([`ply`](Self::ply), the clocks, and
    /// [`side_to_move`](Self::side_to_move)) start empty (`ply = 0`, clocks and
    /// side `None`) and are filled in later via
    /// [`update_snapshot`](Self::update_snapshot).
    ///
    /// # Arguments
    ///
    /// * `variant_id` – the variant string identifier.
    /// * `variant_options` – the options the variant was created with (pass
    ///   [`VariantOptions::default`] for the variant's defaults).
    /// * `white` – the user assigned the white pieces.
    /// * `black` – the user assigned the black pieces.
    /// * `time_control` – the agreed time control.
    /// * `now` – the creation/update timestamp; pass `OffsetDateTime::now_utc()`
    ///   in application code.
    #[must_use]
    pub fn new(
        variant_id: String,
        variant_options: VariantOptions,
        white: UserId,
        black: UserId,
        time_control: TimeControl,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            id: GameId::new(),
            variant_id,
            variant_options,
            white,
            black,
            lifecycle: GameLifecycle::Created,
            outcome: None,
            time_control,
            ply: 0,
            clock_white_ms: None,
            clock_black_ms: None,
            side_to_move: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Records the latest live-game state into the durable snapshot fields and
    /// bumps [`updated_at`](Self::updated_at).
    ///
    /// Callers invoke this as a game progresses so a recovering server can show
    /// an accurate, up-to-date view (move count, clocks, side to move) without
    /// replaying the move history. It does not touch the lifecycle or outcome;
    /// use [`finish`](Self::finish) for terminal transitions.
    ///
    /// # Arguments
    ///
    /// * `ply` – number of half-moves played so far.
    /// * `clock_white_ms` / `clock_black_ms` – remaining clocks in
    ///   milliseconds, or `None` for an untimed game.
    /// * `side_to_move` – whose turn it now is, or `None` if not applicable.
    /// * `now` – the update timestamp.
    pub fn update_snapshot(
        &mut self,
        ply: u32,
        clock_white_ms: Option<u64>,
        clock_black_ms: Option<u64>,
        side_to_move: Option<Color>,
        now: OffsetDateTime,
    ) {
        self.ply = ply;
        self.clock_white_ms = clock_white_ms;
        self.clock_black_ms = clock_black_ms;
        self.side_to_move = side_to_move;
        self.updated_at = now;
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
            VariantOptions::default(),
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
    fn new_starts_with_empty_snapshot() {
        let game = sample_game();
        assert_eq!(game.ply, 0);
        assert!(game.clock_white_ms.is_none());
        assert!(game.clock_black_ms.is_none());
        assert!(game.side_to_move.is_none());
    }

    #[test]
    fn update_snapshot_sets_fields_and_timestamp() {
        let mut game = sample_game();
        let later = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(42);
        game.update_snapshot(7, Some(290_000), Some(300_000), Some(Color::Black), later);

        assert_eq!(game.ply, 7);
        assert_eq!(game.clock_white_ms, Some(290_000));
        assert_eq!(game.clock_black_ms, Some(300_000));
        assert_eq!(game.side_to_move, Some(Color::Black));
        assert_eq!(game.updated_at, later);
        // Lifecycle and creation time are untouched.
        assert_eq!(game.lifecycle, GameLifecycle::Created);
        assert_eq!(game.created_at, OffsetDateTime::UNIX_EPOCH);
    }

    #[test]
    fn snapshot_fields_survive_serde_round_trip() {
        let mut game = sample_game();
        game.lifecycle = GameLifecycle::Active;
        game.update_snapshot(
            12,
            Some(180_000),
            None,
            Some(Color::White),
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(99),
        );
        let json = serde_json::to_string(&game).unwrap();
        let back: Game = serde_json::from_str(&json).unwrap();
        assert_eq!(game, back);
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
