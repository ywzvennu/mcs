//! End-to-end integration tests for player statistics (#134).
//!
//! These seed a set of **finished** games for a user directly through the
//! [`GameRepo`](mcs_storage::GameRepo) — as white and as black, across two time
//! classes, with known winners, draws, and an aborted (no-outcome) game — then
//! read the aggregate back over the public HTTP surface
//! (`GET /users/{id}/stats`). Opponent ratings are seeded through the
//! [`RatingRepo`](mcs_storage::RatingRepo) so the performance rating can be
//! checked against a hand-computed value.
//!
//! Coverage:
//!
//! - per-`(variant, time_class)` W/L/D + total, and the `overall` tally;
//! - draws and aborted/no-outcome games handled correctly;
//! - a hand-computed performance rating from seeded opponent ratings;
//! - the `variant`/`time_class` filters narrow the categories;
//! - an unknown user is a 404.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use mcs_api::{router, AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::{Color, EndReason, Outcome, VariantOptions, VariantRegistry};
use mcs_domain::{Game, Rating, TimeClass, TimeControl, User, UserId};
use mcs_storage::SqlxStorage;
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Wiring
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

/// A blitz time control (5+0 → ~300s → blitz).
fn blitz() -> TimeControl {
    TimeControl::RealTime {
        initial: StdDuration::from_secs(300),
        increment: StdDuration::ZERO,
    }
}

/// A rapid time control (10+0 → ~600s → rapid).
fn rapid() -> TimeControl {
    TimeControl::RealTime {
        initial: StdDuration::from_secs(600),
        increment: StdDuration::ZERO,
    }
}

/// Persists a **finished** rated standard game with the given players, time
/// control, and outcome. `secs` orders the games by creation time.
async fn finished_game(
    state: &AppState,
    white: UserId,
    black: UserId,
    tc: TimeControl,
    outcome: Outcome,
    secs: i64,
) {
    let mut game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white,
        black,
        tc,
        true,
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs),
    );
    game.finish(
        outcome,
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs + 1),
    );
    state
        .storage()
        .games()
        .create(&game)
        .await
        .expect("persist finished game");
}

/// Seeds an opponent's current rating in a `(variant, time_class)`.
async fn seed_rating(state: &AppState, user: UserId, time_class: TimeClass, value: f64) {
    state
        .storage()
        .ratings()
        .upsert(
            user,
            STANDARD_VARIANT_ID,
            time_class,
            &Rating {
                value,
                deviation: 100.0,
                volatility: 0.05,
            },
        )
        .await
        .expect("seed rating");
}

async fn body_json(body: Body) -> Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// `GET /users/{id}/stats` with an optional query string, returning (status, body).
async fn get_stats(state: &AppState, id: UserId, query: &str) -> (StatusCode, Value) {
    let uri = if query.is_empty() {
        format!("/users/{id}/stats")
    } else {
        format!("/users/{id}/stats?{query}")
    };
    let resp = router(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp.into_body()).await)
}

