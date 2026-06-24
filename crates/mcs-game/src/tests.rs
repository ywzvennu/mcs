//! Integration tests for the game session actor.
//!
//! These drive a real standard-chess [`GameSession`](mcs_core::GameSession)
//! through the actor, using a tiny in-memory [`GameRepo`] mock to observe
//! persistence on game end. They mirror how the server uses the crate: spawn an
//! actor, hand out handles, and interact entirely through the type-erased
//! boundary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mcs_core::{
    Action, Color, EndReason, GameError, GameSession, Outcome, VariantOptions, VariantRegistry,
};
use mcs_domain::{Game, GameId, GameLifecycle, TimeControl, UserId};
use mcs_storage::{ActionLogRepo, GameRepo, RecordedAction, StorageError, StorageResult};
use mcs_variant_standard::register;
use mcs_variant_standard::wire::StandardAction;
use time::OffsetDateTime;

use crate::{GameActor, GameCompletionHook, GameSessionError, NoopHook};

// --------------------------------------------------------------------------
// In-memory GameRepo mock.
//
// Only the methods the actor actually calls (`get` and `update`) are
// meaningful; the rest are present to satisfy the trait and are unused by the
// actor, so they fail loudly if a future change starts calling them.
// --------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MockGameRepo {
    games: Mutex<HashMap<GameId, Game>>,
    /// Records each game id passed to `update`, in call order, so tests can
    /// assert the actor persisted exactly once and with the right state.
    updates: Mutex<Vec<Game>>,
}

impl MockGameRepo {
    fn with_game(game: Game) -> Arc<Self> {
        let repo = Self::default();
        repo.games.lock().unwrap().insert(game.id, game);
        Arc::new(repo)
    }

    /// Returns the games recorded by `update`, in order. The actor now writes a
    /// live-snapshot update after every move plus a final finishing write, so
    /// this includes both; use [`finished_updates`](Self::finished_updates) to
    /// isolate the terminal write.
    fn updated_games(&self) -> Vec<Game> {
        self.updates.lock().unwrap().clone()
    }

