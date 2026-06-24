//! Serde wire types for the Reconnaissance Blind Chess (RBC) variant.
//!
//! These strongly typed structs are what the variant serializes through the
//! type-erased [`Action`](mcs_core::Action),
//! [`PlayerView`](mcs_core::PlayerView), and [`Event`](mcs_core::Event)
//! newtypes from `mcs-core`. Keeping them in one place documents the exact JSON
//! shape that crosses the variant boundary — and, critically for an
//! imperfect-information variant, documents precisely which information each
//! shape is allowed to carry.
//!
//! ## The hidden-information contract
//!
//! RBC is an **imperfect-information** variant: neither player ever sees the
//! full board. A player observes only their own pieces plus the result of their
//! own latest sense. The view types here are therefore deliberately *narrow* —
//! [`RbcView`] carries a one-sided FEN (only the requesting player's pieces) and
//! that player's own sense result, and nothing that would leak the opponent's
//! hidden piece locations. See [`crate::game`] for where the redaction happens.

use mcs_core::{Color, GameStatus};
use serde::{Deserialize, Serialize};

/// The phase a player's turn is in.
///
/// An RBC turn is two steps performed by the same player: first a private
/// **sense**, then a **move**. This variant enforces that ordering, so the
/// phase tells a client which kind of action is expected next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// The side to move must sense before they may move.
    Sense,
    /// The side to move has sensed and must now play (or pass) a move.
    Move,
}

/// An action a player can submit in RBC.
///
/// The JSON representation is internally tagged on a `"type"` field:
///
/// - Sense a 3×3 window centred on a square (given in algebraic coordinates,
///   e.g. `e4`); the player privately learns which pieces sit in that window:
///   ```json
///   { "type": "sense", "square": "e4" }
///   ```
/// - Play a move, in UCI long algebraic notation (e.g. `e2e4`, `e7e8q`). The
///   move may be silently revised by the engine (a blocked slider stops short)
///   or may silently fail; the acting player observes only the partial outcome:
///   ```json
///   { "type": "move", "uci": "e2e4" }
///   ```
/// - Pass the move (the sense still happened); advances the turn:
///   ```json
///   { "type": "pass" }
///   ```
/// - Resign the game, handing the win to the opponent:
///   ```json
///   { "type": "resign" }
///   ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RbcAction {
    /// Sense a 3×3 window centred on `square` (algebraic, e.g. `"e4"`).
    Sense {
        /// The centre square of the 3×3 sense window, in algebraic notation.
        square: String,
    },
    /// Play a move, given in UCI long algebraic notation.
    Move {
        /// The move in UCI notation, e.g. `"e2e4"` or `"e7e8q"` (promotion).
        uci: String,
    },
    /// Pass the move phase without playing a move.
    Pass,
    /// Resign, handing the win to the opponent.
    Resign,
}

/// One square of a sense result: a board square and the piece on it (if any).
///
/// This is the only place a player legitimately learns about enemy pieces — and
/// only the ones inside the window *they themselves* just sensed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensedSquare {
    /// The square, in algebraic notation (e.g. `"e4"`).
    pub square: String,
    /// The piece on the square, as a FEN piece character (e.g. `"P"` for a
    /// white pawn, `"n"` for a black knight), or `None` if the square is empty.
    pub piece: Option<String>,
}

/// The result of a player's most recent sense, retained in their private view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenseSnapshot {
    /// The centre square the sense was performed from, in algebraic notation.
    pub center: String,
    /// The squares revealed by the sense, in the engine's iteration order
    /// (rank-descending, file-ascending), each with the piece found there.
    pub squares: Vec<SensedSquare>,
}

