//! The broadcast message published to everyone watching a live game.

use mcs_core::{Event, GameStatus};
use mcs_domain::Clock;
use serde::{Deserialize, Serialize};

/// A live update broadcast to every subscriber of a game.
///
/// One `GameEvent` is published per successfully applied action. It bundles the
/// variant-defined [`Event`]s that the action produced with the game's
/// [`GameStatus`] *after* the action, so a client can render the events and
/// learn — in the same message — whether the game has now finished and with
/// what outcome.
///
/// The message is variant-agnostic: the [`Event`] payloads are the type-erased
/// JSON values from `mcs-core`, so a transport layer can forward a `GameEvent`
/// to any client without knowing which variant is being played. It is
/// [`Serialize`]/[`Deserialize`] so it can be sent straight over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameEvent {
    /// The events emitted by the action, in order, for observers to render.
    ///
    /// For standard chess this is typically a `MovePlayed` event, optionally
    /// followed by a `GameEnded` event on the final move.
    pub events: Vec<Event>,

    /// The game's lifecycle status after the action was applied.
    ///
    /// When this is [`GameStatus::Finished`], the embedded
    /// [`Outcome`](mcs_core::Outcome) is the final result and no further events
    /// will be broadcast for this game.
    pub status: GameStatus,

    /// A snapshot of both players' remaining time after the action, when the
    /// game is played under a real-time [`TimeControl`](mcs_domain::TimeControl).
    ///
    /// It is `None` for unlimited and correspondence games, and for any update
    /// that carries no clock information. The field is skipped during
    /// serialization when absent, so older consumers that do not know about
    /// clocks continue to deserialize unchanged — the addition is
    /// backward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
}

impl GameEvent {
    /// Builds a clock-less `GameEvent` from an action's `events` and resulting
    /// `status`.
    ///
    /// Use [`GameEvent::with_clock`] to attach a real-time clock snapshot.
    #[must_use]
    pub fn new(events: Vec<Event>, status: GameStatus) -> Self {
        Self {
            events,
            status,
            clock: None,
        }
    }

    /// Builds a `GameEvent` carrying a [`Clock`] snapshot alongside the action's
    /// `events` and resulting `status`.
    #[must_use]
    pub fn with_clock(events: Vec<Event>, status: GameStatus, clock: Clock) -> Self {
        Self {
            events,
            status,
            clock: Some(clock),
        }
    }

    /// Returns `true` if this update marks the game as finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.status.is_finished()
    }
}
