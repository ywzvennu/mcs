//! Errors surfaced by the game session actor.

use mcs_core::GameError;
use mcs_storage::StorageError;
use thiserror::Error;

/// Failures that can arise while interacting with a live game session through a
/// [`GameHandle`](crate::GameHandle).
///
/// The variants distinguish three independent failure domains:
///
/// - **game-rule failures** ([`GameSessionError::Game`]) come straight from the
///   underlying [`GameSession`](mcs_core::GameSession) — illegal moves, acting
///   out of turn, acting after the game has finished, and so on;
/// - **persistence failures** ([`GameSessionError::Storage`]) occur when the
///   actor tries to record the final result through the injected
///   [`GameRepo`](mcs_storage::GameRepo);
/// - **mailbox failures** ([`GameSessionError::ActorUnavailable`]) mean the
///   actor task has stopped (its command channel or reply channel closed),
///   which a caller cannot recover from for that game.
#[derive(Debug, Error)]
pub enum GameSessionError {
    /// The underlying game session rejected the operation.
    ///
    /// This is the common, expected case for client-driven play: the action
    /// was illegal, out of turn, malformed, or submitted to a finished game.
    #[error("game error: {0}")]
    Game(#[from] GameError),

    /// Persisting the finished game through the [`GameRepo`] failed.
    ///
    /// The in-memory game still advanced correctly; only the durable record
    /// could not be updated. Callers may wish to retry or alert an operator.
    ///
    /// [`GameRepo`]: mcs_storage::GameRepo
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// The actor task is no longer running, so the command could not be
    /// delivered or its reply could not be received.
    ///
    /// This is terminal for the game: once the actor stops, its
    /// [`GameSession`](mcs_core::GameSession) is gone. It typically means every
    /// [`GameHandle`](crate::GameHandle) was dropped and the actor shut down,
    /// or the actor panicked.
    #[error("game actor is unavailable (mailbox closed)")]
    ActorUnavailable,
}