    /// Returns only the updates that wrote a [`GameLifecycle::Finished`] record,
    /// i.e. the terminal persist (and any redundant repeat, of which there must
    /// be none).
    fn finished_updates(&self) -> Vec<Game> {
        self.updates
            .lock()
            .unwrap()
            .iter()
            .filter(|g| g.lifecycle == GameLifecycle::Finished)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl GameRepo for MockGameRepo {
    async fn create(&self, game: &Game) -> StorageResult<()> {
        self.games.lock().unwrap().insert(game.id, game.clone());
        Ok(())
    }

    async fn get(&self, id: GameId) -> StorageResult<Game> {
        self.games
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(StorageError::NotFound)
    }

    async fn update(&self, game: &Game) -> StorageResult<()> {
        let mut games = self.games.lock().unwrap();
        if !games.contains_key(&game.id) {
            return Err(StorageError::NotFound);
        }
        games.insert(game.id, game.clone());
        self.updates.lock().unwrap().push(game.clone());
        Ok(())
    }

    async fn list_recent(&self, _limit: u32) -> StorageResult<Vec<Game>> {
        unreachable!("the actor never lists games")
    }

    async fn list_for_user(&self, _user: UserId, _limit: u32) -> StorageResult<Vec<Game>> {
        unreachable!("the actor never lists games")
    }

    async fn list_unfinished(&self) -> StorageResult<Vec<Game>> {
        unreachable!("the actor never lists games")
    }
}

/// A [`GameRepo`] whose `update` fails only for the **finishing** write, to
/// exercise the persistence error path on game end while letting the
/// per-move live-snapshot updates (which the actor now also performs) succeed.
#[derive(Debug)]
struct FailingUpdateRepo {
    game: Game,
}

#[async_trait]
impl GameRepo for FailingUpdateRepo {
    async fn create(&self, _game: &Game) -> StorageResult<()> {
        Ok(())
    }

    async fn get(&self, id: GameId) -> StorageResult<Game> {
        if id == self.game.id {
            Ok(self.game.clone())
        } else {
            Err(StorageError::NotFound)
        }
    }

    async fn update(&self, game: &Game) -> StorageResult<()> {
        // Ongoing live-snapshot updates succeed; only the terminal write fails,
        // so the failure lands on game end exactly as these tests intend.
        if game.lifecycle == GameLifecycle::Finished {
            Err(StorageError::Backend("write failed".to_owned()))
        } else {
            Ok(())
        }
    }

    async fn list_recent(&self, _limit: u32) -> StorageResult<Vec<Game>> {
        unreachable!()
    }

    async fn list_for_user(&self, _user: UserId, _limit: u32) -> StorageResult<Vec<Game>> {
        unreachable!()
    }

    async fn list_unfinished(&self) -> StorageResult<Vec<Game>> {
        unreachable!()
    }
}

// --------------------------------------------------------------------------
// In-memory ActionLogRepo mock.
//
// Records every appended `RecordedAction` in call order so tests can assert the
// actor logged exactly one row per applied move, with the right ply, player,
// action, and clocks. `append` rejects a duplicate ply, mirroring the real
// append-only contract.
// --------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MockActionLogRepo {
    actions: Mutex<Vec<RecordedAction>>,
}

impl MockActionLogRepo {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the recorded actions, in append order.
    fn recorded(&self) -> Vec<RecordedAction> {
        self.actions.lock().unwrap().clone()
    }
}

#[async_trait]
impl ActionLogRepo for MockActionLogRepo {
    async fn append(&self, _game_id: GameId, action: &RecordedAction) -> StorageResult<()> {
        let mut actions = self.actions.lock().unwrap();
        if actions.iter().any(|a| a.ply == action.ply) {
            return Err(StorageError::Conflict(format!(
                "duplicate ply {}",
                action.ply
            )));
        }
        actions.push(action.clone());
        Ok(())
    }

    async fn list(&self, _game_id: GameId) -> StorageResult<Vec<RecordedAction>> {
        Ok(self.recorded())
    }

    async fn last_ply(&self, _game_id: GameId) -> StorageResult<Option<u32>> {
        Ok(self.actions.lock().unwrap().iter().map(|a| a.ply).max())
    }
}

/// An [`ActionLogRepo`] whose `append` always fails, to exercise the
/// record-on-move error path.
#[derive(Debug)]
struct FailingAppendLog;

#[async_trait]
impl ActionLogRepo for FailingAppendLog {
    async fn append(&self, _game_id: GameId, _action: &RecordedAction) -> StorageResult<()> {
        Err(StorageError::Backend("append failed".to_owned()))
    }

    async fn list(&self, _game_id: GameId) -> StorageResult<Vec<RecordedAction>> {
        Ok(Vec::new())
    }

