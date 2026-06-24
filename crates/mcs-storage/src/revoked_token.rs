//! Repository trait for the session-token revocation denylist.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::StorageResult;

/// Persistence for the session-token revocation denylist (#101).
///
/// Session JWTs are stateless, so a token cannot be "un-issued": it stays valid
/// until its `exp`. To support logout, each token carries a unique `jti` (JWT
/// ID). Logging out [`revoke`][RevokedTokenRepo::revoke]s the current token's
/// `jti`; every authenticated request then checks
/// [`is_revoked`][RevokedTokenRepo::is_revoked] after verifying the JWT and
/// rejects a revoked token. A different, non-revoked token is unaffected.
///
/// ## Self-trimming
///
/// A revoked entry only needs to outlive the token it denies: once the token's
/// `exp` passes, JWT verification rejects it regardless of the denylist. So
/// [`revoke`][RevokedTokenRepo::revoke] records the token's expiry, and
/// [`purge_expired`][RevokedTokenRepo::purge_expired] drops entries whose expiry
/// has elapsed, keeping the table bounded.
///
/// Implementations must be [`Send`] and [`Sync`] so they can be shared across
/// async tasks behind an `Arc`.
///
/// # Object safety
///
/// This trait is object-safe. Callers may hold it as `&dyn RevokedTokenRepo` or
/// `Arc<dyn RevokedTokenRepo>`.
#[async_trait]
pub trait RevokedTokenRepo: Send + Sync {
    /// Records a token's `jti` as revoked until `expires_at`.
    ///
    /// Idempotent: revoking an already-revoked `jti` is not an error — the entry
    /// is left in place (its expiry is the same token's expiry either way).
    ///
    /// `expires_at` is the token's own expiry; the entry need not outlive it.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`](crate::error::StorageError::Backend) on
    ///   driver-level failures.
    async fn revoke(&self, jti: &str, expires_at: OffsetDateTime) -> StorageResult<()>;

    /// Returns whether `jti` is present in the denylist (i.e. revoked).
    ///
    /// This is checked on every authenticated request, so it is a single
    /// indexed point lookup. An entry whose `expires_at` has already passed is
    /// harmless to report as revoked (the token is independently rejected on
    /// expiry), but `purge_expired` keeps such entries from accumulating.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`](crate::error::StorageError::Backend) on
    ///   driver-level failures.
    async fn is_revoked(&self, jti: &str) -> StorageResult<bool>;

    /// Deletes every denylist entry whose `expires_at` is at or before `now`,
    /// returning how many were removed.
    ///
    /// Such entries are dead weight: the tokens they deny are already rejected
    /// on expiry. Run this periodically (or opportunistically) to keep the
    /// denylist bounded by the number of *unexpired* revoked tokens.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`](crate::error::StorageError::Backend) on
    ///   driver-level failures.
    async fn purge_expired(&self, now: OffsetDateTime) -> StorageResult<u64>;
}
