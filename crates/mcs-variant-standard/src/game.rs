//! The chess [`GameSession`] implementation, backed by `cozy-chess`.
//!
//! A single [`StandardGame`] type serves both ordinary chess and Chess960; they
//! differ only in their start position and in how castling moves are spelled on
//! the wire (see [`CastlingUci`]).

use std::str::FromStr;

use cozy_chess::util::{display_san_move, display_uci_move, parse_uci_move};
use cozy_chess::{Board, GameStatus as BoardStatus, Move};
use mcs_core::{
    Action, ActionEffect, Color, EndReason, Event, GameError, GameSession, GameStatus, Outcome,
    PlayerView,
};

use crate::wire::{StandardAction, StandardEvent, StandardView};

/// The variant identifier for standard chess.
///
/// Re-exported publicly as [`crate::STANDARD_VARIANT_ID`].
pub(crate) const VARIANT_ID: &str = "standard";

/// The variant identifier for Chess960.
///
/// Re-exported publicly as [`crate::CHESS960_VARIANT_ID`].
pub(crate) const CHESS960_VARIANT_ID: &str = "chess960";

/// Number of half-moves (plies) without a pawn move or capture at which a draw
/// becomes *claimable* under the fifty-move rule (50 full moves = 100 plies).
const FIFTY_MOVE_CLAIM_PLIES: u32 = 100;

/// Number of half-moves without a pawn move or capture at which the game is
/// *automatically* drawn under FIDE's seventy-five-move rule (75 full moves =
/// 150 plies). No claim is required.
const SEVENTY_FIVE_MOVE_PLIES: u32 = 150;

/// Number of times a position must repeat for a draw to become *claimable*
/// under the threefold-repetition rule.
const THREEFOLD_REPETITIONS: usize = 3;

/// Number of times a position must repeat for the game to be *automatically*
/// drawn under FIDE's fivefold-repetition rule. No claim is required.
const FIVEFOLD_REPETITIONS: usize = 5;

/// How castling moves are spelled in the UCI strings that cross the wire.
///
/// `cozy-chess` always represents castling internally as *king-captures-own-rook*
/// (Fischer-random / FRC style, e.g. `e1h1` for White kingside). That is correct
/// for Chess960, where the rook can start on any file, but standard clients and
/// the existing protocol expect the classic two-square king move (`e1g1`,
/// `e1c1`, `e8g8`, `e8c8`). This enum selects which spelling the variant accepts
/// from, and renders to, clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastlingUci {
    /// Classic UCI: castling is the two-square king move (`e1g1` / `e1c1`).
    ///
    /// Used by the `standard` variant so existing clients are unaffected.
    Classic,
    /// UCI_960 / king-to-rook: castling targets the rook's square (`e1h1`).
    ///
    /// Used by the `chess960` variant, where the rook file is not fixed.
    KingToRook,
}

/// A single in-progress game of standard chess or Chess960.
///
/// Wraps a [`cozy_chess::Board`] — the source of truth for all rules — plus the
/// bookkeeping the board does not track for us: which variant this is (for the
/// reported id and the castling-UCI convention), a pending draw offer, and the
/// final outcome once the game is over (so resignations and draw agreements,
/// which are not board states, can be recorded).
#[derive(Debug)]
pub struct StandardGame {
    /// The underlying board. `cozy-chess` enforces all move legality.
    board: Board,
    /// The id this session reports (`"standard"` or `"chess960"`).
    variant_id: &'static str,
    /// How castling moves are spelled on the wire for this variant.
    castling_uci: CastlingUci,
    /// The color with an outstanding, unanswered draw offer, if any.
    draw_offer: Option<Color>,
    /// The FIDE position key (cozy-chess [`Board::hash`]) of every position that
    /// has occurred, including the starting position. The hash captures piece
    /// placement, side to move, castling rights, and the en-passant square —
    /// exactly the FIDE notion of position identity for repetition — but not the
    /// move counters. A position is repeated `n` times when its key appears `n`
    /// times in this history.
    position_history: Vec<u64>,
    /// Our own half-move (ply) clock for the fifty/seventy-five-move rules.
    ///
    /// cozy-chess tracks a halfmove clock too, but it *saturates at 100* (it can
    /// never report 150), and its [`Board::status`] auto-draws at 100 — which is
    /// the *claim* threshold here, not an automatic one. So we keep an
    /// independent, unbounded counter: reset to 0 on a pawn move or capture (the
    /// irreversible moves), incremented otherwise.
    halfmove_clock: u32,
    /// The recorded outcome once the game has finished. `None` while ongoing.
    ///
    /// Board-driven endings (checkmate, stalemate, insufficient material) can
    /// always be re-derived from `board`, but resignation, draw-agreement, and
    /// claimed draws cannot, so the outcome is stored explicitly the moment the
    /// game ends for any reason.
    outcome: Option<Outcome>,
}

