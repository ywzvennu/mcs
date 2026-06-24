//! Game outcomes and lifecycle status.

use serde::{Deserialize, Serialize};

use crate::color::Color;
use crate::payload::Event;

/// Why a game ended.
///
/// The common chess reasons are enumerated explicitly so that the server can
/// reason about them. Variants that end for a reason not listed here use
/// [`EndReason::Other`] with a descriptive string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The losing side's king is in check and has no legal escape.
    Checkmate,
    /// The side to move has no legal moves but is not in check.
    Stalemate,
    /// A player resigned.
    Resignation,
    /// A player ran out of time.
    Timeout,
    /// Both players agreed to a draw.
    DrawAgreement,
    /// Neither side has enough material to deliver checkmate.
    InsufficientMaterial,
    /// The fifty-move rule was invoked.
    FiftyMoveRule,
    /// A position repeated the required number of times.
    Repetition,
    /// A variant-specific reason not covered by the cases above.
    Other(String),
}

/// The result of a finished game.
///
/// A `None` winner denotes a draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// The winning side, or `None` for a draw.
    pub winner: Option<Color>,
    /// Why the game ended.
    pub reason: EndReason,
}

impl Outcome {
    /// Builds a decisive outcome won by `winner`.
    #[must_use]
    pub fn win(winner: Color, reason: EndReason) -> Self {
        Self {
            winner: Some(winner),
            reason,
        }
    }

    /// Builds a drawn outcome.
    #[must_use]
    pub fn draw(reason: EndReason) -> Self {
        Self {
            winner: None,
            reason,
        }
    }
}

/// Where a game is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameStatus {
    /// The game is still in progress.
    Ongoing,
    /// The game has ended with the given outcome.
    Finished(Outcome),
}

impl GameStatus {
    /// Returns `true` if the game has finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(self, GameStatus::Finished(_))
    }
}

/// The effect of successfully applying an action.
///
/// It bundles the resulting game status with the events produced by the action
/// so callers can update the lifecycle and broadcast in one step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionEffect {
    /// The game status after the action was applied.
    pub status: GameStatus,
    /// Events emitted by the action, for broadcasting to observers.
    pub events: Vec<Event>,
}
