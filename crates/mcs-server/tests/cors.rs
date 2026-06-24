//! Integration tests for CORS headers (#98).
//!
//! Builds the assembled router in-process with an in-memory SQLite backend and
//! drives it with [`tower::ServiceExt::oneshot`] — no socket is bound — to
//! assert:
//!
//! - A preflight `OPTIONS` request from an **allowed** origin receives the
//!   correct `Access-Control-Allow-Origin`, `Access-Control-Allow-Methods`, and
//!   `Access-Control-Allow-Headers` headers.
//! - A simple `GET` from an **allowed** origin carries `Access-Control-Allow-Origin`.
//! - A request from a **non-allowed** origin does NOT receive an
//!   `Access-Control-Allow-Origin` header.
//! - The `[cors]` config section deserialises correctly from TOML, and the
//!   defaults are sensible (empty origins, no credentials, 3600s max-age,
//!   no wildcard).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mcs_server::{
    config::{Config, CorsSettings},
    router_with_cors,
};
use mcs_storage::SqlxStorage;
use tower::ServiceExt as _;

const ALLOWED_ORIGIN: &str = "https://mcf.example.com";
const OTHER_ORIGIN: &str = "https://evil.example.com";

/// Builds a router with the given [`CorsSettings`] against an in-memory SQLite
/// database. Uses the public [`router_with_cors`] entry point so tests can
/// inject a custom CORS layer without going through `build_app`.
async fn app_with_cors(cors: CorsSettings) -> axum::Router {
    let cfg = Config {
        database_url: "sqlite::memory:".to_owned(),
        cors,
        ..Config::default()
    };
    let storage = Arc::new(
        SqlxStorage::connect(&cfg.database_url)
            .await
            .expect("connect in-memory sqlite"),
    );
    let state = mcs_server::build_state(&cfg, storage, b"test-secret-bytes-not-for-prod".to_vec())
        .expect("build state");
    let cors_layer = cfg.cors.build_cors_layer();
    router_with_cors(state, Some(cors_layer))
}

// ── Allowed-origin tests ─────────────────────────────────────────────────────

/// A preflight OPTIONS to an allowed origin must receive the full set of CORS
/// allow headers back.
#[tokio::test]
async fn preflight_from_allowed_origin_gets_cors_headers() {
    let cors = CorsSettings {
        allowed_origins: vec![ALLOWED_ORIGIN.to_owned()],
        ..CorsSettings::default()
    };
    let app = app_with_cors(cors).await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/health")
                .header("Origin", ALLOWED_ORIGIN)
                .header("Access-Control-Request-Method", "GET")
                .header("Access-Control-Request-Headers", "content-type")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    // Preflight must succeed (200 or 204).
    assert!(
        response.status() == StatusCode::OK || response.status() == StatusCode::NO_CONTENT,
        "preflight status should be 200 or 204, got {}",
        response.status()
    );

    let headers = response.headers();

    let allow_origin = headers
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        allow_origin, ALLOWED_ORIGIN,
        "allow-origin should echo the allowed origin"
    );

    let allow_methods = headers
        .get("access-control-allow-methods")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_uppercase();
    for method in ["GET", "POST", "DELETE", "OPTIONS"] {
        assert!(
            allow_methods.contains(method),
            "allow-methods should include {method}; got {allow_methods:?}"
        );
    }

    let allow_headers = headers
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        allow_headers.contains("authorization") || allow_headers.contains("*"),
        "allow-headers should include authorization; got {allow_headers:?}"
    );
    assert!(
        allow_headers.contains("content-type") || allow_headers.contains("*"),
        "allow-headers should include content-type; got {allow_headers:?}"
    );
}

/// A simple GET from an allowed origin must carry `Access-Control-Allow-Origin`.
#[tokio::test]
async fn get_from_allowed_origin_gets_allow_origin_header() {
    let cors = CorsSettings {
        allowed_origins: vec![ALLOWED_ORIGIN.to_owned()],
        ..CorsSettings::default()
    };
    let app = app_with_cors(cors).await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
                .header("Origin", ALLOWED_ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::OK);

    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        allow_origin, ALLOWED_ORIGIN,
        "simple GET from allowed origin must get Access-Control-Allow-Origin"
    );
}