impl Default for StandardGame {
    /// Creates a standard game from the standard initial position.
    fn default() -> Self {
        Self::new()
    }
}

impl StandardGame {
    /// Creates a new standard-chess game from the initial position.
    #[must_use]
    pub fn new() -> Self {
        Self::from_board(Board::default(), VARIANT_ID, CastlingUci::Classic)
    }

    /// Creates a new Chess960 game from the standard initial position.
    ///
    /// For an arbitrary starting layout use [`StandardGame::chess960`] or
    /// [`StandardGame::from_fen`].
    #[must_use]
    pub fn new_chess960() -> Self {
        Self::from_board(
            Board::default(),
            CHESS960_VARIANT_ID,
            CastlingUci::KingToRook,
        )
    }

    /// Creates a Chess960 game from a Scharnagl start-position number.
    ///
    /// `position` is the standard 0..=959 Chess960 numbering; position `518` is
    /// the classical chess setup.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::InvalidActionPayload`] if `position` is `>= 960`.
    pub fn chess960(position: u32) -> Result<Self, GameError> {
        if position >= 960 {
            return Err(GameError::InvalidActionPayload(format!(
                "chess960 position must be in 0..=959, got {position}"
            )));
        }
        Ok(Self::from_board(
            Board::chess960_startpos(position),
            CHESS960_VARIANT_ID,
            CastlingUci::KingToRook,
        ))
    }

    /// Creates a Chess960 game from a starting FEN.
    ///
    /// Accepts both ordinary FEN (`KQkq`-style castling fields) and Shredder /
    /// X-FEN (`HAha`-style, naming the rook files) — the latter is unambiguous
    /// for the off-centre rook placements Chess960 allows, so it is tried as a
    /// fallback when the standard interpretation does not validate.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::InvalidActionPayload`] if `fen` is not a valid
    /// position under either interpretation.
    pub fn from_fen(fen: &str) -> Result<Self, GameError> {
        // Try standard FEN first; fall back to Shredder FEN, which is the
        // unambiguous form for arbitrary Chess960 castling rights.
        let board = Board::from_fen(fen, false)
            .or_else(|_| Board::from_fen(fen, true))
            .map_err(|e| GameError::InvalidActionPayload(format!("invalid FEN '{fen}': {e}")))?;
        Ok(Self::from_board(
            board,
            CHESS960_VARIANT_ID,
            CastlingUci::KingToRook,
        ))
    }

    /// Internal constructor shared by every entry point.
    fn from_board(board: Board, variant_id: &'static str, castling_uci: CastlingUci) -> Self {
        // Seed the position history with the starting position and the halfmove
        // clock from the board, so games resumed from a FEN inherit the right
        // counters (cozy-chess accepts halfmove clocks up to 100).
        let position_history = vec![board.hash()];
        let halfmove_clock = u32::from(board.halfmove_clock());
        Self {
            board,
            variant_id,
            castling_uci,
            draw_offer: None,
            position_history,
            halfmove_clock,
            outcome: None,
        }
    }

    /// How many times the *current* position has occurred so far, by FIDE
    /// position identity (the cozy-chess hash).
    fn repetition_count(&self) -> usize {
        let current = self.board.hash();
        self.position_history
            .iter()
            .filter(|&&key| key == current)
            .count()
    }

    /// Which kind of draw, if any, the side to move may currently *claim*.
    ///
    /// A draw is claimable when the current position has occurred three or more
    /// times (threefold repetition) or the halfmove clock has reached 100 plies
    /// (the fifty-move rule). Threefold is checked first, matching the order a
    /// player would typically invoke. Returns `None` when no claim is available
    /// or the game is already over.
    fn claimable_draw(&self) -> Option<EndReason> {
        if self.outcome.is_some() {
            return None;
        }
        if self.repetition_count() >= THREEFOLD_REPETITIONS {
            Some(EndReason::Repetition)
        } else if self.halfmove_clock >= FIFTY_MOVE_CLAIM_PLIES {
            Some(EndReason::FiftyMoveRule)
        } else {
            None
        }
    }

    /// Maps a `cozy-chess` color onto the core [`Color`].
    fn from_cc_color(color: cozy_chess::Color) -> Color {
        match color {
            cozy_chess::Color::White => Color::White,
            cozy_chess::Color::Black => Color::Black,
        }
    }