    async fn last_ply(&self, _game_id: GameId) -> StorageResult<Option<u32>> {
        Ok(None)
    }
}

// --------------------------------------------------------------------------
// Helpers.
// --------------------------------------------------------------------------

/// Builds a fresh standard-chess session through the registry, exactly as the
/// server would, keeping the actor variant-agnostic.
fn standard_session() -> Box<dyn GameSession> {
    let mut registry = VariantRegistry::new();
    register(&mut registry);
    registry
        .new_game("standard", &VariantOptions::default())
        .expect("standard variant is registered")
}

/// Builds an `Active` game record for `id` to seed the repo with.
fn active_game(id: GameId) -> Game {
    let mut game = Game::new(
        "standard".to_owned(),
        VariantOptions::default(),
        UserId::new(),
        UserId::new(),
        TimeControl::RealTime {
            initial: Duration::from_secs(300),
            increment: Duration::from_secs(0),
        },
        OffsetDateTime::UNIX_EPOCH,
    );
    game.id = id;
    game.lifecycle = GameLifecycle::Active;
    game
}

/// A `move` action for the given UCI string.
fn mv(uci: &str) -> Action {
    Action::from_typed(&StandardAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// Plays Fool's mate through `handle`, leaving Black checkmated, and returns the
/// effect of the final mating move.
async fn play_fools_mate(handle: &crate::GameHandle) -> mcs_core::ActionEffect {
    handle
        .submit_action(Color::White, mv("f2f3"))
        .await
        .unwrap();
    handle
        .submit_action(Color::Black, mv("e7e5"))
        .await
        .unwrap();
    handle
        .submit_action(Color::White, mv("g2g4"))
        .await
        .unwrap();
    handle
        .submit_action(Color::Black, mv("d8h4"))
        .await
        .unwrap()
}

// --------------------------------------------------------------------------
// Tests.
// --------------------------------------------------------------------------

#[tokio::test]
async fn fools_mate_finishes_and_persists_with_correct_outcome() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo.clone(),
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    let effect = play_fools_mate(&handle).await;
    assert!(effect.status.is_finished());

    // The session itself reports the expected checkmate.
    let outcome = handle.outcome().await.unwrap();
    assert_eq!(
        outcome,
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );

    // The actor wrote the finishing record exactly once, transitioning it to
    // Finished with that same outcome. (Per-move snapshot writes also occur; we
    // isolate the terminal write here.)
    let finished = repo.finished_updates();
    assert_eq!(
        finished.len(),
        1,
        "the finished game should be persisted exactly once"
    );
    assert_eq!(finished[0].id, game_id);
    assert_eq!(finished[0].lifecycle, GameLifecycle::Finished);
    assert_eq!(
        finished[0].outcome,
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
}

#[tokio::test]
async fn events_are_broadcast_to_subscribers() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    let mut events = handle.subscribe();

    // The first move broadcasts an ongoing update with one MovePlayed event.
    handle
        .submit_action(Color::White, mv("f2f3"))
        .await
        .unwrap();
    let update = events.recv().await.expect("event for the first move");
    assert!(!update.is_finished());
    assert_eq!(update.events.len(), 1);

    handle
        .submit_action(Color::Black, mv("e7e5"))
        .await
        .unwrap();
    handle
        .submit_action(Color::White, mv("g2g4"))
        .await
        .unwrap();

    // Drain the two intermediate updates.
    let _ = events.recv().await.unwrap();
    let _ = events.recv().await.unwrap();

    // The mating move broadcasts a finished update carrying both the move and
    // the game-ended events.
    handle
        .submit_action(Color::Black, mv("d8h4"))
        .await
        .unwrap();
    let final_update = events.recv().await.expect("event for the mating move");
    assert!(final_update.is_finished());
    assert_eq!(final_update.events.len(), 2);
    assert_eq!(
        final_update.status,
        mcs_core::GameStatus::Finished(Outcome::win(Color::Black, EndReason::Checkmate))
    );
}

#[tokio::test]
async fn out_of_turn_action_is_rejected() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo.clone(),
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    // Black tries to move first.
    let err = handle
        .submit_action(Color::Black, mv("e7e5"))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        GameSessionError::Game(GameError::NotYourTurn)
    ));

    // Nothing was persisted; the game is still ongoing.
    assert!(repo.updated_games().is_empty());
    assert_eq!(handle.outcome().await.unwrap(), None);
}

#[tokio::test]
async fn illegal_action_is_rejected() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo.clone(),
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    // A pawn cannot jump three squares.
    let err = handle
        .submit_action(Color::White, mv("e2e5"))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        GameSessionError::Game(GameError::IllegalAction)
    ));
    assert!(repo.updated_games().is_empty());
}

