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
//! The shapes deliberately mirror the retired `mcs-variant-standard`'s so
//! existing clients see a familiar action/view/event vocabulary. The
//! history-dependent draw claims cozy-chess provided are preserved: the
//! [`McrAction::ClaimDraw`] action and the [`McrView::can_claim_draw`] flag are
//! offered for the variants whose rules define them (the concrete FIDE-style
//! family — standard, Chess960, and the classic 8x8 variants), driven by the
//! move history the [`McrGame`](crate::McrGame) session accumulates. They differ
//! from cozy-chess's shapes only in that the move event carries no SAN (the
//! `Game` seam renders moves as UCI only).

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
/// - Claim a draw that the current position makes available (threefold
///   repetition or the fifty-move rule) — offered only when
///   [`McrView::can_claim_draw`] is `true`:
///   ```json
///   { "type": "claim_draw" }
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
    /// Claim a draw the current position makes available under a history-dependent
    /// rule (threefold repetition or the fifty-move rule). Legal only for the side
    /// to move, and only while [`McrView::can_claim_draw`] is `true`.
    ClaimDraw,
}

/// What a player (or spectator) is permitted to observe about a
/// **perfect-information** game.
///
/// Almost every variant served by this adapter is perfect-information: both
/// players and any spectator see exactly the same full board, so this one struct
/// is the view returned to every party. The one redacted exception is Fog of War,
/// whose per-player views use the narrower [`FogView`] / [`FogSpectatorView`] /
/// [`FogFinalView`] shapes instead. Jieqi (dark chess) remains unregistered —
/// mcr's `Game` seam exposes only a generic face-down marker, not the stochastic
/// per-piece hidden identity, so it is deferred (#156).
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
///   "draw_offer": null,
///   "can_claim_draw": false
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
    /// Whether the side to move may end the game right now with
    /// [`McrAction::ClaimDraw`] — `true` when the current position has repeated
    /// the threefold count or the fifty-move clock has elapsed. Always `false`
    /// for variants without these history-dependent draw rules.
    pub can_claim_draw: bool,
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

// ---------------------------------------------------------------------------
// Fog of War (Dark Chess) — the one hidden-information variant this adapter
// redacts. Its views are deliberately narrow: a player sees only their own
// pieces plus the enemy pieces their own pieces attack, and a spectator sees
// nothing but public metadata until the game ends. See [`crate::fog`] for where
// the redaction happens.
// ---------------------------------------------------------------------------

/// What a single player is permitted to observe about a Fog of War game.
///
/// Fog of War is an **imperfect-information** variant: each side sees only the
/// squares its own pieces occupy or attack. This view is therefore redacted to
/// the requesting player — [`visible_fen`](FogView::visible_fen) is a one-sided
/// board showing only what that player can see, so the opponent's hidden piece
/// locations are never present in the bytes the player receives.
///
/// Example JSON:
///
/// ```json
/// {
///   "visible_fen": "8/8/8/8/8/8/PPPPPPPP/RNBQKBNR",
///   "side_to_move": "white",
///   "your_color": "white",
///   "legal_moves_uci": ["a2a3", "a2a4", "b1a3", "..."],
///   "status": "ongoing"
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FogView {
    /// A board diagram (FEN piece-placement field) redacted to the requesting
    /// player: **all** of their own pieces, plus any opponent piece standing on
    /// a square one of their pieces attacks. Every opponent piece on an unseen
    /// square is blanked, so the opponent's hidden locations never appear here.
    pub visible_fen: String,
    /// The colour whose turn it is to move.
    pub side_to_move: Color,
    /// The colour of the player this view is for.
    pub your_color: Color,
    /// This player's legal moves in UCI notation — present only while it is
    /// their turn, and empty otherwise (and once the game has finished). A
    /// player's own move list reaches only squares they can already see, so it
    /// carries no hidden opponent information.
    pub legal_moves_uci: Vec<String>,
    /// The lifecycle status of the game (ongoing or finished with an outcome).
    pub status: GameStatus,
}

/// The view a spectator is permitted to observe **while a Fog of War game is in
/// progress**.
///
/// To avoid leaking hidden information to a player who might also be watching,
/// the spectator view is fully redacted until the game ends: it reveals only
/// whose turn it is and the move number — never a piece location. Once finished,
/// [`spectator_view`](mcs_core::GameSession::spectator_view) returns the full
/// final board through a [`FogFinalView`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FogSpectatorView {
    /// The colour whose turn it is to move.
    pub side_to_move: Color,
    /// The full-move number, from the game's FEN — harmless public metadata that
    /// discloses no piece location.
    pub fullmove_number: u32,
    /// Always [`GameStatus::Ongoing`] for this view; a finished game returns a
    /// [`FogFinalView`] instead.
    pub status: GameStatus,
}

/// The view returned to a spectator once a Fog of War game is finished.
///
/// At this point there is no hidden information left to protect, so the full
/// final board is revealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FogFinalView {
    /// The complete final position in Forsyth–Edwards Notation.
    pub fen: String,
    /// The finished status, carrying the game's outcome.
    pub status: GameStatus,
}
