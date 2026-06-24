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

impl From<sqlx::Error> for StorageError {
    /// Maps a sqlx driver error into the driver-agnostic [`StorageError`].
    ///
    /// The mapping follows the contract documented on the repository traits:
    ///
    /// * [`sqlx::Error::RowNotFound`] becomes [`StorageError::NotFound`] — this
    ///   is what `fetch_one` returns when a `get` matches no row.
    /// * A unique-/primary-key constraint violation becomes
    ///   [`StorageError::Conflict`], carrying the constraint name when the
    ///   driver exposes it.
    /// * Everything else (connection failures, timeouts, protocol errors)
    ///   becomes [`StorageError::Backend`].
    fn from(err: sqlx::Error) -> Self {
        if matches!(err, sqlx::Error::RowNotFound) {
            return StorageError::NotFound;
        }

        if let sqlx::Error::Database(db_err) = &err {
            if db_err.is_unique_violation() {
                // `constraint()` is populated by Postgres; SQLite leaves it
                // `None`, so fall back to the driver message.
                let detail = db_err
                    .constraint()
                    .map(str::to_owned)
                    .unwrap_or_else(|| db_err.message().to_owned());
                return StorageError::Conflict(detail);
            }
        }

        StorageError::Backend(err.to_string())
    }
}
