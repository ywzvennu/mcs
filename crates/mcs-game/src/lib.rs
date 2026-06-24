//! # mcs-game
//!
//! The live game **session actor** for the Modular Chess Server.
//!
//! A game in progress is a single [`GameSession`](mcs_core::GameSession) that
//! must be mutated in turn by two players and observed by any number of
//! spectators, all over independent network connections. The session itself is
//! `Send + Sync` but `apply` takes `&mut self`, so concurrent access has to be
//! serialized somewhere. This crate does that with the **actor pattern**: each
//! live game is owned by one asynchronous task that is the sole accessor of the
//! session, and every connection talks to it through a cheap, clonable
//! [`GameHandle`].
//!
//! ## Architecture
//!
//! - [`GameActor::spawn`] takes ownership of a `Box<dyn GameSession>`, an
//!   `Arc<dyn GameRepo>`, an `Arc<dyn ActionLogRepo>`, and an
//!   `Arc<dyn GameCompletionHook>`, spawns the actor task, and returns a
//!   [`GameHandle`].
//! - [`GameHandle`] forwards each call over an `mpsc` command channel and
//!   awaits the actor's reply. Cloning it is cheap, so every connection can
//!   hold one.
//! - On every successful [`submit_action`](GameHandle::submit_action) the actor
//!   broadcasts a [`GameEvent`] to all [`subscribe`](GameHandle::subscribe)rs
//!   over a [`tokio::sync::broadcast`] channel, durably records the action in the
//!   injected [`ActionLogRepo`](mcs_storage::ActionLogRepo) and refreshes the
//!   game's live snapshot through the [`GameRepo`](mcs_storage::GameRepo), and —
//!   when the action ends the game — records the final result through that same
//!   [`GameRepo`](mcs_storage::GameRepo).
//! - The actor owns an authoritative [`ClockEngine`]: it deducts elapsed time on
//!   each move, attaches a [`Clock`](mcs_domain::Clock) snapshot to every
//!   broadcast event, and ends the game with a
//!   [`Timeout`](mcs_core::EndReason::Timeout) result — persisted like any other
//!   ending — when a player flags, including one who simply stops moving.
//!
//! ## Variant-agnostic by construction
//!
//! The actor only ever sees the type-erased `mcs-core` boundary types
//! ([`Action`](mcs_core::Action), [`PlayerView`](mcs_core::PlayerView),
//! [`Event`](mcs_core::Event)). It has **no** runtime dependency on any
//! concrete variant; `mcs-variant-standard` is used only by this crate's tests.
//!
//! ## Example
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use std::time::Duration;
//!
//! use mcs_core::{Action, Color, GameSession};
//! use mcs_domain::{GameId, TimeControl};
//! use mcs_game::{GameActor, NoopHook};
//! use mcs_storage::{ActionLogRepo, GameRepo};
//!
//! # async fn run(
//! #     session: Box<dyn GameSession>,
//! #     repo: Arc<dyn GameRepo>,
//! #     action_log: Arc<dyn ActionLogRepo>,
//! #     game_id: GameId,
//! #     action: Action,
//! # ) {
//! let time_control = TimeControl::RealTime {
//!     initial: Duration::from_secs(300),
//!     increment: Duration::from_secs(2),
//! };
//! // No completion side effect here; production wires in a rating updater.
//! let hook = Arc::new(NoopHook);
//! let handle = GameActor::spawn(game_id, session, repo, action_log, hook, time_control);
//!
//! // A connected client subscribes to the live stream...
//! let mut events = handle.subscribe();
//!
//! // ...and a player submits a move.
//! handle.submit_action(Color::White, action).await.unwrap();
//!
//! // The subscriber receives the broadcast event for that move.
//! let update = events.recv().await.unwrap();
//! assert!(!update.is_finished());
//! # }
//! ```
//!
//! ## Recovery
//!
//! Because every applied move is durably logged, a game in progress survives a
//! server restart. [`recover_game`] rebuilds a live actor from storage: it
//! recreates the variant's session, replays the action log to the current
//! position, and spawns a *resumed* actor (see
//! [`GameActor::spawn_resumed`]) seeded with the persisted ply and clocks — so
//! play continues seamlessly and server downtime is never charged to a player.
//!
//! ## Scope
//!
//! This crate contains the session actor and its authoritative clock engine.
//! Matchmaking lives in a separate crate and is intentionally not implemented
//! here.
#![doc(html_root_url = "https://docs.rs/mcs-game")]

mod actor;
mod clock;
mod completion;
mod error;
mod event;
pub mod matchmaking;
mod recovery;
mod time_source;

pub use actor::{ClockRemaining, GameActor, GameHandle, GameSnapshot};
pub use clock::ClockEngine;
pub use completion::{GameCompletionHook, NoopHook};
pub use error::GameSessionError;
pub use event::GameEvent;
pub use matchmaking::{Matchmaker, MatchmakingError, Pairing, SubmitOutcome};
pub use recovery::{recover_game, RecoveryError};
pub use time_source::{SystemTimeSource, TimeSource};

#[cfg(test)]
mod tests;
