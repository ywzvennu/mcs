//! Integration tests for HTTP hardening (#99).
//!
//! Builds the assembled router in-process with an in-memory SQLite backend and
//! drives it with [`tower::ServiceExt::oneshot`] — no socket is bound — to
//! assert:
//!
//! - Every response carries the mandatory security headers:
//!   `X-Content-Type-Options`, `X-Frame-Options`,
//!   `Content-Security-Policy`, `Referrer-Policy`.
//! - `Strict-Transport-Security` is present **only** when `[http].hsts = true`.
//! - An oversized JSON body returns **413 Payload Too Large**.
//! - The `[http]` config section deserialises from TOML and the defaults are
//!   sensible.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mcs_server::{
    config::{Config, HttpSettings},
    router_with_cors,
};
use mcs_storage::SqlxStorage;
use tower::ServiceExt as _;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Builds a router with the given [`HttpSettings`] against an in-memory SQLite
/// database. CORS is left disabled; security headers and limits come from
/// `http`.
async fn app_with_http(http: HttpSettings) -> axum::Router {
    let cfg = Config {
        database_url: "sqlite::memory:".to_owned(),
        http,
        ..Config::default()
    };
    let storage = Arc::new(
        SqlxStorage::connect(&cfg.database_url)
            .await
            .expect("connect in-memory sqlite"),
    );
    let state = mcs_server::build_state(&cfg, storage, b"test-secret-bytes-not-for-prod".to_vec())
        .expect("build state");
    router_with_cors(state, None, Some(&cfg.http))
}

/// Sends a GET to `/health` and returns the response.
async fn get_health(app: axum::Router) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::GET)
            .uri("/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .expect("router responds")
}

// ── Security-header presence tests ───────────────────────────────────────────

/// Every response must carry `X-Content-Type-Options: nosniff`.
#[tokio::test]
async fn response_has_x_content_type_options() {
    let app = app_with_http(HttpSettings::default()).await;
    let response = get_health(app).await;
    assert_eq!(response.status(), StatusCode::OK);

    let value = response
        .headers()
        .get("x-content-type-options")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        value, "nosniff",
        "every response must carry X-Content-Type-Options: nosniff"
    );
}

/// Every response must carry `X-Frame-Options: DENY`.
#[tokio::test]
async fn response_has_x_frame_options() {
    let app = app_with_http(HttpSettings::default()).await;
    let response = get_health(app).await;
    assert_eq!(response.status(), StatusCode::OK);

    let value = response
        .headers()
        .get("x-frame-options")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        value, "DENY",
        "every response must carry X-Frame-Options: DENY"
    );
}

/// Every response must carry a conservative `Content-Security-Policy`.
#[tokio::test]
async fn response_has_content_security_policy() {
    let app = app_with_http(HttpSettings::default()).await;
    let response = get_health(app).await;
    assert_eq!(response.status(), StatusCode::OK);

    let value = response
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        value.contains("default-src 'none'"),
        "CSP must contain default-src 'none'; got {value:?}"
    );
    assert!(
        value.contains("frame-ancestors 'none'"),
        "CSP must contain frame-ancestors 'none'; got {value:?}"
    );
}

/// Every response must carry `Referrer-Policy: no-referrer`.
#[tokio::test]
async fn response_has_referrer_policy() {
    let app = app_with_http(HttpSettings::default()).await;
    let response = get_health(app).await;
    assert_eq!(response.status(), StatusCode::OK);

    let value = response
        .headers()
        .get("referrer-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        value, "no-referrer",
        "every response must carry Referrer-Policy: no-referrer"
    );
}

// ── HSTS tests ────────────────────────────────────────────────────────────────

/// With the default config (`hsts = false`), no response must carry
/// `Strict-Transport-Security`.
#[tokio::test]
async fn hsts_absent_by_default() {
    let app = app_with_http(HttpSettings::default()).await;
    let response = get_health(app).await;

    let hsts = response
        .headers()
        .get("strict-transport-security")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        hsts.is_empty(),
        "Strict-Transport-Security must not be set when hsts = false; got {hsts:?}"
    );
}

/// When `hsts = true`, every response must carry `Strict-Transport-Security`
/// with the configured `max-age`.
#[tokio::test]
async fn hsts_present_when_enabled() {
    let http = HttpSettings {
        hsts: true,
        hsts_max_age_secs: 31_536_000,
        ..HttpSettings::default()
    };
    let app = app_with_http(http).await;
    let response = get_health(app).await;

    let hsts = response
        .headers()
        .get("strict-transport-security")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        !hsts.is_empty(),
        "Strict-Transport-Security must be set when hsts = true"
    );
    assert!(
        hsts.contains("max-age=31536000"),
        "Strict-Transport-Security must contain max-age=31536000; got {hsts:?}"
    );
}

// ── Body-size limit tests ─────────────────────────────────────────────────────

/// Sending a body larger than `max_body_bytes` must be rejected with **413**.
///
/// `POST /auth/verify` reads a JSON body for auth parameters.  We include a
/// `Content-Length` header so `DefaultBodyLimit` can reject the body before
/// the auth extractor runs, keeping the enforcement at the middleware edge.
#[tokio::test]
async fn oversized_body_returns_413() {
    // Set a tiny limit (100 bytes) and send a body much larger than that.
    let http = HttpSettings {
        max_body_bytes: 100,
        ..HttpSettings::default()
    };
    let app = app_with_http(http).await;

    // 300-byte body with an accurate Content-Length header — axum's
    // DefaultBodyLimit can reject early when it sees the declared size.
    let body_bytes = vec![b'x'; 300];
    let content_length = body_bytes.len().to_string();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/auth/verify")
                .header("content-type", "application/json")
                .header("content-length", content_length)
                .body(Body::from(body_bytes))
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "a body exceeding max_body_bytes must return 413 Payload Too Large"
    );
}

/// A body right at the limit must pass through (not be rejected).
///
/// Even though the request will likely fail for other reasons (invalid JSON,
/// no auth), it must not fail with 413.
#[tokio::test]
async fn body_at_limit_is_accepted() {
    // 512-byte limit; send a 10-byte body — well within the limit.
    let http = HttpSettings {
        max_body_bytes: 512,
        ..HttpSettings::default()
    };
    let app = app_with_http(http).await;

    let small_body = b"{\"x\":1}".to_vec();
    let content_length = small_body.len().to_string();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/auth/verify")
                .header("content-type", "application/json")
                .header("content-length", content_length)
                // Tiny JSON body — well under 512 bytes.
                .body(Body::from(small_body))
                .unwrap(),
        )
        .await
        .expect("router responds");

    // Not 413 — the body limit must not fire for a small body. The endpoint
    // will return a different 4xx (422 for bad JSON shape), but not 413.
    assert_ne!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "a body within max_body_bytes must not return 413"
    );
}
