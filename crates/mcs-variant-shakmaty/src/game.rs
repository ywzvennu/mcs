//! The generic [`GameSession`] adapter shared by every shakmaty variant.

use std::marker::PhantomData;
use std::str::FromStr;

use mcs_core::{
    Action, ActionEffect, Color, EndReason, Event, GameError, GameSession, GameStatus, Outcome,
    PlayerView, VariantOptions,
};
use shakmaty::fen::Fen;
use shakmaty::san::SanPlus;
use shakmaty::uci::UciMove;
use shakmaty::{EnPassantMode, Position};

use crate::spec::VariantSpec;
use crate::wire::{ShakmatyAction, ShakmatyEvent, ShakmatyView};

/// A single in-progress game of the shakmaty variant described by `S`.
///
/// This one type implements [`GameSession`] for the *whole* family: the variant
/// is selected purely by the [`VariantSpec`] type parameter, so move
/// generation, legality, UCI parsing, view building, and termination are
/// written exactly once. It wraps the concrete shakmaty position — the source of
/// truth for all rules — plus the bookkeeping shakmaty does not track for us: a
/// pending draw offer and the final outcome once the game is over (so
/// resignations and draw agreements, which are not board states, can be
/// recorded).
pub struct ShakmatyGame<S: VariantSpec> {
    /// The underlying shakmaty position. shakmaty enforces all move legality.
    position: S::Position,
    /// The color with an outstanding, unanswered draw offer, if any.
    draw_offer: Option<Color>,
    /// The recorded outcome once the game has finished. `None` while ongoing.
    ///
    /// Board-driven endings can always be re-derived from `position`, but
    /// resignation and draw-agreement endings cannot, so the outcome is stored
    /// explicitly the moment the game ends for any reason.
    outcome: Option<Outcome>,
    /// Zero-sized marker tying this game to its variant specification.
    _spec: PhantomData<fn() -> S>,
}

impl<S: VariantSpec> std::fmt::Debug for ShakmatyGame<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShakmatyGame")
            .field("variant_id", &S::ID)
            .field("fen", &self.fen())
            .field("draw_offer", &self.draw_offer)
            .field("outcome", &self.outcome)
            .finish()
    }
}

impl<S: VariantSpec> ShakmatyGame<S> {
    /// Creates a new game from `S`'s starting position for the given `options`.
    ///
    /// # Errors
    ///
    /// Returns whatever [`VariantSpec::starting_position`] reports — for example
    /// an out-of-range Chess960 position number.
    pub fn new(options: &VariantOptions) -> Result<Self, GameError> {
        Ok(Self {
            position: S::starting_position(options)?,
            draw_offer: None,
            outcome: None,
            _spec: PhantomData,
        })
    }

    /// Maps a `shakmaty` color onto the core [`Color`].
    fn from_shakmaty_color(color: shakmaty::Color) -> Color {
        match color {
            shakmaty::Color::White => Color::White,
            shakmaty::Color::Black => Color::Black,
        }
    }

    /// Renders the current position as a (variant-aware) FEN string.
    fn fen(&self) -> String {
        // `EnPassantMode::Legal` only records an en-passant square when a
        // capture is actually possible, matching how engines present a position.
        Fen::from_position(self.position.clone(), EnPassantMode::Legal).to_string()
    }

    /// Returns every legal move in the current position as UCI strings.
    fn legal_moves_uci(&self) -> Vec<String> {
        let mode = self.position.castles().mode();
        self.position
            .legal_moves()
            .iter()
            .map(|m| UciMove::from_move(m, mode).to_string())
            .collect()
    }

