//! Serde wire types shared by every shakmaty variant.
//!
//! These strongly typed structs are what the variants serialize through the
//! type-erased [`Action`](mcs_core::Action),
//! [`PlayerView`](mcs_core::PlayerView), and [`Event`](mcs_core::Event)
//! newtypes from `mcs-core`. They are intentionally identical in shape to the
//! `mcs-variant-standard` wire types so a client speaks one protocol across the
//! whole shakmaty family: moves are UCI long algebraic notation, and the same
//! resign / draw meta-actions apply everywhere.
//!
//! Every variant in this crate is **perfect information** — both players and
//! any spectator observe the same full board — so a single view type serves all
//! of them. The board is carried as a FEN string, which already encodes the
//! variant-specific extras shakmaty tracks: Crazyhouse pockets, Three-check
//! remaining-check counters, and Chess960 castling rights.

use mcs_core::{Color, GameStatus, Outcome};
use serde::{Deserialize, Serialize};

/// An action a player can submit in any shakmaty variant.
///
/// The JSON representation is internally tagged on a `"type"` field:
///
/// - Play a move (UCI long algebraic notation, e.g. `e2e4`, `e7e8q`; Crazyhouse
///   drops use the `@` form, e.g. `N@e4`):
///   ```json
///   { "type": "move", "uci": "e2e4" }
///   ```
/// - Resign the game (legal at any time, on your turn or your opponent's):
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
pub enum ShakmatyAction {
    /// Play a move, given in UCI long algebraic notation.
    Move {
        /// The move in UCI notation, e.g. `"e2e4"`, `"e7e8q"` (promotion), or
        /// `"N@e4"` (a Crazyhouse drop).
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
/// Every shakmaty variant in this crate is a **perfect-information** variant, so
/// both players and any spectator see exactly the same full board. The board is
/// carried as a FEN string, which encodes the variant-specific state (pockets,
/// remaining checks, castling rights) inline.
///
/// Example JSON (initial Three-check position):
///
/// ```json
/// {
///   "variant_id": "threecheck",
///   "fen": "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 3+3 0 1",
///   "side_to_move": "white",
///   "legal_moves_uci": ["a2a3", "a2a4", "b1a3", "..."],
///   "status": "ongoing",
///   "check": false,
///   "draw_offer": null
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShakmatyView {
    /// The stable identifier of the variant being played (e.g. `"atomic"`).
    pub variant_id: String,
    /// The full board position in Forsyth–Edwards Notation, including any
    /// variant-specific fields shakmaty serializes (pockets, remaining checks).
    pub fen: String,
    /// The side whose turn it is to move.
    pub side_to_move: Color,
    /// Every legal move available to `side_to_move`, in UCI notation. Empty once
    /// the game has finished.
    pub legal_moves_uci: Vec<String>,
    /// The lifecycle status of the game (ongoing or finished with an outcome).
    pub status: GameStatus,
    /// Whether the side to move is currently in check. Always `false` in
    /// variants without a royal king (Antichess, Horde for the pawn side).
    pub check: bool,
    /// The color that currently has an outstanding (unanswered) draw offer, or
    /// `None` if no offer is pending. The opponent of this color may answer it
    /// with [`ShakmatyAction::AcceptDraw`] or [`ShakmatyAction::DeclineDraw`].
    pub draw_offer: Option<Color>,
}

/// An event emitted by an action, for broadcasting to observers.
///
/// Like [`ShakmatyAction`], events are internally tagged on `"type"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShakmatyEvent {
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
