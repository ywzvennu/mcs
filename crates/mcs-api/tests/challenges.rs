//! End-to-end integration tests for the direct-challenge endpoints.
//!
//! These drive the real [`axum::Router`] in-process via
//! [`tower::ServiceExt::oneshot`] (no socket is bound), backed by an in-memory
//! SQLite database with the standard-chess variant registered. They exercise the
//! full invite-to-game path: A challenges B, B sees it incoming, B accepts, and
//! the resulting game is retrievable, live in the hub, and playable — plus the
//! authorization, status, and validation rules around each transition.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::{issue_session, SessionConfig};
use mcs_core::VariantRegistry;
use mcs_domain::{ChallengeStatus, User};
use mcs_storage::SqlxStorage;
use mcs_variant_mcr::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

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

const ADDR_A: &str = "0x1111111111111111111111111111111111111111";
const ADDR_B: &str = "0x2222222222222222222222222222222222222222";
const ADDR_C: &str = "0x3333333333333333333333333333333333333333";

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

fn token_for(state: &AppState, user: &User) -> String {
    issue_session(state.session_config(), user.id)
        .expect("mint token")
        .token
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// `POST /challenges` from `token` against `opponent_address`.
fn post_challenge(token: &str, opponent_address: &str, color: &str, rated: bool) -> Request<Body> {
    let body = json!({
        "opponent_address": opponent_address,
        "variant_id": STANDARD_VARIANT_ID,
        "time_control": { "type": "real_time", "initial_secs": 300, "increment_secs": 2 },
        "rated": rated,
        "color": color,
    });
    Request::builder()
        .method("POST")
        .uri("/challenges")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_challenges(token: &str) -> Request<Body> {
    Request::builder()
        .uri("/challenges")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn transition(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Happy path: A challenges B, B accepts, game is created and playable.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn challenge_accept_creates_a_playable_game_with_correct_colours() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state.clone());

    // 1. Alice challenges Bob, wanting White, casual.
    let resp = router
        .clone()
        .oneshot(post_challenge(&alice_token, ADDR_B, "white", false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    let challenge_id = body["id"].as_str().expect("challenge id").to_owned();
    assert_eq!(body["status"], "pending");
    assert_eq!(body["challenger"].as_str().unwrap(), alice.id.to_string());
    assert_eq!(body["challenged"].as_str().unwrap(), bob.id.to_string());
    assert_eq!(body["rated"], false);

    // 2. Bob's listing shows it incoming; Alice's shows it outgoing.
    let bob_list = body_json(
        router
            .clone()
            .oneshot(get_challenges(&bob_token))
            .await
            .unwrap()
            .into_body(),
    )
    .await;
    assert_eq!(bob_list["incoming"].as_array().unwrap().len(), 1);
    assert!(bob_list["outgoing"].as_array().unwrap().is_empty());
    assert_eq!(
        bob_list["incoming"][0]["id"].as_str().unwrap(),
        challenge_id
    );

    let alice_list = body_json(
        router
            .clone()
            .oneshot(get_challenges(&alice_token))
            .await
            .unwrap()
            .into_body(),
    )
    .await;
    assert_eq!(alice_list["outgoing"].as_array().unwrap().len(), 1);
    assert!(alice_list["incoming"].as_array().unwrap().is_empty());

    // 3. Bob accepts → a game is created.
    let resp = router
        .clone()
        .oneshot(transition(
            "POST",
            &format!("/challenges/{challenge_id}/accept"),
            &bob_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let game = body_json(resp.into_body()).await;
    let game_id = game["id"].as_str().expect("game id").to_owned();
    assert_eq!(game["lifecycle"], "active");
    // Colours per Alice's preference: Alice White, Bob Black.
    assert_eq!(game["white"].as_str().unwrap(), alice.id.to_string());
    assert_eq!(game["black"].as_str().unwrap(), bob.id.to_string());
    // The casual flag carried through to the game.
    assert_eq!(game["rated"], false);

    // 4. The game is live in the hub and retrievable.
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

    // 5. The challenge is now Accepted with the game id set, and drops out of
    //    the pending listings.
    let fetched = state
        .storage()
        .challenges()
        .get(challenge_id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(fetched.status, ChallengeStatus::Accepted);
    assert_eq!(fetched.game_id, Some(parsed));

    let bob_list = body_json(
        router
            .oneshot(get_challenges(&bob_token))
            .await
            .unwrap()
            .into_body(),
    )
    .await;
    assert!(bob_list["incoming"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn challenger_black_preference_assigns_colours_to_the_opponent() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state.clone());

    let body = body_json(
        router
            .clone()
            .oneshot(post_challenge(&alice_token, ADDR_B, "black", true))
            .await
            .unwrap()
            .into_body(),
    )
    .await;
    let challenge_id = body["id"].as_str().unwrap().to_owned();

    let game = body_json(
        router
            .oneshot(transition(
                "POST",
                &format!("/challenges/{challenge_id}/accept"),
                &bob_token,
            ))
            .await
            .unwrap()
            .into_body(),
    )
    .await;
    // Alice wanted Black, so Bob (challenged) gets White.
    assert_eq!(game["white"].as_str().unwrap(), bob.id.to_string());
    assert_eq!(game["black"].as_str().unwrap(), alice.id.to_string());
    assert_eq!(game["rated"], true);
}

// ---------------------------------------------------------------------------
// Opponent resolution and self-challenge.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn challenging_an_unknown_address_creates_the_opponent_account() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let alice_token = token_for(&state, &alice);
    let router = router(state.clone());

    // ADDR_B has no account yet; the challenge must still succeed.
    let resp = router
        .oneshot(post_challenge(&alice_token, ADDR_B, "random", true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The opponent account now exists.
    let opponent = state
        .storage()
        .users()
        .find_by_address(&ADDR_B.parse().unwrap())
        .await
        .unwrap();
    assert!(opponent.is_some(), "opponent account was created");
}

#[tokio::test]
async fn self_challenge_is_bad_request() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let alice_token = token_for(&state, &alice);
    let router = router(state);

    let resp = router
        .oneshot(post_challenge(&alice_token, ADDR_A, "white", true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Authorization.
// ---------------------------------------------------------------------------

/// Posts a pending challenge from A to B and returns its id.
async fn pending_challenge(router: &axum::Router, alice_token: &str) -> String {
    let body = body_json(
        router
            .clone()
            .oneshot(post_challenge(alice_token, ADDR_B, "white", true))
            .await
            .unwrap()
            .into_body(),
    )
    .await;
    body["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn challenger_cannot_accept_or_decline_their_own_challenge() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let router = router(state);

    let id = pending_challenge(&router, &alice_token).await;

    for verb in ["accept", "decline"] {
        let resp = router
            .clone()
            .oneshot(transition(
                "POST",
                &format!("/challenges/{id}/{verb}"),
                &alice_token,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "challenger must not {verb} their own challenge"
        );
    }
}

#[tokio::test]
async fn third_party_cannot_accept_a_challenge() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    create_user(&state, ADDR_B).await;
    let carol = create_user(&state, ADDR_C).await;
    let alice_token = token_for(&state, &alice);
    let carol_token = token_for(&state, &carol);
    let router = router(state);

    let id = pending_challenge(&router, &alice_token).await;

    let resp = router
        .oneshot(transition(
            "POST",
            &format!("/challenges/{id}/accept"),
            &carol_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn non_challenger_cannot_cancel_a_challenge() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state);

    let id = pending_challenge(&router, &alice_token).await;

    // Bob (the challenged party) may not cancel — only the challenger may.
    let resp = router
        .oneshot(transition(
            "DELETE",
            &format!("/challenges/{id}"),
            &bob_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Status guards.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accepting_a_non_pending_challenge_is_conflict() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state);

    let id = pending_challenge(&router, &alice_token).await;

    // Bob accepts once.
    let resp = router
        .clone()
        .oneshot(transition(
            "POST",
            &format!("/challenges/{id}/accept"),
            &bob_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A second accept (already Accepted) is a conflict; so is declining it.
    for verb in ["accept", "decline"] {
        let resp = router
            .clone()
            .oneshot(transition(
                "POST",
                &format!("/challenges/{id}/{verb}"),
                &bob_token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{verb} after accept");
    }
}

#[tokio::test]
async fn cancelling_then_accepting_is_conflict() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state);

    let id = pending_challenge(&router, &alice_token).await;

    // Alice cancels her pending challenge.
    let resp = router
        .clone()
        .oneshot(transition(
            "DELETE",
            &format!("/challenges/{id}"),
            &alice_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Bob can no longer accept a canceled challenge.
    let resp = router
        .oneshot(transition(
            "POST",
            &format!("/challenges/{id}/accept"),
            &bob_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn decline_sets_status_and_only_challenged_may_decline() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let bob = create_user(&state, ADDR_B).await;
    let alice_token = token_for(&state, &alice);
    let bob_token = token_for(&state, &bob);
    let router = router(state.clone());

    let id = pending_challenge(&router, &alice_token).await;

    let resp = router
        .oneshot(transition(
            "POST",
            &format!("/challenges/{id}/decline"),
            &bob_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp.into_body()).await;
    assert_eq!(body["status"], "declined");

    let fetched = state
        .storage()
        .challenges()
        .get(id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(fetched.status, ChallengeStatus::Declined);
}

// ---------------------------------------------------------------------------
// Not found / auth.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_challenge_is_not_found() {
    let state = test_app().await;
    let alice = create_user(&state, ADDR_A).await;
    let alice_token = token_for(&state, &alice);
    let router = router(state);

    let unknown = mcs_domain::ChallengeId::new();
    let resp = router
        .oneshot(transition(
            "POST",
            &format!("/challenges/{unknown}/accept"),
            &alice_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unauthenticated_post_challenge_is_unauthorized() {
    let state = test_app().await;
    let router = router(state);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/challenges")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "opponent_address": ADDR_B,
                        "variant_id": STANDARD_VARIANT_ID,
                        "time_control": { "type": "real_time", "initial_secs": 300, "increment_secs": 2 },
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
