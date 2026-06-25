//! Repository trait for auth-session and nonce persistence.

use async_trait::async_trait;
use mcs_domain::EvmAddress;
use time::OffsetDateTime;

use crate::error::StorageResult;

/// Persistence operations for SIWE (Sign-In-With-Ethereum) auth sessions.
///
/// The primary responsibility of this trait is nonce lifecycle management.
/// A nonce is a short-lived, single-use random string that is included in the
/// SIWE message the wallet signs. Enforcing single-use prevents replay attacks:
/// once a nonce has been consumed to verify a signature, it cannot be used
/// again — even if an attacker captured the signed SIWE message.
///
/// ## Replay-prevention contract
///
/// 1. **Issue** – the server generates a random nonce and calls
///    [`store_nonce`][SessionRepo::store_nonce] with an `expires_at` in the
///    near future (typically 5–15 minutes).
/// 2. **Sign** – the client includes the nonce in the EIP-4361 message and
///    signs it with their wallet.
/// 3. **Verify** – the server verifies the signature and calls
///    [`consume_nonce`][SessionRepo::consume_nonce]:
///    - If the nonce exists **and** has not expired, it is **atomically
///      deleted** and `Ok(true)` is returned. The caller may proceed to issue
///      a session token.
///    - If the nonce does not exist (already consumed, or never stored), or
///      has expired, `Ok(false)` is returned. The caller must reject the login.
///    The atomicity guarantee (read-then-delete in a single operation or
///    transaction) is essential: without it, two concurrent verify requests
///    with the same nonce could both see it as valid before either deletes it.
///
/// Implementations must be [`Send`] and [`Sync`] so they can be shared across
/// async tasks and stored behind an `Arc`.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn SessionRepo` or
/// `Arc<dyn SessionRepo>`.
#[async_trait]
pub trait SessionRepo: Send + Sync {
    /// Stores a newly issued nonce for the given Ethereum address.
    ///
    /// If a nonce already exists for this `(address, nonce)` pair it should be
    /// overwritten — the earlier one is superseded.
    ///
    /// `expires_at` is the wall-clock time after which the nonce must not be
    /// accepted, even if it has not been consumed.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn store_nonce(
        &self,
        address: &EvmAddress,
        nonce: &str,
        expires_at: OffsetDateTime,
    ) -> StorageResult<()>;

    /// Atomically consumes a nonce, returning whether it was valid.
    ///
    /// "Consume" means: if the nonce exists for `address` **and** has not
    /// passed its `expires_at` timestamp, delete it and return `Ok(true)`.
    /// In all other cases (unknown, expired) return `Ok(false)` — do **not**
    /// return an error for these expected-negative outcomes.
    ///
    /// Implementations must guarantee that two concurrent calls for the same
    /// `(address, nonce)` pair return `true` at most once.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn consume_nonce(&self, address: &EvmAddress, nonce: &str) -> StorageResult<bool>;

    /// Deletes every nonce whose `expires_at` is at or before `now`, returning
    /// how many were removed.
    ///
    /// Expired nonces have already passed their validity window and can never
    /// be successfully consumed. Run this periodically to keep the `auth_nonces`
    /// table bounded by the number of *unexpired* nonces.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] on driver-level failures.
    async fn purge_expired_nonces(&self, now: OffsetDateTime) -> StorageResult<u64>;
}