    /// Builds the full, perfect-information view of the current position.
    ///
    /// The same view is returned to both players and to spectators, because
    /// every variant in this crate hides no information.
    fn build_view(&self) -> ShakmatyView {
        ShakmatyView {
            variant_id: S::ID.to_owned(),
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

    /// Derives the board-driven outcome of the current position, if the game has
    /// reached an automatic termination.
    ///
    /// shakmaty's [`Position::outcome`] reports these by inspecting the board: it
    /// first consults the variant-specific rules
    /// ([`Position::variant_outcome`], e.g. a king on the hill or the third
    /// check) and then the universal ones (checkmate, stalemate, insufficient
    /// material). A decisive ending is described by
    /// [`VariantSpec::decisive_reason`]; a drawn ending is distinguished into
    /// stalemate vs. insufficient material so the reason is precise.
    fn board_outcome(&self) -> Option<Outcome> {
        let shakmaty_outcome = self.position.outcome()?;
        Some(match shakmaty_outcome.winner() {
            Some(winner) => {
                let winner = Self::from_shakmaty_color(winner);
                Outcome::win(winner, S::decisive_reason(winner, &self.position))
            }
            None => {
                // A drawn board ending is either stalemate or insufficient
                // material; both are reported by shakmaty for the side to move.
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
        let mv = parsed
            .to_move(&self.position)
            .map_err(|_| GameError::IllegalAction)?;

        // Capture SAN and canonical UCI before playing, then play the (legal)
        // move. `play` consumes the position, so swap a clone in temporarily.
        let mode = self.position.castles().mode();
        let san = SanPlus::from_move(self.position.clone(), &mv).to_string();
        let played_uci = UciMove::from_move(&mv, mode).to_string();
        self.position = self
            .position
            .clone()
            .play(&mv)
            // The move was validated as legal above, so this cannot fail.
            .map_err(|_| GameError::IllegalAction)?;

        // A move always supersedes any pending draw offer.
        self.draw_offer = None;

        let mut events = vec![Event::from_typed(&ShakmatyEvent::MovePlayed {
            uci: played_uci,
            san,
            fen: self.fen(),
        })?];

        // Detect an automatic termination produced by the move.
        if let Some(outcome) = self.board_outcome() {
            self.outcome = Some(outcome.clone());
            events.push(Event::from_typed(&ShakmatyEvent::GameEnded { outcome })?);
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
            events: vec![Event::from_typed(&ShakmatyEvent::GameEnded { outcome })?],
        })
    }
}

impl<S: VariantSpec> GameSession for ShakmatyGame<S> {
    fn variant_id(&self) -> &'static str {
        S::ID
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
    /// While the game is ongoing, the side to move gets every legal move plus
    /// the meta-actions available to them: resign, and either offer a draw or
    /// answer an outstanding offer. A player may **resign at any time** — even on
    /// the opponent's turn — so the non-moving side is still offered resignation
    /// (and the chance to answer a draw the opponent offered). Once the game is
    /// finished, no actions are legal.
    fn legal_actions(&self, player: Color) -> Vec<Action> {
        if self.outcome.is_some() {
            return Vec::new();
        }

        let mut actions = Vec::new();
        let serialize = |action: &ShakmatyAction| {
            Action::from_typed(action).expect("shakmaty actions always serialize")
        };

        if self.to_move() == player {
            for uci in self.legal_moves_uci() {
                actions.push(serialize(&ShakmatyAction::Move { uci }));
            }
        }

        // Resignation is always available to either player.
        actions.push(serialize(&ShakmatyAction::Resign));

        // Draw handling: if the opponent has an offer outstanding, this player
        // may accept or decline it; otherwise this player may make an offer
        // (provided they do not already have one pending).
        match self.draw_offer {
            Some(offerer) if offerer == player.opposite() => {
                actions.push(serialize(&ShakmatyAction::AcceptDraw));
                actions.push(serialize(&ShakmatyAction::DeclineDraw));
            }
            Some(_) => {} // This player already has an offer pending.
            None => actions.push(serialize(&ShakmatyAction::OfferDraw)),
        }

        actions
    }

    fn apply(&mut self, player: Color, action: &Action) -> Result<ActionEffect, GameError> {
        if self.outcome.is_some() {
            return Err(GameError::Finished);
        }

        let action: ShakmatyAction = action
            .to_typed()
            .map_err(|e| GameError::InvalidActionPayload(e.to_string()))?;

        match action {
            ShakmatyAction::Move { uci } => self.apply_move(player, &uci),
            // Resigning hands the win to the opponent.
            ShakmatyAction::Resign => {
                self.finish(Outcome::win(player.opposite(), EndReason::Resignation))
            }
            ShakmatyAction::OfferDraw => {
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
                    events: vec![Event::from_typed(&ShakmatyEvent::DrawOffered {
                        by: player,
                    })?],
                })
            }
            ShakmatyAction::AcceptDraw => {
                // Only the opponent of the offerer may accept.
                if self.draw_offer == Some(player.opposite()) {
                    self.finish(Outcome::draw(EndReason::DrawAgreement))
                } else {
                    Err(GameError::IllegalAction)
                }
            }
            ShakmatyAction::DeclineDraw => {
                if self.draw_offer == Some(player.opposite()) {
                    self.draw_offer = None;
                    Ok(ActionEffect {
                        status: self.status(),
                        events: vec![Event::from_typed(&ShakmatyEvent::DrawDeclined {
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
        PlayerView::from_typed(&self.build_view()).expect("shakmaty view always serializes")
    }

    fn spectator_view(&self) -> PlayerView {
        // Identical to a player's view: these variants hide nothing.
        PlayerView::from_typed(&self.build_view()).expect("shakmaty view always serializes")
    }

    fn outcome(&self) -> Option<Outcome> {
        self.outcome.clone()
    }
}
