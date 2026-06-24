//! End-to-end integration tests for the rating subsystem (#41).
//!
//! These drive a real standard-chess game through the live [`GameActor`] over an
//! in-memory SQLite database, exactly as the API does after a seek pairs. When
//! the game finishes, the [`RatingUpdateHook`](mcs_api::RatingUpdateHook) wired
//! into [`AppState`] fires, applies the Glicko-2 update for both players, and
//! persists it. The tests then read the result back through the public HTTP
//! surface (`GET /games/{id}` and `GET /leaderboard`).
//!
//! They cover:
//!
//! - a decisive game: the winner's rating rises and the loser's falls, and both
//!   appear on the leaderboard in the right order;
//! - a drawn game: both ratings move (their deviation shrinks as a game is now
//!   on record) without one side simply beating the other;
//! - a game between unrated players starting from the Glicko-2 seed, and that an
//!   ongoing game produces no rating change (the hook never fires) — neither
//!   path panics.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::{Action, Color, GameSession, VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, GameLifecycle, TimeControl, User, UserId};
use mcs_game::{GameActor, GameHandle};
use mcs_storage::SqlxStorage;
use mcs_variant_standard::wire::StandardAction;
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

/// Builds an [`AppState`] backed by a fresh in-memory SQLite database with the
/// standard variant registered. The same `Arc<SqlxStorage>` backs every
/// repository handle and the rating-update hook, so a game finished through the
/// actor and the ratings read back over HTTP share one database.
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

/// A fresh standard-chess session, built through the registry like the server.
fn standard_session() -> Box<dyn GameSession> {
    let mut registry = VariantRegistry::new();
    register(&mut registry);
    registry
        .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
        .expect("standard variant is registered")
}

/// Persists an `Active` standard game between `white` and `black`, spawns its
/// actor with the state's rating-update hook, registers it in the hub, and
/// returns the live handle.
async fn start_game(state: &AppState, white: UserId, black: UserId) -> (GameId, GameHandle) {
    let mut game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white,
        black,
        TimeControl::Unlimited,
        OffsetDateTime::now_utc(),
    );
    game.lifecycle = GameLifecycle::Active;
    let game_id = game.id;
    state
        .game_repo()
        .create(&game)
        .await
        .expect("persist active game");

    let repo = state.game_repo().clone();
    let action_log = state.action_log().clone();
    let hook = state.completion_hook().clone();
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        action_log,
        hook,
        TimeControl::Unlimited,
    );
    state.game_hub().insert(game_id, handle.clone());
    (game_id, handle)
}

