//! Repository trait for [`User`] persistence.

use async_trait::async_trait;
use mcs_domain::{EvmAddress, User, UserId};

use crate::error::StorageResult;

/// Persistence operations for [`User`] aggregates.
///
/// Implementations must be [`Send`] and [`Sync`] so they can be shared across
/// async tasks and stored behind an `Arc`.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn UserRepo` or
/// `Arc<dyn UserRepo>`.
#[async_trait]
pub trait UserRepo: Send + Sync {
    /// Persists a new [`User`].
    ///
    /// # Errors
    ///
    /// - [`StorageError::Conflict`] if a user with the same `id` or `address`
    ///   already exists.
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn create(&self, user: &User) -> StorageResult<()>;

    /// Retrieves a [`User`] by its [`UserId`].
    ///
    /// # Errors
    ///
    /// - [`StorageError::NotFound`] if no user with the given `id` exists.
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn get(&self, id: UserId) -> StorageResult<User>;

    /// Looks up a [`User`] by their [`EvmAddress`].
    ///
    /// Returns `Ok(None)` when no user is registered with that address — this
    /// is the expected state for a first-time SIWE login. Use
    /// [`upsert_by_address`][UserRepo::upsert_by_address] when you always want
    /// a [`User`] back.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn find_by_address(&self, addr: &EvmAddress) -> StorageResult<Option<User>>;

    /// Returns the existing [`User`] for `addr`, or creates one if none exists.
    ///
    /// This is the canonical entry-point for a Sign-In-With-Ethereum (SIWE)
    /// login flow: the caller does not need to check whether the wallet is new.
    ///
    /// Implementations must handle the race condition where two concurrent
    /// requests create a user for the same address — typically by relying on a
    /// unique index and returning the winner's row.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn upsert_by_address(&self, addr: &EvmAddress) -> StorageResult<User>;
}
