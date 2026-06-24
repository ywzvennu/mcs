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
use mcs_variant_standard::{register as register_standard, STANDARD_VARIANT_ID};

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

/// A registry with the standard variant should list it with its id and display name.
#[tokio::test]
async fn get_variants_lists_standard() {
    let mut registry = VariantRegistry::new();
    register_standard(&mut registry);
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
    assert_eq!(variants.len(), 1);
    assert_eq!(variants[0]["id"], STANDARD_VARIANT_ID);
    assert!(variants[0]["display_name"].as_str().is_some());
}

/// With two variants registered the response contains both, sorted by id.
#[tokio::test]
async fn get_variants_lists_multiple_sorted() {
    let mut registry = VariantRegistry::new();
    register_standard(&mut registry);
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
    assert_eq!(variants.len(), 2);

    // Response must be sorted by id.
    let ids: Vec<&str> = variants
        .iter()
        .map(|v| v["id"].as_str().expect("id is string"))
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "variants should be sorted by id");

    // Both ids must be present.
    assert!(ids.contains(&"standard"));
    assert!(ids.contains(&"rbc"));
}

/// Each variant entry must contain both `id` and `display_name` fields.
#[tokio::test]
async fn get_variants_response_shape() {
    let mut registry = VariantRegistry::new();
    register_standard(&mut registry);
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
    // No extra fields that could break contract.
    let obj = variant.as_object().expect("entry is object");
    assert_eq!(obj.len(), 2, "entry has exactly id + display_name");
}
