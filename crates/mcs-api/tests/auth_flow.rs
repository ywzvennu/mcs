//! End-to-end integration tests for the SIWE auth endpoints and the
//! [`AuthUser`] session extractor.
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket is bound), backed by an in-memory
//! SQLite database. A fixed secp256k1 key plays the role of the wallet, so the
//! whole handshake — request nonce, sign the SIWE message, verify, receive a
//! token, call a protected route — is exercised exactly as a real client would.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use sha3::{Digest, Keccak256};
use time::Duration;

use mcs_api::{router, AppState, AuthUser, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::VariantRegistry;
use mcs_storage::SqlxStorage;
use mcs_variant_mcr::register;

// ---------------------------------------------------------------------------
// Test wallet (fixed key) — mirrors the mcs-auth signing test vector.
// ---------------------------------------------------------------------------

/// A fixed, well-known secp256k1 private key used as a deterministic test
/// vector (the canonical secp256k1/web3 example key — NOT a real account).
const TEST_PRIVATE_KEY: &str = "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";

/// The Ethereum address derived from [`TEST_PRIVATE_KEY`].
const TEST_ADDRESS: &str = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23";

fn signing_key() -> SigningKey {
    SigningKey::from_slice(&hex::decode(TEST_PRIVATE_KEY).unwrap()).unwrap()
}

/// Derives the 20-byte Ethereum address from a signing key (sanity helper).
fn derive_address(sk: &SigningKey) -> [u8; 20] {
    let vk: VerifyingKey = *sk.verifying_key();
    let point = vk.to_encoded_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    let mut out = [0u8; 20];
    out.copy_from_slice(&hash[12..]);
    out
}

/// Produces a 65-byte EIP-191 personal-sign signature over `message`, matching
/// what a wallet returns (recovery id offset by 27), hex-encoded for transport.
fn sign_hex(sk: &SigningKey, message: &str) -> String {
    let parsed: siwe::Message = message.parse().unwrap();
    let prehash = parsed.eip191_hash().unwrap();
    let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&prehash).unwrap();
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&sig.to_bytes());
    out[64] = recid.to_byte() + 27;
    hex::encode(out)
}

// ---------------------------------------------------------------------------
// Test app wiring
// ---------------------------------------------------------------------------

/// The fixed JWT signing secret used by the test states. Shared by every
/// `AppState` in this file so a token minted by one is accepted by another
/// pointed at the same store — the property the "restart" test relies on.
const TEST_SECRET: &[u8] = b"test-secret-key-that-is-definitely-32-bytes!!";

/// Builds an [`AppState`] over the given storage handle, with the fixed test
/// session secret and issuer. Two states built over the *same* storage (e.g.
/// before and after a simulated restart) therefore validate each other's tokens
/// and share one revocation denylist.
fn state_over(storage: Arc<SqlxStorage>) -> AppState {
    let mut registry = VariantRegistry::new();
    register(&mut registry);
    let variants = Arc::new(registry);

    let session = SessionConfig::new(
        TEST_SECRET.to_vec(),
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
    AppState::new(storage, variants, session, siwe)
}

async fn test_state() -> AppState {
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite");
    state_over(Arc::new(storage))
}

/// A trivial protected handler: requiring [`AuthUser`] gates it behind a valid
/// session token. Returns the caller's identity so tests can assert on it.
async fn whoami(user: AuthUser) -> impl IntoResponse {
    Json(json!({
        "user_id": user.user_id,
        "address": user.address,
    }))
}

/// Builds the production router plus a protected `/me` test route on the same
/// state, so the [`AuthUser`] extractor is exercised against real auth output.
fn test_router(state: AppState) -> Router {
    router(state.clone()).merge(Router::new().route("/me", get(whoami)).with_state(state))
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Runs the full happy path up to a minted token, returning `(router, token)`.
async fn login() -> (Router, AppState, String) {
    let state = test_state().await;
    let app = test_router(state.clone());

    // 1. Request a nonce challenge for the wallet address.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    let message = body["message"].as_str().unwrap().to_owned();

    // 2. Sign the canonical message with the wallet key.
    let signature = sign_hex(&signing_key(), &message);

    // 3. Verify the signed challenge and receive a session token.
    let req = Request::builder()
        .method("POST")
        .uri("/auth/verify")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "message": message, "signature": signature }).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    let token = body["token"].as_str().unwrap().to_owned();
    assert_eq!(body["address"].as_str().unwrap(), TEST_ADDRESS);

    (app, state, token)
}

