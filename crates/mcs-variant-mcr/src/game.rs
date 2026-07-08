//! The mcr-backed [`GameSession`] implementation.
//!
//! A single [`McrGame`] type serves every perfect-information variant in mcr's
//! catalog. It wraps an [`mcr::Game`] — the source of truth for all rules and
//! move generation — and adapts it to the variant-agnostic boundary types,
//! adding only the non-board mechanics the server needs (resignation and draw
//! offers), which are not positions mcr can represent.

use mcr::{Game, GameOutcome, VariantRef};
use mcs_core::{
    Action, ActionEffect, Color, EndReason, Event, GameError, GameSession, GameStatus, Outcome,
    PlayerView,
};

use crate::wire::{McrAction, McrEvent, McrView};

/// Number of times a position must occur for a draw to become *claimable* under
/// the threefold-repetition rule. FIDE's claim threshold; mcr's own move-clock
/// draw rules expose the *automatic* fold (five), not this one, so it is named
/// here explicitly.
const THREEFOLD_REPETITION: usize = 3;

/// Halfmove-clock ply count (75 full moves) at which a position is an
/// *automatic* draw under the seventy-five-move rule. mcr's `Game::outcome`
/// already terminates on this from the single position; the adapter only reads
/// it to label the reason precisely.
const SEVENTY_FIVE_MOVE_PLIES: u32 = 150;

/// The history-dependent FIDE-style draw parameters of a concrete 8x8 variant
/// (standard, Chess960, and the classic 8x8 variants), resolved once from the
/// variant's [`mcr` draw rules](mcr::VariantRef::rules) when the game is created.
///
/// These are the draws a single position cannot see — they need the move history
/// the [`McrGame`] accumulates. The purely single-position terminations
/// (checkmate, stalemate, insufficient material, the seventy-five-move rule) are
/// left to [`Game::outcome`] and are not represented here.
#[derive(Debug, Clone, Copy)]
struct FideDraw {
    /// Halfmove-clock ply count at which the move-count ("fifty-move") draw
    /// becomes claimable, from [`mcr`'s `move_rule_plies`](mcr::geometry::rules::DrawRules).
    /// `None` for a variant with no move-count draw rule.
    move_rule_claim_plies: Option<u32>,
    /// Whether this variant keeps a position history for repetition draws.
    tracks_repetition: bool,
    /// Position-occurrence count at which the game is *automatically* drawn by
    /// repetition, from [`mcr`'s `repetition_fold`](mcr::geometry::rules::DrawRules)
    /// (five for the concrete family).
    repetition_auto_fold: usize,
}

impl FideDraw {
    /// Resolves the FIDE-style draw parameters for `variant`, or `None` for the
    /// wide-geometry fairy variants whose history-dependent draws (sennichite,
    /// perpetual check / chase, bikjang, counting, …) are out of this adapter's
    /// scope and deferred with the rest of the wide-variant history rules.
    ///
    /// The concrete 8x8 family shares one FIDE-style draw model (see mcr's
    /// `concrete_draw`): a claimable fifty-move rule at 100 plies, claimable
    /// threefold repetition, and automatic fivefold repetition. Gating on the
    /// concrete family keeps those rules off the wide variants, whose repetition
    /// semantics differ and must not be adjudicated as western threefold draws.
    fn resolve(variant: VariantRef) -> Option<FideDraw> {
        if !matches!(variant, VariantRef::Concrete(_)) {
            return None;
        }
        let draw = variant.rules().draw;
        Some(FideDraw {
            move_rule_claim_plies: draw.move_rule_plies.map(u32::from),
            tracks_repetition: draw.tracks_repetition,
            repetition_auto_fold: draw.repetition_fold,
        })
    }
}

