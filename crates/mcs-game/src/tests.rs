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
use mcs_storage::{GameRepo, StorageError, StorageResult};
use mcs_variant_standard::register;
use mcs_variant_standard::wire::StandardAction;
use time::OffsetDateTime;

use crate::{GameActor, GameSessionError};

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

    /// Returns the games recorded by `update`, in order.
    fn updated_games(&self) -> Vec<Game> {
        self.updates.lock().unwrap().clone()
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
}

/// A [`GameRepo`] whose `update` always fails, to exercise the persistence
/// error path on game end.
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

    async fn update(&self, _game: &Game) -> StorageResult<()> {
        Err(StorageError::Backend("write failed".to_owned()))
    }

    async fn list_recent(&self, _limit: u32) -> StorageResult<Vec<Game>> {
        unreachable!()
    }

    async fn list_for_user(&self, _user: UserId, _limit: u32) -> StorageResult<Vec<Game>> {
        unreachable!()
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
    let handle = GameActor::spawn(game_id, standard_session(), repo.clone());

    let effect = play_fools_mate(&handle).await;
    assert!(effect.status.is_finished());

    // The session itself reports the expected checkmate.
    let outcome = handle.outcome().await.unwrap();
    assert_eq!(
        outcome,
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );

    // The actor persisted exactly once, transitioning the record to Finished
    // with that same outcome.
    let updated = repo.updated_games();
    assert_eq!(
        updated.len(),
        1,
        "the game should be persisted exactly once"
    );
    assert_eq!(updated[0].id, game_id);
    assert_eq!(updated[0].lifecycle, GameLifecycle::Finished);
    assert_eq!(
        updated[0].outcome,
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
}

#[tokio::test]
async fn events_are_broadcast_to_subscribers() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(game_id, standard_session(), repo);

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
    let handle = GameActor::spawn(game_id, standard_session(), repo.clone());

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
    let handle = GameActor::spawn(game_id, standard_session(), repo.clone());

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
    let handle = GameActor::spawn(game_id, standard_session(), repo.clone());

    play_fools_mate(&handle).await;

    // Any further action is refused by the finished session...
    let err = handle
        .submit_action(Color::White, mv("a2a3"))
        .await
        .unwrap_err();
    assert!(matches!(err, GameSessionError::Game(GameError::Finished)));

    // ...and the rejected action triggers no additional persistence.
    assert_eq!(repo.updated_games().len(), 1);
}

#[tokio::test]
async fn views_and_legal_actions_are_served_through_the_handle() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(game_id, standard_session(), repo);

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
    let handle = GameActor::spawn(game_id, standard_session(), repo);

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
    let handle = GameActor::spawn(game_id, standard_session(), repo);
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

#[tokio::test]
async fn dropping_all_handles_stops_the_actor() {
    let game_id = GameId::new();
    let repo = MockGameRepo::with_game(active_game(game_id));
    let handle = GameActor::spawn(game_id, standard_session(), repo);

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