    /// Renders the current position as a FEN string.
    fn fen(&self) -> String {
        // The default `Display` produces standard (FIDE) FEN with `KQkq`-style
        // castling fields, which is what clients expect for both variants.
        self.board.to_string()
    }

    /// Renders a single move as a wire UCI string under this variant's
    /// castling convention.
    fn render_uci(&self, mv: Move) -> String {
        match self.castling_uci {
            // Classic UCI: translate cozy-chess's king-to-rook castle back to the
            // two-square king move (`e1h1` -> `e1g1`).
            CastlingUci::Classic => display_uci_move(&self.board, mv).to_string(),
            // Chess960: keep the king-to-rook spelling cozy-chess emits.
            CastlingUci::KingToRook => mv.to_string(),
        }
    }

    /// Parses a wire UCI string into a board move under this variant's castling
    /// convention.
    fn parse_uci(&self, uci: &str) -> Result<Move, GameError> {
        let parsed = match self.castling_uci {
            // Classic UCI: translate `e1g1`/`e1c1` to cozy-chess's king-to-rook
            // form against the current position.
            CastlingUci::Classic => parse_uci_move(&self.board, uci),
            // Chess960 already uses king-to-rook UCI, so parse it directly.
            CastlingUci::KingToRook => Move::from_str(uci),
        };
        parsed.map_err(|e| GameError::InvalidActionPayload(format!("invalid UCI '{uci}': {e}")))
    }

    /// Returns every legal move in the current position as wire UCI strings.
    fn legal_moves_uci(&self) -> Vec<String> {
        let mut moves = Vec::new();
        self.board.generate_moves(|piece_moves| {
            for mv in piece_moves {
                moves.push(self.render_uci(mv));
            }
            false
        });
        moves
    }

    /// Whether the side to move is currently in check.
    fn is_check(&self) -> bool {
        !self.board.checkers().is_empty()
    }

    /// Builds the full, perfect-information view of the current position.
    ///
    /// The same view is returned to both players and to spectators, because
    /// neither standard chess nor Chess960 hides any information.
    fn build_view(&self) -> StandardView {
        StandardView {
            fen: self.fen(),
            side_to_move: Self::from_cc_color(self.board.side_to_move()),
            // Once the game is over there are no further legal moves to offer.
            legal_moves_uci: if self.outcome.is_some() {
                Vec::new()
            } else {
                self.legal_moves_uci()
            },
            status: self.status(),
            check: self.is_check(),
            draw_offer: self.draw_offer,
            can_claim_draw: self.claimable_draw().is_some(),
        }
    }

    /// Derives the *automatic* outcome of the current position, if the game has
    /// reached a forced termination: checkmate, stalemate, insufficient material,
    /// fivefold repetition, or the seventy-five-move rule.
    ///
    /// [`Board::status`] reports `Won` only when the side to move has no legal
    /// moves *and* is in check — i.e. it has just been checkmated — so the winner
    /// is always the side that did not just move. A `Drawn` status with no legal
    /// moves is stalemate.
    ///
    /// The fifty-move and threefold-repetition rules are **claimable**, not
    /// automatic, and so are deliberately *not* terminated here (cozy-chess's own
    /// `Drawn`-at-100-plies result is therefore ignored while legal moves remain
    /// — that case only makes a draw claimable, handled by [`Self::claimable_draw`]).
    /// Their forced FIDE counterparts — fivefold repetition and the
    /// seventy-five-move rule — *are* automatic and are detected here.
    ///
    /// cozy-chess does **not** itself terminate on insufficient material, so we
    /// detect dead positions explicitly and report them as a draw, matching FIDE
    /// rules and the previous engine's behaviour.
    fn board_outcome(&self) -> Option<Outcome> {
        match self.board.status() {
            BoardStatus::Won => {
                // The side to move is mated, so the *other* side delivered it.
                let winner = Self::from_cc_color(!self.board.side_to_move());
                Some(Outcome::win(winner, EndReason::Checkmate))
            }
            // A `Drawn` status with no legal moves is stalemate; a `Drawn` status
            // with moves still available is the clamped fifty-move-rule case,
            // which is only claimable (handled elsewhere), not automatic.
            BoardStatus::Drawn if self.no_legal_moves() => {
                Some(Outcome::draw(EndReason::Stalemate))
            }
            _ => {
                // Forced (un-claimed) automatic draws take precedence over the
                // claimable ones, then dead positions.
                if self.halfmove_clock >= SEVENTY_FIVE_MOVE_PLIES {
                    Some(Outcome::draw(EndReason::FiftyMoveRule))
                } else if self.repetition_count() >= FIVEFOLD_REPETITIONS {
                    Some(Outcome::draw(EndReason::Repetition))
                } else if self.is_insufficient_material() {
                    // cozy-chess keeps dead positions `Ongoing`; terminate here.
                    Some(Outcome::draw(EndReason::InsufficientMaterial))
                } else {
                    None
                }
            }
        }
    }

