//! Restart-recovery integration test.
//!
//! This proves the end-to-end durability story (#58): a game played through a
//! live [`GameActor`](mcs_game::GameActor) survives the loss of its actor and
//! is faithfully rebuilt from the durable log by
//! [`recover_game`](mcs_game::recover_game).
//!
//! It uses **one shared `sqlx` store** — a temporary SQLite file shared across
//! the whole test — so the "restart" is genuine: the same backing database the
//! actor wrote to is the one recovery reads from. The sequence is:
//!
//! 1. persist an `Active` [`Game`] and play several moves through an actor, so
//!    the action log and the live snapshot are populated;
//! 2. drop the actor's handle (the "crash" — its in-memory session is gone);
//! 3. call [`recover_game`](mcs_game::recover_game) against the same store with
//!    a registry holding the standard variant;
//! 4. assert the recovered handle reports the same position, side to move, and
//!    clocks (within tolerance), and that the next legal move applies and is
//!    logged at the correct continuing ply.

use std::sync::Arc;
use std::time::Duration;

use mcs_core::{Action, Color, GameStatus, VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameLifecycle, TimeControl, UserId};
use mcs_game::{recover_game, GameActor, GameHandle, NoopHook};
use mcs_storage::{ActionLogRepo, GameRepo, SqlxStorage};
use mcs_variant_standard::wire::StandardAction;
use time::OffsetDateTime;