/// A single in-progress game of some perfect-information mcr variant.
///
/// Wraps an [`mcr::Game`] — which enforces all rules and answers every
/// single-position question (legal moves, check, terminal outcome) — plus the
/// small amount of bookkeeping the engine does not track for us: which variant
/// this is (for the reported id), a pending draw offer, and the final outcome
/// once the game is over (so resignations and draw agreements, which are not
/// board states, can be recorded).
///
/// `mcr::Game` is a *single-position* handle with no move history, so mcr reports
/// only the automatic single-position terminations (checkmate, stalemate,
/// insufficient material, the seventy-five-move rule) through [`Game::outcome`].
/// The history-dependent FIDE draws — threefold / fivefold repetition and the
/// fifty-move claim — are adjudicated here instead, from the position history
/// this session accumulates, for the concrete FIDE-style family (see
/// [`FideDraw`]). The wide-geometry fairy variants' history rules (sennichite,
/// perpetual check / chase, …) remain out of scope. Everything the board cannot
/// express — resignation, draw offers and agreements, a claimed draw — ends the
/// game through a meta-action.
#[derive(Debug)]
pub struct McrGame {
    /// The underlying mcr game; enforces all rules and generates all moves. It
    /// is replaced by its successor on every move (mcr games are immutable —
    /// [`Game::play`] returns the next position).
    game: Game,
    /// The id this session reports, the variant's canonical mcr name.
    variant_id: &'static str,
    /// The color with an outstanding, unanswered draw offer, if any.
    draw_offer: Option<Color>,
    /// The FIDE-style history-dependent draw parameters for this variant, or
    /// `None` for the wide fairy variants (whose history draws are out of scope).
    /// When `Some`, the fifty-move / repetition claims and automatic fivefold
    /// repetition are adjudicated from [`Self::position_history`].
    fide: Option<FideDraw>,
    /// The FIDE position identity (see [`Self::position_key`]) of every position
    /// that has occurred, including the starting one, oldest first. Maintained
    /// only when [`Self::fide`] tracks repetition; a position is repeated `n`
    /// times when its key appears `n` times here.
    position_history: Vec<String>,
    /// The recorded outcome once the game has finished. `None` while ongoing.
    ///
    /// Board-driven endings can always be re-derived from `game`, but
    /// resignation, draw agreement, and claimed draws cannot, so the outcome is
    /// stored explicitly the moment the game ends for any reason.
    outcome: Option<Outcome>,
}

impl McrGame {
    /// Creates a new game of `variant` from its starting position.
    #[must_use]
    pub fn new(variant: VariantRef) -> Self {
        Self::wrap(variant, Game::new(variant))
    }

    /// Builds a session around an already-constructed `game` of `variant`,
    /// resolving its draw rules and seeding the repetition history with the
    /// starting position.
    fn wrap(variant: VariantRef, game: Game) -> Self {
        let fide = FideDraw::resolve(variant);
        let mut this = Self {
            game,
            variant_id: variant.name(),
            draw_offer: None,
            fide,
            position_history: Vec::new(),
            outcome: None,
        };
        // Seed the history with the starting position so it counts as its own
        // first occurrence (only when repetition is tracked for this variant).
        if this.tracks_repetition() {
            this.position_history.push(this.position_key());
        }
        this
    }

    /// Creates a game of `variant` from a starting FEN in mcr's dialect.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::InvalidActionPayload`] if `fen` is not a valid
    /// position for `variant`.
    pub fn from_fen(variant: VariantRef, fen: &str) -> Result<Self, GameError> {
        let game = Game::from_fen(variant, fen)
            .map_err(|e| GameError::InvalidActionPayload(format!("invalid FEN '{fen}': {e}")))?;
        Ok(Self::wrap(variant, game))
    }

    /// Whether this variant keeps a position history for repetition draws.
    fn tracks_repetition(&self) -> bool {
        self.fide.is_some_and(|d| d.tracks_repetition)
    }

    /// The FIDE position identity of the current position: the first four fields
    /// of the FEN (piece placement, side to move, castling rights, en-passant
    /// square) — exactly what FIDE counts as the same position for repetition,
    /// excluding the two move clocks.
    fn position_key(&self) -> String {
        let fen = self.game.fen();
        fen.split_whitespace().take(4).collect::<Vec<_>>().join(" ")
    }

    /// The halfmove clock (plies since the last capture or pawn move), read from
    /// the fifth FEN field. Zero when the FEN carries no such field.
    fn halfmove_clock(&self) -> u32 {
        self.game
            .fen()
            .split_whitespace()
            .nth(4)
            .and_then(|field| field.parse().ok())
            .unwrap_or(0)
    }

    /// How many times the current position has occurred so far, by FIDE position
    /// identity. Zero when repetition is not tracked for this variant.
    fn repetition_count(&self) -> usize {
        let current = self.position_key();
        self.position_history
            .iter()
            .filter(|key| **key == current)
            .count()
    }

    /// Which claimable draw, if any, the side to move may invoke right now —
    /// threefold repetition (checked first) or the fifty-move rule. `None` when
    /// no claim is available, the game is over, or the variant has no such rules.
    fn claimable_draw(&self) -> Option<EndReason> {
        if self.outcome.is_some() {
            return None;
        }
        let fide = self.fide?;
        if fide.tracks_repetition && self.repetition_count() >= THREEFOLD_REPETITION {
            return Some(EndReason::Repetition);
        }
        if let Some(plies) = fide.move_rule_claim_plies {
            if self.halfmove_clock() >= plies {
                return Some(EndReason::FiftyMoveRule);
            }
        }
        None
    }