    /// Whether the side to move has no legal moves in the current position.
    fn no_legal_moves(&self) -> bool {
        // `generate_moves` returns `true` only if the closure short-circuited,
        // which happens as soon as the first move is produced.
        !self.board.generate_moves(|_| true)
    }

    /// Whether the position is a dead draw by insufficient mating material.
    ///
    /// Covers the FIDE "impossibility of checkmate" cases that can never be won
    /// by either side regardless of play: king versus king, king and a single
    /// minor piece (bishop or knight) versus king, and king and bishop versus
    /// king and bishop with both bishops on the same colour squares. Positions
    /// with any pawn, rook, or queen — or with enough minor pieces to force mate
    /// — are not dead and return `false`.
    fn is_insufficient_material(&self) -> bool {
        let board = &self.board;
        // Any pawn, rook, or queen means mate is still possible.
        if !(board.pieces(cozy_chess::Piece::Pawn)
            | board.pieces(cozy_chess::Piece::Rook)
            | board.pieces(cozy_chess::Piece::Queen))
        .is_empty()
        {
            return false;
        }

        let knights = board.pieces(cozy_chess::Piece::Knight);
        let bishops = board.pieces(cozy_chess::Piece::Bishop);
        let minors = knights.len() + bishops.len();

        match minors {
            // K vs K.
            0 => true,
            // K + single minor vs K: cannot force mate.
            1 => true,
            // K + B vs K + B is dead only if the bishops are same-coloured.
            2 if knights.is_empty() => {
                let dark = (bishops & cozy_chess::BitBoard::DARK_SQUARES).len();
                // Both bishops dark, or both light.
                dark == 0 || dark == 2
            }
            _ => false,
        }
    }

    /// Applies a UCI move on behalf of `player`, returning the resulting effect.
    fn apply_move(&mut self, player: Color, uci: &str) -> Result<ActionEffect, GameError> {
        // It must be the mover's turn.
        if Self::from_cc_color(self.board.side_to_move()) != player {
            return Err(GameError::NotYourTurn);
        }

        // Parse the UCI string, then validate it against the current position.
        let mv = self.parse_uci(uci)?;
        if !self.board.is_legal(mv) {
            return Err(GameError::IllegalAction);
        }

        // Capture SAN and the canonical wire UCI *before* playing, since both are
        // rendered against the pre-move position.
        let san = display_san_move(&self.board, mv).to_string();
        let played_uci = self.render_uci(mv);
        // The move was validated as legal above, so this cannot fail.
        self.board.play(mv);

        // Update our own halfmove clock. cozy-chess resets *its* clock to 0 on
        // an irreversible move (pawn move or capture) and increments it
        // otherwise, so mirror that decision — but keep our own unbounded count,
        // since the board's saturates at 100.
        if self.board.halfmove_clock() == 0 {
            self.halfmove_clock = 0;
        } else {
            self.halfmove_clock += 1;
        }

        // Record the new position for repetition counting.
        self.position_history.push(self.board.hash());

        // A move always supersedes any pending draw offer.
        self.draw_offer = None;

        let mut events = vec![Event::from_typed(&StandardEvent::MovePlayed {
            uci: played_uci,
            san,
            fen: self.fen(),
        })?];

        // Detect an automatic termination produced by the move.
        if let Some(outcome) = self.board_outcome() {
            self.outcome = Some(outcome.clone());
            events.push(Event::from_typed(&StandardEvent::GameEnded { outcome })?);
        }

        Ok(ActionEffect {
            status: self.status(),
            events,
        })
    }

    /// Records a final outcome and emits the matching `GameEnded` event.
    fn finish(&mut self, outcome: Outcome) -> Result<ActionEffect, GameError> {
        self.outcome = Some(outcome.clone());
        self.draw_offer = None;
        Ok(ActionEffect {
            status: self.status(),
            events: vec![Event::from_typed(&StandardEvent::GameEnded { outcome })?],
        })
    }
}

