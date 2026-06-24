//! Repository trait for [`Seek`] persistence.

use async_trait::async_trait;
use mcs_domain::{Seek, SeekId};

use crate::error::StorageResult;

/// Persistence operations for [`Seek`] matchmaking aggregates.
///
/// A seek represents an open challenge in the matchmaking pool. This trait
/// covers the lifecycle: create, retrieve, remove, and list open seeks.
///
/// Implementations must be [`Send`] and [`Sync`] so they can be shared across
/// async tasks and stored behind an `Arc`.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn SeekRepo` or
/// `Arc<dyn SeekRepo>`.
#[async_trait]
pub trait SeekRepo: Send + Sync {
    /// Persists a new open [`Seek`].
    ///
    /// # Errors
    ///
    /// - [`StorageError::Conflict`] if a seek with the same `id` already
    ///   exists.
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn create(&self, seek: &Seek) -> StorageResult<()>;

    /// Retrieves a [`Seek`] by its [`SeekId`].
    ///
    /// Returns `Ok(None)` when the seek has already been removed (matched or
    /// cancelled), rather than an error, because this is expected in normal
    /// racing scenarios.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn get(&self, id: SeekId) -> StorageResult<Option<Seek>>;

    /// Removes a [`Seek`] from the pool (matched or cancelled).
    ///
    /// This operation is idempotent: removing a seek that no longer exists is
    /// not an error — the desired post-condition (seek absent) is already met.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn remove(&self, id: SeekId) -> StorageResult<()>;

    /// Returns all seeks currently awaiting a match, in no guaranteed order.
    ///
    /// The matchmaking layer should refresh this list frequently and use it to
    /// detect compatible pairs.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn list_open(&self) -> StorageResult<Vec<Seek>>;
}