    /// The *automatic* history-dependent draw the current position triggers —
    /// fivefold repetition — which a single position (and so [`Game::outcome`])
    /// cannot see. `None` when it does not apply.
    fn auto_history_draw(&self) -> Option<Outcome> {
        let fide = self.fide?;
        if fide.tracks_repetition && self.repetition_count() >= fide.repetition_auto_fold {
            Some(Outcome::draw(EndReason::Repetition))
        } else {
            None
        }
    }

    /// Maps an mcr color onto the core [`Color`].
    fn from_mcr_color(color: mcr::Color) -> Color {
        match color {
            mcr::Color::White => Color::White,
            mcr::Color::Black => Color::Black,
        }
    }

    /// Every legal move in the current position as UCI strings, or empty once the
    /// game has finished.
    fn legal_moves_uci(&self) -> Vec<String> {
        if self.outcome.is_some() {
            Vec::new()
        } else {
            self.game.legal_ucis()
        }
    }

    /// Builds the full, perfect-information view of the current position, shared
    /// by both players and spectators.
    fn build_view(&self) -> McrView {
        McrView {
            fen: self.game.fen(),
            side_to_move: self.to_move(),
            legal_moves_uci: self.legal_moves_uci(),
            status: self.status(),
            check: self.game.is_check(),
            draw_offer: self.draw_offer,
            can_claim_draw: self.claimable_draw().is_some(),
        }
    }

    /// Maps an mcr [`GameOutcome`] onto the core [`Outcome`].
    ///
    /// mcr's `Game` seam reports only *winner-or-draw* — it does not label the
    /// terminal reason — so the reason is derived from the check status of the
    /// finished position: a decisive result with the side to move in check is a
    /// checkmate, and a drawn result with the side to move not in check is a
    /// stalemate. Variant-specific terminals that fit neither shape (extinction,
    /// a racing-kings goal, the three-check counter, an antichess wipeout) are
    /// reported as [`EndReason::Other`], since the seam does not distinguish
    /// them. Called after the terminating move has been applied, so `self.game`
    /// is already the final position.
    fn map_outcome(&self, outcome: GameOutcome) -> Outcome {
        match outcome.winner() {
            Some(winner) => {
                let winner = Self::from_mcr_color(winner);
                let reason = if self.game.is_check() {
                    EndReason::Checkmate
                } else {
                    EndReason::Other("variant win condition".to_owned())
                };
                Outcome::win(winner, reason)
            }
            None => Outcome::draw(self.single_position_draw_reason()),
        }
    }

    /// The precise reason for a drawn single-position termination reported by
    /// [`Game::outcome`], derived from the final position.
    ///
    /// For the concrete FIDE-style family the reason is disambiguated: no legal
    /// move is a stalemate; otherwise a drawn position with the halfmove clock at
    /// the seventy-five-move threshold is the move-clock draw and anything else is
    /// insufficient material (repetition is handled separately, not here). The
    /// wide fairy variants keep the coarse label the seam can justify.
    fn single_position_draw_reason(&self) -> EndReason {
        if self.fide.is_some() {
            if self.game.legal_moves().is_empty() {
                return EndReason::Stalemate;
            }
            if self.halfmove_clock() >= SEVENTY_FIVE_MOVE_PLIES {
                return EndReason::FiftyMoveRule;
            }
            return EndReason::InsufficientMaterial;
        }
        if self.game.is_check() {
            EndReason::Other("draw".to_owned())
        } else {
            EndReason::Stalemate
        }
    }

    /// Applies a UCI move on behalf of `player`, returning the resulting effect.
    fn apply_move(&mut self, player: Color, uci: &str) -> Result<ActionEffect, GameError> {
        // It must be the mover's turn.
        if self.to_move() != player {
            return Err(GameError::NotYourTurn);
        }

        // Resolve the UCI string against the current position. mcr's `parse_uci`
        // returns the move only if it names a *legal* one, so this rejects both
        // malformed and illegal strings.
        let mv = self.game.parse_uci(uci).ok_or(GameError::IllegalAction)?;
        // Render the canonical UCI for the event before advancing.
        let played_uci = self.game.to_uci(&mv);
        // The move was validated as legal above, so this cannot panic.
        self.game = self.game.play(&mv);

        // Record the new position for repetition counting (concrete family only).
        if self.tracks_repetition() {
            self.position_history.push(self.position_key());
        }

        // A move always supersedes any pending draw offer.
        self.draw_offer = None;

        let mut events = vec![Event::from_typed(&McrEvent::MovePlayed {
            uci: played_uci,
            fen: self.game.fen(),
        })?];

        // Detect an automatic termination produced by the move: first the
        // single-position terminations mcr reports (checkmate, stalemate,
        // insufficient material, the seventy-five-move rule), then the automatic
        // history-dependent draw the seam cannot see (fivefold repetition).
        let ended = self
            .game
            .outcome()
            .map(|game_outcome| self.map_outcome(game_outcome))
            .or_else(|| self.auto_history_draw());
        if let Some(outcome) = ended {
            self.outcome = Some(outcome.clone());
            events.push(Event::from_typed(&McrEvent::GameEnded { outcome })?);
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
            events: vec![Event::from_typed(&McrEvent::GameEnded { outcome })?],
        })
    }
}