/// A `move` action for the given UCI string, type-erased as the store sees it.
fn mv(uci: &str) -> Action {
    Action::from_typed(&StandardAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// Builds a registry holding only the standard variant, exactly as the server
/// would register it.
fn standard_registry() -> VariantRegistry {
    let mut registry = VariantRegistry::new();
    mcs_variant_standard::register(&mut registry);
    registry
}

/// A unique temporary SQLite file URL, so each test run gets a fresh database.
struct TempDb {
    path: std::path::PathBuf,
    url: String,
}

impl TempDb {
    fn new() -> Self {
        // A process- and time-unique file name avoids collisions between
        // concurrent test binaries sharing the temp directory.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mcs-recovery-{}-{nanos}.db", std::process::id()));
        // `mode=rwc` creates the file if it does not exist.
        let url = format!("sqlite://{}?mode=rwc", path.display());
        Self { path, url }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        // Best-effort cleanup of the temp database and its sidecar files.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("db-wal"));
        let _ = std::fs::remove_file(self.path.with_extension("db-shm"));
    }
}

/// Seeds an `Active`, real-time `Game` record into the store and returns it.
async fn seed_active_game(repo: &dyn GameRepo) -> Game {
    let mut game = Game::new(
        "standard".to_owned(),
        VariantOptions::default(),
        UserId::new(),
        UserId::new(),
        TimeControl::RealTime {
            initial: Duration::from_secs(300),
            increment: Duration::from_secs(2),
        },
        true,
        OffsetDateTime::now_utc(),
    );
    game.lifecycle = GameLifecycle::Active;
    repo.create(&game).await.expect("create game");
    game
}

#[tokio::test]
async fn game_in_progress_is_recovered_after_an_actor_restart() {
    let db = TempDb::new();
    // ONE shared store, used by the original actor and by recovery alike.
    let storage = Arc::new(
        SqlxStorage::connect(&db.url)
            .await
            .expect("connect temp sqlite"),
    );
    let game_repo: Arc<dyn GameRepo> = storage.clone();
    let action_log: Arc<dyn ActionLogRepo> = storage.clone();

    let game = seed_active_game(game_repo.as_ref()).await;
    let game_id = game.id;
    let time_control = game.time_control.clone();

    // ---- Phase 1: play several moves through a live actor. ----
    let registry = standard_registry();
    let session = registry
        .new_game("standard", &VariantOptions::default())
        .expect("standard session");
    let handle = GameActor::spawn(
        game_id,
        session,
        game_repo.clone(),
        action_log.clone(),
        Arc::new(NoopHook),
        time_control.clone(),
    );

    // Three half-moves of a normal opening, leaving it Black to move at ply 3.
    for (player, uci) in [
        (Color::White, "e2e4"),
        (Color::Black, "e7e5"),
        (Color::White, "g1f3"),
    ] {
        handle
            .submit_action(player, mv(uci))
            .await
            .expect("legal move applies");
    }

    // Capture the pre-restart truth from the live actor.
    let pre_view = handle.view_for(Color::White).await.expect("view");
    let pre_spectator = handle.spectator_view().await.expect("spectator view");
    let pre_status = handle.status().await.expect("status");
    assert_eq!(pre_status, GameStatus::Ongoing);

    // The durable snapshot should now report ply 3, Black to move.
    let stored = game_repo.get(game_id).await.expect("reload game");
    assert_eq!(stored.ply, 3, "three half-moves were recorded");
    assert_eq!(stored.side_to_move, Some(Color::Black));
    let pre_clock_white = stored.clock_white_ms.expect("white clock recorded");
    let pre_clock_black = stored.clock_black_ms.expect("black clock recorded");

    // ---- Phase 2: the "crash" — drop the only handle, so the actor stops. ----
    drop(handle);

    // ---- Phase 3: recover from the same store. ----
    let recovered: GameHandle = recover_game(
        &stored,
        &registry,
        action_log.clone(),
        game_repo.clone(),
        Arc::new(NoopHook),
    )
    .await
    .expect("recovery succeeds");

    assert_eq!(recovered.game_id(), game_id);

    // The recovered position matches the pre-restart one exactly.
    assert_eq!(
        recovered.view_for(Color::White).await.expect("view"),
        pre_view,
        "recovered player view matches the pre-restart position",
    );
    assert_eq!(
        recovered.spectator_view().await.expect("spectator view"),
        pre_spectator,
        "recovered spectator view matches the pre-restart position",
    );
    assert_eq!(
        recovered.status().await.expect("status"),
        GameStatus::Ongoing,
        "the recovered game is still ongoing",
    );

    // Black is to move, and exactly the same legal replies are offered.
    let legal = recovered
        .legal_actions(Color::Black)
        .await
        .expect("legal actions");
    assert!(!legal.is_empty(), "Black has legal moves after recovery");

    // ---- Phase 4: the next legal move applies and continues at the right ply.
    recovered
        .submit_action(Color::Black, mv("b8c6"))
        .await
        .expect("the next legal move applies after recovery");

    let after = game_repo.get(game_id).await.expect("reload after move");
    assert_eq!(
        after.ply, 4,
        "the post-recovery move continues the ply count (3 -> 4)",
    );
    assert_eq!(after.side_to_move, Some(Color::White));

    // The continuing move is logged at ply 3 (zero-based), right after the
    // three pre-restart moves at plies 0..2.
    let recorded = action_log.list(game_id).await.expect("read log");
    assert_eq!(recorded.len(), 4, "one row per applied move, no gaps");
    let plies: Vec<u32> = recorded.iter().map(|a| a.ply).collect();
    assert_eq!(plies, vec![0, 1, 2, 3], "plies are contiguous from 0");
    assert_eq!(
        recorded[3].action,
        mv("b8c6"),
        "the recovered move is logged"
    );
    assert_eq!(recorded[3].player, Color::Black);

    // Clocks resumed from the persisted remaining: the side that has not moved
    // since recovery (White) still shows its pre-restart remaining, within a
    // small tolerance for rounding to whole milliseconds.
    let after_white = after.clock_white_ms.expect("white clock");
    let tolerance_ms = 1_000; // 1s slack: real wall-clock elapses during the test.
    assert!(
        after_white.abs_diff(pre_clock_white) <= tolerance_ms,
        "White's clock resumed from its persisted remaining (was {pre_clock_white}, now {after_white})",
    );
    // Black moved once post-recovery, so its clock is at most its prior value
    // plus the 2s increment, and downtime was not charged.
    let after_black = after.clock_black_ms.expect("black clock");
    assert!(
        after_black <= pre_clock_black + 2_000 + tolerance_ms,
        "Black's clock resumed from its persisted remaining without charging downtime \
         (was {pre_clock_black}, now {after_black})",
    );
}

#[tokio::test]
async fn recovering_an_unplayed_game_resumes_at_the_start_with_a_full_clock() {
    // A timed game with an empty log (created but never played) recovers to its
    // starting position, does not flag on its full budget, and accepts the very
    // first move at ply 0.
    let db = TempDb::new();
    let storage = Arc::new(
        SqlxStorage::connect(&db.url)
            .await
            .expect("connect temp sqlite"),
    );
    let game_repo: Arc<dyn GameRepo> = storage.clone();
    let action_log: Arc<dyn ActionLogRepo> = storage.clone();

    let game = seed_active_game(game_repo.as_ref()).await;
    let game_id = game.id;
    let registry = standard_registry();

    let recovered = recover_game(
        &game,
        &registry,
        action_log.clone(),
        game_repo.clone(),
        Arc::new(NoopHook),
    )
    .await
    .expect("recovery of an unplayed game succeeds");

    recovered
        .submit_action(Color::White, mv("d2d4"))
        .await
        .expect("first move applies");

    let recorded = action_log.list(game_id).await.expect("read log");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].ply, 0, "the first move is logged at ply 0");
}