#[tokio::test]
async fn acting_after_finish_is_rejected_and_persists_only_once() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo.clone(),
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    play_fools_mate(&handle).await;

    // Any further action is refused by the finished session...
    let err = handle
        .submit_action(Color::White, mv("a2a3"))
        .await
        .unwrap_err();
    assert!(matches!(err, GameSessionError::Game(GameError::Finished)));

    // ...and the rejected action triggers no additional persistence: the four
    // applied moves each wrote a live snapshot and the finishing move added one
    // terminal write (5 total), and the rejected move adds nothing.
    assert_eq!(repo.finished_updates().len(), 1);
    assert_eq!(repo.updated_games().len(), 5);
}

#[tokio::test]
async fn views_and_legal_actions_are_served_through_the_handle() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    // White has the 20 opening moves plus Resign and OfferDraw.
    let actions = handle.legal_actions(Color::White).await.unwrap();
    assert_eq!(actions.len(), 22);

    // Status starts ongoing.
    assert_eq!(
        handle.status().await.unwrap(),
        mcs_core::GameStatus::Ongoing
    );

    // Standard chess is perfect information: all views agree.
    let white = handle.view_for(Color::White).await.unwrap();
    let black = handle.view_for(Color::Black).await.unwrap();
    let spectator = handle.spectator_view().await.unwrap();
    assert_eq!(white, black);
    assert_eq!(white, spectator);
}

#[tokio::test]
async fn persistence_failure_on_game_end_surfaces_as_storage_error() {
    let game_id = GameId::new();
    let repo = Arc::new(FailingUpdateRepo {
        game: active_game(game_id),
    });
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    handle
        .submit_action(Color::White, mv("f2f3"))
        .await
        .unwrap();
    handle
        .submit_action(Color::Black, mv("e7e5"))
        .await
        .unwrap();
    handle
        .submit_action(Color::White, mv("g2g4"))
        .await
        .unwrap();

    // The mating move applies in memory but persistence fails, surfacing as a
    // storage error rather than a game error.
    let err = handle
        .submit_action(Color::Black, mv("d8h4"))
        .await
        .unwrap_err();
    assert!(matches!(err, GameSessionError::Storage(_)));

    // The in-memory session still recorded the result correctly.
    assert_eq!(
        handle.outcome().await.unwrap(),
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
}

#[tokio::test]
async fn handle_is_cloneable_and_clones_share_one_session() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );
    let other = handle.clone();

    assert_eq!(handle.game_id(), other.game_id());

    // A move submitted through one clone is visible through the other.
    handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap();
    assert_eq!(other.status().await.unwrap(), mcs_core::GameStatus::Ongoing);
    let legal = other.legal_actions(Color::Black).await.unwrap();
    assert!(!legal.is_empty());
}

// --------------------------------------------------------------------------
// Clock integration tests.
//
// These use the crate-private `spawn_with_time_source` to inject a controllable
// virtual clock, so timing assertions never depend on the wall clock. Tests
// that arm the auto-flag timer run on a paused Tokio runtime and advance both
// the virtual UTC clock and Tokio's timer in lockstep through
// `ManualTimeSource::advance`.
// --------------------------------------------------------------------------

use crate::time_source::testing::ManualTimeSource;

/// A [`TimeSource`](crate::TimeSource) that shares one [`ManualTimeSource`]
/// behind an `Arc`, letting a test drive the same virtual clock the spawned
/// actor reads.
#[derive(Debug)]
struct SharedTimeSource(Arc<ManualTimeSource>);

#[async_trait]
impl crate::TimeSource for SharedTimeSource {
    fn now(&self) -> OffsetDateTime {
        self.0.now()
    }

    async fn sleep_until(&self, deadline: OffsetDateTime) {
        self.0.sleep_until(deadline).await;
    }
}

