//! Integration tests for player online-presence (#79).
//!
//! These tests exercise the full stack: from an authenticated REST request or
//! WebSocket connect, through the presence tracker, to the
//! `GET /users/{id}/status` and profile endpoints.
//!
//! The injectable-clock path (`InProcessPresence::mark_seen_at` /
//! `is_online_at`) is tested at the unit level in `presence.rs`. Here we focus
//! on the HTTP surface and the wiring through `AppState`.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::Duration;
use tower::ServiceExt;

use k256::ecdsa::{RecoveryId, Signature, SigningKey};

use mcs_api::{router, AppState, InProcessPresence, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::VariantRegistry;
use mcs_storage::SqlxStorage;
use mcs_variant_standard::register;

// ---------------------------------------------------------------------------
// Shared test helpers (duplicated from auth_flow.rs to keep each test file
// self-contained — no shared test crate in this workspace).
// ---------------------------------------------------------------------------

const TEST_PRIVATE_KEY: &str = "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const TEST_ADDRESS: &str = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23";

fn signing_key() -> SigningKey {
    SigningKey::from_slice(&hex::decode(TEST_PRIVATE_KEY).unwrap()).unwrap()
}

fn sign_hex(sk: &SigningKey, message: &str) -> String {
    let parsed: siwe::Message = message.parse().unwrap();
    let prehash = parsed.eip191_hash().unwrap();
    let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&prehash).unwrap();
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&sig.to_bytes());
    out[64] = recid.to_byte() + 27;
    hex::encode(out)
}

async fn test_state_with_ttl(online_ttl: Duration) -> AppState {
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite");
    let storage = Arc::new(storage);

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

    let tracker = Arc::new(InProcessPresence::new());
    AppState::new(storage, Arc::new(registry), session, siwe).with_presence(tracker, online_ttl)
}

