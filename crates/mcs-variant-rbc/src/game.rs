//! The Reconnaissance Blind Chess [`GameSession`] implementation, backed by
//! `rbc-rs`.

use mcs_core::{
    Action, ActionEffect, Color, EndReason, Event, GameError, GameSession, GameStatus, Outcome,
    PlayerView,
};
use rbc_rs::{Game, GameConfig, MoveOutcome, SenseResult, Square};

use crate::convert::{
    from_rbc_color, move_from_uci, move_to_uci, own_pieces_fen, piece_to_fen_char,
    square_from_algebraic, square_to_algebraic, to_rbc_color,
};
use crate::wire::{
    RbcAction, RbcEvent, RbcFinalView, RbcSpectatorView, RbcView, SenseSnapshot, SensedSquare,
    TurnPhase,
};

/// The variant identifier for Reconnaissance Blind Chess.
///
/// Re-exported publicly as [`crate::RBC_VARIANT_ID`].
pub(crate) const VARIANT_ID: &str = "rbc";

/// A single in-progress game of Reconnaissance Blind Chess.
///
/// Wraps an [`rbc_rs::Game`] — the source of truth for all rules and hidden
/// state — plus the small amount of bookkeeping the adapter needs:
///
/// - a per-turn **phase** flag, because this variant enforces a strict
///   sense-then-move ordering (the side to move must sense before they may
///   move), which `rbc-rs` permits but does not require;
/// - the most recent sense result for **each** player, retained so that a
///   player's private [`view_for`](GameSession::view_for) can echo their own
///   latest sense back to them across subsequent calls;
/// - the recorded final [`Outcome`], so resignations (which are not board
///   states) and engine-driven endings present uniformly.
#[derive(Debug)]
pub struct RbcGame {
    /// The underlying RBC game; enforces all rules and holds the true (hidden)
    /// board. Its `serde` feature lets the whole session be persisted.
    game: Game,
    /// Whether the side to move still owes a sense this turn (`Sense` phase) or
    /// has sensed and must now move (`Move` phase).
    phase: TurnPhase,
    /// Each player's most recent sense result, indexed by colour. Retained so a
    /// player keeps seeing their own latest sense in their view between calls.
    last_sense: [Option<SenseSnapshot>; 2],
    /// The recorded outcome once the game has finished. `None` while ongoing.
    outcome: Option<Outcome>,
}

/// Index into the per-colour arrays.
fn color_index(color: Color) -> usize {
    match color {
        Color::White => 0,
        Color::Black => 1,
    }
}

impl Default for RbcGame {
    /// Creates a game from the standard RBC starting position.
    fn default() -> Self {
        Self::new()
    }
}

impl RbcGame {
    /// Creates a new game from the standard RBC starting position.
    #[must_use]
    pub fn new() -> Self {
        Self {
            game: Game::new(GameConfig::default()),
            phase: TurnPhase::Sense,
            last_sense: [None, None],
            outcome: None,
        }
    }

    /// The colour whose turn it is, derived from the engine.
    ///
    /// Once the game is over `rbc-rs` reports no turn; we keep returning the
    /// last side to move for a stable answer, which callers gate on
    /// [`status`](GameSession::status) anyway.
    fn turn(&self) -> Color {
        self.game.turn().map_or(Color::White, from_rbc_color)
    }

    /// Translates the engine's status into the core [`Outcome`], if finished.
    fn engine_outcome(&self) -> Option<Outcome> {
        match self.game.status() {
            rbc_rs::GameStatus::Ongoing { .. } => None,
            rbc_rs::GameStatus::Won(result) => {
                let winner = from_rbc_color(result.winner);
                let reason = match result.reason {
                    // RBC's defining ending — the king is captured outright.
                    // The core enum has no exact case, so describe it via
                    // `Other`, as the issue specifies.
                    rbc_rs::WinReason::KingCapture => EndReason::Other("king_capture".to_owned()),
                    rbc_rs::WinReason::Resignation => EndReason::Resignation,
                    rbc_rs::WinReason::Timeout => EndReason::Timeout,
                };
                Some(Outcome::win(winner, reason))
            }
            rbc_rs::GameStatus::Draw { reason } => {
                let label = match reason {
                    rbc_rs::DrawReason::MoveLimit => "move_limit",
                    rbc_rs::DrawReason::TurnLimit => "turn_limit",
                };
                Some(Outcome::draw(EndReason::Other(label.to_owned())))
            }
        }
    }

    /// Records the final outcome (if the engine has reached one) and returns
    /// the current status.
    fn refresh_status(&mut self) -> GameStatus {
        if self.outcome.is_none() {
            self.outcome = self.engine_outcome();
        }
        self.status()
    }

