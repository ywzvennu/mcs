//! Serde wire types for the mcr-backed variants.
//!
//! These strongly typed structs are what the adapter serializes through the
//! type-erased [`Action`](mcs_core::Action),
//! [`PlayerView`](mcs_core::PlayerView), and [`Event`](mcs_core::Event) newtypes
//! from `mcs-core`. Keeping them in one place documents the exact JSON shape that
//! crosses the variant boundary. A single set of types serves the whole mcr
//! catalog: every variant speaks UCI (including drop UCIs such as `P@e4` for the
//! shogi / crazyhouse family) and reports its position as an mcr-dialect FEN, so
//! no per-variant wire shape is needed.
//!
//! The shapes deliberately mirror `mcs-variant-standard`'s so existing clients
//! see a familiar action/view/event vocabulary. They differ only where the mcr
//! [`Game`](mcr::Game) seam offers less than cozy-chess does: there is no
//! `claim_draw` action and no `can_claim_draw` view flag (a bare mcr game carries
//! no move history, so the history-dependent repetition / fifty-move claims are
//! not adjudicable here), and the move event carries no SAN (the `Game` seam
//! renders moves as UCI only).

use mcs_core::{Color, GameStatus, Outcome};
use serde::{Deserialize, Serialize};

/// An action a player can submit in an mcr-backed variant.
///
/// The JSON representation is internally tagged on a `"type"` field:
///
/// - Play a move (UCI long algebraic notation, e.g. `e2e4`, `e7e8q`, or a drop
///   like `P@e4` for hand variants):
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
pub enum McrAction {
    /// Play a move, given in UCI long algebraic notation (drops included).
    Move {
        /// The move in UCI notation, e.g. `"e2e4"`, `"e7e8q"` (promotion), or
        /// `"P@e4"` (a drop from the hand).
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
/// Every variant served by this adapter is **perfect-information**: both players
/// and any spectator see exactly the same full board, so this one struct is the
/// view returned to every party. Hidden-information mcr variants (Fog of War,
/// Jieqi) are deliberately not registered here — they need per-player redaction
/// and are deferred (#156).
///
/// The `fen` is mcr's variant FEN dialect, which for hand variants (shogi,
/// crazyhouse, …) already carries the pockets / hand, so no separate field is
/// needed for drops.
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
pub struct McrView {
    /// The full position in mcr's (variant) FEN dialect, including any hand /
    /// pocket for drop variants.
    pub fen: String,
    /// The side whose turn it is to move.
    pub side_to_move: Color,
    /// Every legal move available to `side_to_move`, in UCI notation (drop UCIs
    /// included). Empty once the game has finished.
    pub legal_moves_uci: Vec<String>,
    /// The lifecycle status of the game (ongoing or finished with an outcome).
    pub status: GameStatus,
    /// Whether the side to move is currently in check (always `false` in variants
    /// whose king is not royal).
    pub check: bool,
    /// The color that currently has an outstanding (unanswered) draw offer, or
    /// `None` if no offer is pending. The opponent of this color may answer it
    /// with [`McrAction::AcceptDraw`] or [`McrAction::DeclineDraw`].
    pub draw_offer: Option<Color>,
}

/// An event emitted by an action, for broadcasting to observers.
///
/// Like [`McrAction`], events are internally tagged on `"type"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McrEvent {
    /// A move was played.
    MovePlayed {
        /// The move in UCI notation (drops included).
        uci: String,
        /// The resulting position in mcr's FEN dialect, after the move.
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
