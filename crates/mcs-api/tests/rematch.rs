//! End-to-end integration tests for the `POST /games/{id}/rematch` endpoint.
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket is bound), backed by an in-memory
//! SQLite database with the standard-chess variant registered.
//!
//! # What is tested
//!
//! - **Happy path** — A and B play a game to a finished result; A offers a
//!   rematch (`POST /games/{id}/rematch`), which creates a `Pending` challenge
//!   to B with `color_preference` opposite to A's side, and with the same
//!   variant/time-control/rated flag as the original. B accepts the challenge;
//!   a new playable game is created with colours swapped. `GET /games/{newid}`
//!   confirms it.
//! - **Authorization** — a third party (neither player) gets **403 Forbidden**.
//! - **Game not finished** — an active game gets **409 Conflict**.
//! - **Unknown game** — **404 Not Found**.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::{Action, Color, VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, GameLifecycle, TimeControl, User, UserId};
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

const ADDR_A: &str = "0x1111111111111111111111111111111111111111";
const ADDR_B: &str = "0x2222222222222222222222222222222222222222";
const ADDR_C: &str = "0x3333333333333333333333333333333333333333";

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

/// Creates an `Active` standard game between `white` and `black`, spawns its
/// actor with the state's rating-update hook, registers it in the hub, and
/// returns both the game id and the live handle.
async fn start_game(
    state: &AppState,
    white: UserId,
    black: UserId,
    rated: bool,
) -> (GameId, GameHandle) {
    let mut game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white,
        black,
        TimeControl::Unlimited,
        rated,
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
        {
            let mut reg = VariantRegistry::new();
            register(&mut reg);
            reg.new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
                .expect("standard variant registered")
        },
        repo,
        action_log,
        hook,
        TimeControl::Unlimited,
    );
    state.game_hub().insert(game_id, handle.clone());
    (game_id, handle)
}

/// Drives the game to a finished state by having White resign.
async fn finish_by_resignation(handle: &GameHandle) {
    let resign = Action::from_typed(&McrAction::Resign).expect("resign action");
    handle
        .submit_action(Color::White, resign)
        .await
        .expect("resign succeeds");
    assert!(
        handle.status().await.unwrap().is_finished(),
        "game must be finished after resignation"
    );
}

