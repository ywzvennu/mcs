//! The variant-agnostic game session trait.

use crate::color::Color;
use crate::error::GameError;
use crate::payload::{Action, PlayerView};
use crate::status::{ActionEffect, GameStatus, Outcome};

/// A single in-progress game of some variant.
///
/// This trait is the central abstraction of `mcs-core`. It is deliberately
/// **object-safe**: the rest of the server holds variants as
/// `Box<dyn GameSession>` (see [`VariantRegistry`](crate::VariantRegistry))
/// without compile-time knowledge of which variant is being played. Strong,
/// variant-internal types are erased into the serde-serializable
/// [`Action`]/[`PlayerView`]/[`Event`](crate::Event) payloads at this boundary.
///
/// # Perfect- vs imperfect-information variants
///
/// The trait expresses both classes of variant through a single set of
/// methods; the difference lives entirely in what the view methods return.
///
/// **Perfect information (standard chess).** Both players and any spectator see
/// the same complete position at all times:
///
/// - [`view_for`](GameSession::view_for) returns the full board for either
///   player;
/// - [`spectator_view`](GameSession::spectator_view) returns that same full
///   board;
/// - a turn is one move, so [`legal_actions`](GameSession::legal_actions)
///   yields the legal moves and [`apply`](GameSession::apply) plays one.
///
/// **Imperfect information (Reconnaissance Blind Chess).** RBC is the
/// motivating example for this abstraction. In RBC a turn includes a private
/// **sense** action: a player secretly inspects a small region of the board and
/// privately learns which enemy pieces sit there, then makes a move whose
/// outcome they observe only partially. Neither player sees the full enemy
/// position. This maps onto the trait as follows:
///
/// - [`view_for`](GameSession::view_for) returns only that player's own pieces
///   plus the result of their latest sense — never the opponent's hidden
///   pieces;
/// - [`spectator_view`](GameSession::spectator_view) is **redacted** while the
///   game is [`GameStatus::Ongoing`] (so a spectator cannot leak hidden
///   information to a player) and may reveal the full game only once it is
///   [`GameStatus::Finished`];
/// - the variant exposes "sense" as just another kind of [`Action`], so
///   [`legal_actions`](GameSession::legal_actions) lists the legal senses and
///   moves for the side to move, and [`apply`](GameSession::apply) handles both.
///
/// Implementations must be `Send + Sync` so sessions can be driven from an
/// async server, and `Debug` to aid logging and tests.
pub trait GameSession: Send + Sync + std::fmt::Debug {
    /// The stable identifier of the variant this session plays, matching the
    /// id its [`VariantFactory`](crate::VariantFactory) registers under.
    fn variant_id(&self) -> &'static str;

    /// The color whose turn it is to act.
    fn to_move(&self) -> Color;

    /// The current lifecycle status of the game.
    fn status(&self) -> GameStatus;

    /// The actions `player` may legally submit right now.
    ///
    /// Returns an empty vector when it is not `player`'s turn or the game has
    /// finished. For imperfect-information variants this naturally includes
    /// both sense and move actions for the side to move.
    fn legal_actions(&self, player: Color) -> Vec<Action>;

    /// Applies `action` on behalf of `player`, advancing the game.
    ///
    /// # Errors
    ///
    /// - [`GameError::Finished`] if the game has already ended;
    /// - [`GameError::NotYourTurn`] if it is not `player`'s turn;
    /// - [`GameError::IllegalAction`] if the action is not legal in the current
    ///   position;
    /// - [`GameError::InvalidActionPayload`] if the payload does not decode to
    ///   an action this variant understands.
    fn apply(&mut self, player: Color, action: &Action) -> Result<ActionEffect, GameError>;

    /// The view that `player` is permitted to observe.
    ///
    /// For perfect-information variants this is the full position; for
    /// imperfect-information variants it is the player's private, redacted view.
    fn view_for(&self, player: Color) -> PlayerView;

    /// The view a spectator is permitted to observe.
    ///
    /// May be redacted while the game is ongoing for imperfect-information
    /// variants, to avoid leaking hidden information to players who could be
    /// watching the same broadcast.
    fn spectator_view(&self) -> PlayerView;

    /// The outcome of the game, or `None` if it is still ongoing.
    fn outcome(&self) -> Option<Outcome>;
}
