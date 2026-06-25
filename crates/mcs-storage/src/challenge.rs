//! Repository trait for [`Challenge`] persistence.

use async_trait::async_trait;
use mcs_domain::{Challenge, ChallengeId, UserId};
use time::OffsetDateTime;

use crate::error::StorageResult;

/// Persistence operations for direct [`Challenge`] aggregates.
///
/// A challenge is an invitation from one specific player to another. This trait
/// covers its lifecycle: create it, retrieve it, list the pending ones a user is
/// involved in (incoming / outgoing), and update its status as it is accepted,
/// declined, or canceled.
///
/// Implementations must be [`Send`] and [`Sync`] so they can be shared across
/// async tasks and stored behind an `Arc`.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn ChallengeRepo` or
/// `Arc<dyn ChallengeRepo>`.
#[async_trait]
pub trait ChallengeRepo: Send + Sync {
    /// Persists a new [`Challenge`].
    ///
    /// # Errors
    ///
    /// - [`StorageError::Conflict`](crate::StorageError::Conflict) if a challenge
    ///   with the same `id` already exists.
    /// - [`StorageError::Backend`](crate::StorageError::Backend) on driver-level
    ///   failures.
    async fn create(&self, challenge: &Challenge) -> StorageResult<()>;

    /// Retrieves a [`Challenge`] by its [`ChallengeId`].
    ///
    /// Returns [`StorageError::NotFound`](crate::StorageError::NotFound) when no
    /// challenge matches, mirroring the `get`-by-id convention of
    /// [`UserRepo`](crate::UserRepo) and [`GameRepo`](crate::GameRepo) (which
    /// return the entity directly rather than an `Option`).
    ///
    /// # Errors
    ///
    /// - [`StorageError::NotFound`](crate::StorageError::NotFound) when the id has
    ///   no matching challenge.
    /// - [`StorageError::Backend`](crate::StorageError::Backend) on driver-level
    ///   failures.
    async fn get(&self, id: ChallengeId) -> StorageResult<Challenge>;

    /// Lists the **pending** challenges *issued to* `user` (where `user` is the
    /// challenged party), in no guaranteed order.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`](crate::StorageError::Backend) on driver-level
    ///   failures.
    async fn list_incoming(&self, user: UserId) -> StorageResult<Vec<Challenge>>;

    /// Lists the **pending** challenges *issued by* `user` (where `user` is the
    /// challenger), in no guaranteed order.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`](crate::StorageError::Backend) on driver-level
    ///   failures.
    async fn list_outgoing(&self, user: UserId) -> StorageResult<Vec<Challenge>>;

    /// Persists a status change to an existing [`Challenge`] (accept, decline, or
    /// cancel).
    ///
    /// # Errors
    ///
    /// - [`StorageError::NotFound`](crate::StorageError::NotFound) if no challenge
    ///   with the given `id` exists.
    /// - [`StorageError::Backend`](crate::StorageError::Backend) on driver-level
    ///   failures.
    async fn update(&self, challenge: &Challenge) -> StorageResult<()>;

    /// Deletes resolved (Declined or Canceled) challenges whose `created_at` is
    /// strictly before `older_than`, returning the count removed.
    ///
    /// Accepted, declined, and canceled challenges are terminal. Accepted
    /// challenges are attached to a live game and should be kept for history.
    /// Declined and canceled challenges have no associated game and are safe to
    /// prune after the configured retention window. Run this periodically with a
    /// cutoff of `now - max_age` to keep the table bounded.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`](crate::StorageError::Backend) on driver-level
    ///   failures.
    async fn purge_resolved(&self, older_than: OffsetDateTime) -> StorageResult<u64>;
}
