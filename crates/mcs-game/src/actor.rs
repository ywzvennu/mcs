//! The per-game session actor and its cloneable handle.
//!
//! A live game is owned by exactly one asynchronous task — the **actor**. The
//! actor holds the `Box<dyn GameSession>` and is the only thing that ever
//! touches it, which lets many connections (both players plus any number of
//! spectators) interact with one game concurrently without locking the session:
//! every interaction is funnelled through a command channel and the actor
//! services them one at a time.
//!
//! Callers never see the actor directly. They hold a [`GameHandle`], a cheap,
//! clonable proxy that sends [`Command`]s over an `mpsc` channel and awaits the
//! reply. The handle is the public API of this crate.

use std::sync::Arc;

use mcs_core::{Action, ActionEffect, Color, GameSession, GameStatus, Outcome, PlayerView};
use mcs_domain::{Game, GameId, GameLifecycle};
use mcs_storage::GameRepo;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::error::GameSessionError;
use crate::event::GameEvent;

/// How many `GameEvent`s the broadcast channel retains for slow subscribers
/// before they start observing [`broadcast::error::RecvError::Lagged`].
///
/// A full chess game is a few hundred half-moves, so this comfortably buffers
/// an entire game for a subscriber that connects late or briefly stalls.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// The number of in-flight commands the actor mailbox can hold before senders
/// must wait. Commands are serviced quickly (a single in-memory session
/// operation, plus at most one persistence call on game end), so a modest
/// buffer keeps producers from blocking under normal load.
const COMMAND_CHANNEL_CAPACITY: usize = 64;

/// A request sent from a [`GameHandle`] to the actor task.
///
/// Each variant carries a [`oneshot`] sender on which the actor returns the
/// reply, so the handle method can await the result. Subscribing to the live
/// event stream does not need the actor and so is not a command — see
/// [`GameHandle::subscribe`].
enum Command {
    /// Apply `action` on behalf of `player`, returning the resulting effect.
    SubmitAction {
        player: Color,
        action: Action,
        reply: oneshot::Sender<Result<ActionEffect, GameSessionError>>,
    },
    /// Fetch the view a specific player may observe.
    ViewFor {
        player: Color,
        reply: oneshot::Sender<PlayerView>,
    },
    /// Fetch the spectator view.
    SpectatorView { reply: oneshot::Sender<PlayerView> },
    /// Fetch the legal actions available to `player`.
    LegalActions {
        player: Color,
        reply: oneshot::Sender<Vec<Action>>,
    },
    /// Fetch the current lifecycle status.
    Status { reply: oneshot::Sender<GameStatus> },
    /// Fetch the outcome, if the game has finished.
    Outcome {
        reply: oneshot::Sender<Option<Outcome>>,
    },
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The reply channels are not `Debug`; name the command for tracing
        // without trying to format them.
        let name = match self {
            Command::SubmitAction { player, .. } => return write!(f, "SubmitAction({player})"),
            Command::ViewFor { player, .. } => return write!(f, "ViewFor({player})"),
            Command::SpectatorView { .. } => "SpectatorView",
            Command::LegalActions { player, .. } => return write!(f, "LegalActions({player})"),
            Command::Status { .. } => "Status",
            Command::Outcome { .. } => "Outcome",
        };
        f.write_str(name)
    }
}

/// A cheap, clonable handle to one live game session.
///
/// Cloning a `GameHandle` is inexpensive — it duplicates two channel senders —
/// so every connection interested in a game can hold its own handle. All
/// methods are asynchronous: they forward a [`Command`] to the actor and await
/// the reply.
///
/// The actor runs for as long as at least one `GameHandle` exists. When the
/// last handle is dropped, the actor's command channel closes, the actor task
/// returns, and any outstanding subscribers see the broadcast channel close.
#[derive(Debug, Clone)]
pub struct GameHandle {
    game_id: GameId,
    commands: mpsc::Sender<Command>,
    /// Kept so that [`GameHandle::subscribe`] can produce a receiver even if the
    /// actor is momentarily busy, and so a fresh subscriber never misses the
    /// channel. Subscribing through the actor is still preferred for ordering;
    /// this is the cheap, lock-free fast path.
    events: broadcast::Sender<GameEvent>,
}

