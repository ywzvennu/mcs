//! The readiness probe: `GET /ready`.
//!
//! Liveness (`GET /health`, served by `mcs-server`) answers "is the process
//! up?" and touches no dependency. **Readiness** answers the stricter question
//! "can this node actually serve traffic *right now*?" — it verifies every
//! backing dependency the request path needs before reporting `200`.
//!
//! An orchestrator routes traffic by readiness: a node that is up but cannot
//! reach its database (or, in a cluster, its membership store) should be pulled
//! out of the load-balancer rotation rather than serving errors. So this probe
//! is allowed to be a little more expensive than liveness, but it is still kept
//! cheap — a single bounded query, no scans.
//!
//! # Checks
//!
//! 1. **Database** — a `LIMIT 1` read via
//!    [`GameRepo::list_recent`](mcs_storage::GameRepo::list_recent). Reusing an
//!    existing bounded query avoids adding a storage method just for the probe
//!    and proves the pool can round-trip a real statement.
//! 2. **Cluster membership** — a read of the live node set via
//!    [`NodeRegistry::live_nodes`](mcs_cluster::NodeRegistry::live_nodes).
//!    Single-node this is the in-memory [`LocalRegistry`](mcs_cluster::LocalRegistry)
//!    and always succeeds (a no-op check); when cluster mode is enabled it is
//!    the Redis-backed registry, so this read is exactly the Redis health check
//!    the issue calls for.
//!
//! # Response
//!
//! - **All checks pass** → `200 OK` with `{"status":"ready"}`.
//! - **Any check fails** → `503 Service Unavailable` with
//!   `{"status":"unavailable","failed":"<dependency>"}` naming the first
//!   dependency that failed (`"database"` or `"cluster"`), so an operator reading
//!   the probe output sees immediately *what* is unhealthy.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::metrics::sample_live_games;
use crate::state::AppState;

/// The body of a successful readiness response.
#[derive(Debug, Clone, Serialize)]
struct Ready {
    /// Always `"ready"` when every dependency is healthy.
    status: &'static str,
}

/// The body of a failed readiness response, naming the unhealthy dependency.
#[derive(Debug, Clone, Serialize)]
struct NotReady {
    /// Always `"unavailable"`.
    status: &'static str,
    /// Which dependency failed: `"database"` or `"cluster"`.
    failed: &'static str,
}

/// Builds the readiness sub-router: `GET /ready`.
///
/// Mounted by the composition root alongside the liveness `GET /health`. It
/// carries [`AppState`] so the handler can reach the storage and cluster
/// handles it probes.
pub fn ready_router() -> Router<AppState> {
    Router::new().route("/ready", get(ready))
}

/// The `GET /ready` handler.
///
/// Runs the dependency checks in order (database, then cluster) and returns the
/// first failure as `503`, or `200 {"status":"ready"}` when all pass. Sampling
/// the live-games gauge here keeps the exported metric fresh on every readiness
/// scrape at no extra cost.
async fn ready(State(state): State<AppState>) -> Response {
    // Refresh the live-games gauge while we are touching state anyway, so the
    // exported value tracks reality on every readiness poll.
    let _live = sample_live_games(&state);

    // 1. Database: a single bounded read proves the pool can round-trip.
    if let Err(error) = state.game_repo().list_recent(1).await {
        tracing::warn!(%error, "readiness check failed: database unreachable");
        return not_ready("database");
    }

    // 2. Cluster membership. Single-node this is the in-memory LocalRegistry and
    //    always succeeds; with cluster mode on it is the Redis-backed registry,
    //    so this is the Redis reachability check.
    if let Err(error) = state.cluster().registry().live_nodes().await {
        tracing::warn!(%error, "readiness check failed: cluster membership unreachable");
        return not_ready("cluster");
    }

    (StatusCode::OK, Json(Ready { status: "ready" })).into_response()
}

/// Builds the `503 Service Unavailable` body naming the failed `dependency`.
fn not_ready(dependency: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(NotReady {
            status: "unavailable",
            failed: dependency,
        }),
    )
        .into_response()
}
