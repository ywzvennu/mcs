//! Authentication error type.

use thiserror::Error;

/// An error produced while building, verifying, or issuing authentication
/// material.
///
/// Variants are intentionally coarse so that error messages returned to a
/// client do not leak which precise check failed (a common foot-gun in
/// signature-verification APIs). For example, a malformed signature and a
/// signature from the wrong key are kept distinguishable internally but both
/// represent an authentication failure the caller should treat as "rejected".
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The SIWE challenge string could not be parsed as a valid EIP-4361
    /// message, or a field inside it (such as the address) was malformed.
    #[error("invalid SIWE message")]
    InvalidMessage,

    /// The signature is malformed, the wrong length, or did not
    /// cryptographically verify against the message.
    #[error("signature verification failed")]
    SignatureVerification,

    /// The signature verified, but the recovered signer does not match the
    /// address claimed inside the signed message. This indicates an attempt to
    /// authenticate as an address the caller does not control.
    #[error("recovered signer does not match the claimed address")]
    AddressMismatch,

    /// A time-bound credential is outside its validity window: a SIWE message
    /// whose `not_before` is in the future or `expiration_time` is in the past,
    /// or a session token whose `exp` claim has elapsed.
    #[error("credential is expired or not yet valid")]
    Expired,

    /// A session token is malformed, was signed with a different secret, has an
    /// unexpected issuer, or otherwise failed validation (excluding plain
    /// expiry, which is reported as [`AuthError::Expired`]).
    #[error("invalid session token")]
    InvalidToken,

    /// A failure that does not fit the categories above (for example, the
    /// system clock could not be read, or token encoding failed unexpectedly).
    #[error("authentication error: {0}")]
    Other(String),
}
