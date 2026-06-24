//! Smoke test for the assembled server.
//!
//! Builds the full application against an in-memory SQLite backend and drives
//! the router in-process with [`tower::ServiceExt::oneshot`] — no socket is
//! bound — to assert the wiring is sound: `GET /health` returns `200 OK`, the
//! API's `GET /auth/nonce` route is actually mounted (proving the `mcs-api`
//! router was merged), and `GET /variants` lists the expected variant ids
//! (proving standard, RBC, and the shakmaty family are all registered).

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mcs_server::{config::Config, router};
use mcs_storage::SqlxStorage;
use serde_json::Value;
use tower::ServiceExt as _; // for `oneshot`

/// Builds the application router against a fresh in-memory database.
async fn test_app() -> axum::Router {
    let cfg = Config {
        database_url: "sqlite::memory:".to_owned(),
        ..Config::default()
    };
    let storage = Arc::new(
        SqlxStorage::connect(&cfg.database_url)
            .await
            .expect("connect in-memory sqlite"),
    );
    let state = mcs_server::build_state(&cfg, storage, b"test-secret-bytes-not-for-prod".to_vec())
        .expect("build state");
    router(state)
}

#[tokio::test]
async fn health_returns_ok() {
    let app = test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::OK);

    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(&bytes[..], br#"{"status":"ok"}"#);
}

#[tokio::test]
async fn auth_nonce_route_is_mounted() {
    let app = test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/nonce")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    // The exact status depends on the handler, but a mounted route must NOT be
    // 404. A missing route would prove the `mcs-api` router was not merged.
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn variants_endpoint_lists_all_registered_variants() {
    let app = test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/variants")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::OK);

    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: Value = serde_json::from_slice(&bytes).expect("valid JSON");
    let variants = json["variants"].as_array().expect("variants is array");

    let ids: Vec<&str> = variants
        .iter()
        .map(|v| v["id"].as_str().expect("id is string"))
        .collect();

    // Verify standard, RBC, and a representative sample of the shakmaty family
    // are present — proving all three register calls fired at startup.
    for expected in &["standard", "rbc", "atomic", "chess960", "antichess"] {
        assert!(
            ids.contains(expected),
            "expected variant '{expected}' to be registered; got: {ids:?}"
        );
    }

    // The full set: standard + rbc + 8 shakmaty variants = 10 total.
    assert_eq!(
        ids.len(),
        10,
        "expected 10 registered variants; got {}: {ids:?}",
        ids.len()
    );
}
