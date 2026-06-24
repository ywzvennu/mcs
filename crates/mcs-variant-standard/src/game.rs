//! The standard-chess [`GameSession`] implementation, backed by `shakmaty`.

use std::str::FromStr;

use mcs_core::{
    Action, ActionEffect, Color, EndReason, Event, GameError, GameSession, GameStatus, Outcome,
    PlayerView,
};
use shakmaty::fen::Fen;
use shakmaty::san::SanPlus;
use shakmaty::uci::UciMove;
use shakmaty::{Chess, EnPassantMode, Position};

use crate::wire::{StandardAction, StandardEvent, StandardView};

/// The variant identifier for standard chess.
///
/// Re-exported publicly as [`crate::STANDARD_VARIANT_ID`].
pub(crate) const VARIANT_ID: &str = "standard";

/// A single in-progress game of standard chess.
///
/// Wraps a [`shakmaty::Chess`] position — the source of truth for all rules —
/// plus the bookkeeping that shakmaty does not track for us: a pending draw
/// offer and the final outcome once the game is over (so resignations and draw
/// agreements, which are not board states, can be recorded).
#[derive(Debug)]
pub struct StandardGame {
    /// The underlying chess position. shakmaty enforces all move legality.
    position: Chess,
    /// The color with an outstanding, unanswered draw offer, if any.
    draw_offer: Option<Color>,
    /// The recorded outcome once the game has finished. `None` while ongoing.
    ///
    /// Board-driven endings (checkmate, stalemate, insufficient material) can
    /// always be re-derived from `position`, but resignation and draw-agreement
    /// endings cannot, so the outcome is stored explicitly the moment the game
    /// ends for any reason.
    outcome: Option<Outcome>,
}

impl Default for StandardGame {
    /// Creates a game from the standard initial position.
    fn default() -> Self {
        Self::new()
    }
}

impl StandardGame {
    /// Creates a new game from the standard initial chess position.
    #[must_use]
    pub fn new() -> Self {
        Self {
            position: Chess::default(),
            draw_offer: None,
            outcome: None,
        }
    }

    /// Maps a `shakmaty` color onto the core [`Color`].
    fn from_shakmaty_color(color: shakmaty::Color) -> Color {
        match color {
            shakmaty::Color::White => Color::White,
            shakmaty::Color::Black => Color::Black,
        }
    }

    /// Renders the current position as a FEN string.
    fn fen(&self) -> String {
        // `EnPassantMode::Legal` only records an en-passant square when a
        // capture is actually possible, matching how engines and the FIDE rules
        // present a position.
        Fen::from_position(self.position.clone(), EnPassantMode::Legal).to_string()
    }

    /// Returns every legal move in the current position as UCI strings.
    fn legal_moves_uci(&self) -> Vec<String> {
        self.position
            .legal_moves()
            .iter()
            .map(|m| UciMove::from_move(m, self.position.castles().mode()).to_string())
            .collect()
    }

    /// Builds the full, perfect-information view of the current position.
    ///
    /// The same view is returned to both players and to spectators, because
    /// standard chess hides no information.
    fn build_view(&self) -> StandardView {
        StandardView {
            fen: self.fen(),
            side_to_move: Self::from_shakmaty_color(self.position.turn()),
            // Once the game is over there are no further legal moves to offer.
            legal_moves_uci: if self.outcome.is_some() {
                Vec::new()
            } else {
                self.legal_moves_uci()
            },
            status: self.status(),
            check: self.position.is_check(),
            draw_offer: self.draw_offer,
        }
    }

    /// Derives the board-driven outcome of the current position, if the game
    /// has reached an automatic termination (checkmate, stalemate, or
    /// insufficient material).
    ///
    /// shakmaty's [`Position::outcome`] reports these by inspecting the board;
    /// rule-based draws that depend on move history (threefold repetition, the
    /// fifty-move rule) are not auto-claimed for a standard `Chess` position and
    /// would surface as explicit draw claims, which are out of scope here.
    fn board_outcome(&self) -> Option<Outcome> {
        let shakmaty_outcome = self.position.outcome()?;
        Some(match shakmaty_outcome.winner() {
            Some(winner) => {
                // A decisive board ending in standard chess is always checkmate;
                // shakmaty only reports a winner when the side to move is mated.
                Outcome::win(Self::from_shakmaty_color(winner), EndReason::Checkmate)
            }
            None => {
                // A drawn board ending is either stalemate or insufficient
                // material. Distinguish them so the end reason is precise.
                let reason = if self.position.is_stalemate() {
                    EndReason::Stalemate
                } else {
                    EndReason::InsufficientMaterial
                };
                Outcome::draw(reason)
            }
        })
    }

    /// Applies a UCI move on behalf of `player`, returning the resulting effect.
    fn apply_move(&mut self, player: Color, uci: &str) -> Result<ActionEffect, GameError> {
        // It must be the mover's turn.
        if Self::from_shakmaty_color(self.position.turn()) != player {
            return Err(GameError::NotYourTurn);
        }

        // Parse the UCI string, then validate it against the current position.
        let parsed: UciMove = UciMove::from_str(uci)
            .map_err(|e| GameError::InvalidActionPayload(format!("invalid UCI '{uci}': {e}")))?;
        let chess_move = parsed
            .to_move(&self.position)
            .map_err(|_| GameError::IllegalAction)?;

        // Capture SAN before playing, then play the (legal) move. `play`
        // consumes the position, so swap in a temporary default and restore the
        // advanced position afterwards.
        let san = SanPlus::from_move(self.position.clone(), &chess_move).to_string();
        let played_uci =
            UciMove::from_move(&chess_move, self.position.castles().mode()).to_string();
        let previous = std::mem::take(&mut self.position);
        self.position = previous
            .play(&chess_move)
            // The move was validated as legal above, so this cannot fail.
            .map_err(|_| GameError::IllegalAction)?;

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
        VARIANT_ID
    }

    fn to_move(&self) -> Color {
        Self::from_shakmaty_color(self.position.turn())
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
