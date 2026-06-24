//! Prometheus instrumentation for the MCS HTTP API.
//!
//! This module is the **measurement surface** of the server: it defines the
//! metric names, the HTTP request middleware, and the readiness probe, but it
//! records through the [`metrics`] facade only — it never installs a recorder.
//! The composition root (`mcs-server`) installs a
//! [`PrometheusRecorder`](metrics_exporter_prometheus) at start-up and exposes
//! the rendered text at `GET /metrics`; everything here simply emits into
//! whatever recorder is installed (a no-op recorder when none is, so the macros
//! are always safe to call, including from tests that never install one).
//!
//! # Metric catalogue
//!
//! | Metric                          | Kind      | Labels                       |
//! |---------------------------------|-----------|------------------------------|
//! | [`HTTP_REQUESTS_TOTAL`]         | counter   | `method`, `path`, `status`   |
//! | [`HTTP_REQUEST_DURATION`]       | histogram | `method`, `path`, `status`   |
//! | [`GAMES_LIVE`]                  | gauge     | —                            |
//! | [`GAMES_CREATED_TOTAL`]         | counter   | —                            |
//! | [`RATING_UPDATES_TOTAL`]        | counter   | —                            |
//! | [`WS_CONNECTIONS_ACTIVE`]       | gauge     | —                            |
//!
//! The `path` label is always a **route template** (e.g. `/games/{id}`), never
//! a concrete path carrying an id, so cardinality stays bounded no matter how
//! many distinct games or users are served. See [`http_metrics`].
//!
//! # Domain instrumentation
//!
//! The domain metrics are incremented at the single point each event happens:
//! [`record_game_created`] in
//! [`create_and_spawn_game`](crate::AppState::create_and_spawn_game),
//! [`record_rating_update`] in the rating completion hook
//! ([`crate::rating`]), and [`ws_connection_opened`] /
//! [`ws_connection_closed`] around the WebSocket connection task
//! ([`crate::ws`]). The live-games gauge is sampled lazily by the readiness and
//! metrics handlers via [`sample_live_games`], so it always reflects the hub's
//! current [`len`](crate::GameHub::len) without a background task.

use std::time::Instant;

use axum::extract::MatchedPath;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use metrics::{counter, gauge, histogram};

use crate::state::AppState;

/// Counter: total HTTP requests served, labelled by `method`, route `path`
/// template, and response `status`.
pub const HTTP_REQUESTS_TOTAL: &str = "mcs_http_requests_total";

/// Histogram: HTTP request handling latency in **seconds**, labelled by
/// `method`, route `path` template, and response `status`.
pub const HTTP_REQUEST_DURATION: &str = "mcs_http_request_duration_seconds";

/// Gauge: number of live games currently registered in the game hub.
pub const GAMES_LIVE: &str = "mcs_games_live";

/// Counter: total games created (matchmaking pairings and accepted challenges /
/// rematches alike).
pub const GAMES_CREATED_TOTAL: &str = "mcs_games_created_total";

/// Counter: total post-game rating updates applied.
pub const RATING_UPDATES_TOTAL: &str = "mcs_rating_updates_total";

/// Gauge: number of currently open live-game WebSocket connections.
pub const WS_CONNECTIONS_ACTIVE: &str = "mcs_ws_connections_active";