impl GameHandle {
    /// Returns the identifier of the game this handle controls.
    #[must_use]
    pub fn game_id(&self) -> GameId {
        self.game_id
    }

    /// Submits `action` on behalf of `player`, advancing the game.
    ///
    /// On success the actor has already applied the action to the session,
    /// broadcast the produced events to all subscribers, and — if the action
    /// finished the game — persisted the final result through the injected
    /// [`GameRepo`]. The returned [`ActionEffect`] mirrors what the session
    /// produced.
    ///
    /// # Errors
    ///
    /// - [`GameSessionError::Game`] if the session rejected the action (illegal,
    ///   out of turn, finished, or malformed);
    /// - [`GameSessionError::Storage`] if the action finished the game but the
    ///   result could not be persisted;
    /// - [`GameSessionError::ActorUnavailable`] if the actor task has stopped.
    pub async fn submit_action(
        &self,
        player: Color,
        action: Action,
    ) -> Result<ActionEffect, GameSessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SubmitAction {
                player,
                action,
                reply,
            })
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)?;
        response
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)?
    }

    /// Returns the view `player` is permitted to observe.
    ///
    /// # Errors
    ///
    /// [`GameSessionError::ActorUnavailable`] if the actor task has stopped.
    pub async fn view_for(&self, player: Color) -> Result<PlayerView, GameSessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ViewFor { player, reply })
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)?;
        response
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)
    }

    /// Returns the spectator view of the game.
    ///
    /// # Errors
    ///
    /// [`GameSessionError::ActorUnavailable`] if the actor task has stopped.
    pub async fn spectator_view(&self) -> Result<PlayerView, GameSessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SpectatorView { reply })
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)?;
        response
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)
    }

    /// Returns the actions `player` may legally submit right now.
    ///
    /// # Errors
    ///
    /// [`GameSessionError::ActorUnavailable`] if the actor task has stopped.
    pub async fn legal_actions(&self, player: Color) -> Result<Vec<Action>, GameSessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::LegalActions { player, reply })
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)?;
        response
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)
    }

    /// Returns the current lifecycle status of the game.
    ///
    /// # Errors
    ///
    /// [`GameSessionError::ActorUnavailable`] if the actor task has stopped.
    pub async fn status(&self) -> Result<GameStatus, GameSessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Status { reply })
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)?;
        response
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)
    }

    /// Returns the game's outcome, or `None` if it is still ongoing.
    ///
    /// # Errors
    ///
    /// [`GameSessionError::ActorUnavailable`] if the actor task has stopped.
    pub async fn outcome(&self) -> Result<Option<Outcome>, GameSessionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Outcome { reply })
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)?;
        response
            .await
            .map_err(|_| GameSessionError::ActorUnavailable)
    }

    /// Subscribes to the live event stream for this game.
    ///
    /// The returned [`broadcast::Receiver`] yields one [`GameEvent`] per
    /// applied action from the moment of subscription onward. Events emitted
    /// before subscribing are not replayed; a client that needs the current
    /// position should pair this with a [`view_for`](GameHandle::view_for) or
    /// [`spectator_view`](GameHandle::spectator_view) call.
    ///
    /// A subscriber that falls behind by more than the channel's capacity
    /// observes [`broadcast::error::RecvError::Lagged`] and then resumes from
    /// the newest events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<GameEvent> {
        self.events.subscribe()
    }
}

/// The owner of one live [`GameSession`], driven by an asynchronous task.
///
/// A `GameActor` is never held directly by callers. Construct one with
/// [`GameActor::spawn`], which moves the actor onto a Tokio task and returns a
/// [`GameHandle`] for interacting with it.
pub struct GameActor {
    game_id: GameId,
    session: Box<dyn GameSession>,
    repo: Arc<dyn GameRepo>,
    events: broadcast::Sender<GameEvent>,
    /// Set once the actor has persisted the finished game, so a second
    /// game-ending action (which the session would already reject) can never
    /// trigger a redundant write.
    persisted: bool,
}