/// Finds the category entry for a given time class in a stats body.
fn category<'a>(body: &'a Value, time_class: &str) -> &'a Value {
    body["categories"]
        .as_array()
        .expect("categories array")
        .iter()
        .find(|c| c["time_class"].as_str() == Some(time_class))
        .unwrap_or_else(|| panic!("no {time_class} category in {body}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stats_aggregate_wld_overall_and_performance_rating() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;

    // Distinct opponents so each contributes a known current rating.
    let opp1 = create_user(&state, "0x2222222222222222222222222222222222222222").await;
    let opp2 = create_user(&state, "0x3333333333333333333333333333333333333333").await;
    let opp3 = create_user(&state, "0x4444444444444444444444444444444444444444").await;
    let opp4 = create_user(&state, "0x5555555555555555555555555555555555555555").await;

    // --- Blitz category: alice wins one (as white), loses one (as black) ------
    // Seed each blitz opponent's current rating.
    seed_rating(&state, opp1.id, TimeClass::Blitz, 1600.0).await;
    seed_rating(&state, opp2.id, TimeClass::Blitz, 1400.0).await;

    // Alice (white) beats opp1 (blitz).
    finished_game(
        &state,
        alice.id,
        opp1.id,
        blitz(),
        Outcome::win(Color::White, EndReason::Checkmate),
        0,
    )
    .await;
    // Alice (black) loses to opp2 (white wins) (blitz).
    finished_game(
        &state,
        opp2.id,
        alice.id,
        blitz(),
        Outcome::win(Color::White, EndReason::Resignation),
        1,
    )
    .await;

    // --- Rapid category: alice draws one, wins one ----------------------------
    seed_rating(&state, opp3.id, TimeClass::Rapid, 1500.0).await;
    // opp4 has NO rapid rating → defaults to the Glicko-2 seed (1500).

    // Alice (black) draws opp3 (rapid).
    finished_game(
        &state,
        opp3.id,
        alice.id,
        rapid(),
        Outcome::draw(EndReason::Other("agreement".into())),
        2,
    )
    .await;
    // Alice (white) beats opp4 (rapid); opp4 is unrated → seed 1500.
    finished_game(
        &state,
        alice.id,
        opp4.id,
        rapid(),
        Outcome::win(Color::White, EndReason::Checkmate),
        3,
    )
    .await;

    // --- An aborted game (no outcome) must be skipped entirely ----------------
    let mut aborted = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        alice.id,
        opp1.id,
        blitz(),
        true,
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(4),
    );
    // Mark finished but leave the outcome `None` (aborted / no result recorded).
    aborted.lifecycle = mcs_domain::GameLifecycle::Finished;
    aborted.updated_at = OffsetDateTime::UNIX_EPOCH + Duration::seconds(5);
    state.storage().games().create(&aborted).await.unwrap();

    let (status, body) = get_stats(&state, alice.id, "").await;
    assert_eq!(status, StatusCode::OK);

    // Overall: 2 wins, 1 loss, 1 draw, 4 total (the aborted game is excluded).
    assert_eq!(body["overall"]["wins"].as_u64(), Some(2));
    assert_eq!(body["overall"]["losses"].as_u64(), Some(1));
    assert_eq!(body["overall"]["draws"].as_u64(), Some(1));
    assert_eq!(body["overall"]["total"].as_u64(), Some(4));

    // Blitz category: 1 win, 1 loss, 0 draws, 2 total.
    let blitz_cat = category(&body, "blitz");
    assert_eq!(blitz_cat["wins"].as_u64(), Some(1));
    assert_eq!(blitz_cat["losses"].as_u64(), Some(1));
    assert_eq!(blitz_cat["draws"].as_u64(), Some(0));
    assert_eq!(blitz_cat["total"].as_u64(), Some(2));
    // Performance rating: avg(1600, 1400) + 400*(1-1)/2 = 1500 + 0 = 1500.
    assert_eq!(blitz_cat["performance_rating"].as_i64(), Some(1500));

    // Rapid category: 1 win, 0 losses, 1 draw, 2 total.
    let rapid_cat = category(&body, "rapid");
    assert_eq!(rapid_cat["wins"].as_u64(), Some(1));
    assert_eq!(rapid_cat["losses"].as_u64(), Some(0));
    assert_eq!(rapid_cat["draws"].as_u64(), Some(1));
    assert_eq!(rapid_cat["total"].as_u64(), Some(2));
    // Performance rating: avg(1500, 1500[seed]) + 400*(1-0)/2 = 1500 + 200 = 1700.
    assert_eq!(rapid_cat["performance_rating"].as_i64(), Some(1700));

    // Exactly two categories.
    assert_eq!(body["categories"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn stats_variant_and_time_class_filters_narrow_categories() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let opp = create_user(&state, "0x2222222222222222222222222222222222222222").await;

    // One blitz game and one rapid game.
    finished_game(
        &state,
        alice.id,
        opp.id,
        blitz(),
        Outcome::win(Color::White, EndReason::Checkmate),
        0,
    )
    .await;
    finished_game(
        &state,
        alice.id,
        opp.id,
        rapid(),
        Outcome::win(Color::White, EndReason::Checkmate),
        1,
    )
    .await;

    // time_class filter narrows to the blitz category only; overall still spans both.
    let (status, body) = get_stats(&state, alice.id, "time_class=blitz").await;
    assert_eq!(status, StatusCode::OK);
    let cats = body["categories"].as_array().unwrap();
    assert_eq!(cats.len(), 1);
    assert_eq!(cats[0]["time_class"].as_str(), Some("blitz"));
    assert_eq!(body["overall"]["total"].as_u64(), Some(2));

    // variant filter matching → both categories; non-matching → none.
    let (_, body) = get_stats(&state, alice.id, &format!("variant={STANDARD_VARIANT_ID}")).await;
    assert_eq!(body["categories"].as_array().unwrap().len(), 2);

    let (_, body) = get_stats(&state, alice.id, "variant=chess960").await;
    assert!(body["categories"].as_array().unwrap().is_empty());
    // The overall tally ignores the filter.
    assert_eq!(body["overall"]["total"].as_u64(), Some(2));

    // Combined variant + time_class filter.
    let (_, body) = get_stats(
        &state,
        alice.id,
        &format!("variant={STANDARD_VARIANT_ID}&time_class=rapid"),
    )
    .await;
    let cats = body["categories"].as_array().unwrap();
    assert_eq!(cats.len(), 1);
    assert_eq!(cats[0]["time_class"].as_str(), Some("rapid"));
}

#[tokio::test]
async fn stats_unknown_time_class_is_422() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let (status, _) = get_stats(&state, alice.id, "time_class=hyperbullet").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn stats_unknown_user_is_404() {
    let state = test_app().await;
    let (status, _) = get_stats(&state, UserId::new(), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stats_user_with_no_finished_games_is_empty() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let (status, body) = get_stats(&state, alice.id, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["overall"]["total"].as_u64(), Some(0));
    assert!(body["categories"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn stats_casual_games_have_no_performance_rating() {
    let state = test_app().await;
    let alice = create_user(&state, "0x1111111111111111111111111111111111111111").await;
    let opp = create_user(&state, "0x2222222222222222222222222222222222222222").await;

    // A single CASUAL finished win: it counts towards W/L/D but, having no rated
    // games, the category's performance rating is omitted.
    let mut game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        alice.id,
        opp.id,
        blitz(),
        false, // casual
        OffsetDateTime::UNIX_EPOCH,
    );
    game.finish(
        Outcome::win(Color::White, EndReason::Checkmate),
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(1),
    );
    state.storage().games().create(&game).await.unwrap();

    let (status, body) = get_stats(&state, alice.id, "").await;
    assert_eq!(status, StatusCode::OK);
    let blitz_cat = category(&body, "blitz");
    assert_eq!(blitz_cat["wins"].as_u64(), Some(1));
    assert_eq!(blitz_cat["total"].as_u64(), Some(1));
    // No rated games → performance_rating omitted (serialized as absent / null).
    assert!(blitz_cat["performance_rating"].is_null());
}
