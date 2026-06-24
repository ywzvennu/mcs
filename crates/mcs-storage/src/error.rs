//! Storage error type and result alias.

use thiserror::Error;

/// Errors that can occur while interacting with the persistence layer.
///
/// Concrete database implementations map their driver-specific errors into
/// these variants so that callers never need to depend on a particular driver.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The requested entity does not exist in the store.
    ///
    /// Returned by `get` operations when no row matches the supplied key.
    #[error("not found")]
    NotFound,

    /// A uniqueness constraint was violated.
    ///
    /// The inner string carries a human-readable description of which
    /// constraint fired (e.g. `"users.address"`). Callers that need to
    /// distinguish conflict errors from other errors should match on this
    /// variant; callers that only surface the message can use `Display`.
    #[error("conflict: {0}")]
    Conflict(String),

    /// An unexpected backend error occurred (driver error, connection failure,
    /// timeout, etc.).
    ///
    /// The inner string is the `Display` form of the underlying driver error,
    /// kept as a `String` so this crate stays free of sqlx / any other driver.
    #[error("backend error: {0}")]
    Backend(String),

    /// A serialization or deserialization error occurred while converting
    /// between domain types and their persisted representation.
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Shorthand for `Result<T, StorageError>`.
///
/// All repository methods return this type.
pub type StorageResult<T> = Result<T, StorageError>;
