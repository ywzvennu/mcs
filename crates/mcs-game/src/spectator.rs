//! The spectator-safe snapshot frame published on the cross-node event bus
//! (#109).
//!
//! After each applied action — and on a finish or timeout — the game's owning
//! actor publishes one [`SpectatorFrame`] to the bus topic
//! [`spectator_topic(game_id)`]. A spectator connected to *any* node subscribes
//! to that topic and renders each frame, so watching a game no longer requires a
//! socket to the owning node.
//!
//! # Hidden-information safety
//!
//! The frame carries the session's [`spectator_view`](mcs_core::GameSession::spectator_view),
//! **never** a player view. For a perfect-information variant (standard chess)
//! that is the full position; for a hidden-information variant (RBC) it is the
//! redacted public view while the game is ongoing, revealing the full game only
//! once it is finished. Because the actor sources the frame from
//! `spectator_view()`, a hidden-information variant can never leak a player's
//! secret state to a watcher — the same guarantee the local spectator socket
//! already relies on.

use mcs_core::{GameStatus, PlayerView};
use mcs_domain::{Clock, GameId};
use serde::{Deserialize, Serialize};

/// The bus topic a game's spectator frames are published on.
///
/// Namespaced by game id so a subscriber receives only the game it is watching.
/// The owner publishes here on every applied action; a spectator on any node
/// subscribes to the same topic.
#[must_use]
pub fn spectator_topic(game_id: GameId) -> String {
    format!("game:{game_id}:spectator")
}

/// A spectator-safe snapshot of a game's position, published per applied action.
///
/// Self-contained and idempotent to apply: each frame is a *full* spectator
/// rendering of the position after the action, so a watcher that misses an
/// intermediate frame (the bus is best-effort) resynchronises completely on the
/// next one. It is [`Serialize`]/[`Deserialize`] so it travels over the bus as
/// JSON bytes.
///
/// The [`view`](Self::view) is always the public
/// [`spectator_view`](mcs_core::GameSession::spectator_view) — see the
/// [module docs](self) for the hidden-information guarantee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpectatorFrame {
    /// The public spectator view of the position after the action. Redacted
    /// while ongoing for hidden-information variants; full for perfect-info ones.
    pub view: PlayerView,

    /// The game's lifecycle status after the action. A
    /// [`Finished`](GameStatus::Finished) status carries the final outcome and
    /// signals that no further frames will be published for this game.
    pub status: GameStatus,

    /// Both sides' remaining time after the action, or `None` for an unlimited
    /// game that tracks no clock. Skipped from the JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,

    /// The number of half-moves played so far (the next ply to be recorded).
    pub ply: u32,
}

impl SpectatorFrame {
    /// Builds a frame from the public `view`, post-action `status`, optional
    /// `clock` snapshot, and the current half-move count `ply`.
    #[must_use]
    pub fn new(view: PlayerView, status: GameStatus, clock: Option<Clock>, ply: u32) -> Self {
        Self {
            view,
            status,
            clock,
            ply,
        }
    }

    /// Serializes the frame to JSON bytes for publishing on the event bus.
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] only if the (fixed-shape) frame cannot be
    /// serialized, which does not happen in practice.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Deserializes a frame from JSON bytes received on the event bus.
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] if `bytes` is not a valid serialized
    /// [`SpectatorFrame`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcs_domain::GameId;

    #[test]
    fn topic_is_namespaced_by_game_id() {
        let id = GameId::new();
        assert_eq!(spectator_topic(id), format!("game:{id}:spectator"));
    }

    #[test]
    fn frame_roundtrips_through_bytes() {
        let frame = SpectatorFrame::new(
            PlayerView::new(serde_json::json!({ "fen": "startpos" })),
            GameStatus::Ongoing,
            None,
            3,
        );
        let bytes = frame.to_bytes().unwrap();
        let back = SpectatorFrame::from_bytes(&bytes).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn clock_is_omitted_when_absent() {
        let frame = SpectatorFrame::new(
            PlayerView::new(serde_json::Value::Null),
            GameStatus::Ongoing,
            None,
            0,
        );
        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            !json.contains("clock"),
            "absent clock must be skipped: {json}"
        );
    }
}
