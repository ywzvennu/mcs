//! The per-game **table side-channel** for session-level (non-board) events.
//!
//! A live game has two distinct streams of activity:
//!
//! - **Board events** — the moves, draws, and results produced by the variant,
//!   broadcast by the game's [`GameActor`](mcs_game::GameActor) as
//!   [`GameEvent`](mcs_game::GameEvent)s. These flow through the
//!   [`GameHub`](crate::GameHub) and are the actor's responsibility.
//! - **Table events** — things that happen *around* the board but are not moves:
//!   today, rematch offers and their answers. These are session-level, do not
//!   touch the game session, and must reach a player's socket even after the
//!   game has finished and its actor may have wound down.
//!
//! The [`TableHub`] is the in-memory registry for the second stream, mirroring
//! the [`GameHub`] one-for-one: a concurrency-safe `Map<GameId, _>` that is
//! cheap to clone (it shares an [`Arc`] internally) and lives on
//! [`AppState`](crate::AppState). Each entry is a [`TableChannel`] bundling a
//! [`broadcast`] sender — to which every WebSocket connection for the game
//! subscribes alongside the actor's [`GameEvent`] stream — with the single piece
//! of per-table state the rematch protocol needs: the **pending rematch offer**,
//! recorded as the [`Color`] that offered it.
//!
//! # Why a separate channel
//!
//! Rematch is a *finished-game* interaction: the offer happens once the actor
//! has produced its terminal [`GameEvent`] and the board is over. Routing it
//! through the actor's board-event broadcast would conflate "what changed on the
//! board" with "what the two players are negotiating about the table", and would
//! tie the offer's lifetime to the actor's. A dedicated channel keeps the
//! board protocol untouched and lets the offer outlive the board.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use mcs_core::Color;
use mcs_domain::GameId;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// How many table events the per-game broadcast buffers before a slow consumer
/// loses the oldest.
///
/// Table events are rare (a handful per game at most: an offer, maybe a
/// decline, an accept), so a small buffer is ample. A socket that somehow lags
/// past it observes a [`Lagged`](broadcast::error::RecvError::Lagged), which the
/// WS loop already absorbs by resyncing rather than dropping the connection.
const TABLE_CHANNEL_CAPACITY: usize = 16;

/// A session-level event broadcast to every socket watching a game's *table*
/// (as opposed to its board).
///
/// Published on a game's [`TableChannel`] and forwarded by each connection to
/// its socket as the matching [`ServerMessage`](crate::ws::ServerMessage)
/// rematch frame. Distinct from a board
/// [`GameEvent`](mcs_game::GameEvent): a `TableEvent` never changes the game
/// session, it only reflects the rematch negotiation that happens once a game
/// has finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableEvent {
    /// A player offered a rematch of the finished game.
    RematchOffered {
        /// The color, in the finished game, of the player who offered.
        by: Color,
    },
    /// The other player accepted the pending rematch offer; a new game exists.
    RematchAccepted {
        /// The id of the freshly created rematch game. Both clients open
        /// `/ws/game/{game_id}` on it to start playing.
        game_id: GameId,
    },
    /// A player declined the pending rematch offer; the table is clear again.
    RematchDeclined {
        /// The color, in the finished game, of the player who declined.
        by: Color,
    },
}

/// The per-game table channel: a [`broadcast`] sender plus the pending rematch
/// offer.
///
/// Held behind an [`Arc`] in the [`TableHub`] so every connection for the game
/// shares one channel and one view of the pending offer. The sender is retained
/// even with no live subscribers (it is the publish endpoint); a
/// [`subscribe`](TableChannel::subscribe) hands out a fresh receiver.
///
/// The pending offer is a `Mutex<Option<Color>>`: `Some(color)` means `color`
/// has an outstanding rematch offer awaiting the opponent's answer, `None` means
/// the table is clear. A [`std::sync::Mutex`] is sufficient — every access is a
/// brief, synchronous read-modify-write with no `.await` held across the guard.
#[derive(Debug)]
pub struct TableChannel {
    /// The broadcast sender every connection for this game subscribes to.
    sender: broadcast::Sender<TableEvent>,
    /// The color with an outstanding rematch offer, or `None` if the table is
    /// clear. Guarded by a plain mutex; never held across an `.await`.
    pending_offer: Mutex<Option<Color>>,
}

impl TableChannel {
    /// Creates an empty table channel with no pending offer.
    fn new() -> Self {
        let (sender, _receiver) = broadcast::channel(TABLE_CHANNEL_CAPACITY);
        Self {
            sender,
            pending_offer: Mutex::new(None),
        }
    }

    /// Subscribes to the table's event stream.
    ///
    /// Each WebSocket connection calls this once and forwards every received
    /// [`TableEvent`] to its socket, alongside the board
    /// [`GameEvent`](mcs_game::GameEvent) stream it already consumes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<TableEvent> {
        self.sender.subscribe()
    }

    /// Publishes `event` to every current subscriber.
    ///
    /// Returns the number of subscribers that received it. A send with no
    /// subscribers is not an error — the offer state is still recorded; it just
    /// means no socket is currently listening.
    pub fn publish(&self, event: TableEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// Returns the color of the player with an outstanding rematch offer, if any.
    #[must_use]
    pub fn pending_offer(&self) -> Option<Color> {
        *self
            .pending_offer
            .lock()
            .expect("table offer lock poisoned")
    }

    /// Records `by` as the player with an outstanding rematch offer, replacing
    /// any previous one.
    pub fn set_pending_offer(&self, by: Color) {
        *self
            .pending_offer
            .lock()
            .expect("table offer lock poisoned") = Some(by);
    }

    /// Clears any outstanding rematch offer, returning the color that had been
    /// offering (or `None` if the table was already clear).
    pub fn clear_pending_offer(&self) -> Option<Color> {
        self.pending_offer
            .lock()
            .expect("table offer lock poisoned")
            .take()
    }

    /// Clears the outstanding offer only if it was made by `by`.
    ///
    /// Returns `true` if an offer by `by` was cleared. Used on disconnect so a
    /// dropped offerer's stale offer is removed without disturbing an offer the
    /// *other* player may have made in the meantime.
    pub fn clear_pending_offer_by(&self, by: Color) -> bool {
        let mut guard = self
            .pending_offer
            .lock()
            .expect("table offer lock poisoned");
        if *guard == Some(by) {
            *guard = None;
            true
        } else {
            false
        }
    }
}

