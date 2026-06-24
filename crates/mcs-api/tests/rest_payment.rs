//! Integration tests for the x402 payment gate on game creation (#45).
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket), backed by an in-memory SQLite
//! database with the standard variant registered. They cover BOTH modes:
//!
//! - **disabled** (the default): `POST /seeks` is free and behaves exactly as
//!   the `rest_game` flow does — an authenticated post queues the seek.
//! - **enabled** (via [`AppState::with_payment`]): an authenticated `POST /seeks`
//!   with no `X-PAYMENT` header is answered `402` with the requirements in
//!   `accepts`; the same request carrying a valid mock `X-PAYMENT` succeeds and
//!   queues the seek. Read endpoints stay free in both modes.
//!
//! # Ordering
//!
//! The payment layer wraps the creation route, so it runs *before* the
//! handler's auth extractor: an unpaid request gets `402` regardless of auth,
//! and a paid request still needs a valid session. The tests assert both edges.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::VariantRegistry;
use mcs_domain::User;
use mcs_payments::{MockVerifier, PaymentPayload, PaymentRequirements, X_PAYMENT};
use mcs_storage::SqlxStorage;
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

/// Builds a payment-free [`AppState`] backed by in-memory SQLite with the
/// standard variant registered.
async fn base_state() -> AppState {
    let storage = Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect + migrate in-memory sqlite"),
    );

    let mut registry = VariantRegistry::new();
    register(&mut registry);

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
    AppState::new(storage, Arc::new(registry), session, siwe)
}

/// The payment terms the gated state advertises.
fn test_requirements() -> PaymentRequirements {
    PaymentRequirements {
        scheme: "exact".into(),
        network: "base-sepolia".into(),
        max_amount_required: "10000".into(),
        resource: "/seeks".into(),
        description: "Create an MCS game.".into(),
        mime_type: "application/json".into(),
        pay_to: "0xRecipient".into(),
        max_timeout_seconds: 300,
        asset: "0xUSDC".into(),
        extra: None,
    }
}

/// Persists a fresh user with the given address and returns it.
async fn create_user(state: &AppState, address: &str) -> User {
    let user = User::new(
        address.parse().expect("valid evm address"),
        None,
        OffsetDateTime::now_utc(),
    );
    state
        .storage()
        .users()
        .create(&user)
        .await
        .expect("create user");
    user
}

/// Mints a session token for `user`, exactly as `/auth/verify` would.
fn token_for(state: &AppState, user: &User) -> String {
    issue_session(state.session_config(), user.id)
        .expect("mint token")
        .token
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn seek_body() -> Value {
    json!({
        "variant_id": STANDARD_VARIANT_ID,
        "time_control": { "type": "real_time", "initial_secs": 300, "increment_secs": 2 },
        "color_preference": "white",
    })
}

/// A valid mock `X-PAYMENT` header value matching [`test_requirements`].
fn valid_payment_header() -> String {
    PaymentPayload {
        x402_version: 1,
        scheme: "exact".into(),
        network: "base-sepolia".into(),
        payload: json!({ "from": "0xPayer", "authorization": "0xdeadbeef" }),
    }
    .to_header()
    .unwrap()
}

// ---------------------------------------------------------------------------
// Disabled (default): identical to the existing rest_game flow.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn payments_disabled_post_seek_queues_as_before() {
    let state = base_state().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let token = token_for(&state, &alice);
    let router = router(state.clone());

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/seeks")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(seek_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["status"], "queued", "got {body}");
    assert_eq!(state.matchmaker().open_seeks().await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Enabled: 402 without payment, success with a valid mock payment.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn payments_enabled_post_seek_without_payment_is_402_with_accepts() {
    let state = base_state()
        .await
        .with_payment(test_requirements(), Arc::new(MockVerifier));
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let token = token_for(&state, &alice);
    let router = router(state.clone());

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/seeks")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(seek_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["x402Version"], 1);
    let accepts = body["accepts"].as_array().expect("accepts array");
    assert_eq!(accepts.len(), 1);
    assert_eq!(accepts[0]["scheme"], "exact");
    assert_eq!(accepts[0]["resource"], "/seeks");

    // The seek was never created: the gate stops the request before the handler.
    assert!(state.matchmaker().open_seeks().await.unwrap().is_empty());
}

#[tokio::test]
async fn payments_enabled_post_seek_with_valid_payment_queues() {
    let state = base_state()
        .await
        .with_payment(test_requirements(), Arc::new(MockVerifier));
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let token = token_for(&state, &alice);
    let router = router(state.clone());

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/seeks")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .header(X_PAYMENT, valid_payment_header())
                .body(Body::from(seek_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["status"], "queued", "got {body}");
    assert_eq!(state.matchmaker().open_seeks().await.unwrap().len(), 1);
}

#[tokio::test]
async fn payments_enabled_unpaid_request_is_402_before_auth() {
    // The payment layer wraps the route, so an unauthenticated, unpaid request
    // is answered `402` (payment is checked before the handler's auth extractor).
    let state = base_state()
        .await
        .with_payment(test_requirements(), Arc::new(MockVerifier));
    let router = router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/seeks")
                .header("content-type", "application/json")
                .body(Body::from(seek_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
}

#[tokio::test]
async fn payments_enabled_paid_but_unauthenticated_is_401() {
    // A valid payment clears the gate, but the handler still requires a session:
    // a paid yet unauthenticated request is `401`.
    let state = base_state()
        .await
        .with_payment(test_requirements(), Arc::new(MockVerifier));
    let router = router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/seeks")
                .header("content-type", "application/json")
                .header(X_PAYMENT, valid_payment_header())
                .body(Body::from(seek_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn payments_enabled_read_endpoints_stay_free() {
    // Only the creation route is gated; reads need no payment.
    let state = base_state()
        .await
        .with_payment(test_requirements(), Arc::new(MockVerifier));
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let router = router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .uri(format!("/users/{}", alice.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}
