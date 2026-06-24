//! Observability configuration and subscriber installation.

use tracing_subscriber::{fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

use crate::InitError;

/// Selects the log output format.
///
/// - [`LogFormat::Pretty`] — human-readable, multi-line format suitable for
///   local development.
/// - [`LogFormat::Json`] — machine-readable, single-line JSON suitable for
///   structured log aggregation in production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable, coloured output (default).
    #[default]
    Pretty,
    /// Newline-delimited JSON for log aggregation pipelines.
    Json,
}

/// Configuration for the global tracing subscriber.
///
/// Pass this to [`crate::init`] once at process startup.
///
/// # Example
///
/// ```rust
/// use mcs_observability::{ObservabilityConfig, LogFormat};
///
/// let cfg = ObservabilityConfig {
///     format: LogFormat::Pretty,
///     default_directive: "debug".to_owned(),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct ObservabilityConfig {
    /// Output format for log lines.
    pub format: LogFormat,
    /// Fallback filter directive used when `RUST_LOG` is not set.
    ///
    /// The syntax follows [`tracing_subscriber::EnvFilter`].  A plain level
    /// string such as `"info"` enables that level for all targets; a
    /// comma-separated list such as `"info,mcs_api=debug"` enables
    /// per-target overrides.
    pub default_directive: String,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Pretty,
            default_directive: "info".to_owned(),
        }
    }
}

/// Installs the tracing subscriber described by `config`.
///
/// This is the implementation backing [`crate::init`].  It is kept in this
/// module so the public `init` function can have a clean, short signature.
pub(crate) fn install(config: &ObservabilityConfig) -> Result<(), InitError> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.default_directive));

    let registry = tracing_subscriber::registry().with(filter);

    match config.format {
        LogFormat::Pretty => registry
            .with(fmt::layer().pretty())
            .try_init()
            .map_err(InitError::from),
        LogFormat::Json => registry
            .with(fmt::layer().json())
            .try_init()
            .map_err(InitError::from),
    }
}