/// A concurrency-safe registry of per-game [`TableChannel`]s keyed by [`GameId`].
///
/// The session-level mirror of the [`GameHub`](crate::GameHub): where the game
/// hub registers the *board* actor for a game, the table hub registers its
/// *table* side-channel. It is cheap to [`Clone`] — every clone shares the same
/// map through an [`Arc`] — so it lives on [`AppState`](crate::AppState) and is
/// handed to every request.
///
/// Channels are created lazily on first use via
/// [`get_or_create`](TableHub::get_or_create): the first connection or rematch
/// action for a game makes the channel, and subsequent ones share it. They are
/// dropped with [`remove`](TableHub::remove) once a game is fully done with.
#[derive(Clone, Default)]
pub struct TableHub {
    tables: Arc<RwLock<HashMap<GameId, Arc<TableChannel>>>>,
}

impl TableHub {
    /// Creates an empty table hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the table channel for `game_id`, creating an empty one if none
    /// exists yet.
    ///
    /// This is the single entry point both the WebSocket connection (to
    /// [`subscribe`](TableChannel::subscribe)) and the rematch action handlers
    /// (to read/record the pending offer and [`publish`](TableChannel::publish))
    /// use, so they always share one channel per game. The returned [`Arc`] is
    /// independent of the hub lock, which is released before this returns.
    #[must_use]
    pub fn get_or_create(&self, game_id: GameId) -> Arc<TableChannel> {
        // Fast path: the channel already exists. A read lock suffices and is the
        // common case (every connection after the first, every rematch action).
        if let Some(channel) = self
            .tables
            .read()
            .expect("table hub lock poisoned")
            .get(&game_id)
            .cloned()
        {
            return channel;
        }

        // Slow path: create under the write lock, re-checking in case a racing
        // caller created it between our read and write.
        let mut tables = self.tables.write().expect("table hub lock poisoned");
        tables
            .entry(game_id)
            .or_insert_with(|| Arc::new(TableChannel::new()))
            .clone()
    }

    /// Returns the table channel for `game_id`, or `None` if none exists.
    #[must_use]
    pub fn get(&self, game_id: GameId) -> Option<Arc<TableChannel>> {
        self.tables
            .read()
            .expect("table hub lock poisoned")
            .get(&game_id)
            .cloned()
    }

    /// Removes and returns the table channel for `game_id`, if registered.
    pub fn remove(&self, game_id: GameId) -> Option<Arc<TableChannel>> {
        self.tables
            .write()
            .expect("table hub lock poisoned")
            .remove(&game_id)
    }

    /// Returns the number of registered table channels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.read().expect("table hub lock poisoned").len()
    }

    /// Returns `true` if no table channels are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for TableHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid locking to print every entry; the live count is the useful
        // signal and satisfies `missing_debug_implementations`.
        f.debug_struct("TableHub")
            .field("live_tables", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_or_create_returns_the_same_channel_for_a_game() {
        let hub = TableHub::new();
        let id = GameId::new();
        let a = hub.get_or_create(id);
        let b = hub.get_or_create(id);
        assert!(Arc::ptr_eq(&a, &b), "the same game shares one channel");
        assert_eq!(hub.len(), 1);
    }

    #[test]
    fn distinct_games_get_distinct_channels() {
        let hub = TableHub::new();
        let a = hub.get_or_create(GameId::new());
        let b = hub.get_or_create(GameId::new());
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(hub.len(), 2);
    }

    #[test]
    fn pending_offer_records_clears_and_reports() {
        let channel = TableChannel::new();
        assert_eq!(channel.pending_offer(), None);

        channel.set_pending_offer(Color::White);
        assert_eq!(channel.pending_offer(), Some(Color::White));

        // Clearing by the wrong color is a no-op.
        assert!(!channel.clear_pending_offer_by(Color::Black));
        assert_eq!(channel.pending_offer(), Some(Color::White));

        // Clearing by the offerer's color clears it.
        assert!(channel.clear_pending_offer_by(Color::White));
        assert_eq!(channel.pending_offer(), None);
    }

    #[test]
    fn clear_pending_offer_returns_the_prior_offerer() {
        let channel = TableChannel::new();
        channel.set_pending_offer(Color::Black);
        assert_eq!(channel.clear_pending_offer(), Some(Color::Black));
        assert_eq!(channel.clear_pending_offer(), None);
    }

    #[tokio::test]
    async fn published_events_reach_subscribers() {
        let channel = TableChannel::new();
        let mut rx = channel.subscribe();
        let game_id = GameId::new();
        channel.publish(TableEvent::RematchAccepted { game_id });
        let received = rx.recv().await.expect("event delivered");
        assert_eq!(received, TableEvent::RematchAccepted { game_id });
    }

    #[test]
    fn remove_drops_the_channel() {
        let hub = TableHub::new();
        let id = GameId::new();
        let _ = hub.get_or_create(id);
        assert!(hub.remove(id).is_some());
        assert!(hub.is_empty());
        assert!(hub.get(id).is_none());
    }
}
