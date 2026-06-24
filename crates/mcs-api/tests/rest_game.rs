//! End-to-end integration tests for the REST seek/game/profile endpoints.
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket is bound), backed by an in-memory
//! SQLite database with the standard-chess variant registered. They exercise the
//! full matchmaking-to-game path: user A queues a seek, user B posts a
//! compatible seek, a game is created and is then retrievable via `GET
//! /games/{id}`, present in the live-game hub, and listed by `GET /games`.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::VariantRegistry;
use mcs_domain::{User, UserId};
use mcs_storage::SqlxStorage;
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

/// Builds an [`AppState`] backed by a fresh in-memory SQLite database with the
/// standard variant registered. The same `Arc<SqlxStorage>` is coerced into
/// every repository handle inside [`AppState::new`], so tests can seed users
/// through `state.storage()` and the API reads them back over one database.
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
    issue_session(state.session_config(), user.id)
        .expect("mint token")
        .token
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The JSON body of a standard blitz seek with the given colour preference.
fn seek_body(color: &str) -> Value {
    json!({
        "variant_id": STANDARD_VARIANT_ID,
        "time_control": { "type": "real_time", "initial_secs": 300, "increment_secs": 2 },
        "color_preference": color,
    })
}

/// `POST /seeks` as `token`, returning the response.
fn post_seek(token: &str, color: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/seeks")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(seek_body(color).to_string()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Happy path: queue then pair into a created, retrievable, hub-registered game.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compatible_seeks_create_a_retrievable_game() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let bob = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);

    let router = router(state.clone());

    // 1. Alice posts a seek wanting White; nobody is waiting → queued.
    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "white"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["status"], "queued", "got {body}");
    assert!(body["seek_id"].is_string(), "queued seek carries an id");

    // The seek is now open in the pool.
    assert_eq!(state.matchmaker().open_seeks().await.unwrap().len(), 1);

    // 2. Bob posts a compatible seek wanting Black → paired, a game is created.
    let resp = router
        .clone()
        .oneshot(post_seek(&bob_token, "black"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["status"], "paired", "got {body}");

    let game = &body["game"];
    let game_id = game["id"].as_str().expect("game id present").to_owned();
    assert_eq!(game["variant_id"], STANDARD_VARIANT_ID);
    assert_eq!(game["lifecycle"], "active");
    // Colour preferences are honoured: Alice (White) and Bob (Black).
    assert_eq!(game["white"].as_str().unwrap(), alice.id.to_string());
    assert_eq!(game["black"].as_str().unwrap(), bob.id.to_string());

    // The pool is drained and the live game is registered in the hub.
    assert!(state.matchmaker().open_seeks().await.unwrap().is_empty());
    let parsed: mcs_domain::GameId = game_id.parse().unwrap();
    assert!(
        state.game_hub().get(parsed).is_some(),
        "the created game must be live in the hub"
    );

    // 3. The game is retrievable by id.
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
    let fetched = body_json(resp.into_body()).await;
    assert_eq!(fetched["id"].as_str().unwrap(), game_id);

    // 4. The game appears in the recent-games list.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/games")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp.into_body()).await;
    let games = list["games"].as_array().expect("games array");
    assert!(
        games.iter().any(|g| g["id"].as_str() == Some(&game_id)),
        "the created game must be listed; got {list}"
    );
}

// ---------------------------------------------------------------------------
// Seek cancellation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creator_can_cancel_their_own_seek() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let alice_token = token_for(&state, &alice);
    let router = router(state.clone());

    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "random"))
        .await
        .unwrap();
    let body = body_json(resp.into_body()).await;
    let seek_id = body["seek_id"].as_str().unwrap().to_owned();

    let resp = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/seeks/{seek_id}"))
                .header("authorization", format!("Bearer {alice_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(state.matchmaker().open_seeks().await.unwrap().is_empty());
}

#[tokio::test]
async fn non_creator_cannot_cancel_a_seek() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let bob = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state.clone());

    let resp = router
        .clone()
        .oneshot(post_seek(&alice_token, "random"))
        .await
        .unwrap();
    let seek_id = body_json(resp.into_body()).await["seek_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Bob may not cancel Alice's seek.
    let resp = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/seeks/{seek_id}"))
                .header("authorization", format!("Bearer {bob_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // Alice's seek is untouched.
    assert_eq!(state.matchmaker().open_seeks().await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Profiles.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn public_profile_exposes_only_public_fields() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let router = router(state.clone());

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
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["id"].as_str().unwrap(), alice.id.to_string());
    assert_eq!(
        body["address"].as_str().unwrap(),
        "0x1111111111111111111111111111111111111111"
    );
    assert!(body["created_at"].is_string());
}

#[tokio::test]
async fn profile_route_returns_the_authenticated_caller() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let alice_token = token_for(&state, &alice);
    let router = router(state.clone());

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/profile")
                .header("authorization", format!("Bearer {alice_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["id"].as_str().unwrap(), alice.id.to_string());
}

// ---------------------------------------------------------------------------
// Negative cases.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_post_seek_is_unauthorized() {
    let state = test_app().await;
    let router = router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/seeks")
                .header("content-type", "application/json")
                .body(Body::from(seek_body("white").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_game_is_not_found() {
    let state = test_app().await;
    let router = router(state);

    let unknown = mcs_domain::GameId::new();
    let resp = router
        .oneshot(
            Request::builder()
                .uri(format!("/games/{unknown}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_user_profile_is_not_found() {
    let state = test_app().await;
    let router = router(state);

    let unknown = UserId::new();
    let resp = router
        .oneshot(
            Request::builder()
                .uri(format!("/users/{unknown}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
