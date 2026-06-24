//! Integration tests for `GET /games/{id}/moves` and `GET /games/{id}/pgn`.
//!
//! The tests use the same in-process test harness as the other REST tests:
//! the real [`axum::Router`] is driven via [`tower::ServiceExt::oneshot`]
//! (no socket is bound), backed by an in-memory SQLite database with the
//! standard-chess variant registered.
//!
//! # RBC test
//!
//! The RBC variant is also registered so we can verify that `GET /pgn` returns
//! 409 Conflict for non-board variants while `GET /moves` still works.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::OffsetDateTime;
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::{Color, VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, TimeControl, User};
use mcs_game::GameActor;
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
use mcs_variant_standard::{register as register_standard, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

struct TestApp {
    state: AppState,
    storage: Arc<SqlxStorage>,
}

/// Builds a [`TestApp`] with both standard and RBC variants registered.
async fn test_app() -> TestApp {
    let storage = Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect + migrate in-memory sqlite"),
    );

    let mut registry = VariantRegistry::new();
    register_standard(&mut registry);
    mcs_variant_rbc::register(&mut registry);
    let variants = Arc::new(registry);

    let session = SessionConfig::new(
        b"test-secret-key-that-is-definitely-32-bytes!!".to_vec(),
        time::Duration::hours(1),
        "mcs-test".to_owned(),
    );
    let siwe = SiweConfig::new(
        "localhost".to_owned(),
        "https://localhost".to_owned(),
        1,
        "Sign in to MCS.".to_owned(),
        time::Duration::minutes(10),
    );

    TestApp {
        state: AppState::new(storage.clone(), variants, session, siwe),
        storage,
    }
}

/// Persists a fresh user with the given address and returns it.
async fn create_user(app: &TestApp, address: &str) -> User {
    let user = User::new(
        address.parse().expect("valid evm address"),
        None,
        OffsetDateTime::now_utc(),
    );
    app.state
        .storage()
        .users()
        .create(&user)
        .await
        .expect("create user");
    user
}

/// Mints a session token for `user`.
fn token_for(app: &TestApp, user: &User) -> String {
    issue_session(app.state.session_config(), user.id).expect("mint token")
}

/// Creates a game for the given variant, persists the record, spawns the
/// actor, and registers it in the hub. Returns the game id.
async fn spawn_game(app: &TestApp, white: &User, black: &User, variant_id: &str) -> GameId {
    let mut registry = VariantRegistry::new();
    register_standard(&mut registry);
    mcs_variant_rbc::register(&mut registry);
    let session = registry
        .new_game(variant_id, &VariantOptions::default())
        .expect("variant registered");

    let time_control = TimeControl::RealTime {
        initial: Duration::from_secs(300),
        increment: Duration::from_secs(2),
    };
    let game = Game::new(
        variant_id.to_owned(),
        VariantOptions::default(),
        white.id,
        black.id,
        time_control.clone(),
        OffsetDateTime::now_utc(),
    );
    let game_id = game.id;
    app.state
        .storage()
        .games()
        .create(&game)
        .await
        .expect("persist game record");

    let repo: Arc<dyn GameRepo> = app.storage.clone();
    let action_log: Arc<dyn ActionLogRepo> = app.storage.clone();
    let hook = app.state.completion_hook().clone();
    let handle = GameActor::spawn(game_id, session, repo, action_log, hook, time_control);
    app.state.game_hub().insert(game_id, handle);

    game_id
}

/// Creates a standard-chess game and returns its id.
async fn spawn_standard_game(app: &TestApp, white: &User, black: &User) -> GameId {
    spawn_game(app, white, black, STANDARD_VARIANT_ID).await
}

/// Submits a move directly to the game actor via its handle.
///
/// `color` is the player submitting the action (White or Black).
async fn submit_action(app: &TestApp, game_id: GameId, color: Color, action: serde_json::Value) {
    let handle = app
        .state
        .game_hub()
        .get(game_id)
        .expect("game registered in hub");
    handle
        .submit_action(color, mcs_core::Action::new(action))
        .await
        .expect("submit action");
}

/// Reads the response body as JSON.
async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Reads the response body as text.
async fn body_text(body: Body) -> String {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Posts a standard seek via the HTTP router.
fn post_seek(token: &str, color: &str) -> Request<Body> {
    let body = serde_json::json!({
        "variant_id": STANDARD_VARIANT_ID,
        "time_control": { "type": "real_time", "initial_secs": 300, "increment_secs": 2 },
        "color_preference": color,
    })
    .to_string();
    Request::builder()
        .method("POST")
        .uri("/seeks")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body))
        .unwrap()
}

