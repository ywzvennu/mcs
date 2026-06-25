//! Repository trait for per-user, per-variant [`Rating`] persistence.

use async_trait::async_trait;
use mcs_domain::{Rating, UserId};

use crate::error::StorageResult;

/// Persistence operations for Glicko-2 [`Rating`] records.
///
/// Ratings are keyed by `(user_id, variant_id)`.  The variant identifier is an
/// application-level string (e.g. `"standard"`, `"chess960"`) — this crate
/// treats it as an opaque `TEXT` column and imposes no foreign-key constraint.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn RatingRepo` or
/// `Arc<dyn RatingRepo>`.
#[async_trait]
pub trait RatingRepo: Send + Sync {
    /// Returns the current [`Rating`] for `user` in `variant_id`, or `None` if
    /// no rating record exists yet.
    ///
    /// A missing record is a normal, expected outcome for a newly registered
    /// player who has not yet played a rated game in this variant; callers
    /// should treat it as the Glicko-2 seed rating rather than an error.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn get(&self, user: UserId, variant_id: &str) -> StorageResult<Option<Rating>>;

    /// Inserts or replaces the [`Rating`] for `user` in `variant_id`.
    ///
    /// If a rating row already exists for the `(user_id, variant_id)` pair, all
    /// three fields (`value`, `deviation`, `volatility`) are overwritten with
    /// the supplied values. If no row exists, one is created.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn upsert(&self, user: UserId, variant_id: &str, rating: &Rating) -> StorageResult<()>;

    /// Returns the top `limit` players for `variant_id`, ordered by `value`
    /// descending (highest-rated first).
    ///
    /// If fewer than `limit` rows exist for the variant the returned `Vec` is
    /// shorter than `limit`. An empty variant returns an empty `Vec`.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn leaderboard(
        &self,
        variant_id: &str,
        limit: u32,
    ) -> StorageResult<Vec<(UserId, Rating)>>;

    /// Returns every variant rating `user` holds, as `(variant_id, rating)`
    /// pairs.
    ///
    /// A player with no rating row in any variant yields an empty `Vec` — that
    /// is the normal state for a freshly registered user who has not yet played
    /// a rated game. The returned order is unspecified; callers that need a
    /// stable order should sort by `variant_id`.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn list_for_user(&self, user: UserId) -> StorageResult<Vec<(String, Rating)>>;
}