/// A real-time `active_game` record with the given budget and increment.
fn real_time_game(id: GameId, initial_secs: u64, increment_secs: u64) -> Game {
    let mut game = Game::new(
        "standard".to_owned(),
        VariantOptions::default(),
        UserId::new(),
        UserId::new(),
        TimeControl::RealTime {
            initial: Duration::from_secs(initial_secs),
            increment: Duration::from_secs(increment_secs),
        },
        OffsetDateTime::UNIX_EPOCH,
    );
    game.id = id;
    game.lifecycle = GameLifecycle::Active;
    game
}

#[tokio::test(start_paused = true)]
async fn move_events_carry_live_clock_snapshots() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(real_time_game(game_id, 300, 2));
    let time = Arc::new(ManualTimeSource::new(OffsetDateTime::UNIX_EPOCH));
    let tc = TimeControl::RealTime {
        initial: Duration::from_secs(300),
        increment: Duration::from_secs(2),
    };
    let handle = GameActor::spawn_with_time_source(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        tc,
        Box::new(SharedTimeSource(time.clone())),
    );

    let mut events = handle.subscribe();

    // White spends 10 seconds, then moves: 300 - 10 + 2 = 292 remaining.
    time.advance(Duration::from_secs(10)).await;
    handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap();
    let update = events.recv().await.unwrap();
    let clock = update.clock.expect("real-time events carry a clock");
    assert_eq!(clock.white_remaining(), Duration::from_secs(292));
    assert_eq!(clock.black_remaining(), Duration::from_secs(300));

    // Black spends 5 seconds, then moves: 300 - 5 + 2 = 297 remaining.
    time.advance(Duration::from_secs(5)).await;
    handle
        .submit_action(Color::Black, mv("e7e5"))
        .await
        .unwrap();
    let update = events.recv().await.unwrap();
    let clock = update.clock.unwrap();
    assert_eq!(clock.white_remaining(), Duration::from_secs(292));
    assert_eq!(clock.black_remaining(), Duration::from_secs(297));
}

#[tokio::test(start_paused = true)]
async fn player_who_stops_moving_loses_on_time() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(real_time_game(game_id, 3, 0));
    let time = Arc::new(ManualTimeSource::new(OffsetDateTime::UNIX_EPOCH));
    let tc = TimeControl::RealTime {
        initial: Duration::from_secs(3),
        increment: Duration::from_secs(0),
    };
    let handle = GameActor::spawn_with_time_source(
        game_id,
        standard_session(),
        repo.clone(),
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        tc,
        Box::new(SharedTimeSource(time.clone())),
    );

    let mut events = handle.subscribe();

    // White never moves. Advancing past their 3-second budget fires the armed
    // flag timer and the actor ends the game on time.
    time.advance(Duration::from_secs(4)).await;

    let update = events.recv().await.expect("a timeout event is broadcast");
    assert!(update.is_finished());
    assert_eq!(
        update.status,
        mcs_core::GameStatus::Finished(Outcome::win(Color::Black, EndReason::Timeout))
    );

    // The outcome is queryable and persisted exactly once as a timeout.
    assert_eq!(
        handle.outcome().await.unwrap(),
        Some(Outcome::win(Color::Black, EndReason::Timeout))
    );
    let updated = repo.updated_games();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].lifecycle, GameLifecycle::Finished);
    assert_eq!(
        updated[0].outcome,
        Some(Outcome::win(Color::Black, EndReason::Timeout))
    );
}

#[tokio::test(start_paused = true)]
async fn moving_after_flagging_is_rejected() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(real_time_game(game_id, 2, 0));
    let time = Arc::new(ManualTimeSource::new(OffsetDateTime::UNIX_EPOCH));
    let tc = TimeControl::RealTime {
        initial: Duration::from_secs(2),
        increment: Duration::from_secs(0),
    };
    let handle = GameActor::spawn_with_time_source(
        game_id,
        standard_session(),
        repo.clone(),
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        tc,
        Box::new(SharedTimeSource(time.clone())),
    );

    // White overruns the budget, then tries to move anyway: rejected as finished.
    time.advance(Duration::from_secs(5)).await;
    let err = handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap_err();
    assert!(matches!(err, GameSessionError::Game(GameError::Finished)));

    assert_eq!(
        handle.outcome().await.unwrap(),
        Some(Outcome::win(Color::Black, EndReason::Timeout))
    );
}

