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

use mcs_core::{
    Action, ActionEffect, Color, EndReason, GameSession, GameStatus, Outcome, PlayerView,
};
use mcs_domain::{Game, GameId, GameLifecycle, TimeControl};
use mcs_storage::GameRepo;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::clock::ClockEngine;
use crate::completion::GameCompletionHook;
use crate::error::GameSessionError;
use crate::event::GameEvent;
use crate::time_source::{SystemTimeSource, TimeSource};

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
    /// Invoked once, after the finished result is persisted, so subsystems such
    /// as ratings or payouts can react without the actor depending on them.
    hook: Arc<dyn GameCompletionHook>,
    events: broadcast::Sender<GameEvent>,
    /// The authoritative clock for this game, or `None` for an unlimited game
    /// (which has no clock to track, never flags, and reports no clock in its
    /// events). For real-time and correspondence games this engine is the source
    /// of truth for remaining time and flag detection.
    clock: Option<ClockEngine>,
    /// The actor's source of "now" and of flag-deadline sleeps. Injected so
    /// tests can drive time deterministically.
    time: Box<dyn TimeSource>,
    /// Set when the actor itself ended the game on time (a flag), since the
    /// underlying [`GameSession`] has no notion of timeouts and still reports
    /// itself ongoing. Holds the timeout [`Outcome`] so later queries and
    /// rejected actions reflect the finished result.
    timed_out: Option<Outcome>,
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
    ///
    /// `time_control` arms the authoritative clock: the actor deducts elapsed
    /// time on each move, includes a [`Clock`](mcs_domain::Clock) snapshot in
    /// every broadcast [`GameEvent`], and — for real-time and correspondence
    /// games — ends the game with a [`EndReason::Timeout`] result if the side
    /// to move flags, even if that player simply stops moving.
    ///
    /// `hook` is run exactly once when the game finishes, immediately after the
    /// final record is persisted (see [`GameCompletionHook`]). Callers that want
    /// no completion side effect pass an `Arc<NoopHook>`.
    #[must_use]
    pub fn spawn(
        game_id: GameId,
        session: Box<dyn GameSession>,
        repo: Arc<dyn GameRepo>,
        hook: Arc<dyn GameCompletionHook>,
        time_control: TimeControl,
    ) -> GameHandle {
        Self::spawn_with_time_source(
            game_id,
            session,
            repo,
            hook,
            time_control,
            Box::new(SystemTimeSource),
        )
    }

    /// Like [`spawn`](GameActor::spawn) but with an injected [`TimeSource`].
    ///
    /// Used by tests to drive "now" and flag-deadline sleeps deterministically;
    /// production code uses [`spawn`](GameActor::spawn), which supplies the real
    /// wall clock.
    pub(crate) fn spawn_with_time_source(
        game_id: GameId,
        session: Box<dyn GameSession>,
        repo: Arc<dyn GameRepo>,
        hook: Arc<dyn GameCompletionHook>,
        time_control: TimeControl,
        time: Box<dyn TimeSource>,
    ) -> GameHandle {
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        let handle = GameHandle {
            game_id,
            commands: commands_tx,
            events: events_tx.clone(),
        };

        // An unlimited game has genuinely no clock to track or report; every
        // other time control gets an authoritative engine, started for the side
        // to move while the game is still ongoing.
        let clock = if time_control.is_unlimited() {
            None
        } else {
            let mut clock = ClockEngine::new(&time_control);
            if !session.status().is_finished() {
                clock.start(session.to_move(), time.now());
            }
            Some(clock)
        };

        let actor = GameActor {
            game_id,
            session,
            repo,
            hook,
            events: events_tx,
            clock,
            time,
            timed_out: None,
            persisted: false,
        };

        tokio::spawn(actor.run(commands_rx));

        handle
    }

    /// The actor's event loop.
    ///
    /// It services commands until the channel closes, and — while a real-time or
    /// correspondence clock is running — concurrently waits on the current flag
    /// deadline so a player who stops moving still loses on time. When the
    /// deadline elapses the actor re-validates the flag against its
    /// [`TimeSource`] before ending the game, so a spuriously early wake is
    /// harmless.
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        loop {
            match self.flag_deadline() {
                Some(deadline) => {
                    tokio::select! {
                        // Bias towards commands so a move that refreshes the
                        // clock is not pre-empted by a stale deadline.
                        biased;
                        maybe_command = commands.recv() => match maybe_command {
                            Some(command) => self.handle(command).await,
                            None => break,
                        },
                        () = self.time.sleep_until(deadline) => {
                            self.check_flag().await;
                        }
                    }
                }
                None => match commands.recv().await {
                    Some(command) => self.handle(command).await,
                    None => break,
                },
            }
        }
        tracing::debug!(game_id = %self.game_id, "game actor stopped: all handles dropped");
    }

    /// Returns `true` if the game is over for any reason — the session reached a
    /// terminal position, or the actor ended it on time.
    fn is_over(&self) -> bool {
        self.timed_out.is_some() || self.session.status().is_finished()
    }

    /// The game's effective status, accounting for an actor-declared timeout
    /// that the underlying session does not know about.
    fn effective_status(&self) -> GameStatus {
        match &self.timed_out {
            Some(outcome) => GameStatus::Finished(outcome.clone()),
            None => self.session.status(),
        }
    }

    /// The game's effective outcome, accounting for a timeout.
    fn effective_outcome(&self) -> Option<Outcome> {
        self.timed_out.clone().or_else(|| self.session.outcome())
    }

    /// The instant at which the side to move flags, if the clock is running and
    /// the game is still ongoing.
    fn flag_deadline(&self) -> Option<OffsetDateTime> {
        if self.is_over() {
            return None;
        }
        self.clock.as_ref().and_then(ClockEngine::flag_deadline)
    }

    /// Re-checks the clock against the current instant and, if a side has
    /// flagged, ends the game on time. Called both when the flag timer fires and
    /// after every accepted action.
    async fn check_flag(&mut self) {
        if self.is_over() {
            return;
        }
        let Some(clock) = self.clock.as_ref() else {
            return;
        };
        let now = self.time.now();
        if let Some(flagged) = clock.flagged(now) {
            let outcome = Outcome::win(flagged.opposite(), EndReason::Timeout);
            self.finish_on_time(outcome, now).await;
        }
    }

    /// Ends a still-running game with `outcome` at `now`: records the timeout,
    /// broadcasts a final finished event, and persists the result.
    async fn finish_on_time(&mut self, outcome: Outcome, now: OffsetDateTime) {
        self.timed_out = Some(outcome.clone());
        let status = GameStatus::Finished(outcome.clone());
        // Freeze the clock snapshot at the flag instant for the final event.
        let snapshot = self.clock.as_ref().map(|c| c.snapshot(now));
        let event = match snapshot {
            Some(clock) => GameEvent::with_clock(Vec::new(), status, clock),
            None => GameEvent::new(Vec::new(), status),
        };
        let _ = self.events.send(event);

        if let Err(error) = self.persist_finished(outcome).await {
            tracing::error!(
                game_id = %self.game_id,
                %error,
                "failed to persist timeout result",
            );
        }
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
                // A flagged game offers no legal actions.
                self.check_flag().await;
                let actions = if self.is_over() {
                    Vec::new()
                } else {
                    self.session.legal_actions(player)
                };
                let _ = reply.send(actions);
            }
            Command::Status { reply } => {
                self.check_flag().await;
                let _ = reply.send(self.effective_status());
            }
            Command::Outcome { reply } => {
                self.check_flag().await;
                let _ = reply.send(self.effective_outcome());
            }
        }
    }

    /// Applies an action, updates the clock, broadcasts the resulting events
    /// (with a live clock snapshot), and persists on game end.
    ///
    /// Before applying, it re-checks the clock: a player who tries to move after
    /// already flagging loses on time rather than having their late move
    /// accepted. After a successful, non-finishing move it records the elapsed
    /// time against the mover and starts the opponent's clock.
    async fn submit_action(
        &mut self,
        player: Color,
        action: &Action,
    ) -> Result<ActionEffect, GameSessionError> {
        // A move that arrives after the player has already flagged must not be
        // accepted; end the game on time first.
        self.check_flag().await;
        if self.is_over() {
            return Err(GameSessionError::Game(mcs_core::GameError::Finished));
        }

        let now = self.time.now();
        let effect = self.session.apply(player, action)?;

        // Advance the clock: the mover spends their elapsed time and gains the
        // increment, then the opponent's clock starts. Only do this while the
        // game continues; a finishing move stops the clock entirely.
        let clock_snapshot = if let Some(clock) = self.clock.as_mut() {
            if effect.status.is_finished() {
                Some(clock.snapshot(now))
            } else {
                clock.on_move(player, now);
                Some(clock.snapshot(now))
            }
        } else {
            None
        };

        // Broadcast to live observers. A send error only means there are no
        // subscribers right now, which is not a failure of the action.
        let event = match clock_snapshot {
            Some(snapshot) => {
                GameEvent::with_clock(effect.events.clone(), effect.status.clone(), snapshot)
            }
            None => GameEvent::new(effect.events.clone(), effect.status.clone()),
        };
        let _ = self.events.send(event);

        // When the game has just finished, durably record the result. This is
        // done after the in-memory apply so that a transient storage failure
        // does not lose the move; callers can retry persistence separately.
        if let GameStatus::Finished(outcome) = &effect.status {
            self.persist_finished(outcome.clone()).await?;
        }

        Ok(effect)
    }

    /// Loads the [`Game`] record, marks it finished with `outcome`, writes it
    /// back, and runs the completion hook. Idempotent: persists — and runs the
    /// hook — at most once per actor.
    ///
    /// The hook is invoked only after the record is durably persisted, so a
    /// consumer (e.g. a rating updater) sees the finished game in storage. It
    /// runs on this same actor task; the [`GameCompletionHook`] contract forbids
    /// it from panicking, so a hook failure never disturbs the game.
    async fn persist_finished(&mut self, outcome: Outcome) -> Result<(), GameSessionError> {
        if self.persisted {
            return Ok(());
        }

        let mut game: Game = self.repo.get(self.game_id).await?;
        game.finish(outcome.clone(), OffsetDateTime::now_utc());
        debug_assert_eq!(game.lifecycle, GameLifecycle::Finished);
        self.repo.update(&game).await?;
        self.persisted = true;

        tracing::info!(game_id = %self.game_id, "game finished and persisted");

        // Notify subsystems (ratings, payouts, …) of the result. This runs after
        // the write succeeds so the hook can rely on the finished record being
        // visible, and after `persisted` is set so it fires exactly once.
        self.hook.on_finished(&game, &outcome).await;

        Ok(())
    }
}
