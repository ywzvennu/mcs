//! Error type returned by [`NodeRegistry`](crate::NodeRegistry) operations.

/// Failure modes for cluster coordination.
///
/// Backend-specific errors (a dropped Redis connection, a malformed reply, a
/// timeout) are flattened into [`ClusterError::Backend`] with a human-readable
/// message so callers do not have to depend on any particular backend's error
/// crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClusterError {
    /// The coordination backend (e.g. Redis) reported a failure.
    #[error("cluster backend error: {0}")]
    Backend(String),
}

#[cfg(feature = "redis")]
impl From<redis::RedisError> for ClusterError {
    fn from(value: redis::RedisError) -> Self {
        Self::Backend(value.to_string())
    }
}
