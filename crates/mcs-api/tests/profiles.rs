//! End-to-end integration tests for username editing and per-user rating
//! endpoints (#126).
//!
//! These drive the real [`axum::Router`] in-process over an in-memory SQLite
//! database with the standard variant registered, exactly as the other REST
//! suites do. They cover:
//!
//! - `PUT /profile`: success, case-insensitive uniqueness conflict (409),
//!   invalid input (422), and changing/re-using a name;
//! - `GET /users/{id}/ratings`: every variant is listed with the `provisional`
//!   flag derived from each rating's deviation;
//! - `GET /users/{id}/rating-history`: a rated game appends two history rows,
//!   and the endpoint returns them most-recent-first.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::{Action, Color, GameSession, VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, GameLifecycle, Rating, TimeClass, TimeControl, User, UserId};
use mcs_game::{GameActor, GameHandle};
use mcs_storage::SqlxStorage;
use mcs_variant_mcr::wire::McrAction;
use mcs_variant_mcr::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

async fn test_app() -> AppState {
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

fn token_for(state: &AppState, user: &User) -> String {
    issue_session(state.session_config(), user.id)
        .expect("mint token")
        .token
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Sends `PUT /profile` as `token` with the given username, returning the status
/// and parsed body.
async fn put_profile(state: &AppState, token: &str, username: &str) -> (StatusCode, Value) {
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/profile")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(json!({ "username": username }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp.into_body()).await)
}

// ---------------------------------------------------------------------------
// PUT /profile — username editing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_profile_sets_username_and_reflects_in_profile() {
    let state = test_app().await;
    let user = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let token = token_for(&state, &user);

    let (status, body) = put_profile(&state, &token, "alice").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"].as_str(), Some("alice"));

    // The change is reflected in the public profile DTO too.
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/users/{}", user.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let profile = body_json(resp.into_body()).await;
    assert_eq!(profile["username"].as_str(), Some("alice"));
}

#[tokio::test]
async fn put_profile_change_then_reuse_same_name() {
    let state = test_app().await;
    let user = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let token = token_for(&state, &user);

    assert_eq!(put_profile(&state, &token, "alice").await.0, StatusCode::OK);
    // Change to a new name.
    let (status, body) = put_profile(&state, &token, "bob").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"].as_str(), Some("bob"));
    // Re-using one's own current name (in a different casing) is allowed.
    let (status, body) = put_profile(&state, &token, "BOB").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"].as_str(), Some("BOB"));
}

#[tokio::test]
async fn put_profile_case_insensitive_conflict_is_409() {
    let state = test_app().await;
    let alice = create_user(&state, "0x3333333333333333333333333333333333333333").await;
    let bob = create_user(&state, "0x4444444444444444444444444444444444444444").await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);

    assert_eq!(
        put_profile(&state, &alice_token, "Carol").await.0,
        StatusCode::OK
    );
    // Bob requests the same name in a different casing.
    let (status, _) = put_profile(&state, &bob_token, "carol").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn put_profile_invalid_username_is_422() {
    let state = test_app().await;
    let user = create_user(&state, "0x5555555555555555555555555555555555555555").await;
    let token = token_for(&state, &user);

    // Too short.
    assert_eq!(
        put_profile(&state, &token, "ab").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Too long (21 chars).
    assert_eq!(
        put_profile(&state, &token, "abcdefghijklmnopqrstu").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Illegal character.
    assert_eq!(
        put_profile(&state, &token, "bad name").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        put_profile(&state, &token, "with$ign").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn put_profile_requires_auth() {
    let state = test_app().await;
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/profile")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "username": "alice" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// GET /users/{id}/ratings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn user_ratings_lists_all_variants_with_provisional_flag() {
    let state = test_app().await;
    let user = create_user(&state, "0x6666666666666666666666666666666666666666").await;

    // A reliable rating (deviation under the 110 threshold) and a provisional
    // one (deviation above it).
    state
        .storage()
        .ratings()
        .upsert(
            user.id,
            "standard",
            TimeClass::Blitz,
            &Rating {
                value: 1700.0,
                deviation: 80.0,
                volatility: 0.05,
            },
        )
        .await
        .unwrap();
    state
        .storage()
        .ratings()
        .upsert(
            user.id,
            "chess960",
            TimeClass::Blitz,
            &Rating {
                value: 1500.0,
                deviation: 300.0,
                volatility: 0.06,
            },
        )
        .await
        .unwrap();

    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/users/{}/ratings", user.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;

    assert_eq!(body["user_id"].as_str(), Some(user.id.to_string().as_str()));
    let ratings = body["ratings"].as_array().expect("ratings array");
    assert_eq!(ratings.len(), 2);

    // Find each variant entry and check the provisional flag.
    let standard = ratings
        .iter()
        .find(|r| r["variant_id"] == "standard")
        .expect("standard entry");
    assert_eq!(standard["rating"]["value"].as_f64(), Some(1700.0));
    assert_eq!(standard["time_class"].as_str(), Some("blitz"));
    assert_eq!(standard["provisional"].as_bool(), Some(false));

    let chess960 = ratings
        .iter()
        .find(|r| r["variant_id"] == "chess960")
        .expect("chess960 entry");
    assert_eq!(chess960["provisional"].as_bool(), Some(true));
}

#[tokio::test]
async fn user_ratings_empty_for_unrated_user() {
    let state = test_app().await;
    let user = create_user(&state, "0x7777777777777777777777777777777777777777").await;

    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/users/{}/ratings", user.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert!(body["ratings"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn user_ratings_unknown_user_is_404() {
    let state = test_app().await;
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/users/{}/ratings", UserId::new()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// GET /users/{id}/rating-history — populated by a finished rated game
// ---------------------------------------------------------------------------

fn standard_session() -> Box<dyn GameSession> {
    let mut registry = VariantRegistry::new();
    register(&mut registry);
    registry
        .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
        .expect("standard variant is registered")
}

/// Persists an `Active`, rated standard game, spawns its actor with the state's
/// rating-update hook (which records history), and returns the live handle.
async fn start_rated_game(state: &AppState, white: UserId, black: UserId) -> (GameId, GameHandle) {
    let mut game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white,
        black,
        TimeControl::Unlimited,
        true,
        OffsetDateTime::now_utc(),
    );
    game.lifecycle = GameLifecycle::Active;
    let game_id = game.id;
    state
        .game_repo()
        .create(&game)
        .await
        .expect("persist active game");

    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        state.game_repo().clone(),
        state.action_log().clone(),
        state.completion_hook().clone(),
        TimeControl::Unlimited,
    );
    state.game_hub().insert(game_id, handle.clone());
    (game_id, handle)
}

fn resign() -> Action {
    Action::from_typed(&McrAction::Resign).expect("serializable")
}

async fn get_rating_history(
    state: &AppState,
    user: UserId,
    variant: &str,
    time_class: TimeClass,
) -> Value {
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/users/{user}/rating-history?variant={variant}&time_class={}",
                    time_class.as_str()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp.into_body()).await
}

#[tokio::test]
async fn rated_game_appends_two_history_rows_returned_ordered() {
    let state = test_app().await;
    let white = create_user(&state, "0x8888888888888888888888888888888888888888").await;
    let black = create_user(&state, "0x9999999999999999999999999999999999999999").await;

    let (game_id, handle) = start_rated_game(&state, white.id, black.id).await;
    // White resigns: the game finishes and the rating-update hook fires,
    // appending one history row per player.
    handle.submit_action(Color::White, resign()).await.unwrap();
    assert!(handle.status().await.unwrap().is_finished());

    // White's history: exactly one snapshot from this game.
    let white_hist = get_rating_history(
        &state,
        white.id,
        STANDARD_VARIANT_ID,
        TimeClass::Correspondence,
    )
    .await;
    assert_eq!(
        white_hist["user_id"].as_str(),
        Some(white.id.to_string().as_str())
    );
    assert_eq!(white_hist["variant_id"].as_str(), Some(STANDARD_VARIANT_ID));
    assert_eq!(white_hist["time_class"].as_str(), Some("correspondence"));
    let white_entries = white_hist["entries"].as_array().expect("entries");
    assert_eq!(white_entries.len(), 1);
    assert_eq!(
        white_entries[0]["game_id"].as_str(),
        Some(game_id.to_string().as_str())
    );
    // The loser's recorded value fell below the seed.
    assert!(white_entries[0]["value"].as_f64().unwrap() < 1500.0);

    // Black's history: one snapshot, with the winner's value above the seed.
    let black_hist = get_rating_history(
        &state,
        black.id,
        STANDARD_VARIANT_ID,
        TimeClass::Correspondence,
    )
    .await;
    let black_entries = black_hist["entries"].as_array().expect("entries");
    assert_eq!(black_entries.len(), 1);
    assert!(black_entries[0]["value"].as_f64().unwrap() > 1500.0);

    // A second rated game between the same players appends a second snapshot;
    // the listing is most-recent-first.
    let (game2, handle2) = start_rated_game(&state, white.id, black.id).await;
    handle2.submit_action(Color::White, resign()).await.unwrap();
    assert!(handle2.status().await.unwrap().is_finished());

    let white_hist = get_rating_history(
        &state,
        white.id,
        STANDARD_VARIANT_ID,
        TimeClass::Correspondence,
    )
    .await;
    let entries = white_hist["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2);
    // Most-recent-first: the second game leads.
    assert_eq!(
        entries[0]["game_id"].as_str(),
        Some(game2.to_string().as_str())
    );
    assert_eq!(
        entries[1]["game_id"].as_str(),
        Some(game_id.to_string().as_str())
    );

    // The provisional flag on the ratings endpoint reflects the shrinking
    // deviation: after two rated games the deviation is well above 110, so the
    // rating is still provisional.
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/users/{}/ratings", white.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_json(resp.into_body()).await;
    let standard = body["ratings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["variant_id"] == STANDARD_VARIANT_ID)
        .expect("standard rating");
    let deviation = standard["rating"]["deviation"].as_f64().unwrap();
    assert_eq!(standard["provisional"].as_bool(), Some(deviation > 110.0));
}

#[tokio::test]
async fn rating_history_empty_for_unplayed_variant() {
    let state = test_app().await;
    let user = create_user(&state, "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").await;
    let body = get_rating_history(
        &state,
        user.id,
        STANDARD_VARIANT_ID,
        TimeClass::Correspondence,
    )
    .await;
    assert!(body["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn rating_history_unknown_user_is_404() {
    let state = test_app().await;
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/users/{}/rating-history?variant=standard",
                    UserId::new()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
