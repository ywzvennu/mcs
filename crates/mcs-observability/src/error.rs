//! Error types for [`mcs_observability`](crate).

/// Errors that can occur when initialising the tracing subscriber.
#[derive(Debug)]
pub struct InitError(pub(crate) tracing_subscriber::util::TryInitError);

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to install tracing subscriber: {}", self.0)
    }
}

impl std::error::Error for InitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<tracing_subscriber::util::TryInitError> for InitError {
    fn from(e: tracing_subscriber::util::TryInitError) -> Self {
        Self(e)
    }
}