/// What a single player is permitted to observe about an RBC game.
///
/// RBC is a **perfect-information** variant's opposite: this view is redacted
/// to the requesting player. It carries only:
///
/// - `own_fen`: a board diagram showing **only the requesting player's own
///   pieces** — every opponent square is rendered empty, so the opponent's
///   hidden locations are never present in the bytes the player receives;
/// - `last_sense`: the result of that player's own most recent sense (the one
///   sanctioned channel through which they learn about a few enemy pieces);
/// - `last_capture_square`: where the opponent captured one of this player's
///   pieces on the previous turn, if any (RBC discloses this much to the victim);
/// - turn/phase/status metadata.
///
/// Example JSON:
///
/// ```json
/// {
///   "own_fen": "8/8/8/8/8/8/PPPPPPPP/RNBQKBNR",
///   "side_to_move": "white",
///   "your_color": "white",
///   "phase": "sense",
///   "last_sense": null,
///   "last_capture_square": null,
///   "status": "ongoing"
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RbcView {
    /// A board diagram (FEN piece-placement field) containing **only the
    /// requesting player's own pieces**; opponent squares are blank.
    pub own_fen: String,
    /// The colour whose turn it is to act.
    pub side_to_move: Color,
    /// The colour of the player this view is for.
    pub your_color: Color,
    /// The phase the side to move is in (sense first, then move). `None` once
    /// the game has finished.
    pub phase: Option<TurnPhase>,
    /// The requesting player's own most recent sense result, or `None` if they
    /// have not sensed yet (or not since their last move).
    pub last_sense: Option<SenseSnapshot>,
    /// The square where the opponent captured one of the requesting player's
    /// pieces on the previous turn, in algebraic notation, or `None`.
    pub last_capture_square: Option<String>,
    /// The lifecycle status of the game (ongoing or finished with an outcome).
    pub status: GameStatus,
}

/// The view a spectator is permitted to observe while the game is ongoing.
///
/// To avoid leaking hidden information to a player who might also be watching
/// the broadcast, the spectator view is **redacted** until the game ends: it
/// reveals only the turn count, whose turn it is, and the current phase — never
/// any piece location. Once the game is finished, [`spectator_view`] instead
/// returns the full final position (see [`RbcFinalView`]).
///
/// [`spectator_view`]: mcs_core::GameSession::spectator_view
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RbcSpectatorView {
    /// The colour whose turn it is to act.
    pub side_to_move: Color,
    /// The phase the side to move is in.
    pub phase: TurnPhase,
    /// The number of completed turns (plies) so far.
    pub turn_count: usize,
    /// Always [`GameStatus::Ongoing`] for this view; a finished game returns an
    /// [`RbcFinalView`] instead.
    pub status: GameStatus,
}

/// The view returned to a spectator once the game is finished.
///
/// At this point there is no hidden information left to protect, so the full
/// final board is revealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RbcFinalView {
    /// The complete final position in Forsyth–Edwards Notation.
    pub fen: String,
    /// The number of completed turns (plies) played.
    pub turn_count: usize,
    /// The finished status, carrying the game's outcome.
    pub status: GameStatus,
}

/// An event emitted by an action, for broadcasting to observers.
///
/// Like [`RbcAction`], events are internally tagged on `"type"`. Crucially,
/// these events are designed to be broadcastable to **both** players and to
/// spectators without leaking hidden information: a move event reports only that
/// the side to move acted and whether *a* capture occurred (and on which
/// square, which RBC discloses), never the full move or the opponent's
/// position. The sensing player's private result is delivered through
/// [`RbcView::last_sense`], not through a broadcast event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RbcEvent {
    /// A player sensed. Only the fact that a sense happened is broadcast; the
    /// squares and pieces learned are private to the sensing player and are
    /// surfaced through their [`RbcView::last_sense`], not here.
    Sensed {
        /// The colour that performed the sense.
        by: Color,
    },
    /// A player completed their move phase.
    ///
    /// The fields are limited to what RBC publicly discloses: whether a capture
    /// occurred and, if so, on which square. The mover's piece, origin, and the
    /// opponent's position are all withheld.
    MovePlayed {
        /// The colour that moved.
        by: Color,
        /// Whether the move captured a piece.
        captured: bool,
        /// The square on which a capture occurred, in algebraic notation, if
        /// any. RBC publicly announces the capture square to both players.
        capture_square: Option<String>,
    },
    /// The game ended.
    GameEnded {
        /// The final outcome of the game.
        outcome: mcs_core::Outcome,
    },
}
