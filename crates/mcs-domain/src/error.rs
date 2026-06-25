//! Domain-level error types.

use thiserror::Error;

/// An error produced by domain validation or construction logic.
///
/// All variants represent a caller-supplied value that failed a domain
/// invariant. No I/O errors appear here — this crate is pure logic.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// A string did not parse as a valid `0x`-prefixed 40-hex-char Ethereum
    /// address.
    #[error("invalid Ethereum address: {0}")]
    InvalidAddress(String),

    /// A string did not parse as a valid UUID.
    #[error("invalid id: {0}")]
    InvalidId(String),

    /// A value violated a domain invariant (e.g. an unrecognised enum spelling).
    #[error("validation error: {0}")]
    Validation(String),
}
