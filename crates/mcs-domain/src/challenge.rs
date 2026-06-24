//! Direct-challenge aggregate.
//!
//! A [`Challenge`] is an invitation from one specific player (the *challenger*)
//! to another specific player (the *challenged*) to play a game on agreed terms.
//! Unlike a [`Seek`](crate::Seek) — which floats in an open pool and is paired by
//! the matchmaker against an unknown opponent — a challenge names its opponent up
//! front and never enters matchmaking. Accepting one creates the game directly.
//!
//! ## Lifecycle
//!
//! A challenge starts [`Pending`](ChallengeStatus::Pending) and moves to exactly
//! one terminal state:
//!
//! - [`Accepted`](ChallengeStatus::Accepted) — the challenged player took it up;
//!   the resulting [`GameId`] is recorded on the challenge.
//! - [`Declined`](ChallengeStatus::Declined) — the challenged player refused.
//! - [`Canceled`](ChallengeStatus::Canceled) — the challenger withdrew it.
//!
//! The transitions are guarded by [`Challenge::accept`], [`Challenge::decline`],
//! and [`Challenge::cancel`], each of which only fires from `Pending`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{ChallengeId, GameId, UserId};
use crate::seek::ColorPreference;
use crate::time_control::TimeControl;

/// Where a direct challenge is in its lifecycle.
///
/// A challenge is created [`Pending`](ChallengeStatus::Pending) and transitions
/// to exactly one terminal state. The terminal states never transition further.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeStatus {
    /// The challenge is open and awaiting the challenged player's response.
    Pending,
    /// The challenged player accepted; a game was created (see
    /// [`Challenge::game_id`]).
    Accepted,
    /// The challenged player declined the invitation.
    Declined,
    /// The challenger withdrew the invitation before it was answered.
    Canceled,
}

