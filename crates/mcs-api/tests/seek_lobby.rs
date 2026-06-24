//! End-to-end integration tests for the browsable/joinable seek lobby (#77).
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket is bound), backed by an in-memory
//! SQLite database with the standard-chess variant registered. They exercise the
//! lobby path: a player posts a seek, it shows up in `GET /seeks`, a second
//! player joins it with `POST /seeks/{id}/accept`, a playable game is created
//! (retrievable, hub-registered, with colours per the creator's preference), and
//! the seek leaves the lobby — plus the negative and concurrency cases.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::VariantRegistry;
use mcs_domain::{SeekId, User};
use mcs_storage::{ClaimOutcome, SeekRepo, SqlxStorage};
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring (mirrors crates/mcs-api/tests/rest_game.rs).
// ---------------------------------------------------------------------------

/// Builds an [`AppState`] backed by a fresh in-memory SQLite database with the
/// standard variant registered.
async fn test_app() -> AppState {
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite");
    let storage = Arc::new(storage);

    let mut registry = VariantRegistry::new();
    register(&mut registry);
    let variants = Arc::new(registry);

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
    AppState::new(storage, variants, session, siwe)
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
    issue_session(state.session_config(), user.id).expect("mint token")
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The JSON body of a standard blitz seek with the given colour preference and
/// rated flag.
fn seek_body(color: &str, rated: bool) -> Value {
    json!({
        "variant_id": STANDARD_VARIANT_ID,
        "time_control": { "type": "real_time", "initial_secs": 300, "increment_secs": 2 },
        "color_preference": color,
        "rated": rated,
    })
}

/// `POST /seeks` as `token`, returning the request.
fn post_seek(token: &str, color: &str, rated: bool) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/seeks")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(seek_body(color, rated).to_string()))
        .unwrap()
}

/// `POST /seeks/{id}/accept` as `token`.
fn accept_seek(token: &str, seek_id: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/seeks/{seek_id}/accept"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// `GET /seeks` (public).
fn get_seeks() -> Request<Body> {
    Request::builder()
        .uri("/seeks")
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Happy path: post → browse → accept → playable game, lobby drains.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_seek_can_be_browsed_and_accepted_into_a_game() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let bob = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state.clone());

    // 1. Alice posts a *casual* seek wanting White; nobody is waiting → queued.
    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "white", false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["status"], "queued", "got {body}");
    let seek_id = body["seek_id"].as_str().expect("seek id").to_owned();

    // 2. The lobby lists exactly Alice's seek, with her address and terms.
    let resp = router.clone().oneshot(get_seeks()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    let seeks = body["seeks"].as_array().expect("seeks array");
    assert_eq!(seeks.len(), 1, "exactly one open seek; got {body}");
    let listed = &seeks[0];
    assert_eq!(listed["seek_id"].as_str().unwrap(), seek_id);
    assert_eq!(
        listed["creator"]["user_id"].as_str().unwrap(),
        alice.id.to_string()
    );
    assert_eq!(
        listed["creator"]["address"].as_str().unwrap(),
        "0x1111111111111111111111111111111111111111"
    );
    assert_eq!(listed["variant_id"], STANDARD_VARIANT_ID);
    assert_eq!(listed["color_preference"], "white");
    assert_eq!(listed["rated"], false);

    // 3. Bob joins Alice's seek directly → a game is created.
    let resp = router
        .clone()
        .oneshot(accept_seek(&bob_token, &seek_id))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let game = body_json(resp.into_body()).await;
    let game_id = game["id"].as_str().expect("game id").to_owned();
    assert_eq!(game["variant_id"], STANDARD_VARIANT_ID);
    assert_eq!(game["lifecycle"], "active");
    // Colours follow Alice's preference (White); Bob takes Black.
    assert_eq!(game["white"].as_str().unwrap(), alice.id.to_string());
    assert_eq!(game["black"].as_str().unwrap(), bob.id.to_string());
    // The seek's casual flag carries into the game.
    assert_eq!(game["rated"], false);

    // 4. The game is live in the hub and retrievable by id.
    let parsed: mcs_domain::GameId = game_id.parse().unwrap();
    assert!(
        state.game_hub().get(parsed).is_some(),
        "the created game must be live in the hub"
    );
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 5. The lobby no longer lists the seek.
    let resp = router.oneshot(get_seeks()).await.unwrap();
    let body = body_json(resp.into_body()).await;
    assert!(
        body["seeks"].as_array().unwrap().is_empty(),
        "accepted seek must leave the lobby; got {body}"
    );
}