/// A `move` action for the given UCI string.
fn mv(uci: &str) -> Action {
    Action::from_typed(&StandardAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// The `Resign` action.
fn resign() -> Action {
    Action::from_typed(&StandardAction::Resign).expect("serializable")
}

/// An `OfferDraw` action.
fn offer_draw() -> Action {
    Action::from_typed(&StandardAction::OfferDraw).expect("serializable")
}

/// An `AcceptDraw` action.
fn accept_draw() -> Action {
    Action::from_typed(&StandardAction::AcceptDraw).expect("serializable")
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// `GET /games/{id}`, returning the parsed JSON body.
async fn get_game_json(state: &AppState, game_id: GameId) -> Value {
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/games/{game_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp.into_body()).await
}

/// `GET /leaderboard?variant=&limit=`, returning the parsed JSON body.
async fn get_leaderboard(state: &AppState, variant: &str, limit: u32) -> Value {
    let resp = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/leaderboard?variant={variant}&limit={limit}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp.into_body()).await
}

const SEED_RATING: f64 = 1500.0;

// ---------------------------------------------------------------------------
// Decisive game: winner up, loser down, leaderboard ordered.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn decisive_game_moves_both_ratings_and_populates_leaderboard() {
    let state = test_app().await;
    let white = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&state, "0x2222222222222222222222222222222222222222").await;

    let (game_id, handle) = start_game(&state, white.id, black.id).await;

    // White resigns on move one, handing Black the win. `submit_action` returns
    // only after the actor has persisted the finished game and run the hook, so
    // the rating update is durable by the time this awaits.
    handle.submit_action(Color::White, resign()).await.unwrap();
    assert!(handle.status().await.unwrap().is_finished());

    // `GET /games/{id}` now carries both players' updated ratings for the
    // variant.
    let game = get_game_json(&state, game_id).await;
    let white_rating = game["white_rating"]["value"]
        .as_f64()
        .expect("white rating");
    let black_rating = game["black_rating"]["value"]
        .as_f64()
        .expect("black rating");

    // The winner (Black) rose above the seed; the loser (White) fell below it.
    assert!(
        black_rating > SEED_RATING,
        "winner's rating should rise, got {black_rating}"
    );
    assert!(
        white_rating < SEED_RATING,
        "loser's rating should fall, got {white_rating}"
    );

    // The deviation shrank now that each player has a result on record.
    assert!(game["white_rating"]["deviation"].as_f64().unwrap() < 350.0);
    assert!(game["black_rating"]["deviation"].as_f64().unwrap() < 350.0);

    // The leaderboard lists both, highest-rated (the winner) first.
    let board = get_leaderboard(&state, STANDARD_VARIANT_ID, 10).await;
    let entries = board["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2, "both players are now rated: {board}");
    assert_eq!(
        entries[0]["user_id"].as_str().unwrap(),
        black.id.to_string()
    );
    assert_eq!(
        entries[1]["user_id"].as_str().unwrap(),
        white.id.to_string()
    );
    assert!(
        entries[0]["rating"]["value"].as_f64().unwrap()
            > entries[1]["rating"]["value"].as_f64().unwrap()
    );
    // The address was resolved best-effort for the ranked players.
    assert_eq!(
        entries[0]["address"].as_str().unwrap(),
        "0x2222222222222222222222222222222222222222"
    );
}

// ---------------------------------------------------------------------------
// Drawn game: both ratings update, neither side simply beats the other.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drawn_game_updates_both_ratings_symmetrically() {
    let state = test_app().await;
    let white = create_user(&state, "0x3333333333333333333333333333333333333333").await;
    let black = create_user(&state, "0x4444444444444444444444444444444444444444").await;

    let (game_id, handle) = start_game(&state, white.id, black.id).await;

    // White offers a draw, Black accepts: the game ends drawn and the hook fires.
    handle
        .submit_action(Color::White, offer_draw())
        .await
        .unwrap();
    handle
        .submit_action(Color::Black, accept_draw())
        .await
        .unwrap();
    assert!(handle.status().await.unwrap().is_finished());

    let game = get_game_json(&state, game_id).await;
    let white_rating = game["white_rating"]["value"].as_f64().unwrap();
    let black_rating = game["black_rating"]["value"].as_f64().unwrap();

    // Two equally-rated players drawing keeps both essentially at the seed (a
    // draw between equals is the expected result), but the recorded game shrinks
    // each player's deviation below the unrated 350.
    assert!((white_rating - SEED_RATING).abs() < 1.0);
    assert!((black_rating - SEED_RATING).abs() < 1.0);
    assert!(game["white_rating"]["deviation"].as_f64().unwrap() < 350.0);
    assert!(game["black_rating"]["deviation"].as_f64().unwrap() < 350.0);
}

// ---------------------------------------------------------------------------
// Unrated / ongoing paths: no panic, and an ongoing game has no rating change.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ongoing_game_reports_seed_ratings_and_empty_leaderboard() {
    let state = test_app().await;
    let white = create_user(&state, "0x5555555555555555555555555555555555555555").await;
    let black = create_user(&state, "0x6666666666666666666666666666666666666666").await;

    let (game_id, handle) = start_game(&state, white.id, black.id).await;

    // Play one non-finishing move: the completion hook must not fire.
    handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap();
    assert!(!handle.status().await.unwrap().is_finished());

    // Both players are still unrated, so `GET /games/{id}` reports them at the
    // Glicko-2 seed and the leaderboard is empty — no panic on the unrated path.
    let game = get_game_json(&state, game_id).await;
    assert_eq!(game["white_rating"]["value"].as_f64().unwrap(), SEED_RATING);
    assert_eq!(game["black_rating"]["value"].as_f64().unwrap(), SEED_RATING);
    assert_eq!(game["white_rating"]["deviation"].as_f64().unwrap(), 350.0);

    let board = get_leaderboard(&state, STANDARD_VARIANT_ID, 10).await;
    assert!(
        board["entries"].as_array().unwrap().is_empty(),
        "no game has finished, so no ratings exist yet: {board}"
    );
}

#[tokio::test]
async fn leaderboard_for_unknown_variant_is_empty() {
    let state = test_app().await;
    // Querying a variant nobody has played returns an empty list, not an error.
    let board = get_leaderboard(&state, "chess960", 10).await;
    assert_eq!(board["variant"].as_str().unwrap(), "chess960");
    assert!(board["entries"].as_array().unwrap().is_empty());
}