    /// Builds the [`SenseSnapshot`] wire form of an engine sense result.
    fn snapshot_sense(result: &SenseResult) -> SenseSnapshot {
        SenseSnapshot {
            center: square_to_algebraic(result.action.center),
            squares: result
                .squares
                .iter()
                .map(|sensed| SensedSquare {
                    square: square_to_algebraic(sensed.square),
                    piece: sensed.piece.map(|p| piece_to_fen_char(p).to_string()),
                })
                .collect(),
        }
    }

    /// Applies a sense on behalf of `player`, recording the private result.
    fn apply_sense(&mut self, player: Color, square: &str) -> Result<ActionEffect, GameError> {
        let center = square_from_algebraic(square)?;
        // Find the engine's sense action for this centre. The default config has
        // exactly one sense token, so each centre appears at most once.
        let action = self
            .game
            .sense_actions()
            .into_iter()
            .find(|a| a.center == center)
            .ok_or(GameError::IllegalAction)?;
        let result = self
            .game
            .sense_with(action)
            .map_err(|_| GameError::IllegalAction)?
            // The default token reveals immediately, so a result is always
            // returned; treat a missing one defensively as illegal.
            .ok_or(GameError::IllegalAction)?;

        self.last_sense[color_index(player)] = Some(Self::snapshot_sense(&result));
        // Having sensed, the player now owes a move.
        self.phase = TurnPhase::Move;

        Ok(ActionEffect {
            status: self.status(),
            events: vec![Event::from_typed(&RbcEvent::Sensed { by: player })?],
        })
    }

    /// Applies a move (or pass) on behalf of `player`, advancing the turn.
    fn apply_move(
        &mut self,
        player: Color,
        requested: Option<rbc_rs::Move>,
    ) -> Result<ActionEffect, GameError> {
        let outcome: MoveOutcome = self
            .game
            .apply_move(requested)
            .map_err(|_| GameError::IllegalAction)?;

        // A new turn for the opponent always begins in the sense phase. Clear
        // the mover's retained sense so their view reflects only the new turn.
        self.phase = TurnPhase::Sense;
        self.last_sense[color_index(player)] = None;

        let capture_square = outcome.capture.map(|c| square_to_algebraic(c.square));
        let mut events = vec![Event::from_typed(&RbcEvent::MovePlayed {
            by: player,
            captured: outcome.capture.is_some(),
            capture_square,
        })?];

        if let Some(result) = self.refresh_status().is_finished().then(|| {
            self.outcome
                .clone()
                .expect("finished status carries an outcome")
        }) {
            events.push(Event::from_typed(&RbcEvent::GameEnded { outcome: result })?);
        }

        Ok(ActionEffect {
            status: self.status(),
            events,
        })
    }

    /// Records a resignation by `player`, handing the win to the opponent.
    fn apply_resign(&mut self, player: Color) -> Result<ActionEffect, GameError> {
        self.game
            .resign(to_rbc_color(player))
            .map_err(|_| GameError::Finished)?;
        let outcome = self.refresh_status();
        let final_outcome = self
            .outcome
            .clone()
            .expect("a resignation finishes the game");
        Ok(ActionEffect {
            status: outcome,
            events: vec![Event::from_typed(&RbcEvent::GameEnded {
                outcome: final_outcome,
            })?],
        })
    }

    /// The square (algebraic) where the opponent last captured one of
    /// `player`'s pieces, if any.
    fn last_capture_square_for(&self, player: Color) -> Option<String> {
        self.game
            .opponent_capture_square(to_rbc_color(player))
            .map(square_to_algebraic)
    }

    /// The number of completed turns (plies), from the engine history.
    fn turn_count(&self) -> usize {
        self.game.history().len()
    }

    /// The complete current position as a FEN string. Used only for the
    /// spectator's view **after** the game has finished, when nothing is hidden.
    fn full_fen(&self) -> String {
        self.game.to_fen()
    }
}

