//! Integration tests for recover-on-demand live game handles (#65).
//!
//! [`AppState::get_or_recover`](mcs_api::AppState::get_or_recover) lets any node
//! serve an in-progress game by rebuilding its actor from the durable action log
//! the first time a client reaches for it, instead of relying on an eager
//! in-memory handle. These tests drive that path directly over a real
//! SQLite-backed [`SqlxStorage`]:
//!
//! - a game is created and partly played through the normal actor path, its
//!   handle is dropped from the hub to simulate a cold node, and `get_or_recover`
//!   revives it at the same position/clocks so play continues at the right ply;
//! - many concurrent `get_or_recover` calls for the same absent game spawn
//!   exactly one actor (no duplicate plies, no split broadcast);
//! - an unknown game and a finished game both resolve to `None`.

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use mcs_api::{AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::{Action, Color, VariantOptions};
use mcs_domain::{Game, GameId, GameLifecycle, TimeControl, User};
use mcs_game::{GameActor, GameHandle};
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
use mcs_variant_standard::{register, STANDARD_VARIANT_ID};

// ---------------------------------------------------------------------------
// Test wiring
// ---------------------------------------------------------------------------

/// The concrete storage is kept alongside the [`AppState`] so a test can spawn
/// or recover an actor over the very same in-memory database the API reads.
struct TestApp {
    state: AppState,
    storage: Arc<SqlxStorage>,
}

/// Builds an [`AppState`] backed by a fresh in-memory SQLite database with the
/// standard-chess variant registered.
async fn test_app() -> TestApp {
    let storage = SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite");
    let storage = Arc::new(storage);

    let mut registry = mcs_core::VariantRegistry::new();
    register(&mut registry);
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

/// Creates a standard-chess game between `white` and `black`, persists the
/// `Active` record, spawns its actor, and registers the handle in the hub.
async fn spawn_game(app: &TestApp, white: &User, black: &User) -> GameId {
    let mut registry = mcs_core::VariantRegistry::new();
    register(&mut registry);
    let session = registry
        .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
        .expect("standard variant registered");

    let time_control = TimeControl::RealTime {
        initial: Duration::from_secs(300),
        increment: Duration::from_secs(2),
    };
    let mut game = Game::new(
        STANDARD_VARIANT_ID.to_owned(),
        VariantOptions::default(),
        white.id,
        black.id,
        time_control.clone(),
        OffsetDateTime::now_utc(),
    );
    game.lifecycle = GameLifecycle::Active;
    let game_id = game.id;
    app.storage
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

/// A UCI move action for the standard variant.
fn uci(mv: &str) -> Action {
    serde_json::from_value(serde_json::json!({ "type": "move", "uci": mv }))
        .expect("valid move action")
}

/// The FEN of the position the handle currently shows from White's view.
async fn fen(handle: &GameHandle) -> String {
    let view = handle.view_for(Color::White).await.expect("view");
    view.as_value()["fen"]
        .as_str()
        .expect("fen present")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Revive-and-continue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_or_recover_revives_an_absent_game_and_play_continues() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    // Play a couple of moves through the live actor (1. e4 e5).
    {
        let handle = app.state.game_hub().get(game_id).expect("live");
        handle
            .submit_action(Color::White, uci("e2e4"))
            .await
            .expect("1. e4");
        handle
            .submit_action(Color::Black, uci("e7e5"))
            .await
            .expect("1... e5");
    }
    // Capture the on-record state before going cold.
    let recorded_before = app.storage.list(game_id).await.expect("log");
    assert_eq!(recorded_before.len(), 2, "two plies recorded");
    let game_before = app.storage.get(game_id).await.expect("record");
    assert_eq!(game_before.ply, 2, "snapshot ply advanced to 2");

    // Simulate a cold node: drop the live handle from the hub. The actor task
    // winds down once every handle is gone; the durable record/log remain.
    let dropped = app.state.game_hub().remove(game_id);
    assert!(dropped.is_some(), "the handle was registered");
    drop(dropped);
    assert!(app.state.game_hub().is_empty(), "no live games remain");

    // Recover on demand: same position, side to move, and ply.
    let handle = app
        .state
        .get_or_recover(game_id)
        .await
        .expect("recover ok")
        .expect("game is unfinished, so a handle is returned");
    let revived_fen = fen(&handle).await;
    assert!(
        revived_fen.contains(" w "),
        "after 1. e4 e5 it is White to move; got {revived_fen}"
    );
    assert!(
        revived_fen.starts_with("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w"),
        "revived to the exact position after 1. e4 e5; got {revived_fen}"
    );

    // The next move continues at the right ply and is logged.
    handle
        .submit_action(Color::White, uci("g1f3"))
        .await
        .expect("2. Nf3 continues the recovered game");

    let recorded_after = app.storage.list(game_id).await.expect("log");
    assert_eq!(
        recorded_after.len(),
        3,
        "the recovered actor appended a third ply, not a duplicate"
    );
    // Plies are zero-indexed: e4 = 0, e5 = 1, so the recovered move is ply 2 —
    // it continues the log where recovery seeded it, not from zero.
    assert_eq!(recorded_after[2].ply, 2, "the new move continues at ply 2");
    assert_eq!(recorded_after[2].player, Color::White);

    // The hub now holds the revived handle: a second call is the fast path.
    let again = app
        .state
        .get_or_recover(game_id)
        .await
        .expect("ok")
        .expect("still live");
    assert!(
        again.subscribe().same_channel(&handle.subscribe()),
        "the fast path returns the same live actor"
    );
}

// ---------------------------------------------------------------------------
// Concurrency: recover exactly once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_get_or_recover_spawns_exactly_one_actor() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    // Play one move, then go cold so every racing caller must recover.
    {
        let handle = app.state.game_hub().get(game_id).expect("live");
        handle
            .submit_action(Color::White, uci("e2e4"))
            .await
            .expect("1. e4");
    }
    app.state.game_hub().remove(game_id);
    assert!(app.state.game_hub().is_empty());

    // Fire many concurrent recoveries for the same absent game.
    const N: usize = 16;
    let mut tasks = Vec::with_capacity(N);
    for _ in 0..N {
        let state = app.state.clone();
        tasks.push(tokio::spawn(
            async move { state.get_or_recover(game_id).await },
        ));
    }
    let mut handles = Vec::with_capacity(N);
    for task in tasks {
        let handle = task
            .await
            .expect("task did not panic")
            .expect("recover ok")
            .expect("unfinished game yields a handle");
        handles.push(handle);
    }

    // All callers must observe one and the same actor: every returned handle
    // shares the broadcast channel of the single spawned actor.
    let first = handles[0].subscribe();
    for handle in &handles[1..] {
        assert!(
            handle.subscribe().same_channel(&first),
            "every concurrent caller observed the same single actor"
        );
    }

    // And there is exactly one actor's worth of state: one move played by one of
    // the shared handles advances the log by a single ply — no duplicate ply-0
    // entries, no double-spawn artifacts. Plies are zero-indexed: e4 = 0, e5 = 1.
    handles[0]
        .submit_action(Color::Black, uci("e7e5"))
        .await
        .expect("1... e5 on the single recovered actor");
    let recorded = app.storage.list(game_id).await.expect("log");
    assert_eq!(
        recorded.len(),
        2,
        "exactly one actor: ply 0 (pre-cold) + ply 1 (post-recover), no duplicates"
    );
    assert_eq!(recorded[0].ply, 0);
    assert_eq!(recorded[1].ply, 1);
}

// ---------------------------------------------------------------------------
// Unknown and finished games resolve to None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_game_recovers_to_none() {
    let app = test_app().await;
    let missing = GameId::new();
    let resolved = app
        .state
        .get_or_recover(missing)
        .await
        .expect("a missing game is not an error");
    assert!(resolved.is_none(), "an unknown game has no live actor");
}

#[tokio::test]
async fn finished_game_recovers_to_none() {
    let app = test_app().await;
    let white = create_user(&app, "0x1111111111111111111111111111111111111111").await;
    let black = create_user(&app, "0x2222222222222222222222222222222222222222").await;
    let game_id = spawn_game(&app, &white, &black).await;

    // Drop the live handle and mark the durable record finished.
    app.state.game_hub().remove(game_id);
    let mut game = app.storage.get(game_id).await.expect("record");
    game.lifecycle = GameLifecycle::Finished;
    app.storage
        .update(&game)
        .await
        .expect("persist finished lifecycle");

    let resolved = app.state.get_or_recover(game_id).await.expect("ok");
    assert!(
        resolved.is_none(),
        "a finished game has no live actor to revive"
    );
    assert!(
        app.state.game_hub().is_empty(),
        "recovery of a finished game inserts nothing"
    );
}
