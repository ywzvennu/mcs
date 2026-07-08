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

/// A single in-progress game of some perfect-information mcr variant.
///
/// Wraps an [`mcr::Game`] — which enforces all rules and answers every
/// single-position question (legal moves, check, terminal outcome) — plus the
/// small amount of bookkeeping the engine does not track for us: which variant
/// this is (for the reported id), a pending draw offer, and the final outcome
/// once the game is over (so resignations and draw agreements, which are not
/// board states, can be recorded).
///
/// `mcr::Game` is a *single-position* handle with no move history, so the
/// history-dependent draw rules (threefold / fivefold repetition, the
/// fifty/seventy-five-move clocks, sennichite, …) are not adjudicated here.
/// Only the automatic single-position terminations mcr reports through
/// [`Game::outcome`] end a game on the board; everything else ends it through a
/// meta-action (resignation or a draw agreement).
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
    /// The recorded outcome once the game has finished. `None` while ongoing.
    ///
    /// Board-driven endings can always be re-derived from `game`, but
    /// resignation and draw agreement cannot, so the outcome is stored
    /// explicitly the moment the game ends for any reason.
    outcome: Option<Outcome>,
}

impl McrGame {
    /// Creates a new game of `variant` from its starting position.
    #[must_use]
    pub fn new(variant: VariantRef) -> Self {
        Self {
            game: Game::new(variant),
            variant_id: variant.name(),
            draw_offer: None,
            outcome: None,
        }
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
        Ok(Self {
            game,
            variant_id: variant.name(),
            draw_offer: None,
            outcome: None,
        })
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
            None => {
                let reason = if self.game.is_check() {
                    EndReason::Other("draw".to_owned())
                } else {
                    EndReason::Stalemate
                };
                Outcome::draw(reason)
            }
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

        // A move always supersedes any pending draw offer.
        self.draw_offer = None;

        let mut events = vec![Event::from_typed(&McrEvent::MovePlayed {
            uci: played_uci,
            fen: self.game.fen(),
        })?];

        // Detect an automatic single-position termination produced by the move.
        if let Some(game_outcome) = self.game.outcome() {
            let outcome = self.map_outcome(game_outcome);
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