impl GameSession for McrGame {
    fn variant_id(&self) -> &'static str {
        self.variant_id
    }

    fn to_move(&self) -> Color {
        Self::from_mcr_color(self.game.to_move())
    }

    fn status(&self) -> GameStatus {
        match &self.outcome {
            Some(outcome) => GameStatus::Finished(outcome.clone()),
            None => GameStatus::Ongoing,
        }
    }

    /// The actions `player` may submit right now.
    ///
    /// While the game is ongoing, the side to move gets every legal move plus the
    /// meta-actions available to them: resign, and either offer a draw or answer
    /// an outstanding offer. A player may **resign at any time** — even on the
    /// opponent's turn — so the non-moving side is still offered resignation (and
    /// the chance to answer a draw the opponent offered). Once the game is
    /// finished, no actions are legal.
    fn legal_actions(&self, player: Color) -> Vec<Action> {
        if self.outcome.is_some() {
            return Vec::new();
        }

        let mut actions = Vec::new();
        let serialize =
            |action: &McrAction| Action::from_typed(action).expect("mcr actions always serialize");

        if self.to_move() == player {
            for uci in self.legal_moves_uci() {
                actions.push(serialize(&McrAction::Move { uci }));
            }
            // ...and may claim a draw when the position is currently eligible
            // (threefold repetition or the fifty-move rule).
            if self.claimable_draw().is_some() {
                actions.push(serialize(&McrAction::ClaimDraw));
            }
        }

        // Resignation is always available to either player.
        actions.push(serialize(&McrAction::Resign));

        // Draw handling: if the opponent has an offer outstanding, this player
        // may accept or decline it; otherwise this player may make an offer
        // (provided they do not already have one pending).
        match self.draw_offer {
            Some(offerer) if offerer == player.opposite() => {
                actions.push(serialize(&McrAction::AcceptDraw));
                actions.push(serialize(&McrAction::DeclineDraw));
            }
            Some(_) => {} // This player already has an offer pending.
            None => actions.push(serialize(&McrAction::OfferDraw)),
        }

        actions
    }

    fn apply(&mut self, player: Color, action: &Action) -> Result<ActionEffect, GameError> {
        if self.outcome.is_some() {
            return Err(GameError::Finished);
        }

        let action: McrAction = action
            .to_typed()
            .map_err(|e| GameError::InvalidActionPayload(e.to_string()))?;

        match action {
            McrAction::Move { uci } => self.apply_move(player, &uci),
            // Resigning hands the win to the opponent.
            McrAction::Resign => {
                self.finish(Outcome::win(player.opposite(), EndReason::Resignation))
            }
            McrAction::OfferDraw => {
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
                    events: vec![Event::from_typed(&McrEvent::DrawOffered { by: player })?],
                })
            }
            McrAction::AcceptDraw => {
                // Only the opponent of the offerer may accept.
                if self.draw_offer == Some(player.opposite()) {
                    self.finish(Outcome::draw(EndReason::DrawAgreement))
                } else {
                    Err(GameError::IllegalAction)
                }
            }
            McrAction::ClaimDraw => {
                // A draw claim stands in for a move, so only the side to move may
                // claim, and only when the position is actually eligible.
                if self.to_move() != player {
                    return Err(GameError::NotYourTurn);
                }
                match self.claimable_draw() {
                    Some(reason) => self.finish(Outcome::draw(reason)),
                    None => Err(GameError::IllegalAction),
                }
            }
            McrAction::DeclineDraw => {
                if self.draw_offer == Some(player.opposite()) {
                    self.draw_offer = None;
                    Ok(ActionEffect {
                        status: self.status(),
                        events: vec![Event::from_typed(&McrEvent::DrawDeclined { by: player })?],
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
        PlayerView::from_typed(&self.build_view()).expect("mcr view always serializes")
    }

    fn spectator_view(&self) -> PlayerView {
        // Identical to a player's view: these variants hide nothing.
        PlayerView::from_typed(&self.build_view()).expect("mcr view always serializes")
    }

    fn outcome(&self) -> Option<Outcome> {
        self.outcome.clone()
    }
}
