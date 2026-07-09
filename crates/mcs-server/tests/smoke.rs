//! Smoke test for the assembled server.
//!
//! Builds the full application against an in-memory SQLite backend and drives
//! the router in-process with [`tower::ServiceExt::oneshot`] — no socket is
//! bound — to assert the wiring is sound: `GET /health` returns `200 OK`, the
//! API's `GET /auth/nonce` route is actually mounted (proving the `mcs-api`
//! router was merged), and `GET /variants` lists the expected variant ids
//! (proving standard, Chess960, RBC, and the mcr fairy catalog are all
//! registered).

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

    // Verify the core adapters are present — proving both register calls fired at
    // startup (`mcs_variant_rbc::register` adds rbc, and `mcs_variant_mcr::register`
    // adds mcr's whole catalog, which since #155 includes standard + chess960). A
    // representative sample of the mcr catalog is checked too.
    for expected in &[
        "standard",
        "chess960",
        "rbc",
        "kingofthehill",
        "shogi",
        "xiangqi",
    ] {
        assert!(
            ids.contains(expected),
            "expected variant '{expected}' to be registered; got: {ids:?}"
        );
    }

    // The full set: rbc (1) plus mcr's whole catalog (119 — nothing deferred;
    // since #156 Fog of War is redacted-and-registered and the phased Duck /
    // Placement / Sittuyin are handled, since #163 jieqi is redacted-and-registered
    // too, and standard and chess960 are mcr-owned) = 120.
    assert_eq!(
        ids.len(),
        120,
        "expected 120 registered variants; got {}: {ids:?}",
        ids.len()
    );

    // No id appears twice across the rbc and mcr adapters.
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        ids.len(),
        "variant ids must be unique: {ids:?}"
    );
}