#[tokio::test(start_paused = true)]
async fn unlimited_game_never_flags_and_omits_clock() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let time = Arc::new(ManualTimeSource::new(OffsetDateTime::UNIX_EPOCH));
    let handle = GameActor::spawn_with_time_source(
        game_id,
        standard_session(),
        repo.clone(),
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
        Box::new(SharedTimeSource(time.clone())),
    );

    // A ten-day idle with no clock pressure leaves the game ongoing.
    time.advance(Duration::from_secs(10 * 24 * 60 * 60)).await;
    assert_eq!(
        handle.status().await.unwrap(),
        mcs_core::GameStatus::Ongoing
    );
    assert!(repo.updated_games().is_empty());

    // An unlimited game's events carry no clock snapshot.
    let mut events = handle.subscribe();
    handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap();
    let update = events.recv().await.unwrap();
    assert!(update.clock.is_none());
}

#[tokio::test]
async fn dropping_all_handles_stops_the_actor() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    // A subscriber held across the drop observes the broadcast channel close
    // once the actor task ends.
    let mut events = handle.subscribe();
    drop(handle);

    // The actor task observes its command channel close and returns, which
    // drops the broadcast sender and closes the receiver.
    let result = events.recv().await;
    assert!(
        matches!(
            result,
            Err(tokio::sync::broadcast::error::RecvError::Closed)
        ),
        "expected the broadcast channel to close, got {result:?}"
    );
}

// --------------------------------------------------------------------------
// Completion-hook integration tests.
//
// These verify the actor invokes the injected `GameCompletionHook` exactly once
// when a game finishes — and never while it is ongoing — passing the persisted,
// finished game and its outcome. A concrete `RatingUpdateHook` is tested in
// `mcs-api`; here we only assert the actor's contract with the trait.
// --------------------------------------------------------------------------

/// A [`GameCompletionHook`] that records each `(Game, Outcome)` it is told about,
/// so a test can assert the actor fired it the expected number of times with the
/// expected payload.
#[derive(Debug, Default)]
struct RecordingHook {
    calls: Mutex<Vec<(Game, Outcome)>>,
}

impl RecordingHook {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn calls(&self) -> Vec<(Game, Outcome)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl GameCompletionHook for RecordingHook {
    async fn on_finished(&self, game: &Game, outcome: &Outcome) {
        self.calls
            .lock()
            .unwrap()
            .push((game.clone(), outcome.clone()));
    }
}

#[tokio::test]
async fn completion_hook_fires_once_with_finished_game_and_outcome() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let hook = RecordingHook::new();
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        hook.clone(),
        TimeControl::Unlimited,
    );

    play_fools_mate(&handle).await;

    // The hook fired exactly once, with the persisted finished record and the
    // checkmate outcome.
    let calls = hook.calls();
    assert_eq!(calls.len(), 1, "the hook must fire exactly once");
    let (game, outcome) = &calls[0];
    assert_eq!(game.id, game_id);
    assert_eq!(game.lifecycle, GameLifecycle::Finished);
    assert_eq!(*outcome, Outcome::win(Color::Black, EndReason::Checkmate));
    assert_eq!(game.outcome.as_ref(), Some(outcome));
}

#[tokio::test]
async fn completion_hook_does_not_fire_while_the_game_is_ongoing() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let hook = RecordingHook::new();
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        hook.clone(),
        TimeControl::Unlimited,
    );

    // A non-finishing move must not trigger the completion hook.
    handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap();
    assert!(
        hook.calls().is_empty(),
        "the hook must not fire on an ongoing game"
    );
}