/// Posts two compatible seeks and returns the newly created game id.
async fn pair_into_game(app: &TestApp, white: &User, black: &User) -> GameId {
    let white_token = token_for(app, white);
    let black_token = token_for(app, black);
    let r = router(app.state.clone());

    let resp = r
        .clone()
        .oneshot(post_seek(&white_token, "white"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = r
        .clone()
        .oneshot(post_seek(&black_token, "black"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["status"], "paired");
    body["game"]["id"].as_str().unwrap().parse().unwrap()
}

// ---------------------------------------------------------------------------
// GET /games/{id}/moves — happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn moves_returns_empty_for_new_game() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = pair_into_game(&app, &white, &black).await;

    let r = router(app.state.clone());
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/moves"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["game_id"].as_str().unwrap(), game_id.to_string());
    assert_eq!(body["moves"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn moves_returns_actions_in_ply_order() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;

    let game_id = spawn_standard_game(&app, &white, &black).await;

    // Play two moves: 1. e4 (White), then 1... e5 (Black).
    submit_action(
        &app,
        game_id,
        Color::White,
        serde_json::json!({ "type": "move", "uci": "e2e4" }),
    )
    .await;
    submit_action(
        &app,
        game_id,
        Color::Black,
        serde_json::json!({ "type": "move", "uci": "e7e5" }),
    )
    .await;

    // Give the actor a moment to flush the action log.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r = router(app.state.clone());
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/moves"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;

    let moves = body["moves"].as_array().expect("moves array");
    assert_eq!(moves.len(), 2, "two moves were played; got {body}");

    // Ply 0 = White's 1. e4, ply 1 = Black's 1... e5.
    assert_eq!(moves[0]["ply"].as_u64().unwrap(), 0);
    assert_eq!(moves[0]["player"].as_str().unwrap(), "white");
    assert_eq!(moves[0]["action"]["type"].as_str().unwrap(), "move");
    assert_eq!(moves[0]["action"]["uci"].as_str().unwrap(), "e2e4");

    assert_eq!(moves[1]["ply"].as_u64().unwrap(), 1);
    assert_eq!(moves[1]["player"].as_str().unwrap(), "black");
    assert_eq!(moves[1]["action"]["type"].as_str().unwrap(), "move");
    assert_eq!(moves[1]["action"]["uci"].as_str().unwrap(), "e7e5");

    // created_at must be an RFC 3339 timestamp.
    assert!(moves[0]["created_at"].as_str().is_some());
    assert!(moves[1]["created_at"].as_str().is_some());
}

#[tokio::test]
async fn moves_records_correct_player_colors() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_standard_game(&app, &white, &black).await;

    submit_action(
        &app,
        game_id,
        Color::White,
        serde_json::json!({ "type": "move", "uci": "d2d4" }),
    )
    .await;
    submit_action(
        &app,
        game_id,
        Color::Black,
        serde_json::json!({ "type": "move", "uci": "d7d5" }),
    )
    .await;
    submit_action(
        &app,
        game_id,
        Color::White,
        serde_json::json!({ "type": "move", "uci": "c2c4" }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r = router(app.state.clone());
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/moves"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    let moves = body["moves"].as_array().unwrap();
    assert_eq!(moves.len(), 3);

    assert_eq!(moves[0]["player"].as_str().unwrap(), "white");
    assert_eq!(moves[1]["player"].as_str().unwrap(), "black");
    assert_eq!(moves[2]["player"].as_str().unwrap(), "white");
}

// ---------------------------------------------------------------------------
// GET /games/{id}/moves — 404 for unknown game
// ---------------------------------------------------------------------------

#[tokio::test]
async fn moves_returns_404_for_unknown_game() {
    let app = test_app().await;
    let unknown = GameId::new();
    let r = router(app.state.clone());

    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{unknown}/moves"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// GET /games/{id}/pgn — happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pgn_for_game_with_no_moves() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = pair_into_game(&app, &white, &black).await;

    let r = router(app.state.clone());
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/pgn"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("content-type header")
        .to_str()
        .unwrap();
    assert!(content_type.contains("text/plain"), "got {content_type}");

    let pgn = body_text(resp.into_body()).await;
    // Seven-tag roster must be present.
    assert!(pgn.contains("[Event \"MCS game\"]"), "got:\n{pgn}");
    assert!(pgn.contains("[Site \"mcs\"]"), "got:\n{pgn}");
    assert!(pgn.contains("[Variant \"standard\"]"), "got:\n{pgn}");
    assert!(pgn.contains("[Result \"*\"]"), "got:\n{pgn}");
    // No moves — just the result token "*".
    assert!(pgn.contains("*"), "result token: {pgn}");
}

#[tokio::test]
async fn pgn_contains_correct_movetext_and_tags() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_standard_game(&app, &white, &black).await;

    // Play the Ruy Lopez opening (three moves for White, two for Black).
    submit_action(
        &app,
        game_id,
        Color::White,
        serde_json::json!({ "type": "move", "uci": "e2e4" }),
    )
    .await;
    submit_action(
        &app,
        game_id,
        Color::Black,
        serde_json::json!({ "type": "move", "uci": "e7e5" }),
    )
    .await;
    submit_action(
        &app,
        game_id,
        Color::White,
        serde_json::json!({ "type": "move", "uci": "g1f3" }),
    )
    .await;
    submit_action(
        &app,
        game_id,
        Color::Black,
        serde_json::json!({ "type": "move", "uci": "b8c6" }),
    )
    .await;
    submit_action(
        &app,
        game_id,
        Color::White,
        serde_json::json!({ "type": "move", "uci": "f1b5" }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r = router(app.state.clone());
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/pgn"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let pgn = body_text(resp.into_body()).await;

    // Tags.
    assert!(pgn.contains("[Event \"MCS game\"]"), "event tag: {pgn}");
    assert!(pgn.contains("[Variant \"standard\"]"), "variant tag: {pgn}");
    assert!(
        pgn.contains(&format!("[White \"{}\"]", white.id)),
        "white tag: {pgn}"
    );
    assert!(
        pgn.contains(&format!("[Black \"{}\"]", black.id)),
        "black tag: {pgn}"
    );

    // Movetext: 1. e2e4 e7e5 2. g1f3 b8c6 3. f1b5 *
    assert!(pgn.contains("1. e2e4 e7e5"), "move 1: {pgn}");
    assert!(pgn.contains("2. g1f3 b8c6"), "move 2: {pgn}");
    assert!(pgn.contains("3. f1b5"), "move 3: {pgn}");
    // Unfinished game → result token "*".
    assert!(pgn.contains("*"), "result token: {pgn}");
}

#[tokio::test]
async fn pgn_date_tag_is_correctly_formatted() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = pair_into_game(&app, &white, &black).await;

    let r = router(app.state.clone());
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/pgn"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let pgn = body_text(resp.into_body()).await;

    // Date tag must look like [Date "YYYY.MM.DD"]
    // Verify the format manually without a regex dependency.
    let date_line = pgn
        .lines()
        .find(|l| l.starts_with("[Date "))
        .expect("Date tag present");
    // e.g. [Date "2026.06.24"]
    assert!(date_line.len() >= 14, "Date tag too short: {date_line}");
    let inner = date_line
        .trim_start_matches("[Date \"")
        .trim_end_matches("\"]");
    let parts: Vec<&str> = inner.split('.').collect();
    assert_eq!(parts.len(), 3, "Date must be YYYY.MM.DD; got {inner}");
    assert_eq!(parts[0].len(), 4, "year must be 4 digits; got {inner}");
    assert_eq!(parts[1].len(), 2, "month must be 2 digits; got {inner}");
    assert_eq!(parts[2].len(), 2, "day must be 2 digits; got {inner}");
}

// ---------------------------------------------------------------------------
// GET /games/{id}/pgn — 404 for unknown game
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pgn_returns_404_for_unknown_game() {
    let app = test_app().await;
    let unknown = GameId::new();
    let r = router(app.state.clone());

    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{unknown}/pgn"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// GET /games/{id}/pgn — 409 for non-board variant (RBC)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pgn_returns_409_for_rbc_game() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;

    // Spawn an RBC game — RBC turns start with a mandatory sense.
    let game_id = spawn_game(&app, &white, &black, "rbc").await;

    // Submit a sense as White (first required action in an RBC turn).
    submit_action(
        &app,
        game_id,
        Color::White,
        serde_json::json!({ "type": "sense", "square": "e4" }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // /moves must still work: returns the sense action.
    let r = router(app.state.clone());
    let resp = r
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/moves"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "/moves must work for RBC");
    let body = body_json(resp.into_body()).await;
    let moves = body["moves"].as_array().unwrap();
    assert!(!moves.is_empty(), "sense action must be recorded");
    assert_eq!(moves[0]["action"]["type"].as_str().unwrap(), "sense");

    // /pgn must return 409 Conflict.
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}/pgn"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "/pgn must return 409 for RBC"
    );

    // The error body should explain why PGN is unavailable.
    let body = body_json(resp.into_body()).await;
    let detail = body["detail"].as_str().expect("detail field");
    assert!(
        detail.contains("rbc") || detail.contains("sense"),
        "error detail should mention the variant or action type; got: {detail}"
    );
}
