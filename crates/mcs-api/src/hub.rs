//! The in-memory registry of running live games.
//!
//! A [`GameHub`] maps each in-progress [`GameId`] to the cloneable
//! [`GameHandle`] that drives its actor. It is the shared rendezvous point
//! between the *creators* of games and the *observers* of games:
//!
//! - The REST game endpoints (#14) spawn a [`GameActor`](mcs_game::GameActor)
//!   for a freshly created game and [`insert`](GameHub::insert) its handle here.
//! - The WebSocket endpoint (#15, see [`crate::ws`]) looks a game up by id with
//!   [`get`](GameHub::get) so a connecting client can stream and submit moves.
//!
//! The hub holds only *live* games (those with a running actor). A finished game
//! whose actor has stopped is removed with [`remove`](GameHub::remove); its
//! durable record lives in storage, not here.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use mcs_domain::GameId;
use mcs_game::GameHandle;

/// A concurrency-safe registry of running [`GameHandle`]s keyed by [`GameId`].
///
/// The hub is cheap to [`Clone`] — every clone shares the same underlying map
/// through an [`Arc`] — so it can be stored in [`AppState`](crate::AppState) and
/// handed to every request. Reads (the common case: a client connecting to an
/// existing game) take a shared lock; the map is mutated only when a game is
/// created or torn down.
///
/// A [`GameHandle`] is itself cheap to clone, so [`get`](GameHub::get) returns
/// an owned handle and releases the lock immediately rather than handing out a
/// guard — callers never hold the hub lock while awaiting the actor.
#[derive(Clone, Default)]
pub struct GameHub {
    games: Arc<RwLock<HashMap<GameId, GameHandle>>>,
}

impl GameHub {
    /// Creates an empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `handle` under `game_id`, returning the handle it replaced, if
    /// any.
    ///
    /// The REST creation endpoints (#14) call this immediately after spawning a
    /// game's actor so the WebSocket endpoint can find it. Re-inserting the same
    /// id (for example after a server-side restart of an actor) replaces the old
    /// handle and returns it for the caller to drop.
    pub fn insert(&self, game_id: GameId, handle: GameHandle) -> Option<GameHandle> {
        self.games
            .write()
            .expect("game hub lock poisoned")
            .insert(game_id, handle)
    }

    /// Returns a clone of the handle for `game_id`, or `None` if no live game is
    /// registered under it.
    ///
    /// The returned handle is independent of the hub: the lock is released
    /// before this method returns, so a caller can interact with the actor
    /// without blocking other hub users.
    #[must_use]
    pub fn get(&self, game_id: GameId) -> Option<GameHandle> {
        self.games
            .read()
            .expect("game hub lock poisoned")
            .get(&game_id)
            .cloned()
    }

    /// Removes and returns the handle for `game_id`, if it was registered.
    ///
    /// Call this once a game's actor has stopped (the game finished or every
    /// other handle was dropped) to release the hub's reference. Dropping the
    /// returned handle, together with every other outstanding clone, lets the
    /// actor task wind down.
    pub fn remove(&self, game_id: GameId) -> Option<GameHandle> {
        self.games
            .write()
            .expect("game hub lock poisoned")
            .remove(&game_id)
    }

    /// Returns the number of live games currently registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.games.read().expect("game hub lock poisoned").len()
    }

    /// Returns `true` if no live games are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for GameHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid printing every handle; the live game count is the useful signal
        // for tracing and satisfies the workspace `missing_debug_implementations`
        // lint without locking for a potentially long format.
        f.debug_struct("GameHub")
            .field("live_games", &self.len())
            .finish()
    }
}
