//! Repository trait for append-only per-user, per-variant rating history.

use async_trait::async_trait;
use mcs_domain::{RatingHistoryEntry, UserId};

use crate::error::StorageResult;

/// Persistence operations for the append-only rating-history log.
///
/// Each [`RatingHistoryEntry`] is a snapshot of a player's rating in a variant
/// taken the moment a rated game was scored. The log is append-only: the
/// [`RatingUpdateHook`](../../mcs_api/rating/struct.RatingUpdateHook.html)
/// records one row per player after each rated game, so a single rated game
/// appends exactly two rows (one for White, one for Black).
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn RatingHistoryRepo`
/// or `Arc<dyn RatingHistoryRepo>`.
#[async_trait]
pub trait RatingHistoryRepo: Send + Sync {
    /// Appends a single rating-history snapshot.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn record(&self, entry: &RatingHistoryEntry) -> StorageResult<()>;

    /// Returns up to `limit` history snapshots for `user` in `variant_id`,
    /// **most-recent-first** (descending by recorded time).
    ///
    /// A player with no history in the variant yields an empty `Vec`.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn list(
        &self,
        user: UserId,
        variant_id: &str,
        limit: u32,
    ) -> StorageResult<Vec<RatingHistoryEntry>>;
}