// ---------------------------------------------------------------------------
// Sanity: the test wallet matches the pinned address.
// ---------------------------------------------------------------------------

#[test]
fn test_wallet_address_matches_vector() {
    let derived = derive_address(&signing_key());
    let expected = hex::decode(&TEST_ADDRESS[2..]).unwrap();
    assert_eq!(derived.as_slice(), expected.as_slice());
}

// ---------------------------------------------------------------------------
// Happy path: nonce -> sign -> verify -> token -> protected route -> 200.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_login_flow_reaches_protected_route() {
    let (app, _state, token) = login().await;

    let req = Request::builder()
        .method("GET")
        .uri("/me")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp.into_body()).await;
    assert_eq!(body["address"].as_str().unwrap(), TEST_ADDRESS);
    assert!(body["user_id"].is_string());
}

#[tokio::test]
async fn nonce_response_carries_structured_challenge() {
    let state = test_state().await;
    let app = test_router(state);

    let req = Request::builder()
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp.into_body()).await;
    let challenge = &body["challenge"];
    assert_eq!(challenge["address"].as_str().unwrap(), TEST_ADDRESS);
    assert_eq!(challenge["domain"].as_str().unwrap(), "localhost");
    assert_eq!(challenge["chain_id"].as_u64().unwrap(), 1);
    let nonce = challenge["nonce"].as_str().unwrap();
    assert!(nonce.len() >= 8);
    // The canonical message must embed the same nonce.
    assert!(body["message"].as_str().unwrap().contains(nonce));
}

// ---------------------------------------------------------------------------
// Negative cases.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_address_query_is_unprocessable() {
    let state = test_state().await;
    let app = test_router(state);

    let req = Request::builder()
        .uri("/auth/nonce?address=not-an-address")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // Deserialization of the EvmAddress query param fails -> client error.
    assert!(resp.status().is_client_error(), "got {}", resp.status());
}

