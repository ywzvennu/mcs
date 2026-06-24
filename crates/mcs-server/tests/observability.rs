//! Integration tests for the operational endpoints (#88).
//!
//! Builds the full application against an in-memory SQLite backend and drives
//! the router in-process with [`tower::ServiceExt::oneshot`] — no socket is
//! bound — to assert the observability wiring:
//!
//! - `GET /metrics` renders Prometheus text that includes the HTTP request
//!   metric (after driving a request so the counter is non-zero) and the
//!   live-games gauge.
//! - `GET /ready` returns `200 {"status":"ready"}` when the database is
//!   reachable, and `503` naming the failed dependency once the database is
//!   broken out from under it.
//! - `GET /health` stays trivially `200 {"status":"ok"}`.
//!
//! The Prometheus recorder is a process-global singleton installed once by the
//! server's `metrics::handle`; building the router twice in this binary is safe
//! because that installation is guarded by a `OnceLock`.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mcs_server::{config::Config, router};
use mcs_storage::SqlxStorage;
use tower::ServiceExt as _; // for `oneshot`

/// Connects a fresh in-memory database and returns the storage handle.
async fn fresh_storage() -> Arc<SqlxStorage> {
    Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite"),
    )
}

/// Builds the application router over `storage`.
fn app_with(storage: Arc<SqlxStorage>) -> axum::Router {
    let cfg = Config {
        database_url: "sqlite::memory:".to_owned(),
        ..Config::default()
    };
    let state = mcs_server::build_state(&cfg, storage, b"test-secret-bytes-not-for-prod".to_vec())
        .expect("build state");
    router(state)
}

/// Reads a response body into a UTF-8 string.
async fn body_string(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

/// Issues `GET uri` against a one-shot clone of `app`, returning the response.
async fn get(app: &axum::Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router responds")
}

#[tokio::test]
async fn metrics_endpoint_renders_prometheus_text() {
    let app = app_with(fresh_storage().await);

    // Drive a couple of requests first so the HTTP counter is non-zero and the
    // exposition is not empty.
    assert_eq!(get(&app, "/health").await.status(), StatusCode::OK);
    assert_eq!(get(&app, "/variants").await.status(), StatusCode::OK);
    // Hitting /ready samples the live-games gauge, so it is exported too.
    let ready = get(&app, "/ready").await;
    assert_eq!(ready.status(), StatusCode::OK);

    let response = get(&app, "/metrics").await;
    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/plain"),
        "metrics should be served as text/plain; got {content_type:?}"
    );

    let body = body_string(response).await;

    // The HTTP request metric must appear with a non-zero count, proving the
    // middleware recorded the requests we drove above into the live recorder.
    assert!(
        body.contains("mcs_http_requests_total"),
        "metrics should include the HTTP request counter; got:\n{body}"
    );
    // The live-games gauge must be exported (it is 0 here — no games — but the
    // series is present because `/ready` sampled it).
    assert!(
        body.contains("mcs_games_live"),
        "metrics should include the live-games gauge; got:\n{body}"
    );
}

#[tokio::test]
async fn health_is_trivially_ok() {
    let app = app_with(fresh_storage().await);

    let response = get(&app, "/health").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, r#"{"status":"ok"}"#);
}

#[tokio::test]
async fn ready_is_ok_when_the_database_is_reachable() {
    let app = app_with(fresh_storage().await);

    let response = get(&app, "/ready").await;
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_string(response).await;
    assert!(
        body.contains(r#""status":"ready""#),
        "ready body should report ready; got: {body}"
    );
}

#[tokio::test]
async fn ready_is_503_when_the_database_is_broken() {
    // Connect normally, then break the DB out from under the server by dropping
    // the `games` table on the live pool. The readiness probe's `list_recent(1)`
    // read then fails, which is exactly the unhealthy-dependency case.
    let storage = fresh_storage().await;
    sqlx::query("DROP TABLE games")
        .execute(storage.pool())
        .await
        .expect("drop games table");

    let app = app_with(storage);

    let response = get(&app, "/ready").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let body = body_string(response).await;
    assert!(
        body.contains(r#""failed":"database""#),
        "503 body should name the failed dependency; got: {body}"
    );

    // Liveness is independent of the database and must still be 200.
    let health = get(&app, "/health").await;
    assert_eq!(health.status(), StatusCode::OK);
}