// ---------------------------------------------------------------------------
// Colour preference: a Black-preferring creator keeps Black.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creator_keeps_black_preference_on_accept() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let bob = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state.clone());

    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "black", true))
        .await
        .unwrap();
    let seek_id = body_json(resp.into_body()).await["seek_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = router
        .oneshot(accept_seek(&bob_token, &seek_id))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let game = body_json(resp.into_body()).await;
    // Alice (Black) and Bob (White).
    assert_eq!(game["black"].as_str().unwrap(), alice.id.to_string());
    assert_eq!(game["white"].as_str().unwrap(), bob.id.to_string());
    assert_eq!(game["rated"], true);
}

// ---------------------------------------------------------------------------
// Negative cases.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accepting_own_seek_is_bad_request() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let alice_token = token_for(&state, &alice);
    let router = router(state.clone());

    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "random", true))
        .await
        .unwrap();
    let seek_id = body_json(resp.into_body()).await["seek_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = router
        .oneshot(accept_seek(&alice_token, &seek_id))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // The seek is untouched — still open in the lobby.
    assert_eq!(state.matchmaker().open_seeks().await.unwrap().len(), 1);
}

#[tokio::test]
async fn accepting_unknown_seek_is_not_found() {
    let state = test_app().await;
    let bob = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let bob_token = token_for(&state, &bob);
    let router = router(state);

    let unknown = SeekId::new();
    let resp = router
        .oneshot(accept_seek(&bob_token, &unknown.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn accepting_an_already_taken_seek_is_rejected() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let bob = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let carol = create_user(&state, "0x3333333333333333333333333333333333333333").await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let carol_token = token_for(&state, &carol);
    let router = router(state);

    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "white", true))
        .await
        .unwrap();
    let seek_id = body_json(resp.into_body()).await["seek_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Bob takes the seek first.
    let resp = router
        .clone()
        .oneshot(accept_seek(&bob_token, &seek_id))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Carol's later accept of the now-consumed seek is rejected. Once a seek is
    // gone, the handler reports it as not-found (it reads as absent before the
    // claim); a seek lost *racingly* between read and claim surfaces as 409
    // instead — both are valid "you can't have it" answers (see the concurrent
    // test for the 409 path).
    let resp = router
        .oneshot(accept_seek(&carol_token, &seek_id))
        .await
        .unwrap();
    assert!(
        matches!(resp.status(), StatusCode::NOT_FOUND | StatusCode::CONFLICT),
        "a taken seek must be 404 or 409, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn accepting_unauthenticated_is_unauthorized() {
    let state = test_app().await;
    let router = router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/seeks/{}/accept", SeekId::new()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Concurrency: two simultaneous accepts yield exactly one game.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_accepts_produce_exactly_one_game() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let bob = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let carol = create_user(&state, "0x3333333333333333333333333333333333333333").await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let carol_token = token_for(&state, &carol);
    let router = router(state.clone());

    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "white", true))
        .await
        .unwrap();
    let seek_id = body_json(resp.into_body()).await["seek_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Fire Bob's and Carol's accepts concurrently against the one open seek.
    let (bob_resp, carol_resp) = tokio::join!(
        router.clone().oneshot(accept_seek(&bob_token, &seek_id)),
        router.clone().oneshot(accept_seek(&carol_token, &seek_id)),
    );
    let mut statuses = [bob_resp.unwrap().status(), carol_resp.unwrap().status()];
    statuses.sort();

    // Exactly one accept wins (200); the other loses the claim (409).
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "exactly one concurrent accept must succeed"
    );

    // The seek is gone and the lobby is empty.
    assert!(state.matchmaker().open_seeks().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Storage-level: the atomic claim reports prior existence exactly once.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_is_atomic_and_single_winner() {
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite");

    let seek = mcs_domain::Seek::new(
        mcs_domain::UserId::new(),
        STANDARD_VARIANT_ID.to_owned(),
        mcs_domain::TimeControl::Unlimited,
        mcs_domain::ColorPreference::White,
        true,
        OffsetDateTime::now_utc(),
    );
    SeekRepo::create(&storage, &seek).await.unwrap();

    // First claim wins; the second observes it already gone.
    assert_eq!(
        SeekRepo::claim(&storage, seek.id).await.unwrap(),
        ClaimOutcome::Claimed
    );
    assert_eq!(
        SeekRepo::claim(&storage, seek.id).await.unwrap(),
        ClaimOutcome::AlreadyClaimed
    );
}
