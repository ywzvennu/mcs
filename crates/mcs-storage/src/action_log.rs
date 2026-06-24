//! Append-only action log: the durable record of every action played in a game.

use async_trait::async_trait;
use mcs_core::{Action, Color};
use mcs_domain::GameId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::StorageResult;

/// A single action recorded against a game, together with the clock readings and
/// timestamp captured when it was played.
///
/// One [`RecordedAction`] corresponds to one half-move (`ply`). The sequence of
/// recorded actions for a game, ordered by `ply`, is the authoritative move
/// history: replaying it reconstructs the game from its initial position, which
/// is how a recovering server rebuilds a live session. This is distinct from the
/// live *snapshot* persisted on the game record, which only carries the latest
/// observed position rather than the full history.
///
/// The wrapped [`Action`] is a type-erased serde value, so the log stays
/// variant-agnostic: any variant's action round-trips through it unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedAction {
    /// Zero-based half-move index within the game. Unique per game; appends must
    /// supply strictly distinct values (a duplicate is a [`Conflict`]).
    ///
    /// [`Conflict`]: crate::StorageError::Conflict
    pub ply: u32,

    /// The colour of the player who took the action.
    pub player: Color,

    /// The type-erased action payload, as defined by the game's variant.
    pub action: Action,

    /// White's remaining clock in milliseconds when the action was recorded;
    /// `None` for untimed games.
    pub clock_white_ms: Option<u64>,

    /// Black's remaining clock in milliseconds when the action was recorded;
    /// `None` for untimed games.
    pub clock_black_ms: Option<u64>,

    /// When the action was recorded (UTC).
    pub created_at: OffsetDateTime,
}

/// Persistence operations for a game's append-only action log.
///
/// Implementations must be [`Send`] and [`Sync`] so they can be shared across
/// async tasks and stored behind an `Arc`.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn ActionLogRepo` or
/// `Arc<dyn ActionLogRepo>`.
#[async_trait]
pub trait ActionLogRepo: Send + Sync {
    /// Appends `action` to `game_id`'s log.
    ///
    /// The log is append-only and keyed by `(game_id, ply)`, so re-appending an
    /// already-recorded `ply` is rejected rather than silently overwritten. This
    /// makes a double-append (e.g. a retried request, or two writers racing on
    /// the same move) detectable by the caller.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Conflict`] if an action with the same `(game_id, ply)`
    ///   already exists.
    /// - [`StorageError::Serialization`] if the action cannot be encoded.
    /// - [`StorageError::Backend`] on driver-level failures.
    ///
    /// [`StorageError::Conflict`]: crate::StorageError::Conflict
    /// [`StorageError::Serialization`]: crate::StorageError::Serialization
    /// [`StorageError::Backend`]: crate::StorageError::Backend
    async fn append(&self, game_id: GameId, action: &RecordedAction) -> StorageResult<()>;

    /// Returns every action recorded for `game_id`, ordered by `ply` ascending.
    ///
    /// Returns an empty vector for a game with no recorded actions (including a
    /// game that does not exist — the log makes no existence claim about the
    /// game itself).
    ///
    /// # Errors
    ///
    /// - [`StorageError::Serialization`] if a stored row cannot be decoded.
    /// - [`StorageError::Backend`] on driver-level failures.
    ///
    /// [`StorageError::Serialization`]: crate::StorageError::Serialization
    /// [`StorageError::Backend`]: crate::StorageError::Backend
    async fn list(&self, game_id: GameId) -> StorageResult<Vec<RecordedAction>>;

    /// Returns the highest `ply` recorded for `game_id`, or `None` if the log is
    /// empty.
    ///
    /// This is the cheap "where did we leave off?" query a recovering server
    /// uses to decide the next expected `ply` without reading the whole log.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Serialization`] if the stored value cannot be decoded.
    /// - [`StorageError::Backend`] on driver-level failures.
    ///
    /// [`StorageError::Serialization`]: crate::StorageError::Serialization
    /// [`StorageError::Backend`]: crate::StorageError::Backend
    async fn last_ply(&self, game_id: GameId) -> StorageResult<Option<u32>>;
}