impl GameSession for StandardGame {
    fn variant_id(&self) -> &'static str {
        self.variant_id
    }

    fn to_move(&self) -> Color {
        Self::from_cc_color(self.board.side_to_move())
    }

    fn status(&self) -> GameStatus {
        match &self.outcome {
            Some(outcome) => GameStatus::Finished(outcome.clone()),
            None => GameStatus::Ongoing,
        }
    }

    /// The actions `player` may submit right now.
    ///
    /// While the game is ongoing, the side to move gets every legal chess move
    /// plus the meta-actions available to them: resign, and either offer a draw
    /// or answer an outstanding offer. A player may **resign at any time** —
    /// even on the opponent's turn — so the non-moving side is still offered
    /// resignation (and the chance to answer a draw the opponent offered). Once
    /// the game is finished, no actions are legal.
    fn legal_actions(&self, player: Color) -> Vec<Action> {
        if self.outcome.is_some() {
            return Vec::new();
        }

        let mut actions = Vec::new();
        let serialize = |action: &StandardAction| {
            Action::from_typed(action).expect("standard actions always serialize")
        };

        if self.to_move() == player {
            // The side to move gets all legal moves.
            for uci in self.legal_moves_uci() {
                actions.push(serialize(&StandardAction::Move { uci }));
            }
            // ...and may claim a draw if the position is currently eligible
            // (threefold repetition or the fifty-move rule).
            if self.claimable_draw().is_some() {
                actions.push(serialize(&StandardAction::ClaimDraw));
            }
        }

        // Resignation is always available to either player.
        actions.push(serialize(&StandardAction::Resign));

        // Draw handling: if the opponent has an offer outstanding, this player
        // may accept or decline it; otherwise this player may make an offer
        // (provided they do not already have one pending).
        match self.draw_offer {
            Some(offerer) if offerer == player.opposite() => {
                actions.push(serialize(&StandardAction::AcceptDraw));
                actions.push(serialize(&StandardAction::DeclineDraw));
            }
            Some(_) => {} // This player already has an offer pending.
            None => actions.push(serialize(&StandardAction::OfferDraw)),
        }

        actions
    }

    fn apply(&mut self, player: Color, action: &Action) -> Result<ActionEffect, GameError> {
        if self.outcome.is_some() {
            return Err(GameError::Finished);
        }

        let action: StandardAction = action
            .to_typed()
            .map_err(|e| GameError::InvalidActionPayload(e.to_string()))?;

        match action {
            StandardAction::Move { uci } => self.apply_move(player, &uci),
            // Resigning hands the win to the opponent.
            StandardAction::Resign => {
                self.finish(Outcome::win(player.opposite(), EndReason::Resignation))
            }
            StandardAction::OfferDraw => {
                // An offer is a no-op if this player already has one pending; it
                // simply produces no effect rather than erroring.
                if self.draw_offer == Some(player) {
                    return Ok(ActionEffect {
                        status: self.status(),
                        events: Vec::new(),
                    });
                }
                self.draw_offer = Some(player);
                Ok(ActionEffect {
                    status: self.status(),
                    events: vec![Event::from_typed(&StandardEvent::DrawOffered {
                        by: player,
                    })?],
                })
            }
            StandardAction::AcceptDraw => {
                // Only the opponent of the offerer may accept.
                if self.draw_offer == Some(player.opposite()) {
                    self.finish(Outcome::draw(EndReason::DrawAgreement))
                } else {
                    Err(GameError::IllegalAction)
                }
            }
            StandardAction::ClaimDraw => {
                // A draw claim stands in for a move, so only the side to move
                // may claim, and only when the position is actually eligible.
                if self.to_move() != player {
                    return Err(GameError::NotYourTurn);
                }
                match self.claimable_draw() {
                    Some(reason) => self.finish(Outcome::draw(reason)),
                    None => Err(GameError::IllegalAction),
                }
            }
            StandardAction::DeclineDraw => {
                if self.draw_offer == Some(player.opposite()) {
                    self.draw_offer = None;
                    Ok(ActionEffect {
                        status: self.status(),
                        events: vec![Event::from_typed(&StandardEvent::DrawDeclined {
                            by: player,
                        })?],
                    })
                } else {
                    Err(GameError::IllegalAction)
                }
            }
        }
    }

    fn view_for(&self, player: Color) -> PlayerView {
        // Perfect information: every player sees the same full board.
        let _ = player;
        PlayerView::from_typed(&self.build_view()).expect("standard view always serializes")
    }

    fn spectator_view(&self) -> PlayerView {
        // Identical to a player's view: standard chess hides nothing.
        PlayerView::from_typed(&self.build_view()).expect("standard view always serializes")
    }

    fn outcome(&self) -> Option<Outcome> {
        self.outcome.clone()
    }
}