#[tokio::test]
async fn bad_signature_is_unauthorized() {
    let state = test_state().await;
    let app = test_router(state);

    // Get a real nonce/message first.
    let req = Request::builder()
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let body = body_json(resp.into_body()).await;
    let message = body["message"].as_str().unwrap().to_owned();

    // A syntactically valid (65-byte) but cryptographically wrong signature.
    let bogus = hex::encode([0u8; 65]);
    let req = Request::builder()
        .method("POST")
        .uri("/auth/verify")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "message": message, "signature": bogus }).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_signature_hex_is_bad_request() {
    let state = test_state().await;
    let app = test_router(state);

    let req = Request::builder()
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let message = body_json(resp.into_body()).await["message"]
        .as_str()
        .unwrap()
        .to_owned();

    let req = Request::builder()
        .method("POST")
        .uri("/auth/verify")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "message": message, "signature": "zzzz-not-hex" }).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reused_nonce_is_unauthorized() {
    let state = test_state().await;
    let app = test_router(state);

    // Obtain and sign a real challenge.
    let req = Request::builder()
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .body(Body::empty())
        .unwrap();
    let message = body_json(app.clone().oneshot(req).await.unwrap().into_body()).await["message"]
        .as_str()
        .unwrap()
        .to_owned();
    let signature = sign_hex(&signing_key(), &message);

    let verify_req = || {
        Request::builder()
            .method("POST")
            .uri("/auth/verify")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "message": message, "signature": signature }).to_string(),
            ))
            .unwrap()
    };

    // First verify succeeds and consumes the nonce.
    let resp = app.clone().oneshot(verify_req()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Replaying the identical (message, signature) pair must be rejected.
    let resp = app.oneshot(verify_req()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_bearer_is_unauthorized() {
    let state = test_state().await;
    let app = test_router(state);

    let req = Request::builder().uri("/me").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_bearer_token_is_unauthorized() {
    let state = test_state().await;
    let app = test_router(state);

    let req = Request::builder()
        .uri("/me")
        .header("authorization", "Bearer not.a.jwt")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_scheme_authorization_is_unauthorized() {
    let state = test_state().await;
    let app = test_router(state);

    let req = Request::builder()
        .uri("/me")
        .header("authorization", "Basic dXNlcjpwYXNz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Logout / token revocation (#101).
// ---------------------------------------------------------------------------

/// Runs nonce -> sign -> verify against an already-built `app`, returning the
/// minted session token. Used by the revocation tests, which need several
/// tokens issued against one shared state.
async fn token_on(app: &Router) -> String {
    let req = Request::builder()
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .body(Body::empty())
        .unwrap();
    let message = body_json(app.clone().oneshot(req).await.unwrap().into_body()).await["message"]
        .as_str()
        .unwrap()
        .to_owned();
    let signature = sign_hex(&signing_key(), &message);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/verify")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "message": message, "signature": signature }).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp.into_body()).await["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Calls `GET /me` with the given bearer token and returns the status.
async fn me_status(app: &Router, token: &str) -> StatusCode {
    let req = Request::builder()
        .uri("/me")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// Calls `POST /auth/logout` with the given bearer token and returns the status.
async fn logout_status(app: &Router, token: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri("/auth/logout")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn logout_revokes_the_current_token() {
    let app = test_router(test_state().await);
    let token = token_on(&app).await;

    // The token works before logout.
    assert_eq!(me_status(&app, &token).await, StatusCode::OK);

    // Logout returns 204 and revokes this token.
    assert_eq!(logout_status(&app, &token).await, StatusCode::NO_CONTENT);

    // The same token is now rejected on an authenticated request.
    assert_eq!(me_status(&app, &token).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logout_only_revokes_the_token_used() {
    let app = test_router(test_state().await);
    // Two distinct tokens for the same account.
    let token_a = token_on(&app).await;
    let token_b = token_on(&app).await;
    assert_ne!(token_a, token_b, "each login mints a distinct token");

    // Log out token A only.
    assert_eq!(logout_status(&app, &token_a).await, StatusCode::NO_CONTENT);

    // A is rejected; B — never revoked — still works.
    assert_eq!(me_status(&app, &token_a).await, StatusCode::UNAUTHORIZED);
    assert_eq!(me_status(&app, &token_b).await, StatusCode::OK);
}

#[tokio::test]
async fn logout_without_bearer_is_unauthorized() {
    let app = test_router(test_state().await);
    let req = Request::builder()
        .method("POST")
        .uri("/auth/logout")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn second_logout_with_revoked_token_is_unauthorized() {
    let app = test_router(test_state().await);
    let token = token_on(&app).await;

    // The first logout succeeds and revokes the token.
    assert_eq!(logout_status(&app, &token).await, StatusCode::NO_CONTENT);
    // A second logout cannot even authenticate: the `AuthUser` extractor rejects
    // the now-revoked token with 401 before the handler runs. (The underlying
    // `revoke` is itself idempotent — see the storage-layer tests — but logout
    // requires a still-valid session, so it is unreachable a second time.)
    assert_eq!(logout_status(&app, &token).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revocation_survives_a_restart() {
    // A file-backed SQLite DB so the denylist persists across two independent
    // `SqlxStorage` connections (a simulated process restart). The first state
    // mints and revokes a token; a fresh state over the same file must still
    // reject it.
    let path = std::env::temp_dir().join(format!("mcs-logout-{}.db", std::process::id()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    // Clean up any leftover from a previous run.
    let _ = std::fs::remove_file(&path);

    let token = {
        let storage = Arc::new(
            SqlxStorage::connect(&url)
                .await
                .expect("connect + migrate file sqlite"),
        );
        let app = test_router(state_over(storage));
        let token = token_on(&app).await;
        assert_eq!(logout_status(&app, &token).await, StatusCode::NO_CONTENT);
        // Confirmed revoked on the original state.
        assert_eq!(me_status(&app, &token).await, StatusCode::UNAUTHORIZED);
        token
    };

    // "Restart": a brand-new state/store over the same database file.
    let storage = Arc::new(
        SqlxStorage::connect(&url)
            .await
            .expect("reconnect file sqlite"),
    );
    let app = test_router(state_over(storage));

    // The revocation persisted: the token is still rejected after restart.
    assert_eq!(me_status(&app, &token).await, StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn token_with_wrong_secret_is_unauthorized() {
    // A token minted by a different server (different secret) must be rejected.
    let app = test_router(test_state().await);

    let foreign = SessionConfig::new(
        b"a-totally-different-secret-key-value-here!!".to_vec(),
        Duration::hours(1),
        "mcs-test".to_owned(),
    );
    let token = mcs_auth::issue_session(&foreign, mcs_domain::UserId::new())
        .unwrap()
        .token;

    let req = Request::builder()
        .uri("/me")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