/// Tower middleware that records one HTTP request's count and latency.
///
/// Mounted on the API router via [`axum::middleware::from_fn`], it wraps every
/// request: it reads the matched **route template** (so `/games/{id}` rather
/// than `/games/abc-123`, keeping label cardinality bounded), times the inner
/// service, then increments [`HTTP_REQUESTS_TOTAL`] and observes
/// [`HTTP_REQUEST_DURATION`] — both labelled by `method`, `path`, and the
/// response `status`.
///
/// A request that does not match any route (a 404) has no
/// [`MatchedPath`]; it is recorded under the literal path label `"<unmatched>"`
/// so unmatched traffic is still counted without exploding cardinality on
/// arbitrary client-supplied URLs.
pub async fn http_metrics(
    matched: Option<MatchedPath>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    // The matched route template keeps the `path` label low-cardinality. Falling
    // back to a fixed sentinel means random 404 URLs never become labels.
    let path = matched
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_owned());

    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed = start.elapsed().as_secs_f64();

    let status = response.status().as_u16().to_string();
    let labels = [
        ("method", method.as_str().to_owned()),
        ("path", path),
        ("status", status),
    ];

    counter!(HTTP_REQUESTS_TOTAL, &labels).increment(1);
    histogram!(HTTP_REQUEST_DURATION, &labels).record(elapsed);

    response
}

/// Records a game-creation event: increments [`GAMES_CREATED_TOTAL`].
///
/// Called once from [`create_and_spawn_game`](crate::AppState::create_and_spawn_game),
/// the single creation path shared by matchmaking, challenge acceptance, and
/// live rematches.
pub fn record_game_created() {
    counter!(GAMES_CREATED_TOTAL).increment(1);
}

/// Records a post-game rating update: increments [`RATING_UPDATES_TOTAL`].
///
/// Called once per finished **rated** game from the rating completion hook
/// (see [`crate::rating`]) after both players' ratings have been persisted.
pub fn record_rating_update() {
    counter!(RATING_UPDATES_TOTAL).increment(1);
}

/// Records that a live-game WebSocket connection opened: increments
/// [`WS_CONNECTIONS_ACTIVE`].
///
/// Paired with [`ws_connection_closed`], which must run exactly once when the
/// same connection ends, so the gauge tracks the count of currently open
/// sockets.
pub fn ws_connection_opened() {
    gauge!(WS_CONNECTIONS_ACTIVE).increment(1.0);
}

/// Records that a live-game WebSocket connection closed: decrements
/// [`WS_CONNECTIONS_ACTIVE`].
///
/// The counterpart of [`ws_connection_opened`]; run from the connection task's
/// teardown so a clean disconnect, a client drop, and an actor stop all release
/// the gauge.
pub fn ws_connection_closed() {
    gauge!(WS_CONNECTIONS_ACTIVE).decrement(1.0);
}

/// Samples the live-games gauge [`GAMES_LIVE`] from the current hub size.
///
/// This is a pull-style sample: it reads [`GameHub::len`](crate::GameHub::len)
/// and sets the gauge, returning the value. The readiness and metrics handlers
/// call it just before responding, so a `/metrics` scrape always reflects the
/// hub's size at scrape time without a background sampler task.
pub fn sample_live_games(state: &AppState) -> usize {
    let live = state.game_hub().len();
    // `as f64` is exact for any plausible live-game count.
    gauge!(GAMES_LIVE).set(live as f64);
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exported metric names are a stable scrape contract: a dashboard or
    /// alert keys off them, so pin the exact strings to catch an accidental
    /// rename.
    #[test]
    fn metric_names_are_stable() {
        assert_eq!(HTTP_REQUESTS_TOTAL, "mcs_http_requests_total");
        assert_eq!(HTTP_REQUEST_DURATION, "mcs_http_request_duration_seconds");
        assert_eq!(GAMES_LIVE, "mcs_games_live");
        assert_eq!(GAMES_CREATED_TOTAL, "mcs_games_created_total");
        assert_eq!(RATING_UPDATES_TOTAL, "mcs_rating_updates_total");
        assert_eq!(WS_CONNECTIONS_ACTIVE, "mcs_ws_connections_active");
    }

    /// Every recording helper is callable without a recorder installed: the
    /// `metrics` facade routes to a no-op recorder by default, so these never
    /// panic in a test process that never installs an exporter.
    #[test]
    fn recording_helpers_are_safe_without_a_recorder() {
        record_game_created();
        record_rating_update();
        ws_connection_opened();
        ws_connection_closed();
    }
}
