//! Repository trait for [`Game`] persistence.

use async_trait::async_trait;
use mcs_domain::{Game, GameId, UserId};

use crate::error::StorageResult;

/// Persistence operations for [`Game`] aggregates.
///
/// Implementations must be [`Send`] and [`Sync`] so they can be shared across
/// async tasks and stored behind an `Arc`.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn GameRepo` or
/// `Arc<dyn GameRepo>`.
#[async_trait]
pub trait GameRepo: Send + Sync {
    /// Persists a new [`Game`] record.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Conflict`] if a game with the same `id` already
    ///   exists.
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn create(&self, game: &Game) -> StorageResult<()>;

    /// Retrieves a [`Game`] by its [`GameId`].
    ///
    /// # Errors
    ///
    /// - [`StorageError::NotFound`] if no game with the given `id` exists.
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn get(&self, id: GameId) -> StorageResult<Game>;

    /// Persists changes to an existing [`Game`] record.
    ///
    /// Callers invoke this after mutating the aggregate in memory — for
    /// example after calling [`Game::finish`] to record an outcome, or after
    /// transitioning the lifecycle to [`GameLifecycle::Active`].
    ///
    /// [`GameLifecycle::Active`]: mcs_domain::GameLifecycle::Active
    ///
    /// # Errors
    ///
    /// - [`StorageError::NotFound`] if the game does not exist.
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn update(&self, game: &Game) -> StorageResult<()>;

    /// Returns the most recently created games, newest first.
    ///
    /// `limit` caps the result count. Pass a small value (e.g. 20) for
    /// lobby-style displays, or a larger one for admin views.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn list_recent(&self, limit: u32) -> StorageResult<Vec<Game>>;

    /// Returns games in which `user` participated, most recent first.
    ///
    /// The implementation should include games where the user played either
    /// colour. `limit` caps the result count.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn list_for_user(&self, user: UserId, limit: u32) -> StorageResult<Vec<Game>>;

    /// Returns every [`GameLifecycle::Finished`][mcs_domain::GameLifecycle::Finished]
    /// game in which `user` played either colour, ordered by `created_at`
    /// (oldest first).
    ///
    /// This is the aggregation source for per-player statistics (win/loss/draw
    /// tallies and performance ratings): the caller walks the full finished
    /// history and folds it into per-`(variant, time_class)` totals in
    /// application code.
    ///
    /// Unlike [`list_for_user`](GameRepo::list_for_user) the result is
    /// **unbounded** — every finished game counts towards the player's record,
    /// so none may be dropped. A very active player therefore returns many rows;
    /// a denormalised, incrementally-maintained stats cache is the future
    /// optimisation when this read becomes hot.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn finished_games_for_user(&self, user: UserId) -> StorageResult<Vec<Game>>;

    /// Returns all games whose lifecycle is not
    /// [`GameLifecycle::Finished`][mcs_domain::GameLifecycle::Finished] —
    /// i.e. games still `Created` or `Active` — ordered by `created_at`
    /// (oldest first).
    ///
    /// This is the recovery hook: after a restart the server lists the games
    /// that were still in progress and rebuilds their live sessions from each
    /// record's variant and snapshot. The result is unbounded because every
    /// in-progress game must be recovered.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn list_unfinished(&self) -> StorageResult<Vec<Game>>;
}
