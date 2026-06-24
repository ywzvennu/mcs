//! Serde wire types for the standard-chess and Chess960 variants.
//!
//! These strongly typed structs are what both variants serialize through the
//! type-erased [`Action`](mcs_core::Action),
//! [`PlayerView`](mcs_core::PlayerView), and [`Event`](mcs_core::Event)
//! newtypes from `mcs-core`. Keeping them in one place documents the exact JSON
//! shape that crosses the variant boundary. The two variants share these types
//! and differ only in how castling moves are spelled in the UCI strings —
//! classic (`e1g1`) for `standard`, king-to-rook (`e1h1`) for `chess960`. See
//! the [crate docs](crate) for details.

use mcs_core::{Color, GameStatus, Outcome};
use serde::{Deserialize, Serialize};

/// An action a player can submit in standard chess.
///
/// The JSON representation is internally tagged on a `"type"` field:
///
/// - Play a move (UCI long algebraic notation, e.g. `e2e4`, `e7e8q`):
///   ```json
///   { "type": "move", "uci": "e2e4" }
///   ```
/// - Resign the game (legal at any time on your turn or your opponent's):
///   ```json
///   { "type": "resign" }
///   ```
/// - Offer a draw to the opponent:
///   ```json
///   { "type": "offer_draw" }
///   ```
/// - Accept a draw the opponent has offered:
///   ```json
///   { "type": "accept_draw" }
///   ```
/// - Decline a draw the opponent has offered:
///   ```json
///   { "type": "decline_draw" }
///   ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StandardAction {
    /// Play a move, given in UCI long algebraic notation.
    Move {
        /// The move in UCI notation, e.g. `"e2e4"` or `"e7e8q"` (promotion).
        uci: String,
    },
    /// Resign, handing the win to the opponent.
    Resign,
    /// Offer a draw to the opponent.
    OfferDraw,
    /// Accept a draw the opponent has offered.
    AcceptDraw,
    /// Decline a draw the opponent has offered.
    DeclineDraw,
}

/// What a player (or spectator) is permitted to observe about a game.
///
/// Standard chess is a **perfect-information** variant: both players and any
/// spectator see exactly the same full board. This struct therefore carries the
/// complete position. For an imperfect-information variant (e.g. Reconnaissance
/// Blind Chess) this type would instead be redacted per player — for example
/// hiding the opponent's pieces — and `view_for` would return different data to
/// each side.
///
/// Example JSON:
///
/// ```json
/// {
///   "fen": "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
///   "side_to_move": "white",
///   "legal_moves_uci": ["a2a3", "a2a4", "b1a3", "..."],
///   "status": "ongoing",
///   "check": false,
///   "draw_offer": null
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandardView {
    /// The full board position in Forsyth–Edwards Notation.
    pub fen: String,
    /// The side whose turn it is to move.
    pub side_to_move: Color,
    /// Every legal move available to `side_to_move`, in UCI notation. Empty
    /// once the game has finished.
    pub legal_moves_uci: Vec<String>,
    /// The lifecycle status of the game (ongoing or finished with an outcome).
    pub status: GameStatus,
    /// Whether the side to move is currently in check.
    pub check: bool,
    /// The color that currently has an outstanding (unanswered) draw offer, or
    /// `None` if no offer is pending. The opponent of this color may answer it
    /// with [`StandardAction::AcceptDraw`] or [`StandardAction::DeclineDraw`].
    pub draw_offer: Option<Color>,
}

/// An event emitted by an action, for broadcasting to observers.
///
/// Like [`StandardAction`], events are internally tagged on `"type"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StandardEvent {
    /// A move was played.
    MovePlayed {
        /// The move in UCI notation.
        uci: String,
        /// The move in SAN (standard algebraic notation), e.g. `"Nf3"`.
        san: String,
        /// The resulting board position in FEN, after the move.
        fen: String,
    },
    /// A player offered a draw.
    DrawOffered {
        /// The color that made the offer.
        by: Color,
    },
    /// A pending draw offer was declined.
    DrawDeclined {
        /// The color that declined the offer.
        by: Color,
    },
    /// The game ended.
    GameEnded {
        /// The final outcome of the game.
        outcome: Outcome,
    },
}