/// A direct invitation from one player to a specific opponent.
///
/// Holds both identities, the agreed terms (variant, time control, rated flag,
/// and the challenger's colour preference), the current [`status`](Self::status),
/// and — once accepted — the [`game_id`](Self::game_id) of the game it created.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Challenge {
    /// Stable identifier for this challenge.
    pub id: ChallengeId,
    /// The user who issued the challenge.
    pub challenger: UserId,
    /// The user the challenge was issued to.
    pub challenged: UserId,
    /// Which chess variant the challenge is for (e.g. `"standard"`).
    pub variant_id: String,
    /// The timing rules the challenger proposes.
    pub time_control: TimeControl,
    /// Whether the proposed game is **rated** (feeds the Glicko-2 update) or
    /// casual (exempt from rating changes).
    pub rated: bool,
    /// Which side the *challenger* prefers; the challenged player takes the
    /// other side (see [`Challenge::accept`]).
    pub color_preference: ColorPreference,
    /// The current lifecycle state.
    pub status: ChallengeStatus,
    /// The game created when the challenge was accepted; `None` until then (and
    /// for the declined/canceled terminal states).
    pub game_id: Option<GameId>,
    /// When this challenge was created (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl Challenge {
    /// Creates a new [`Pending`](ChallengeStatus::Pending) challenge with a
    /// freshly generated [`ChallengeId`] and no associated game yet.
    ///
    /// # Arguments
    ///
    /// * `challenger` – the user issuing the invitation.
    /// * `challenged` – the specific opponent being invited.
    /// * `variant_id` – the variant string identifier.
    /// * `time_control` – the proposed time control.
    /// * `rated` – whether the proposed game is rated.
    /// * `color_preference` – the side the challenger wants to play.
    /// * `created_at` – creation timestamp; pass `OffsetDateTime::now_utc()` in
    ///   application code.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        challenger: UserId,
        challenged: UserId,
        variant_id: String,
        time_control: TimeControl,
        rated: bool,
        color_preference: ColorPreference,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id: ChallengeId::new(),
            challenger,
            challenged,
            variant_id,
            time_control,
            rated,
            color_preference,
            status: ChallengeStatus::Pending,
            game_id: None,
            created_at,
        }
    }

    /// Returns `true` when the challenge is still awaiting a response.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.status == ChallengeStatus::Pending
    }

    /// Marks the challenge [`Accepted`](ChallengeStatus::Accepted), recording the
    /// [`GameId`] of the game it created.
    ///
    /// Only a [`Pending`](ChallengeStatus::Pending) challenge can be accepted;
    /// calling this on an already-terminal challenge returns `false` and leaves
    /// the challenge untouched, so callers can treat the boolean as "did this
    /// transition apply".
    pub fn accept(&mut self, game_id: GameId) -> bool {
        if !self.is_pending() {
            return false;
        }
        self.status = ChallengeStatus::Accepted;
        self.game_id = Some(game_id);
        true
    }

    /// Marks the challenge [`Declined`](ChallengeStatus::Declined).
    ///
    /// Only a [`Pending`](ChallengeStatus::Pending) challenge can be declined;
    /// otherwise this returns `false` and leaves the challenge untouched.
    pub fn decline(&mut self) -> bool {
        if !self.is_pending() {
            return false;
        }
        self.status = ChallengeStatus::Declined;
        true
    }

    /// Marks the challenge [`Canceled`](ChallengeStatus::Canceled).
    ///
    /// Only a [`Pending`](ChallengeStatus::Pending) challenge can be canceled;
    /// otherwise this returns `false` and leaves the challenge untouched.
    pub fn cancel(&mut self) -> bool {
        if !self.is_pending() {
            return false;
        }
        self.status = ChallengeStatus::Canceled;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn sample_challenge() -> Challenge {
        Challenge::new(
            UserId::new(),
            UserId::new(),
            "standard".to_owned(),
            TimeControl::RealTime {
                initial: Duration::from_secs(300),
                increment: Duration::from_secs(2),
            },
            true,
            ColorPreference::White,
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn new_starts_pending_with_no_game() {
        let c = sample_challenge();
        assert_eq!(c.status, ChallengeStatus::Pending);
        assert!(c.is_pending());
        assert!(c.game_id.is_none());
    }

    #[test]
    fn new_generates_unique_ids() {
        assert_ne!(sample_challenge().id, sample_challenge().id);
    }

    #[test]
    fn accept_sets_status_and_game_id() {
        let mut c = sample_challenge();
        let game = GameId::new();
        assert!(c.accept(game));
        assert_eq!(c.status, ChallengeStatus::Accepted);
        assert_eq!(c.game_id, Some(game));
    }

    #[test]
    fn decline_sets_status() {
        let mut c = sample_challenge();
        assert!(c.decline());
        assert_eq!(c.status, ChallengeStatus::Declined);
        assert!(c.game_id.is_none());
    }

    #[test]
    fn cancel_sets_status() {
        let mut c = sample_challenge();
        assert!(c.cancel());
        assert_eq!(c.status, ChallengeStatus::Canceled);
        assert!(c.game_id.is_none());
    }

    #[test]
    fn transitions_only_fire_from_pending() {
        // Once accepted, no further transition applies.
        let mut c = sample_challenge();
        assert!(c.accept(GameId::new()));
        let snapshot = c.clone();
        assert!(!c.decline());
        assert!(!c.cancel());
        assert!(!c.accept(GameId::new()));
        assert_eq!(c, snapshot, "a terminal challenge is left untouched");

        // Declined and canceled are equally terminal.
        let mut declined = sample_challenge();
        assert!(declined.decline());
        assert!(!declined.accept(GameId::new()));
        assert_eq!(declined.status, ChallengeStatus::Declined);

        let mut canceled = sample_challenge();
        assert!(canceled.cancel());
        assert!(!canceled.decline());
        assert_eq!(canceled.status, ChallengeStatus::Canceled);
    }

    #[test]
    fn serde_round_trip_pending() {
        let c = sample_challenge();
        let json = serde_json::to_string(&c).unwrap();
        let back: Challenge = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn serde_round_trip_accepted() {
        let mut c = sample_challenge();
        c.accept(GameId::new());
        let json = serde_json::to_string(&c).unwrap();
        let back: Challenge = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn status_serde_round_trip() {
        for status in [
            ChallengeStatus::Pending,
            ChallengeStatus::Accepted,
            ChallengeStatus::Declined,
            ChallengeStatus::Canceled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: ChallengeStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }
}
