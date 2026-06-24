//! The game-completion hook: a side effect run once a game finishes.
//!
//! When a [`GameActor`](crate::GameActor) ends a game — by a finishing move, a
//! resignation, a draw agreement, or a clock flag — it has already persisted the
//! final [`Game`] record before anything else observes the result. Some
//! subsystems need to react to that transition: ratings must be updated, payouts
//! settled, notifications sent. Rather than teach the actor about any of them,
//! it invokes a single [`GameCompletionHook`] after persistence.
//!
//! The trait is deliberately abstract. `mcs-game` knows only that *something*
//! wants to be told a game finished and with what [`Outcome`]; it has no
//! dependency on the rating engine, the payment layer, or any other consumer.
//! The HTTP layer wires in a concrete implementation (e.g. a rating updater) and
//! hands it to [`GameActor::spawn`](crate::GameActor::spawn). Tests and callers
//! that want no side effect at all use [`NoopHook`].

use async_trait::async_trait;

use mcs_core::Outcome;
use mcs_domain::Game;

/// A side effect invoked exactly once, after a game has finished and its final
/// result has been durably persisted.
///
/// The actor calls [`on_finished`](GameCompletionHook::on_finished) on the
/// transition to [`GameLifecycle::Finished`](mcs_domain::GameLifecycle::Finished),
/// passing the persisted [`Game`] (so the variant and the two players are
/// available) and the [`Outcome`] that ended it. It is called at most once per
/// game, on the same task as persistence, so a slow hook backpressures only that
/// one game's actor.
///
/// # Contract
///
/// - **Run after persistence.** The `Game` passed in has already been written
///   back as finished, so an implementation may safely read it from storage.
/// - **Must not panic.** The actor awaits the hook on its own task; a panic
///   would take the game's actor down. Implementations should treat every error
///   (a missing rating row, a storage hiccup, an anonymous player) as a
///   no-op-and-log rather than unwinding.
/// - **Side-effect only.** The return type is `()`: the hook cannot change the
///   game's result, only react to it.
///
/// # Object safety
///
/// The trait is object-safe; the actor holds it as
/// `Arc<dyn GameCompletionHook>`.
#[async_trait]
pub trait GameCompletionHook: Send + Sync {
    /// Reacts to `game` having just finished with `outcome`.
    ///
    /// Called once, after the finished record is persisted. Implementations
    /// must not panic (see the [trait contract](GameCompletionHook)).
    async fn on_finished(&self, game: &Game, outcome: &Outcome);
}

/// A [`GameCompletionHook`] that does nothing.
///
/// The default for callers that want no completion side effect — every existing
/// `mcs-game` test, and any deployment without a rating or payment subsystem
/// wired in. Cloning is free; it holds no state.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHook;

#[async_trait]
impl GameCompletionHook for NoopHook {
    async fn on_finished(&self, _game: &Game, _outcome: &Outcome) {}
}