impl std::fmt::Debug for GameActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Box<dyn GameSession>` is `Debug`, but `Arc<dyn GameRepo>` is not, so
        // we cannot derive this. Name the fields that are useful for tracing.
        f.debug_struct("GameActor")
            .field("game_id", &self.game_id)
            .field("session", &self.session)
            .field("persisted", &self.persisted)
            .finish_non_exhaustive()
    }
}

impl GameActor {
    /// Spawns an actor that owns `session` and returns a handle to it.
    ///
    /// The actor takes ownership of the boxed [`GameSession`] and an
    /// `Arc<dyn GameRepo>` used to persist the final result when the game ends.
    /// It runs on a freshly spawned Tokio task until every [`GameHandle`] is
    /// dropped.
    ///
    /// `game_id` identifies the [`Game`] record this session corresponds to;
    /// the actor loads it, marks it [`GameLifecycle::Finished`] with the
    /// [`Outcome`], and writes it back through `repo` when play concludes.
    #[must_use]
    pub fn spawn(
        game_id: GameId,
        session: Box<dyn GameSession>,
        repo: Arc<dyn GameRepo>,
    ) -> GameHandle {
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        let handle = GameHandle {
            game_id,
            commands: commands_tx,
            events: events_tx.clone(),
        };

        let actor = GameActor {
            game_id,
            session,
            repo,
            events: events_tx,
            persisted: false,
        };

        tokio::spawn(actor.run(commands_rx));

        handle
    }

    /// The actor's event loop: service commands until the channel closes.
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        while let Some(command) = commands.recv().await {
            self.handle(command).await;
        }
        tracing::debug!(game_id = %self.game_id, "game actor stopped: all handles dropped");
    }

    /// Dispatches a single command.
    async fn handle(&mut self, command: Command) {
        match command {
            Command::SubmitAction {
                player,
                action,
                reply,
            } => {
                let result = self.submit_action(player, &action).await;
                // The receiver may have gone away (e.g. the caller timed out);
                // dropping the reply is harmless.
                let _ = reply.send(result);
            }
            Command::ViewFor { player, reply } => {
                let _ = reply.send(self.session.view_for(player));
            }
            Command::SpectatorView { reply } => {
                let _ = reply.send(self.session.spectator_view());
            }
            Command::LegalActions { player, reply } => {
                let _ = reply.send(self.session.legal_actions(player));
            }
            Command::Status { reply } => {
                let _ = reply.send(self.session.status());
            }
            Command::Outcome { reply } => {
                let _ = reply.send(self.session.outcome());
            }
        }
    }

    /// Applies an action, broadcasts its events, and persists on game end.
    async fn submit_action(
        &mut self,
        player: Color,
        action: &Action,
    ) -> Result<ActionEffect, GameSessionError> {
        let effect = self.session.apply(player, action)?;

        // Broadcast to live observers. A send error only means there are no
        // subscribers right now, which is not a failure of the action.
        let event = GameEvent::new(effect.events.clone(), effect.status.clone());
        let _ = self.events.send(event);

        // When the game has just finished, durably record the result. This is
        // done after the in-memory apply so that a transient storage failure
        // does not lose the move; callers can retry persistence separately.
        if let GameStatus::Finished(outcome) = &effect.status {
            self.persist_finished(outcome.clone()).await?;
        }

        Ok(effect)
    }

    /// Loads the [`Game`] record, marks it finished with `outcome`, and writes
    /// it back. Idempotent: persists at most once per actor.
    async fn persist_finished(&mut self, outcome: Outcome) -> Result<(), GameSessionError> {
        if self.persisted {
            return Ok(());
        }

        let mut game: Game = self.repo.get(self.game_id).await?;
        game.finish(outcome, OffsetDateTime::now_utc());
        debug_assert_eq!(game.lifecycle, GameLifecycle::Finished);
        self.repo.update(&game).await?;
        self.persisted = true;

        tracing::info!(game_id = %self.game_id, "game finished and persisted");
        Ok(())
    }
}