#[tokio::test]
async fn completion_hook_is_not_run_when_persistence_fails() {
    let game_id = GameId::new();
    let repo = Arc::new(FailingUpdateRepo {
        game: active_game(game_id),
    });
    let hook = RecordingHook::new();
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo,
        MockActionLogRepo::new(),
        hook.clone(),
        TimeControl::Unlimited,
    );

    // Play to checkmate; the finishing move applies in memory but the write
    // fails, surfacing as a storage error.
    handle
        .submit_action(Color::White, mv("f2f3"))
        .await
        .unwrap();
    handle
        .submit_action(Color::Black, mv("e7e5"))
        .await
        .unwrap();
    handle
        .submit_action(Color::White, mv("g2g4"))
        .await
        .unwrap();
    let err = handle
        .submit_action(Color::Black, mv("d8h4"))
        .await
        .unwrap_err();
    assert!(matches!(err, GameSessionError::Storage(_)));

    // The hook runs only after a successful persist, so a failed write leaves it
    // untouched.
    assert!(
        hook.calls().is_empty(),
        "the hook must not fire when persistence fails"
    );
}

// --------------------------------------------------------------------------
// Action-log recording and live-snapshot integration tests.
//
// These assert the actor's durability contract: every applied player action is
// appended to the log exactly once at a monotonically increasing ply, carrying
// the right player, the exact `Action`, and clock millis matching the post-move
// snapshot; and the game's live snapshot tracks the position move by move. The
// finishing move still records its action *and* marks the game finished.
// --------------------------------------------------------------------------

