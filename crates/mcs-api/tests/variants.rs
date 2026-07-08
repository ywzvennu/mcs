//! Integration tests for the `GET /variants` discovery endpoint.
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket is bound) and assert that the
//! endpoint lists exactly the variants that were registered in the
//! [`VariantRegistry`] passed to [`AppState`].

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::Duration;
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::VariantRegistry;
use mcs_storage::SqlxStorage;
use mcs_variant_mcr::{register as register_mcr, CHESS960_VARIANT_ID, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Builds an [`AppState`] backed by a fresh in-memory SQLite database with the
/// given variant registry.
async fn test_app_with_registry(registry: VariantRegistry) -> axum::Router {
    let storage = Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect + migrate in-memory sqlite"),
    );

    let session = SessionConfig::new(
        b"test-secret-key-that-is-definitely-32-bytes!!".to_vec(),
        Duration::hours(1),
        "mcs-test".to_owned(),
    );
    let siwe = SiweConfig::new(
        "localhost".to_owned(),
        "https://localhost".to_owned(),
        1,
        "Sign in to MCS.".to_owned(),
        Duration::minutes(10),
    );

    let state = AppState::new(storage, Arc::new(registry), session, siwe);
    router(state)
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).expect("response is valid JSON")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An empty registry should return an empty `variants` list.
#[tokio::test]
async fn get_variants_empty_registry() {
    let app = test_app_with_registry(VariantRegistry::new()).await;

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
    let json = body_json(response.into_body()).await;
    assert_eq!(json["variants"], serde_json::json!([]));
}

/// Registering the mcr catalog adds `standard` and `chess960` (among the rest of
/// the fairy variants), each with an id and display name.
#[tokio::test]
async fn get_variants_lists_standard() {
    let mut registry = VariantRegistry::new();
    register_mcr(&mut registry);
    let app = test_app_with_registry(registry).await;

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
    let json = body_json(response.into_body()).await;
    let variants = json["variants"].as_array().expect("variants is array");
    // mcr registers its whole catalog (112 variants since #155).
    assert_eq!(variants.len(), 112);

    let ids: Vec<&str> = variants
        .iter()
        .map(|v| v["id"].as_str().expect("id is string"))
        .collect();
    assert!(ids.contains(&STANDARD_VARIANT_ID));
    assert!(ids.contains(&CHESS960_VARIANT_ID));
    assert!(variants
        .iter()
        .all(|v| v["display_name"].as_str().is_some()));
}

/// With the mcr catalog and RBC registered the response contains standard,
/// chess960, and rbc, sorted by id.
#[tokio::test]
async fn get_variants_lists_multiple_sorted() {
    let mut registry = VariantRegistry::new();
    register_mcr(&mut registry);
    mcs_variant_rbc::register(&mut registry);
    let app = test_app_with_registry(registry).await;

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
    let json = body_json(response.into_body()).await;
    let variants = json["variants"].as_array().expect("variants is array");
    // mcr's catalog (112) plus rbc.
    assert_eq!(variants.len(), 113);

    // Response must be sorted by id.
    let ids: Vec<&str> = variants
        .iter()
        .map(|v| v["id"].as_str().expect("id is string"))
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "variants should be sorted by id");

    // All ids must be present.
    assert!(ids.contains(&"standard"));
    assert!(ids.contains(&"chess960"));
    assert!(ids.contains(&"rbc"));
}

/// Each variant entry must contain both `id` and `display_name` fields.
#[tokio::test]
async fn get_variants_response_shape() {
    let mut registry = VariantRegistry::new();
    register_mcr(&mut registry);
    let app = test_app_with_registry(registry).await;

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
    let json = body_json(response.into_body()).await;
    let variant = &json["variants"][0];

    assert!(variant.get("id").is_some(), "entry must have 'id'");
    assert!(
        variant.get("display_name").is_some(),
        "entry must have 'display_name'"
    );
    // The enriched render-oriented fields (#157) must be present.
    assert!(
        variant.get("board_width").is_some(),
        "entry must have 'board_width'"
    );
    assert!(
        variant.get("board_height").is_some(),
        "entry must have 'board_height'"
    );
    assert!(
        variant.get("has_hand").is_some(),
        "entry must have 'has_hand'"
    );
    assert!(
        variant.get("start_fen").is_some(),
        "entry must have 'start_fen'"
    );
}

/// A single variant's enriched entry, found by id, or `None`.
fn find_variant(json: &Value, id: &str) -> Option<Value> {
    json["variants"]
        .as_array()
        .expect("variants is array")
        .iter()
        .find(|v| v["id"].as_str() == Some(id))
        .cloned()
}

/// The enriched metadata (#157) must reflect each variant's real board geometry
/// and hand capability: `standard` is 8x8 with no hand, while the large-board
/// shogi/xiangqi variants report their true (non-8) dimensions, and `shogi` has a
/// hand.
#[tokio::test]
async fn get_variants_reports_board_geometry_and_hand() {
    let mut registry = VariantRegistry::new();
    register_mcr(&mut registry);
    let app = test_app_with_registry(registry).await;

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
    let json = body_json(response.into_body()).await;

    // Standard chess: an ordinary 8x8 board with no hand and a start FEN.
    let standard = find_variant(&json, STANDARD_VARIANT_ID).expect("standard is listed");
    assert_eq!(standard["board_width"], 8);
    assert_eq!(standard["board_height"], 8);
    assert_eq!(standard["has_hand"], false);
    assert!(
        standard["start_fen"].as_str().is_some(),
        "standard must carry a start FEN"
    );

    // Shogi: a 9x9 board with a hand (drops).
    let shogi = find_variant(&json, "shogi").expect("shogi is listed");
    assert_eq!(shogi["board_width"], 9);
    assert_eq!(shogi["board_height"], 9);
    assert_eq!(shogi["has_hand"], true);

    // Xiangqi: a 9-file, 10-rank board with no hand.
    let xiangqi = find_variant(&json, "xiangqi").expect("xiangqi is listed");
    assert_eq!(xiangqi["board_width"], 9);
    assert_eq!(xiangqi["board_height"], 10);
    assert_eq!(xiangqi["has_hand"], false);
}
