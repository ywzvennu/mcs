//! The Prometheus metrics recorder and the `GET /metrics` scrape endpoint.
//!
//! This module is the **exporter** half of the observability story (#88): it
//! installs a [`metrics_exporter_prometheus`] recorder as the process-global
//! [`metrics`] sink and exposes the rendered metrics at `GET /metrics`. The
//! *measurement* half — the metric names, the HTTP middleware, and the domain
//! counters — lives in [`mcs_api::metrics`], which records through the facade
//! and is decoupled from any particular exporter.
//!
//! # One global recorder
//!
//! A `metrics` recorder is a process-wide singleton: it can be installed exactly
//! once. The library wiring ([`crate::router`], [`crate::build_app`]) is, by
//! contrast, exercised many times in a single test binary. [`handle`] therefore
//! installs the recorder behind a [`OnceLock`] and hands back a clone of the
//! cached [`PrometheusHandle`] on every later call — so building the app twice
//! in one process is safe, and tests get a working `/metrics` without racing on
//! installation.

use std::sync::OnceLock;

use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// The process-global Prometheus render handle, installed at most once.
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Returns the process-global [`PrometheusHandle`], installing the recorder on
/// first call.
///
/// The first invocation builds a [`PrometheusBuilder`], installs it as the
/// global [`metrics`] recorder, and caches the resulting render handle; every
/// later invocation returns a clone of that cached handle. Installation happens
/// at most once per process, so calling [`crate::router`] repeatedly (as the
/// test suite does) never attempts a second, failing install.
///
/// If installation fails — only really possible when another recorder was
/// already installed by something outside this module — the error is logged and
/// a fresh, *uninstalled* handle is returned so `/metrics` still renders (it
/// will simply report whatever the already-installed recorder exposes, or an
/// empty body) rather than panicking the server at start-up.
pub fn handle() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| match PrometheusBuilder::new().install_recorder() {
            Ok(handle) => {
                tracing::info!("installed Prometheus metrics recorder");
                handle
            }
            Err(error) => {
                tracing::warn!(%error, "failed to install Prometheus recorder; /metrics may be empty");
                // Build a detached handle so `/metrics` still has something to
                // render. Rendering it is harmless even though it is not wired
                // to the global recorder.
                PrometheusBuilder::new().build_recorder().handle()
            }
        })
        .clone()
}

/// Builds the `GET /metrics` sub-router backed by `handle`.
///
/// The endpoint renders the current Prometheus exposition text with the
/// `text/plain; version=0.0.4` content type Prometheus expects. It is a plain
/// state-free route — the render handle is captured by the closure — so it does
/// not depend on [`AppState`](mcs_api::AppState) and is merged into the
/// top-level router by [`crate::router`].
pub fn metrics_router(handle: PrometheusHandle) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
            let body = handle.render();
            std::future::ready(
                ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response(),
            )
        }),
    )
}
