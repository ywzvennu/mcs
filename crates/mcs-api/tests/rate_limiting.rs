//! Integration tests for the per-IP HTTP rate limiting (#100).
//!
//! These drive the real [`mcs_api::router`] in-process via
//! [`tower::ServiceExt::oneshot`]. Because `oneshot` does not attach a
//! `ConnectInfo` peer address, the test state is configured to trust an
//! `X-Forwarded-For` header so each request can present a client IP — the same
//! path a deployment behind a reverse proxy uses. Hammering a rate-limited route
//! past its limit must return **429 Too Many Requests** with a `Retry-After`
//! header; staying within the limit (or using a different IP) must be unaffected.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use mcs_api::{router, ApiError, AppState, LimitsConfig, RateLimitTier, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::{VariantOptions, VariantRegistry};
use mcs_domain::{TimeControl, UserId};
use mcs_storage::SqlxStorage;
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};
use time::Duration;

const TEST_ADDRESS: &str = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23";

/// Builds an [`AppState`] whose limits trust `X-Forwarded-For` and rate the
/// nonce route at `nonce_per_minute` requests per IP.
async fn state_with_nonce_rate(nonce_per_minute: u32) -> AppState {
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

    let limits = LimitsConfig {
        nonce: RateLimitTier::per_minute(nonce_per_minute),
        trusted_proxy_header: Some("x-forwarded-for".to_owned()),
        ..LimitsConfig::default()
    };

    AppState::new(storage, Arc::new(registry), session, siwe).with_limits(limits)
}

/// A `GET /auth/nonce` request presenting `ip` via the trusted proxy header.
fn nonce_request(ip: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/auth/nonce?address={TEST_ADDRESS}"))
        .header("x-forwarded-for", ip)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn nonce_within_limit_is_allowed() {
    // A generous limit (the default 10/min) lets a handful of requests through.
    let app = router(state_with_nonce_rate(10).await);
    for i in 0..5 {
        let resp = app
            .clone()
            .oneshot(nonce_request("198.51.100.10"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "request {i} within limit");
    }
}

#[tokio::test]
async fn hammering_nonce_past_limit_returns_429_with_retry_after() {
    // A bucket of 3 tokens: 3 succeed, the 4th is throttled.
    let app = router(state_with_nonce_rate(3).await);
    let ip = "203.0.113.5";

    for i in 0..3 {
        let resp = app.clone().oneshot(nonce_request(ip)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "request {i} within the burst of 3"
        );
    }

    // The 4th request (N+1) exceeds the bucket.
    let resp = app.clone().oneshot(nonce_request(ip)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the 4th request past the limit must be 429"
    );
    let retry_after = resp
        .headers()
        .get("retry-after")
        .expect("429 must carry a Retry-After header")
        .to_str()
        .unwrap();
    assert!(
        retry_after.parse::<u64>().is_ok(),
        "Retry-After must be whole seconds, got {retry_after:?}"
    );
}

#[tokio::test]
async fn rate_limit_is_keyed_per_ip() {
    // One IP exhausts its bucket; a different IP is unaffected.
    let app = router(state_with_nonce_rate(1).await);

    // First IP: 1 allowed, then throttled.
    let resp = app
        .clone()
        .oneshot(nonce_request("203.0.113.1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(nonce_request("203.0.113.1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // A different IP still gets its own fresh bucket.
    let resp = app
        .clone()
        .oneshot(nonce_request("203.0.113.2"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a different IP must have its own bucket"
    );
}

#[tokio::test]
async fn disabled_rate_limit_never_throttles() {
    // A 0 rate disables the limit entirely.
    let app = router(state_with_nonce_rate(0).await);
    let ip = "192.0.2.50";
    for i in 0..30 {
        let resp = app.clone().oneshot(nonce_request(ip)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "request {i} with limit off");
    }
}

// ---------------------------------------------------------------------------
// Per-user live-game cap (#100), exercised through `create_and_spawn_game`.
// ---------------------------------------------------------------------------

/// Builds an [`AppState`] capping each user at `max_games_per_user` live games.
async fn state_with_game_cap(max_games_per_user: u32) -> AppState {
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

    let limits = LimitsConfig {
        max_games_per_user,
        ..LimitsConfig::default()
    };
    AppState::new(storage, Arc::new(registry), session, siwe).with_limits(limits)
}

async fn spawn_game(state: &AppState, white: UserId, black: UserId) -> Result<(), ApiError> {
    state
        .create_and_spawn_game(
            white,
            black,
            STANDARD_VARIANT_ID,
            TimeControl::Unlimited,
            false,
            VariantOptions::default(),
        )
        .await
        .map(|_| ())
}

#[tokio::test]
async fn per_user_game_cap_rejects_over_the_limit() {
    // Cap each user at 2 simultaneous live games.
    let state = state_with_game_cap(2).await;
    let alice = UserId::new();
    let (b, c, d) = (UserId::new(), UserId::new(), UserId::new());

    // Alice can be in two games.
    spawn_game(&state, alice, b)
        .await
        .expect("1st game under cap");
    spawn_game(&state, alice, c)
        .await
        .expect("2nd game under cap");
    assert_eq!(state.live_games().count(alice), 2);

    // A third game for Alice is refused with 429.
    let err = spawn_game(&state, alice, d)
        .await
        .expect_err("3rd game must be refused");
    assert!(
        matches!(err, ApiError::TooManyRequests(_)),
        "over-cap creation must be TooManyRequests, got {err:?}"
    );
    assert_eq!(err.status_code(), StatusCode::TOO_MANY_REQUESTS);

    // The refused creation did not bump the opponent's count.
    assert_eq!(
        state.live_games().count(d),
        0,
        "a refused creation must not leak a slot"
    );
}

#[tokio::test]
async fn finishing_a_game_frees_a_per_user_slot() {
    // Cap of 1: Alice can hold exactly one live game at a time.
    let state = state_with_game_cap(1).await;
    let alice = UserId::new();
    let bob = UserId::new();

    spawn_game(&state, alice, bob).await.expect("1st under cap");
    assert_eq!(state.live_games().count(alice), 1);
    assert!(
        spawn_game(&state, alice, UserId::new()).await.is_err(),
        "a 2nd concurrent game is over the cap of 1"
    );

    // Releasing Alice's slot (what the completion hook does on game finish) frees
    // room for a new game.
    state.live_games().release(alice);
    assert_eq!(state.live_games().count(alice), 0);
    spawn_game(&state, alice, UserId::new())
        .await
        .expect("a freed slot allows a new game");
}

#[tokio::test]
async fn zero_game_cap_is_unlimited() {
    let state = state_with_game_cap(0).await;
    let alice = UserId::new();
    for i in 0..10 {
        spawn_game(&state, alice, UserId::new())
            .await
            .unwrap_or_else(|e| panic!("game {i} with cap off failed: {e:?}"));
    }
    // With the cap disabled nothing is tracked.
    assert_eq!(state.live_games().count(alice), 0);
}
