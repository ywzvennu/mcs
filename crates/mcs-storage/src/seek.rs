//! Repository trait for [`Seek`] persistence.

use async_trait::async_trait;
use mcs_domain::{Seek, SeekId};

use crate::error::StorageResult;

/// Whether [`SeekRepo::claim`] actually removed a row.
///
/// Returned by the atomic claim used to join an open seek: exactly one of any
/// number of concurrent claimants observes [`Claimed`](ClaimOutcome::Claimed);
/// every other observes [`AlreadyClaimed`](ClaimOutcome::AlreadyClaimed). This
/// is a self-documenting alternative to a bare `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This caller removed the seek and is the one party entitled to act on it.
    Claimed,
    /// The seek was already gone (matched, cancelled, or claimed by a racing
    /// caller); this caller must not proceed.
    AlreadyClaimed,
}

impl ClaimOutcome {
    /// Returns `true` only for the caller that won the claim.
    #[must_use]
    pub fn is_claimed(self) -> bool {
        matches!(self, ClaimOutcome::Claimed)
    }
}

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

    /// Atomically removes a [`Seek`] and reports whether it had existed.
    ///
    /// This is the primitive a direct join (`POST /seeks/{id}/accept`) builds on:
    /// the delete *is* the test, so when several callers race to accept the same
    /// open seek, exactly one observes [`ClaimOutcome::Claimed`] and proceeds to
    /// create the game; every other observes
    /// [`ClaimOutcome::AlreadyClaimed`] and is rejected. [`remove`](Self::remove)
    /// cannot express this because it is deliberately silent about prior
    /// existence.
    ///
    /// # Atomicity
    ///
    /// Implementations **must** perform the existence check and the removal as a
    /// single atomic step (e.g. a `DELETE … RETURNING`/`rows_affected` on a SQL
    /// store, or a single locked `HashMap::remove` in memory). The default
    /// implementation below is *not* atomic and is provided only so existing
    /// in-memory test doubles keep compiling; production stores override it.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn claim(&self, id: SeekId) -> StorageResult<ClaimOutcome> {
        // Non-atomic fallback: adequate only for single-threaded test doubles.
        // Concurrency-correct stores override this with a single atomic delete.
        if self.get(id).await?.is_some() {
            self.remove(id).await?;
            Ok(ClaimOutcome::Claimed)
        } else {
            Ok(ClaimOutcome::AlreadyClaimed)
        }
    }

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
