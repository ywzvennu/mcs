//! Load / concurrency stress test for the game machinery (#110).
//!
//! This is an **`#[ignore]`d, run-on-demand** test (it spins up hundreds of
//! games and is heavier than a unit test should be in the default suite). It
//! drives three pieces of the live-game stack under heavy concurrency and
//! asserts their invariants hold — no double-booking, no actor inconsistency, no
//! deadlock, and the in-memory caps behaving — so we have production-confidence
//! that the actor/matchmaker/clock paths are sound under contention.
//!
//! Run it explicitly with:
//!
//! ```text
//! cargo test -p mcs-api --test loadtest -- --ignored loadtest
//! ```
//!
//! (or `cargo test -- --ignored loadtest` from the workspace root). It is left
//! out of the default `cargo test` run by `#[ignore]`, so CI stays fast.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::task::JoinSet;

use mcs_api::{AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::{Action, Color, VariantOptions};
use mcs_domain::{ColorPreference, Seek, TimeControl, User, UserId};
use mcs_game::SubmitOutcome;
use mcs_storage::{ActionLogRepo, SqlxStorage};
use mcs_variant_mcr::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/// Builds an [`AppState`] over a fresh in-memory SQLite database with the
/// standard variant registered.
async fn test_app() -> (AppState, Arc<SqlxStorage>) {
    let storage = Arc::new(
        SqlxStorage::connect("sqlite::memory:")
            .await
            .expect("connect + migrate in-memory sqlite"),
    );
    let mut registry = mcs_core::VariantRegistry::new();
    register(&mut registry);
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
    let state = AppState::new(storage.clone(), Arc::new(registry), session, siwe);
    (state, storage)
}

/// Persists a fresh user with a deterministic-but-unique EVM address derived
/// from `n`, returning it.
async fn create_user(state: &AppState, n: usize) -> User {
    let address = format!("0x{:040x}", n + 1);
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

/// A UCI move action for the standard variant.
fn uci(mv: &str) -> Action {
    serde_json::from_value(serde_json::json!({ "type": "move", "uci": mv }))
        .expect("valid move action")
}

/// A short, fully-legal standard-chess opening line, alternating White/Black.
/// Eight plies is enough to exercise repeated actor round-trips per game while
/// keeping the test bounded.
const OPENING: &[(Color, &str)] = &[
    (Color::White, "e2e4"),
    (Color::Black, "e7e5"),
    (Color::White, "g1f3"),
    (Color::Black, "b8c6"),
    (Color::White, "f1c4"),
    (Color::Black, "f8c5"),
    (Color::White, "e1g1"), // White castles kingside.
    (Color::Black, "g8f6"),
];

fn blitz() -> TimeControl {
    TimeControl::RealTime {
        initial: Duration::from_secs(300),
        increment: Duration::from_secs(2),
    }
}

// ---------------------------------------------------------------------------
// Many concurrent games, each played through its actor.
// ---------------------------------------------------------------------------

/// Spawns hundreds of independent games and plays a legal opening line on each
/// concurrently, then asserts every game's actor stayed consistent and the
/// in-memory caps tracked correctly. Named `loadtest_*` so `--ignored loadtest`
/// selects it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load test; run explicitly with --ignored loadtest"]
async fn loadtest_many_concurrent_games_stay_consistent() {
    let (state, storage) = test_app().await;

    const GAMES: usize = 300;

    // Distinct user pair per game, so each user holds exactly one live game and
    // the per-user live-game cap (default 50) is never the limiting factor.
    let mut game_ids = Vec::with_capacity(GAMES);
    let mut players = Vec::with_capacity(GAMES);
    for i in 0..GAMES {
        let white = create_user(&state, i * 2).await;
        let black = create_user(&state, i * 2 + 1).await;
        let game = state
            .create_and_spawn_game(
                white.id,
                black.id,
                STANDARD_VARIANT_ID,
                blitz(),
                false,
                VariantOptions::default(),
            )
            .await
            .expect("create and spawn game");
        game_ids.push(game.id);
        players.push((white.id, black.id));
    }

    assert_eq!(
        state.game_hub().len(),
        GAMES,
        "every created game is live in the hub"
    );

    // Play the opening line on all games concurrently. Each task owns one game's
    // handle and submits its plies in order; across games the tasks race freely.
    let mut set = JoinSet::new();
    for &game_id in &game_ids {
        let handle = state.game_hub().get(game_id).expect("live handle");
        set.spawn(async move {
            for (color, mv) in OPENING {
                handle
                    .submit_action(*color, uci(mv))
                    .await
                    .expect("legal opening move applies");
            }
            // Read the snapshot back so we exercise the query path under load.
            let snapshot = handle.snapshot().await.expect("snapshot");
            (game_id, snapshot.ply)
        });
    }

    // Every task must complete (no deadlock, no panic) within a generous bound.
    let mut completed = 0usize;
    while let Some(res) = set.join_next().await {
        let (game_id, ply) = res.expect("a game task panicked or was cancelled");
        assert_eq!(
            ply as usize,
            OPENING.len(),
            "game {game_id} advanced to exactly the number of plies submitted"
        );
        completed += 1;
    }
    assert_eq!(completed, GAMES, "all game tasks completed");

    // Durable consistency: each game's action log holds exactly the plies we
    // submitted, in order — the actor never dropped, duplicated, or reordered a
    // move under concurrency.
    for &game_id in &game_ids {
        let log = storage.list(game_id).await.expect("read action log");
        assert_eq!(
            log.len(),
            OPENING.len(),
            "game {game_id} recorded every submitted ply exactly once"
        );
        for (ply, recorded) in log.iter().enumerate() {
            assert_eq!(recorded.ply as usize, ply, "plies are dense and in order");
            assert_eq!(
                recorded.player, OPENING[ply].0,
                "the recorded mover matches the submitted color"
            );
        }
    }

    // In-memory cap bookkeeping: every player has exactly one live game counted,
    // and no player was double-booked.
    let mut seen = HashSet::new();
    for &(white, black) in &players {
        assert_eq!(state.live_games().count(white), 1, "white counts one game");
        assert_eq!(state.live_games().count(black), 1, "black counts one game");
        assert!(seen.insert(white), "no user reused across games (white)");
        assert!(seen.insert(black), "no user reused across games (black)");
    }
}

// ---------------------------------------------------------------------------
// Heavy concurrent matchmaker submits: no user double-booked.
// ---------------------------------------------------------------------------

/// Hammers the matchmaker with many simultaneous submits and asserts the core
/// safety invariant at scale: no user is ever placed in two pairings, every
/// pairing has distinct colors, and the seek pool accounts for every submission.
/// This is the load-scale complement to the unit-level concurrency test in the
/// matchmaker module.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load test; run explicitly with --ignored loadtest"]
async fn loadtest_matchmaker_never_double_books_under_load() {
    let (state, _storage) = test_app().await;
    let matchmaker = state.matchmaker().clone();

    const SUBMITS: usize = 500;

    let mut set = JoinSet::new();
    for _ in 0..SUBMITS {
        let mm = matchmaker.clone();
        let user = UserId::new();
        let seek = Seek::new(
            user,
            STANDARD_VARIANT_ID.to_owned(),
            blitz(),
            ColorPreference::Random,
            false,
            OffsetDateTime::UNIX_EPOCH,
        );
        set.spawn(async move { mm.submit(seek).await.expect("submit succeeds") });
    }

    let mut pairings = Vec::new();
    let mut queued = 0usize;
    while let Some(res) = set.join_next().await {
        match res.expect("submit task did not panic") {
            SubmitOutcome::Paired(p) => pairings.push(p),
            SubmitOutcome::Queued(_) => queued += 1,
        }
    }

    // No user appears in two pairings; each pairing has two distinct players.
    let mut seen: HashSet<UserId> = HashSet::new();
    for p in &pairings {
        assert_ne!(p.white, p.black, "a pairing has two distinct players");
        assert!(
            seen.insert(p.white),
            "user double-booked (white): {:?}",
            p.white
        );
        assert!(
            seen.insert(p.black),
            "user double-booked (black): {:?}",
            p.black
        );
    }

    // Conservation: every submission ended up either paired or still queued.
    assert_eq!(
        pairings.len() + queued,
        SUBMITS,
        "every submission produced exactly one outcome"
    );
    // Each pairing consumed two seekers; the rest are still open in the pool.
    let open = matchmaker.open_seeks().await.expect("list open").len();
    assert_eq!(
        pairings.len() * 2 + open,
        SUBMITS,
        "paired players plus open seeks account for every submission"
    );
}