// ── Non-allowed-origin test ───────────────────────────────────────────────────

/// A request from a non-listed origin must NOT receive an
/// `Access-Control-Allow-Origin` header — the browser will then block it.
#[tokio::test]
async fn request_from_non_allowed_origin_gets_no_allow_origin_header() {
    let cors = CorsSettings {
        allowed_origins: vec![ALLOWED_ORIGIN.to_owned()],
        ..CorsSettings::default()
    };
    let app = app_with_cors(cors).await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
                .header("Origin", OTHER_ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        allow_origin.is_empty(),
        "non-allowed origin must not get Access-Control-Allow-Origin; got {allow_origin:?}"
    );
}

// ── Default / no-CORS test ───────────────────────────────────────────────────

/// With the default (empty) config, no request — even a same-origin one —
/// should receive an `Access-Control-Allow-Origin` header.
#[tokio::test]
async fn default_config_sends_no_allow_origin_header() {
    let app = app_with_cors(CorsSettings::default()).await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
                .header("Origin", ALLOWED_ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        allow_origin.is_empty(),
        "default config (no allowed_origins) must not set Access-Control-Allow-Origin"
    );
}

// ── `allow_any_origin` test ──────────────────────────────────────────────────

/// With `allow_any_origin = true`, every origin receives `*`.
#[tokio::test]
async fn allow_any_origin_sends_wildcard() {
    let cors = CorsSettings {
        allow_any_origin: true,
        ..CorsSettings::default()
    };
    let app = app_with_cors(cors).await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
                .header("Origin", OTHER_ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        allow_origin, "*",
        "allow_any_origin = true must send Access-Control-Allow-Origin: *"
    );
}

// ── Config-parsing tests ─────────────────────────────────────────────────────

/// The default [`CorsSettings`] has sensible values:
/// no origins, no credentials, 3600s max-age, no wildcard.
#[test]
fn cors_defaults_are_sensible() {
    let cors = CorsSettings::default();
    assert!(
        cors.allowed_origins.is_empty(),
        "default allowed_origins must be empty (no cross-origin allowed)"
    );
    assert!(
        !cors.allow_credentials,
        "default allow_credentials must be false"
    );
    assert_eq!(cors.max_age_secs, 3600, "default max_age_secs must be 3600");
    assert!(
        !cors.allow_any_origin,
        "default allow_any_origin must be false"
    );
}

/// The `[cors]` section in `config.toml` is parsed correctly, and unset keys
/// take their defaults.
#[allow(clippy::result_large_err)]
#[test]
fn cors_section_parses_from_toml() {
    figment::Jail::expect_with(|jail| {
        jail.create_file(
            "config.toml",
            r#"
                [cors]
                allowed_origins = ["https://mcf.example.com", "https://staging.mcf.example.com"]
                allow_credentials = true
                max_age_secs = 7200
            "#,
        )?;
        let cfg = Config::load().expect("load config with [cors]");
        assert_eq!(
            cfg.cors.allowed_origins,
            ["https://mcf.example.com", "https://staging.mcf.example.com"]
        );
        assert!(cfg.cors.allow_credentials);
        assert_eq!(cfg.cors.max_age_secs, 7200);
        // Unset key keeps its default.
        assert!(!cfg.cors.allow_any_origin);
        // An unrelated section is unaffected.
        assert!(!cfg.payments.enabled);
        Ok(())
    });
}

/// Setting only `allow_any_origin = true` leaves the other keys at their
/// defaults.
#[allow(clippy::result_large_err)]
#[test]
fn cors_allow_any_origin_parses_from_toml() {
    figment::Jail::expect_with(|jail| {
        jail.create_file(
            "config.toml",
            r#"
                [cors]
                allow_any_origin = true
            "#,
        )?;
        let cfg = Config::load().expect("load config with allow_any_origin");
        assert!(cfg.cors.allow_any_origin);
        assert!(cfg.cors.allowed_origins.is_empty());
        assert!(!cfg.cors.allow_credentials);
        Ok(())
    });
}
