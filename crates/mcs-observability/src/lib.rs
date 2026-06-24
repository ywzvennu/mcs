//! # mcs-observability
//!
//! Reusable observability primitives for the Modular Chess Server.
//!
//! This crate provides:
//!
//! - [`ObservabilityConfig`] and [`LogFormat`] — configuration for the global
//!   tracing subscriber.
//! - [`init`] — installs the configured tracing subscriber at application
//!   startup.
//! - [`http_trace_layer`] — a pre-configured
//!   [`tower_http::trace::TraceLayer`] that records HTTP method, path, status
//!   code, and latency.
//! - [`request_id_layers`] — a pair of Tower layers that attach an
//!   `x-request-id` to every request (reading an existing header or generating
//!   a UUID v4) and propagate it to the response.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use mcs_observability::{ObservabilityConfig, LogFormat, init, http_trace_layer, request_id_layers};
//! use tower::ServiceBuilder;
//!
//! // Initialise once at startup, typically in `main`.
//! let cfg = ObservabilityConfig {
//!     format: LogFormat::Pretty,
//!     default_directive: "info".to_owned(),
//! };
//! init(&cfg).expect("tracing subscriber already installed");
//!
//! // Compose the Tower middleware stack.
//! let (set_id, propagate_id) = request_id_layers();
//! let _stack = ServiceBuilder::new()
//!     .layer(set_id)
//!     .layer(propagate_id)
//!     .layer(http_trace_layer());
//! ```

#![doc(html_root_url = "https://docs.rs/mcs-observability")]
#![warn(missing_docs)]

pub mod config;
pub mod error;
pub mod request_id;
pub mod trace;

#[cfg(test)]
mod tests;

pub use config::{LogFormat, ObservabilityConfig};
pub use error::InitError;
pub use request_id::request_id_layers;
pub use trace::http_trace_layer;

/// Installs the global [`tracing_subscriber`] registry.
///
/// The subscriber honours the `RUST_LOG` environment variable; when that is
/// absent or empty the value of [`ObservabilityConfig::default_directive`] is
/// used instead.  The output format is controlled by
/// [`ObservabilityConfig::format`].
///
/// # Errors
///
/// Returns [`InitError`] if a global subscriber is already installed (e.g. in
/// tests that call `init` more than once).  Call this function **once** at
/// process startup.
pub fn init(config: &ObservabilityConfig) -> Result<(), InitError> {
    config::install(config)
}