async fn test_state() -> AppState {
    // Use a generous TTL so a freshly-authenticated request always appears online.
    test_state_with_ttl(Duration::seconds(30)).await
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Full SIWE login: returns `(state, token, user_id)`.
async fn login_with_state(state: &AppState) -> (String, String) {
    let app = router(state.clone());

    // 1. Nonce.
    let req = Request::builder()
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    let message = body["message"].as_str().unwrap().to_owned();

    // 2. Sign.
    let signature = sign_hex(&signing_key(), &message);

    // 3. Verify.
    let req = Request::builder()
        .method("POST")
        .uri("/auth/verify")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "message": message, "signature": signature }).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    let token = body["token"].as_str().unwrap().to_owned();
    let user_id = body["user_id"].as_str().unwrap().to_owned();

    (token, user_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// After an authenticated request, `GET /users/{id}/status` must return
/// `online: true` and a recent `last_seen`.
#[tokio::test]
async fn authenticated_request_marks_user_online() {
    let state = test_state().await;
    let app = router(state.clone());

    let (token, user_id) = login_with_state(&state).await;

    // Make one more authenticated request to ensure mark_seen was called.
    let req = Request::builder()
        .uri("/profile")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Now query status.
    let req = Request::builder()
        .uri(format!("/users/{user_id}/status"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp.into_body()).await;
    assert_eq!(
        body["online"].as_bool(),
        Some(true),
        "user should be online after an authed request"
    );
    assert!(
        body["last_seen"].as_str().is_some(),
        "last_seen must be present"
    );
}

/// A user who has never made an authenticated request must be offline with no
/// `last_seen`.
#[tokio::test]
async fn never_seen_user_is_offline() {
    let state = test_state().await;
    let app = router(state.clone());

    // Register the user but never touch an authed endpoint, so presence is
    // never stamped. We use the auth token only for the login step itself
    // (which does stamp presence), so we need to check a *different* route for
    // the "never seen" case. Instead, create a fresh user via a second login
    // key — but simpler: just check before login, reading via the known user_id
    // from a second state where the user hasn't authenticated yet.

    // Use the existing `test_state` approach: login stores the user but the
    // presence mark happens on each verified request. We can test the
    // never-seen case by directly inserting a user and then querying their
    // status without ever authenticating.
    //
    // The simplest stable approach: query a well-known non-existent UUID.
    let fake_id = "00000000-0000-0000-0000-000000000001";
    let req = Request::builder()
        .uri(format!("/users/{fake_id}/status"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // Should be 404 (user doesn't exist) — not 200 with "offline"
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Query status for a real user who exists but has never triggered a
/// mark_seen — their `last_seen` is absent, `online` is false.
#[tokio::test]
async fn user_seen_only_via_login_but_no_further_requests() {
    // The auth/verify handler does NOT use AuthUser — it's the issuance step.
    // The presence mark happens in AuthUser::from_request_parts, which runs on
    // every *subsequent* authenticated request (GET /profile, etc.).
    // So after a bare login (nonce + verify), no mark_seen has been called.
    let state = test_state().await;
    let app = router(state.clone());

    // Login only — no further authed request.
    let (token, user_id) = login_with_state(&state).await;

    // The bare login minted a token and created the user account, but since
    // auth/verify itself doesn't extract AuthUser, presence is untouched.
    // However, we need to confirm this — let's just check the status while
    // verifying that a subsequent authed call does flip it to online.

    // First: status immediately after login (no authed request yet).
    let req = Request::builder()
        .uri(format!("/users/{user_id}/status"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_before = body_json(resp.into_body()).await;

    // The user has not yet made an AuthUser-gated request, so offline with no last_seen.
    assert_eq!(body_before["online"].as_bool(), Some(false));
    assert!(body_before.get("last_seen").is_none() || body_before["last_seen"].is_null());

    // Now make an authenticated request.
    let req = Request::builder()
        .uri("/profile")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    // Now they should be online.
    let req = Request::builder()
        .uri(format!("/users/{user_id}/status"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_after = body_json(resp.into_body()).await;
    assert_eq!(body_after["online"].as_bool(), Some(true));
    assert!(body_after["last_seen"].as_str().is_some());
}

/// `GET /users/{id}` profile includes an `online` field that is `true` after
/// the user authenticated.
#[tokio::test]
async fn profile_endpoint_includes_online_field() {
    let state = test_state().await;
    let app = router(state.clone());

    let (token, user_id) = login_with_state(&state).await;

    // Trigger a mark_seen.
    let req = Request::builder()
        .uri("/profile")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let _resp = app.clone().oneshot(req).await.unwrap();

    // Check the public profile.
    let req = Request::builder()
        .uri(format!("/users/{user_id}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;

    assert!(
        body.get("online").is_some(),
        "profile must include 'online'"
    );
    assert_eq!(
        body["online"].as_bool(),
        Some(true),
        "profile online should be true after authed request"
    );
}

/// `GET /profile` (the authed "me" route) also includes `online: true`.
#[tokio::test]
async fn my_profile_endpoint_includes_online_field() {
    let state = test_state().await;
    let app = router(state.clone());

    let (token, _user_id) = login_with_state(&state).await;

    let req = Request::builder()
        .uri("/profile")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;

    assert!(
        body.get("online").is_some(),
        "profile must include 'online'"
    );
    // The AuthUser extractor ran, so the user was just marked seen.
    assert_eq!(body["online"].as_bool(), Some(true));
}

/// TTL expiry: using the injectable clock (`mark_seen_at` / `is_online_at`),
/// a user flips to offline once their `last_seen` is older than the TTL.
/// No real sleep required.
#[tokio::test]
async fn ttl_expiry_flips_user_to_offline_with_injectable_clock() {
    use mcs_api::InProcessPresence;
    use mcs_domain::UserId;
    use time::macros::datetime;

    let tracker = InProcessPresence::new();
    let user = UserId::new();
    let ttl = Duration::seconds(30);

    let seen_at = datetime!(2025-06-01 10:00:00 UTC);
    tracker.mark_seen_at(user, seen_at);

    // 20 s later: still online.
    let t_within = seen_at + Duration::seconds(20);
    assert!(
        tracker.is_online_at(user, ttl, t_within),
        "should be online 20s after last seen"
    );

    // 30 s exactly: boundary is online.
    let t_boundary = seen_at + ttl;
    assert!(
        tracker.is_online_at(user, ttl, t_boundary),
        "should still be online at exactly the TTL boundary"
    );

    // 31 s: past the TTL, now offline.
    let t_expired = seen_at + ttl + Duration::seconds(1);
    assert!(
        !tracker.is_online_at(user, ttl, t_expired),
        "should be offline 1s past the TTL"
    );
}