/// Plays a full Fool's-mate game through the actor with a real-time clock driven
/// by a manual time source, and asserts the action log and live snapshot track
/// every move — including the mating move, which is both logged and finishing.
#[tokio::test(start_paused = true)]
async fn each_applied_move_is_recorded_and_snapshots_the_live_game() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(real_time_game(game_id, 300, 2));
    let log = MockActionLogRepo::new();
    let time = Arc::new(ManualTimeSource::new(OffsetDateTime::UNIX_EPOCH));
    let tc = TimeControl::RealTime {
        initial: Duration::from_secs(300),
        increment: Duration::from_secs(2),
    };
    let handle = GameActor::spawn_with_time_source(
        game_id,
        standard_session(),
        repo.clone(),
        log.clone(),
        Arc::new(NoopHook),
        tc,
        Box::new(SharedTimeSource(time.clone())),
    );

    // Each side spends one second per move, so the post-move clocks are
    // predictable: mover loses 1s, gains the 2s increment (net +1s).
    let moves = [
        (Color::White, "f2f3"),
        (Color::Black, "e7e5"),
        (Color::White, "g2g4"),
        (Color::Black, "d8h4"),
    ];
    for (player, uci) in moves {
        time.advance(Duration::from_secs(1)).await;
        handle.submit_action(player, mv(uci)).await.unwrap();
    }

    // Exactly one recorded action per applied move, at plies 0..4, with the
    // right player and the exact `Action` submitted.
    let recorded = log.recorded();
    assert_eq!(recorded.len(), 4, "one log row per applied move");
    for (i, ((player, uci), row)) in moves.iter().zip(&recorded).enumerate() {
        assert_eq!(row.ply, i as u32, "plies increase monotonically from 0");
        assert_eq!(row.player, *player);
        assert_eq!(row.action, mv(uci), "the exact submitted action is logged");
        // Timed game: both clocks are recorded.
        assert!(row.clock_white_ms.is_some());
        assert!(row.clock_black_ms.is_some());
    }

    // After White's first move (1s spent, +2s increment): 300 - 1 + 2 = 301s.
    assert_eq!(recorded[0].clock_white_ms, Some(301_000));
    assert_eq!(recorded[0].clock_black_ms, Some(300_000));
    // After Black's reply: White unchanged, Black 300 - 1 + 2 = 301s.
    assert_eq!(recorded[1].clock_white_ms, Some(301_000));
    assert_eq!(recorded[1].clock_black_ms, Some(301_000));

    // The live snapshot tracks the position: the final write before the finish
    // recorded ply 4 with the same clocks the log carries. We assert the
    // snapshot writes (lifecycle still Active) advance the ply each move.
    let snapshots: Vec<_> = repo
        .updated_games()
        .into_iter()
        .filter(|g| g.lifecycle == GameLifecycle::Active)
        .collect();
    assert_eq!(
        snapshots.len(),
        4,
        "one live-snapshot write per applied move"
    );
    for (i, snap) in snapshots.iter().enumerate() {
        assert_eq!(snap.ply, i as u32 + 1, "snapshot ply counts applied moves");
    }
    // The first three moves leave a side to move; the mating move clears it.
    assert_eq!(snapshots[0].side_to_move, Some(Color::Black));
    assert_eq!(snapshots[1].side_to_move, Some(Color::White));
    assert_eq!(snapshots[2].side_to_move, Some(Color::Black));
    assert_eq!(
        snapshots[3].side_to_move, None,
        "the finishing move records no side to move"
    );
    // Snapshot clocks match the log's clock readings for the same move.
    assert_eq!(snapshots[0].clock_white_ms, recorded[0].clock_white_ms);
    assert_eq!(snapshots[0].clock_black_ms, recorded[0].clock_black_ms);

    // The finishing move both logged its action and finished the game.
    let finished = repo.finished_updates();
    assert_eq!(finished.len(), 1, "the game is finished exactly once");
    assert_eq!(finished[0].lifecycle, GameLifecycle::Finished);
    assert_eq!(
        finished[0].outcome,
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
    assert_eq!(
        handle.outcome().await.unwrap(),
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
}

/// An untimed game records `None` clocks on every log row and snapshot.
#[tokio::test]
async fn untimed_game_records_no_clocks() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let log = MockActionLogRepo::new();
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo.clone(),
        log.clone(),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap();

    let recorded = log.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].ply, 0);
    assert_eq!(recorded[0].player, Color::White);
    assert!(recorded[0].clock_white_ms.is_none());
    assert!(recorded[0].clock_black_ms.is_none());

    let snapshots = repo.updated_games();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].ply, 1);
    assert_eq!(snapshots[0].side_to_move, Some(Color::Black));
    assert!(snapshots[0].clock_white_ms.is_none());
}

#[tokio::test]
async fn append_failure_surfaces_as_storage_error_without_corrupting_the_session() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(
        game_id,
        standard_session(),
        repo.clone(),
        Arc::new(FailingAppendLog),
        Arc::new(NoopHook),
        TimeControl::Unlimited,
    );

    // The move applies in memory but its log append fails, surfacing as a
    // storage error rather than a game error.
    let err = handle
        .submit_action(Color::White, mv("e2e4"))
        .await
        .unwrap_err();
    assert!(matches!(err, GameSessionError::Storage(_)));

    // The in-memory session is not rolled back: the move is live, so it is now
    // Black's turn. (At-least-once: the move is live but was not durably
    // recorded; recovery replays only what was logged.)
    assert_eq!(
        handle.status().await.unwrap(),
        mcs_core::GameStatus::Ongoing
    );
    // White moving again is rejected as out of turn — proof the apply advanced
    // the side to move rather than being undone.
    let out_of_turn = handle
        .submit_action(Color::White, mv("d2d4"))
        .await
        .unwrap_err();
    assert!(matches!(
        out_of_turn,
        GameSessionError::Game(GameError::NotYourTurn)
    ));

    // A failed append wrote no snapshot (the actor returns before updating it),
    // and the rejected out-of-turn move is not recorded either.
    assert!(repo.updated_games().is_empty());
}
