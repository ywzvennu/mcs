//! Repository trait for per-user, per-`(variant, time_class)` [`Rating`]
//! persistence.

use async_trait::async_trait;
use mcs_domain::{Rating, TimeClass, UserId};

use crate::error::StorageResult;

/// Persistence operations for Glicko-2 [`Rating`] records.
///
/// Ratings are keyed by `(user_id, variant_id, time_class)`.  The variant
/// identifier is an application-level string (e.g. `"standard"`, `"chess960"`) —
/// this crate treats it as an opaque `TEXT` column and imposes no foreign-key
/// constraint. The [`TimeClass`] splits each variant's rating per pace (bullet,
/// blitz, rapid, classical, correspondence), so a player holds an independent
/// rating for every `(variant, time_class)` combination they have played.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn RatingRepo` or
/// `Arc<dyn RatingRepo>`.
#[async_trait]
pub trait RatingRepo: Send + Sync {
    /// Returns the current [`Rating`] for `user` in `variant_id` at
    /// `time_class`, or `None` if no rating record exists yet.
    ///
    /// A missing record is a normal, expected outcome for a player who has not
    /// yet played a rated game in this `(variant, time_class)`; callers should
    /// treat it as the Glicko-2 seed rating rather than an error.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn get(
        &self,
        user: UserId,
        variant_id: &str,
        time_class: TimeClass,
    ) -> StorageResult<Option<Rating>>;

    /// Inserts or replaces the [`Rating`] for `user` in `variant_id` at
    /// `time_class`.
    ///
    /// If a rating row already exists for the
    /// `(user_id, variant_id, time_class)` triple, all three fields (`value`,
    /// `deviation`, `volatility`) are overwritten with the supplied values. If
    /// no row exists, one is created.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn upsert(
        &self,
        user: UserId,
        variant_id: &str,
        time_class: TimeClass,
        rating: &Rating,
    ) -> StorageResult<()>;

    /// Returns one page of the leaderboard for `(variant_id, time_class)`,
    /// ordered by `value` descending (highest-rated first).
    ///
    /// `offset` is the zero-based starting position in the full ranking; the
    /// first page has `offset = 0`. If `offset` is beyond the last ranked
    /// player the returned `Vec` is empty.  If fewer than `limit` rows remain
    /// after the offset the returned `Vec` is shorter than `limit`.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn leaderboard(
        &self,
        variant_id: &str,
        time_class: TimeClass,
        offset: u32,
        limit: u32,
    ) -> StorageResult<Vec<(UserId, Rating)>>;

    /// Returns the total number of players with a rating in
    /// `(variant_id, time_class)`.
    ///
    /// This is the denominator for pagination: a caller can compute the total
    /// number of pages as `ceil(total / page_size)`.  An empty bucket returns
    /// `0`.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn leaderboard_count(
        &self,
        variant_id: &str,
        time_class: TimeClass,
    ) -> StorageResult<u64>;

    /// Returns every rating `user` holds, as `(variant_id, time_class, rating)`
    /// triples.
    ///
    /// A player with no rating row yields an empty `Vec` — that is the normal
    /// state for a freshly registered user who has not yet played a rated game.
    /// The returned order is unspecified; callers that need a stable order
    /// should sort by `(variant_id, time_class)`.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn list_for_user(&self, user: UserId) -> StorageResult<Vec<(String, TimeClass, Rating)>>;
}