impl GameSession for RbcGame {
    fn variant_id(&self) -> &'static str {
        VARIANT_ID
    }

    fn to_move(&self) -> Color {
        self.turn()
    }

    fn status(&self) -> GameStatus {
        match &self.outcome {
            Some(outcome) => GameStatus::Finished(outcome.clone()),
            None => GameStatus::Ongoing,
        }
    }

    /// The actions `player` may submit right now.
    ///
    /// Only the side to move has board actions, and which ones depend on the
    /// phase: in the [`Sense`](TurnPhase::Sense) phase they get one sense action
    /// per board square; in the [`Move`](TurnPhase::Move) phase they get every
    /// candidate move plus a pass. Either player may resign at any time on their
    /// own turn. Once the game is finished, no actions are legal.
    ///
    /// Note that the opponent's turn yields no actions other than (potentially)
    /// nothing — RBC turns are strictly sequential.
    fn legal_actions(&self, player: Color) -> Vec<Action> {
        if self.outcome.is_some() || player != self.to_move() {
            return Vec::new();
        }

        let mut actions = Vec::new();
        let serialize =
            |action: &RbcAction| Action::from_typed(action).expect("rbc actions always serialize");

        match self.phase {
            TurnPhase::Sense => {
                for sense in self.game.sense_actions() {
                    actions.push(serialize(&RbcAction::Sense {
                        square: square_to_algebraic(sense.center),
                    }));
                }
            }
            TurnPhase::Move => {
                for mv in self.game.move_actions() {
                    actions.push(serialize(&RbcAction::Move {
                        uci: move_to_uci(mv),
                    }));
                }
                // Passing the move is always available once sensed.
                actions.push(serialize(&RbcAction::Pass));
            }
        }

        // Resignation is available to the side to move at any phase.
        actions.push(serialize(&RbcAction::Resign));
        actions
    }

    /// Applies `action` on behalf of `player`.
    ///
    /// Enforces both turn order and phase order: a sense outside the sense
    /// phase, or a move before sensing, is rejected with
    /// [`GameError::IllegalAction`]; acting out of turn is
    /// [`GameError::NotYourTurn`].
    fn apply(&mut self, player: Color, action: &Action) -> Result<ActionEffect, GameError> {
        if self.outcome.is_some() {
            return Err(GameError::Finished);
        }
        if player != self.to_move() {
            return Err(GameError::NotYourTurn);
        }

        let action: RbcAction = action
            .to_typed()
            .map_err(|e| GameError::InvalidActionPayload(e.to_string()))?;

        match action {
            RbcAction::Sense { square } => {
                if self.phase != TurnPhase::Sense {
                    // Already sensed this turn; a second sense is out of phase.
                    return Err(GameError::IllegalAction);
                }
                self.apply_sense(player, &square)
            }
            RbcAction::Move { uci } => {
                if self.phase != TurnPhase::Move {
                    // Must sense before moving.
                    return Err(GameError::IllegalAction);
                }
                let mv = move_from_uci(&uci)?;
                self.apply_move(player, Some(mv))
            }
            RbcAction::Pass => {
                if self.phase != TurnPhase::Move {
                    return Err(GameError::IllegalAction);
                }
                self.apply_move(player, None)
            }
            // Resigning is legal in either phase on the player's own turn.
            RbcAction::Resign => self.apply_resign(player),
        }
    }

    /// The private view permitted to `player`.
    ///
    /// This is the heart of the hidden-information guarantee: the returned view
    /// contains only `player`'s own pieces (via a one-sided FEN) plus their own
    /// latest sense and the publicly disclosed last-capture square. The
    /// opponent's piece locations are never serialized into it.
    fn view_for(&self, player: Color) -> PlayerView {
        let own_fen = own_pieces_fen(player, |sq: Square| self.game.piece_at(sq));
        let phase = if self.outcome.is_some() {
            None
        } else {
            Some(self.phase)
        };
        let view = RbcView {
            own_fen,
            side_to_move: self.to_move(),
            your_color: player,
            phase,
            last_sense: self.last_sense[color_index(player)].clone(),
            last_capture_square: self.last_capture_square_for(player),
            status: self.status(),
        };
        PlayerView::from_typed(&view).expect("rbc view always serializes")
    }

    /// The spectator's view.
    ///
    /// **Redacted while ongoing** — only the turn count, side to move, and phase
    /// are revealed, never a piece location, so a spectator cannot relay hidden
    /// information to a player. Once the game is finished the full final board
    /// is revealed instead.
    fn spectator_view(&self) -> PlayerView {
        match &self.outcome {
            Some(_) => {
                let view = RbcFinalView {
                    fen: self.full_fen(),
                    turn_count: self.turn_count(),
                    status: self.status(),
                };
                PlayerView::from_typed(&view).expect("rbc final view always serializes")
            }
            None => {
                let view = RbcSpectatorView {
                    side_to_move: self.to_move(),
                    phase: self.phase,
                    turn_count: self.turn_count(),
                    status: GameStatus::Ongoing,
                };
                PlayerView::from_typed(&view).expect("rbc spectator view always serializes")
            }
        }
    }

    fn outcome(&self) -> Option<Outcome> {
        self.outcome.clone()
    }
}
