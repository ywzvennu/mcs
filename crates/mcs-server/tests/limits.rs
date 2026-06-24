//! Integration tests for abuse-protection limits (#100) through the assembled
//! server router.
//!
//! Builds the full `build_state` + `router_with_cors` stack against an in-memory
//! SQLite backend (no socket bound) and drives it with
//! [`tower::ServiceExt::oneshot`], asserting that the `[limits]` config is wired
//! end-to-end: hammering a rate-limited route past the configured per-IP rate
//! returns **429 Too Many Requests** with a `Retry-After` header, while requests
//! within the limit are unaffected. The test trusts an `X-Forwarded-For` header
//! so each `oneshot` (which attaches no peer address) can present a client IP —
//! the same path a deployment behind a reverse proxy uses.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mcs_server::config::{Config, LimitsSettings};
use mcs_storage::SqlxStorage;
use tower::ServiceExt as _;

const TEST_ADDRESS: &str = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23";

/// Builds the assembled router with the given `[limits]` settings against an
/// in-memory SQLite database, via the same `build_state` path `build_app` uses.
async fn app_with_limits(limits: LimitsSettings) -> axum::Router {
    let cfg = Config {
        database_url: "sqlite::memory:".to_owned(),
        limits,
        ..Config::default()
    };
    let storage = Arc::new(
        SqlxStorage::connect(&cfg.database_url)
            .await
            .expect("connect in-memory sqlite"),
    );
    let state = mcs_server::build_state(&cfg, storage, b"test-secret-bytes-not-for-prod".to_vec())
        .expect("build state");
    mcs_server::router_with_cors(state, None, Some(&cfg.http))
}

fn nonce_request(ip: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .header("x-forwarded-for", ip)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn config_driven_rate_limit_throttles_past_the_limit() {
    let limits = LimitsSettings {
        nonce_per_minute: 2,
        trusted_proxy_header: Some("x-forwarded-for".to_owned()),
        ..LimitsSettings::default()
    };
    let app = app_with_limits(limits).await;
    let ip = "203.0.113.42";

    // The first two requests (the configured burst) succeed.
    for i in 0..2 {
        let resp = app.clone().oneshot(nonce_request(ip)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "request {i} within limit");
    }

    // The third (N+1) is throttled with a Retry-After header.
    let resp = app.clone().oneshot(nonce_request(ip)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        resp.headers().contains_key("retry-after"),
        "a 429 must carry a Retry-After header"
    );
}

#[tokio::test]
async fn default_limits_do_not_throttle_a_handful_of_requests() {
    // The built-in default nonce rate (10/min) leaves room for a few requests.
    let limits = LimitsSettings {
        trusted_proxy_header: Some("x-forwarded-for".to_owned()),
        ..LimitsSettings::default()
    };
    let app = app_with_limits(limits).await;
    for i in 0..5 {
        let resp = app
            .clone()
            .oneshot(nonce_request("198.51.100.7"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "request {i} within default");
    }
}