/// `POST /games/{id}/rematch` as `token`.
fn post_rematch(game_id: GameId, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/games/{game_id}/rematch"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// `POST /challenges/{id}/accept` as `token`.
fn accept_challenge(challenge_id: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/challenges/{challenge_id}/accept"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Happy path: white offers rematch, black accepts, colours swap.
// ---------------------------------------------------------------------------

/// The full happy path when **White** (player A) offers the rematch.
///
/// - Original game: A = White, B = Black.
/// - A calls `POST /games/{id}/rematch` → a Pending challenge is returned with
///   `challenger = A`, `challenged = B`, `color_preference = black` (A requests
///   black this time — opposite of what they just played).
/// - The variant, time-control, and rated flag are copied from the finished game.
/// - B calls `POST /challenges/{cid}/accept` → a new playable game, colours
///   swapped: B = White, A = Black.
/// - `GET /games/{newid}` returns 200 and `lifecycle = active`.
#[tokio::test]
async fn white_offers_rematch_colours_swap_on_accept() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let r = router(state.clone());

    // 1. Play a rated game; Alice = White, Bob = Black. White resigns → finished.
    let (game_id, handle) = start_game(&state, alice.id, bob.id, true).await;
    finish_by_resignation(&handle).await;

    // 2. Alice (White in the original) offers a rematch.
    let resp = r
        .clone()
        .oneshot(post_rematch(game_id, &alice_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "rematch must succeed");
    let challenge = body_json(resp.into_body()).await;

    let challenge_id = challenge["id"].as_str().expect("challenge id").to_owned();
    assert_eq!(challenge["status"], "pending");
    assert_eq!(
        challenge["challenger"].as_str().unwrap(),
        alice.id.to_string(),
        "Alice is the challenger"
    );
    assert_eq!(
        challenge["challenged"].as_str().unwrap(),
        bob.id.to_string(),
        "Bob is the challenged"
    );
    // Alice played White last time → she requests Black for the rematch.
    assert_eq!(
        challenge["color_preference"], "black",
        "challenger's color_preference must be opposite their previous side"
    );
    // Game terms are copied verbatim.
    assert_eq!(challenge["variant_id"], STANDARD_VARIANT_ID);
    assert_eq!(challenge["rated"], true);

    // 3. Bob accepts the rematch challenge.
    let resp = r
        .clone()
        .oneshot(accept_challenge(&challenge_id, &bob_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "accept must succeed");
    let new_game = body_json(resp.into_body()).await;

    let new_game_id = new_game["id"].as_str().expect("new game id").to_owned();
    assert_eq!(new_game["lifecycle"], "active");

    // Colours are swapped: Alice (challenger, requested Black) plays Black;
    // Bob (challenged) plays White.
    assert_eq!(
        new_game["white"].as_str().unwrap(),
        bob.id.to_string(),
        "Bob must be White in the rematch (colours swapped)"
    );
    assert_eq!(
        new_game["black"].as_str().unwrap(),
        alice.id.to_string(),
        "Alice must be Black in the rematch (colours swapped)"
    );

    // 4. The new game is retrievable.
    let resp = r
        .oneshot(
            Request::builder()
                .uri(format!("/games/{new_game_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "new game must be retrievable"
    );
    let fetched = body_json(resp.into_body()).await;
    assert_eq!(fetched["id"].as_str().unwrap(), new_game_id);
    assert_eq!(fetched["lifecycle"], "active");
}

/// When **Black** (player B) offers the rematch, the color_preference is
/// `white` (opposite of Black), so they become White in the rematch.
#[tokio::test]
async fn black_offers_rematch_colour_preference_is_white() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let r = router(state.clone());

    // Alice = White, Bob = Black.
    let (game_id, handle) = start_game(&state, alice.id, bob.id, false).await;
    finish_by_resignation(&handle).await;

    // Bob (Black in the original) offers the rematch → wants White this time.
    let resp = r
        .clone()
        .oneshot(post_rematch(game_id, &bob_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let challenge = body_json(resp.into_body()).await;

    assert_eq!(
        challenge["challenger"].as_str().unwrap(),
        bob.id.to_string()
    );
    assert_eq!(
        challenge["challenged"].as_str().unwrap(),
        alice.id.to_string()
    );
    // Bob played Black last time → he requests White for the rematch.
    assert_eq!(challenge["color_preference"], "white");
    // The casual flag is preserved.
    assert_eq!(challenge["rated"], false);

    // Alice accepts.
    let challenge_id = challenge["id"].as_str().unwrap().to_owned();
    let resp = r
        .oneshot(accept_challenge(&challenge_id, &alice_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let new_game = body_json(resp.into_body()).await;

    // Bob wanted White; he gets it. Alice (challenged) gets Black.
    assert_eq!(new_game["white"].as_str().unwrap(), bob.id.to_string());
    assert_eq!(new_game["black"].as_str().unwrap(), alice.id.to_string());
}

// ---------------------------------------------------------------------------
// Authorization error: non-player gets 403.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_player_gets_forbidden() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let carol = create_user(&state, ADDR_C).await;
    let carol_token = token_for(&state, &carol);
    let r = router(state.clone());

    let (game_id, handle) = start_game(&state, alice.id, bob.id, true).await;
    finish_by_resignation(&handle).await;

    let resp = r
        .oneshot(post_rematch(game_id, &carol_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Conflict error: game has not yet finished.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn active_game_gets_conflict() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let r = router(state.clone());

    // The game is Active (not Finished).
    let (game_id, _handle) = start_game(&state, alice.id, bob.id, true).await;

    let resp = r
        .oneshot(post_rematch(game_id, &alice_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// Not-found error: unknown game id.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_game_gets_not_found() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let alice_token = token_for(&state, &alice);
    let r = router(state.clone());

    let unknown = GameId::new();
    let resp = r
        .oneshot(post_rematch(unknown, &alice_token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Unauthenticated request gets 401.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_rematch_is_unauthorized() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let r = router(state.clone());

    let (game_id, handle) = start_game(&state, alice.id, bob.id, true).await;
    finish_by_resignation(&handle).await;

    // No authorization header.
    let resp = r
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/games/{game_id}/rematch"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
