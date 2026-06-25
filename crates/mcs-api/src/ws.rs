//! The live-game WebSocket endpoint and its JSON protocol.
//!
//! A connected client opens one WebSocket per game at `GET /ws/game/{id}` and
//! then plays or watches that game in real time. The endpoint bridges the
//! socket to the game's [`GameActor`](mcs_game::GameActor): inbound client
//! frames become [`submit_action`](mcs_game::GameHandle::submit_action) calls,
//! and the actor's broadcast [`GameEvent`]s are pushed back out, each rendered
//! from the connecting player's own [`PlayerView`] so partial-information
//! variants never leak the opponent's hidden state.
//!
//! # Authentication and WebSocket subprotocol handshake
//!
//! Browsers cannot set an `Authorization` header on a `WebSocket` handshake.
//! The preferred browser-compatible mechanism is the `Sec-WebSocket-Protocol`
//! header, which the browser `WebSocket` API exposes as the second constructor
//! argument. The **handshake contract** is:
//!
//! - The client offers **two** subprotocols in decreasing preference order:
//!   1. `mcs.v1` — the application protocol identifier.
//!   2. `mcs.token.<jwt>` — the session JWT embedded in the protocol name.
//!
//!   Example request header:
//!   ```text
//!   Sec-WebSocket-Protocol: mcs.v1, mcs.token.<jwt>
//!   ```
//!
//! - The server extracts the JWT from the `mcs.token.<…>` entry, validates it
//!   with [`verify_session`](mcs_auth::verify_session), and on success **echoes
//!   back only `mcs.v1`** in the upgrade response — the secret token is never
//!   echoed, satisfying the RFC 6455 requirement that the server select one
//!   of the offered protocols without leaking credentials.
//!
//!   Example response header:
//!   ```text
//!   Sec-WebSocket-Protocol: mcs.v1
//!   ```
//!
//! **Token resolution precedence:**
//! 1. `Sec-WebSocket-Protocol` (preferred, browser-compatible).
//! 2. `?token=<jwt>` query parameter (**deprecated** — it leaks into server
//!    logs; keep it only for backward compatibility with non-browser clients).
//!
//! An unauthenticated or invalid request — with neither a valid subprotocol
//! token nor a valid query-param token — is rejected with **401 Unauthorized**
//! before the socket is upgraded, so it never reaches the streaming task.
//!
//! The verified [`UserId`](mcs_domain::UserId) is matched against the game's
//! players to resolve the connection's [`Role`]: White, Black, or a read-only
//! Spectator.
//!
//! # Protocol
//!
//! All frames are JSON text, tagged on a `"type"` field, and versioned by
//! [`PROTOCOL_VERSION`] (echoed in the opening [`ServerMessage::Snapshot`]).
//! The shapes are [`ClientMessage`] (client → server) and [`ServerMessage`]
//! (server → client).
//!
//! 1. On connect the server sends exactly one [`ServerMessage::Snapshot`]. It
//!    fully describes the current position from the connection's perspective —
//!    the player's view, the game status, the connection's color, both clocks,
//!    the half-move count (`ply`), and whose turn it is — so a freshly
//!    *reconnecting* client can resync to the live state in a single frame
//!    without re-rendering the game from scratch.
//! 2. Thereafter every applied action produces a [`ServerMessage::Update`]
//!    carrying the per-player [`PlayerView`] and the broadcast [`GameEvent`].
//! 3. A client submits play with [`ClientMessage::Submit`]; a rejected action
//!    (illegal, out of turn, finished, or sent by a spectator) comes back as a
//!    [`ServerMessage::Error`] without closing the socket.
//!
//! # Draw offers
//!
//! A draw is just an ordinary board action, so it needs no dedicated message:
//! a player offers, accepts, or declines a draw by submitting the variant's
//! `offer_draw` / `accept_draw` / `decline_draw` [`Action`] through
//! [`ClientMessage::Submit`]. The variant emits the corresponding `draw_offered`
//! / `draw_declined` events (and ends the game on accept), which the actor
//! broadcasts as a [`GameEvent`]; both players therefore receive the offer and
//! its answer as a normal [`ServerMessage::Update`]. Accepting finishes the game
//! drawn, delivered to both sides in the same update stream.
//!
//! # Rematch (the live path, #84)
//!
//! Once a game has **finished**, the two players can negotiate a rematch live
//! over their open sockets — no polling the REST endpoint. Unlike a move, a
//! rematch is *not* a board action (the board is over), so it travels on a
//! separate per-game **table side-channel** (see [`crate::table`]) that every
//! connection subscribes to alongside the actor's board-event stream:
//!
//! - A player sends [`ClientMessage::RematchOffer`]; the server records the
//!   pending offer and publishes [`ServerMessage::RematchOffered`] to both
//!   sockets.
//! - The *other* player sends [`ClientMessage::RematchAccept`]; the server
//!   creates a brand-new game with the colours swapped (the lichess convention,
//!   reusing the REST rematch colour logic), clears the offer, and publishes
//!   [`ServerMessage::RematchAccepted`] carrying the new `game_id` so both
//!   clients open `/ws/game/{game_id}`.
//! - Either player may send [`ClientMessage::RematchDecline`] to clear a pending
//!   offer, publishing [`ServerMessage::RematchDeclined`].
//!
//! Only the two players may take part (a spectator is answered with an
//! [`ServerMessage::Error`]); offers are only valid once the game is
//! [`Finished`](GameStatus::Finished); and the offerer cannot accept their own
//! offer. A dropped offerer's pending offer is cleared on disconnect.
//!
//! The offline [`POST /games/{id}/rematch`](crate::challenges) REST endpoint is
//! unchanged and remains the path for a player who is *not* connected: it issues
//! a durable challenge the opponent can accept later. The WS flow here is the
//! live, both-players-present complement to it.
//!
//! # Reconnect & resync
//!
//! The game runs in its own actor, wholly independent of any socket: a
//! disconnect never resigns, pauses, or ends the game, and clocks keep ticking.
//! A reconnecting client therefore simply opens a new socket and is brought up
//! to date by the opening [`ServerMessage::Snapshot`], which reflects every move
//! and clock tick that happened while it was away.
//!
//! Two mechanisms make a brief drop seamless:
//!
//! - **Catch-up replay (`?since_ply=N`).** A client that knows the last ply it
//!   rendered may reconnect with the optional `since_ply` query parameter. After
//!   the snapshot, the server replays the actions recorded *after* ply `N` as
//!   [`ServerMessage::Replay`] frames, so a short gap need not re-render the
//!   board. To avoid leaking hidden information, raw recorded actions are
//!   streamed **only for perfect-information variants** (detected by the
//!   connection's [`PlayerView`] being equal to the public spectator view); for
//!   a hidden-information variant the snapshot alone is the resync and no replay
//!   is sent (it is always correct, just less incremental).
//! - **Self-heal on lag.** If this connection falls so far behind the broadcast
//!   buffer that it observes a [`Lagged`](RecvError::Lagged), the server does
//!   *not* drop it: it sends a fresh [`ServerMessage::Snapshot`] to resync and
//!   then resumes streaming from the newest events.
//!
//! # Cluster routing & failover (#68)
//!
//! Each game has exactly one **owning node**, computed — with no inter-node
//! chatter — by rendezvous-hashing the game id over the live membership set (see
//! [`mcs_cluster`]). Before upgrading, the handler asks "do I own this game?":
//!
//! - **This node owns it** (always true single-node) → it serves the game
//!   locally, reviving the actor from the durable action log on first access via
//!   [`AppState::get_or_recover`]. This *is* the failover path: when a node dies,
//!   its games rehash to survivors, and a survivor revives each one the first
//!   time a client connects — there is no migration step.
//! - **Another node owns it** → the decision depends on the connection's role:
//!   - a **player** is **not** upgraded. The handler answers with **421
//!     Misdirected Request** and a small JSON body naming the owner and the exact
//!     WebSocket URL to reconnect to (the original `token`/`since_ply` query is
//!     preserved), plus a `Location` header. A smart load balancer can route by
//!     game id and never hit this; a plain client simply reconnects to the URL.
//!   - a **spectator** is served **locally, with no redirect** (#109). Spectating
//!     is read-only, so this node does not need the live actor: it builds the
//!     opening snapshot from a **read-only durable reconstruction** (the action
//!     log replayed into a transient session, whose spectator view is read — no
//!     live actor is inserted here) and then subscribes to the game's
//!     `game:{id}:spectator` topic on the cross-node [`EventBus`](mcs_cluster::EventBus),
//!     streaming each published spectator frame to the client.
//!
//! ## Spectator broadcast: at-least-once / ordering caveats (#109)
//!
//! The spectator event bus is **best-effort, at-most-once** (like the underlying
//! `tokio::broadcast` and Redis pub/sub). Every published frame is a *full*
//! spectator snapshot, which makes that acceptable:
//!
//! - A watcher that connects after a move still bootstraps from the durable
//!   reconstruction, so it never starts behind.
//! - A lagged subscriber may **skip** intermediate frames; because each frame is
//!   a complete snapshot, the next delivered frame fully resynchronises it — the
//!   client is never left with a partial position.
//! - Frames a subscriber actually observes arrive in per-game order.
//!
//! The frame carries the public **spectator view**, never a player view, so a
//! hidden-information variant (RBC) stays redacted while ongoing and only reveals
//! the full game once finished — a watcher on any node can never see a player's
//! secret state.
//!
//! ## Failover model & limits
//!
//! Ownership is a pure function of the *current* live set, and actors are rebuilt
//! on demand from the durable log, so failover needs no special code: surviving
//! nodes simply start answering for the dead node's games. The limits follow
//! from that design: each clock resumes from its last persisted remaining time
//! (downtime is not charged); a game with zero recorded actions revives to its
//! initial position; and an in-flight socket to a node that dies must be
//! reconnected by the client (or steered by the load balancer) to the new owner.

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::StreamExt;

use std::sync::Arc;

use mcs_auth::verify_session;
use mcs_cluster::NodeInfo;
use mcs_core::{Action, Color, GameStatus, PlayerView};
use mcs_domain::{Clock, Game, GameId, UserId};
use mcs_game::{
    spectator_topic, GameEvent, GameHandle, GameSessionError, GameSnapshot, SpectatorFrame,
};
use mcs_storage::ActionLogRepo;

use crate::challenges::rematch_colors;
use crate::error::ApiError;
use crate::state::AppState;
use crate::table::{TableChannel, TableEvent};

/// The version of the WebSocket game protocol implemented by this module.
///
/// It is included in the opening [`ServerMessage::Snapshot`] so a client can
/// detect a server it does not understand and refuse to proceed. Bump it on any
/// breaking change to the [`ClientMessage`] / [`ServerMessage`] schema.
///
/// Version `2` enriched the opening snapshot for reconnect/resync — it carries
/// `clock`, `ply`, and `side_to_move` alongside the original
/// `view`/`status`/`your_color` — and added the `?since_ply=N` catch-up
/// mechanism with its [`ServerMessage::Replay`] frames.
///
/// Version `3` adds the live **rematch** flow (#84): the new client frames
/// [`ClientMessage::RematchOffer`], [`ClientMessage::RematchAccept`], and
/// [`ClientMessage::RematchDecline`], and the new server frames
/// [`ServerMessage::RematchOffered`], [`ServerMessage::RematchAccepted`], and
/// [`ServerMessage::RematchDeclined`], delivered over a per-game **table
/// side-channel** (see [`crate::table`]). Draw offers were already supported as
/// ordinary [`ClientMessage::Submit`] actions and are unchanged.
pub const PROTOCOL_VERSION: u32 = 3;

// ---------------------------------------------------------------------------
// Connection role
// ---------------------------------------------------------------------------

/// How the authenticated caller participates in a particular game.
///
/// Resolved once, at connection time, by comparing the verified
/// [`UserId`](mcs_domain::UserId) against the game record's `white`/`black`
/// players. It decides which [`PlayerView`] the connection receives and whether
/// [`ClientMessage::Submit`] is honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The caller is a player; their moves are submitted as this [`Color`].
    Player(Color),
    /// The caller is not a player in this game and may only observe.
    Spectator,
}

impl Role {
    /// The color whose [`PlayerView`] this connection should see.
    ///
    /// A spectator observes the game from White's perspective via the
    /// dedicated spectator view; the color here only selects which `view_for`
    /// to render for a player.
    fn color(self) -> Option<Color> {
        match self {
            Role::Player(color) => Some(color),
            Role::Spectator => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol messages
// ---------------------------------------------------------------------------

/// A message sent **from a client to the server** over the game socket.
///
/// JSON, internally tagged on `"type"`:
///
/// ```json
/// { "type": "submit", "action": { "type": "move", "uci": "e2e4" } }
/// { "type": "chat", "text": "good luck!" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Submit an [`Action`] to the game on behalf of the connection's player.
    ///
    /// The action is the variant's own encoding, so a single message type
    /// covers moves, resignations, and draw offers/answers. Spectator
    /// connections have their `Submit` rejected with a [`ServerMessage::Error`].
    Submit {
        /// The variant-defined action to apply (e.g. a UCI move for standard
        /// chess).
        action: Action,
    },
    /// A free-text chat line. Accepted and acknowledged but not yet broadcast to
    /// other connections; reserved so the schema is stable as table chat lands.
    Chat {
        /// The message text the client typed.
        text: String,
    },
    /// Offer the opponent a rematch of the just-finished game (#84).
    ///
    /// Valid only from a **player** (a spectator is rejected with a
    /// [`ServerMessage::Error`]) and only once the game is
    /// [`Finished`](GameStatus::Finished). Records the pending offer on the game's
    /// table channel and publishes [`ServerMessage::RematchOffered`] to both
    /// players. A second offer simply replaces the pending one.
    RematchOffer,
    /// Accept the opponent's pending rematch offer (#84).
    ///
    /// Valid only from the player who did **not** make the offer: the offerer
    /// cannot accept their own offer (rejected with a [`ServerMessage::Error`]),
    /// and an accept with no pending offer is likewise an error. On success the
    /// server creates a new game with the colours swapped and publishes
    /// [`ServerMessage::RematchAccepted`] carrying its id.
    RematchAccept,
    /// Decline the pending rematch offer (#84).
    ///
    /// Clears the pending offer and publishes [`ServerMessage::RematchDeclined`].
    /// Valid only from a player; a decline with no pending offer is a no-op
    /// acknowledged silently.
    RematchDecline,
}

/// Both sides' remaining clock time, in whole milliseconds, as carried in a
/// [`ServerMessage::Snapshot`].
///
/// Derived from the game-level [`GameSnapshot`]'s [`Clock`] reading taken at the
/// snapshot instant, so a (re)connecting client can render an accurate clock —
/// including a live countdown for the side to move — straight from the opening
/// frame. Absent for an unlimited game, which tracks no clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockView {
    /// White's remaining time, in whole milliseconds, as of the snapshot.
    pub white_ms: u64,
    /// Black's remaining time, in whole milliseconds, as of the snapshot.
    pub black_ms: u64,
}

impl ClockView {
    /// Builds a [`ClockView`] from a domain [`Clock`] snapshot, truncating each
    /// side's remaining duration to whole milliseconds (clocks only ever round
    /// *down*).
    fn from_clock(clock: &Clock) -> Self {
        Self {
            white_ms: whole_millis(clock.white_remaining()),
            black_ms: whole_millis(clock.black_remaining()),
        }
    }
}

/// A message sent **from the server to a client** over the game socket.
///
/// JSON, internally tagged on `"type"`. The first frame is always a
/// [`ServerMessage::Snapshot`]; subsequent frames are [`ServerMessage::Replay`]
/// (only right after a `?since_ply` reconnect), [`ServerMessage::Update`], or
/// [`ServerMessage::Error`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The opening frame: the full current game state from the connection's
    /// perspective, sufficient on its own to (re)synchronise the client.
    ///
    /// Besides the player's `view`, `status`, and `your_color`, it carries the
    /// game-level position metadata — both `clock`s, the half-move count `ply`,
    /// and whose turn it is (`side_to_move`) — sampled atomically from the actor
    /// via [`GameHandle::snapshot`], so the four never disagree. A reconnecting
    /// client can therefore resume from a single frame, with clocks and turn
    /// already advanced to reflect anything that happened while it was away.
    Snapshot {
        /// The protocol version the server speaks (see [`PROTOCOL_VERSION`]).
        protocol_version: u32,
        /// The connection's player view of the current position. For a
        /// spectator this is the public spectator view.
        view: PlayerView,
        /// The game's lifecycle status at the time of the snapshot.
        status: GameStatus,
        /// The color this connection plays, or `None` for a spectator.
        your_color: Option<Color>,
        /// Both sides' remaining time as of the snapshot, or `None` for an
        /// unlimited game. Skipped from the JSON when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clock: Option<ClockView>,
        /// The number of half-moves played so far (the next ply to be recorded).
        ply: u32,
        /// Whose turn it is, or `None` once the game has finished.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        side_to_move: Option<Color>,
    },
    /// A historical action replayed during `?since_ply` catch-up.
    ///
    /// Sent zero or more times immediately after the opening
    /// [`Snapshot`](ServerMessage::Snapshot), one
    /// per action recorded *after* the requested ply, in ascending `ply` order,
    /// so a briefly-dropped client can re-apply just the moves it missed instead
    /// of re-rendering the whole game. Only emitted for perfect-information
    /// variants; see the module-level "Reconnect & resync" notes.
    Replay {
        /// The zero-based half-move index of the replayed action.
        ply: u32,
        /// The color of the player who took the action.
        player: Color,
        /// The variant-defined action that was applied at this ply.
        action: Action,
    },
    /// A live update produced by an applied action.
    ///
    /// `view` is re-rendered for *this* connection's color, so an
    /// imperfect-information variant only ever reveals what the recipient is
    /// allowed to see; `event` carries the variant-defined events and the
    /// post-action status (and clock, when timed).
    Update {
        /// The recipient's view of the position after the action.
        view: PlayerView,
        /// The broadcast event describing what changed.
        event: GameEvent,
    },
    /// A player offered a rematch of the finished game (#84).
    ///
    /// Published on the game's table side-channel and delivered to **both**
    /// players' sockets, so each can show the offer. Carries the color, in the
    /// finished game, of the player who offered.
    RematchOffered {
        /// The color of the player who offered the rematch.
        by: Color,
    },
    /// The pending rematch offer was accepted; a new game now exists (#84).
    ///
    /// Delivered to both players. The `game_id` identifies the freshly created
    /// rematch game (colours swapped from the finished one); both clients open
    /// `/ws/game/{game_id}` to start playing it.
    RematchAccepted {
        /// The id of the new rematch game.
        game_id: GameId,
    },
    /// The pending rematch offer was declined; the table is clear again (#84).
    ///
    /// Delivered to both players. Carries the color, in the finished game, of the
    /// player who declined.
    RematchDeclined {
        /// The color of the player who declined the rematch.
        by: Color,
    },
    /// A recoverable error: the socket stays open and the client may retry.
    ///
    /// Sent when a [`ClientMessage::Submit`] is rejected (illegal, out of turn,
    /// finished, spectator) or a client frame could not be parsed.
    Error {
        /// A human-readable, caller-safe description of what went wrong.
        message: String,
    },
}

impl ServerMessage {
    /// Serializes this message to a JSON text [`Message`] for the socket.
    ///
    /// Serialization of these fixed-shape enums cannot fail in practice; a
    /// surprising failure is surfaced as an [`ServerMessage::Error`] frame
    /// rather than panicking the connection task.
    fn into_ws_message(self) -> Message {
        match serde_json::to_string(&self) {
            Ok(json) => Message::Text(Utf8Bytes::from(json)),
            Err(error) => {
                let fallback = format!(r#"{{"type":"error","message":"{error}"}}"#);
                Message::Text(Utf8Bytes::from(fallback))
            }
        }
    }

    /// Convenience constructor for an [`ServerMessage::Error`] frame.
    fn error(message: impl Into<String>) -> Self {
        ServerMessage::Error {
            message: message.into(),
        }
    }
}

/// Renders a table side-channel [`TableEvent`] as the matching wire frame.
///
/// The table channel carries session-level events (rematch offers and their
/// answers); each maps one-to-one to a [`ServerMessage`] rematch variant. The
/// connection task forwards table events to its socket through this conversion,
/// exactly as it forwards board [`GameEvent`]s through
/// [`ServerMessage::Update`].
impl From<TableEvent> for ServerMessage {
    fn from(event: TableEvent) -> Self {
        match event {
            TableEvent::RematchOffered { by } => ServerMessage::RematchOffered { by },
            TableEvent::RematchAccepted { game_id } => ServerMessage::RematchAccepted { game_id },
            TableEvent::RematchDeclined { by } => ServerMessage::RematchDeclined { by },
        }
    }
}

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

/// The query string of the WebSocket handshake:
/// `[?token=<jwt>][&since_ply=<n>]`.
#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    /// The session JWT supplied as a query parameter.
    ///
    /// **Deprecated.** Prefer passing the JWT via the `Sec-WebSocket-Protocol`
    /// header as `mcs.token.<jwt>` (see the module-level authentication
    /// documentation). The query-parameter path leaks the token into server
    /// access logs and is retained only for backward compatibility with
    /// non-browser clients. It is ignored when a valid subprotocol token is
    /// also present.
    #[serde(default)]
    token: Option<String>,
    /// An optional catch-up cursor: the last ply the reconnecting client had
    /// already rendered. When present, the server replays the actions recorded
    /// *after* this ply (perfect-information variants only) right after the
    /// opening snapshot. Omitted on a first connection.
    #[serde(default)]
    since_ply: Option<u32>,
}

// ---------------------------------------------------------------------------
// Cluster redirect (#68)
// ---------------------------------------------------------------------------

/// The identity of the node that owns a game, as carried in a redirect body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerInfo {
    /// The owning node's stable id.
    pub id: String,
    /// The owning node's externally reachable base URL (e.g. `http://10.0.0.7:8080`).
    pub address: String,
}

/// The JSON body of a **421 Misdirected Request** cluster redirect.
///
/// Returned by the WS handler when the connected node is *not* the rendezvous
/// owner of the requested game. It tells the client exactly where to reconnect:
/// `ws_url` is the owner's address with the game path and the original query
/// (token, `since_ply`) preserved, so a client can switch sockets without
/// re-deriving anything. A `Location` header carries the same URL for HTTP-aware
/// clients and proxies.
///
/// # Routing contract
///
/// - A **load balancer** that understands the game id can route the handshake to
///   the owning node directly and never produce this response.
/// - A **plain client** that connects to any node and receives this body must
///   close the socket attempt and reconnect to `ws_url` (or follow `Location`).
///
/// Ownership can change as membership changes, so a client should always be
/// prepared to receive a redirect and follow it, even mid-game after a failover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectBody {
    /// The node that owns this game and should serve the socket.
    pub owner: OwnerInfo,
    /// The WebSocket URL on the owning node to reconnect to, with the original
    /// query string (token, `since_ply`) preserved.
    pub ws_url: String,
}

/// Builds the 421 redirect [`Response`] pointing at `owner` for `game_id`.
///
/// `query` is the raw, already-validated handshake query string (without the
/// leading `?`); it is appended verbatim so the token and any `since_ply` survive
/// the redirect. The owner `address` is used as a base URL and the
/// `/ws/game/{id}` path is appended; a trailing slash on the address is trimmed
/// so we never emit a double slash.
fn redirect_to_owner(owner: &NodeInfo, game_id: GameId, query: &str) -> Response {
    let base = owner.address.trim_end_matches('/');
    let mut ws_url = format!("{base}/ws/game/{game_id}");
    if !query.is_empty() {
        ws_url.push('?');
        ws_url.push_str(query);
    }

    let body = RedirectBody {
        owner: OwnerInfo {
            id: owner.id.to_string(),
            address: owner.address.clone(),
        },
        ws_url: ws_url.clone(),
    };
    let json = serde_json::to_vec(&body).expect("RedirectBody is always serializable");

    // 421 Misdirected Request: the request reached a node that cannot serve this
    // game. The `Location` header mirrors `ws_url` for HTTP-aware clients/proxies.
    (
        StatusCode::MISDIRECTED_REQUEST,
        [
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (header::LOCATION, ws_url),
        ],
        axum::body::Body::from(json),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Builds the WebSocket sub-router: `GET /ws/game/{id}`.
pub fn ws_router() -> axum::Router<AppState> {
    use axum::routing::get;
    axum::Router::new().route("/ws/game/{id}", get(game_socket))
}

/// The WebSocket application-protocol identifier echoed in the upgrade response.
///
/// The browser `WebSocket` API requires that the server select one of the
/// protocols the client offered; browsers reject a response that selects a
/// protocol the client did not offer. We therefore echo `mcs.v1` — and
/// never echo `mcs.token.<jwt>`, which would leak the credential.
const MCS_PROTOCOL: &str = "mcs.v1";

/// The prefix that marks a bearer-token subprotocol entry.
///
/// The client places the JWT immediately after this prefix:
/// `mcs.token.<jwt>`. Any offered subprotocol whose value starts with this
/// string is treated as a token carrier; only the first match is used.
const TOKEN_PROTOCOL_PREFIX: &str = "mcs.token.";

/// Extracts a JWT from the `Sec-WebSocket-Protocol` offered-protocols list.
///
/// Searches the offered protocols for the first entry whose value starts with
/// [`TOKEN_PROTOCOL_PREFIX`] (`"mcs.token."`), and strips that prefix to
/// return the embedded JWT. Returns `None` when no such entry is present.
fn extract_subprotocol_token(upgrade: &WebSocketUpgrade) -> Option<String> {
    upgrade.requested_protocols().find_map(|hv| {
        let s = hv.to_str().ok()?;
        s.strip_prefix(TOKEN_PROTOCOL_PREFIX).map(str::to_owned)
    })
}

/// The `GET /ws/game/{id}` handler: authenticate, route, resolve the role, then
/// upgrade.
///
/// Authentication, cluster routing, and role resolution all happen *before* the
/// upgrade so a failure is a normal HTTP error response (401/404/421) the client
/// can read, rather than a dropped socket. Only once the caller is known to be a
/// valid player or spectator of an existing, live game **that this node owns** is
/// the connection handed to [`run_connection`].
///
/// Token resolution order:
/// 1. `Sec-WebSocket-Protocol: mcs.v1, mcs.token.<jwt>` (preferred).
/// 2. `?token=<jwt>` query parameter (deprecated fallback).
///
/// The raw query string is taken alongside the parsed [`ConnectQuery`] so that,
/// on a cluster redirect, the `since_ply` parameter can be preserved verbatim
/// in the reconnect URL.
async fn game_socket(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ConnectQuery>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // 1. Parse the path id. A malformed id is a 422, mirroring the REST routes.
    let game_id: GameId = id
        .parse()
        .map_err(|_| ApiError::UnprocessableEntity(format!("invalid game id: {id}")))?;

    // 2. Resolve the session token.
    //
    //    Precedence:
    //    a) `Sec-WebSocket-Protocol` token (`mcs.token.<jwt>`) — preferred,
    //       because browsers can only pass auth via the subprotocol list.
    //    b) `?token=<jwt>` query parameter — deprecated fallback for non-browser
    //       clients; kept for backward compatibility but leaks into access logs.
    //
    //    Either path validates the JWT with `verify_session`; the first
    //    successful parse wins. An absent or invalid token in both places is
    //    a 401, matching the `AuthUser` extractor behaviour.
    let token: String = extract_subprotocol_token(&upgrade)
        .or(query.token)
        .ok_or_else(|| ApiError::Unauthorized("authentication failed".to_owned()))?;
    let claims = verify_session(state.session_config(), &token)?;
    let user_id = claims.sub;

    // Mark the connecting user as active so presence tracks WebSocket sessions
    // in the same way it tracks REST requests (via the AuthUser extractor).
    state.presence().mark_seen(user_id);

    // 3. Resolve the caller's role from the persisted game record. A user who is
    //    neither player connects as a spectator. Resolved *before* the routing
    //    decision because a spectator is served on any node (it does not redirect),
    //    while a player follows the owner-routing path (#109).
    let game = state.storage().games().get(game_id).await?;
    let role = if game.white == user_id {
        Role::Player(Color::White)
    } else if game.black == user_id {
        Role::Player(Color::Black)
    } else {
        Role::Spectator
    };

    // 4. Cluster routing (#68 + #109): does *this* node own the game? Ownership is
    //    the rendezvous owner of the game id over the live membership set.
    //    Single-node the live set is just this node, so this is always true and
    //    no redirect is ever emitted — byte-for-byte the pre-cluster behaviour.
    //
    //    When another node owns the game the two roles diverge:
    //    - a **player** is redirected to the owner (do NOT upgrade), since the
    //      live actor that accepts their moves lives only there; while
    //    - a **spectator** is *not* redirected. Spectating is read-only, so this
    //      node serves it locally from a durable, read-only reconstruction plus
    //      the cross-node spectator event bus — see `run_spectator_connection`.
    let cluster = state.cluster();
    let nodes = cluster.registry().live_nodes().await.map_err(|error| {
        tracing::error!(%game_id, %error, "failed to read cluster membership");
        ApiError::Internal(format!("failed to read cluster membership: {error}"))
    })?;
    let this_node_owns = match mcs_cluster::owner(&game_id.to_string(), &nodes) {
        Some(owner) if owner.id != cluster.this_node().id => {
            if let Role::Player(_) = role {
                tracing::debug!(%game_id, owner = %owner.id, "redirecting player WS to the owning node");
                return Ok(redirect_to_owner(
                    owner,
                    game_id,
                    raw_query.as_deref().unwrap_or(""),
                ));
            }
            // A spectator on a non-owner node: served locally via the bus.
            false
        }
        // This node owns the game, or the live set is momentarily empty (which
        // cannot happen with the local default; for a real registry we still
        // serve locally, since this node is a live member able to recover the
        // game from the durable log).
        _ => true,
    };

    // 4b. Spectator on a node that does NOT own the game (cluster only): serve it
    //     from a durable read-only snapshot plus a subscription to the game's
    //     spectator topic on the event bus, without inserting a live actor here
    //     and without redirecting (#109). Players never reach this branch (they
    //     were redirected above); single-node this branch is never taken because
    //     this node always owns the game.
    if matches!(role, Role::Spectator) && !this_node_owns {
        return serve_remote_spectator(state, game_id, game, upgrade, user_id).await;
    }

    // 5. This node owns the game (the only single-node path). Resolve the live
    //    actor, reviving it from the durable log if this node has no in-memory
    //    handle for it (a cold node, or a game evicted after a restart). An
    //    unknown or already-finished game has no live actor and is a 404 as before.
    let handle = state
        .get_or_recover(game_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no live game: {game_id}")))?;

    // 6. The action-log repo lets a `?since_ply` reconnect replay the moves the
    //    client missed. Cloned out of the state so the connection task owns it.
    let action_log = state.action_log().clone();
    let since_ply = query.since_ply;

    // 7. Resolve (or create) this game's table side-channel (#84): the connection
    //    subscribes to it for live rematch events alongside the board stream, and
    //    the rematch handlers publish onto it. The whole `AppState` is moved in so
    //    a `RematchAccept` can create the swapped follow-up game.
    let table = state.table_hub().get_or_create(game_id);

    // 8. Connection caps (#100): reserve a slot against the global and per-user
    //    WebSocket connection caps *before* upgrading. Over the cap, reject the
    //    upgrade with 429 (and a short Retry-After) rather than opening a socket
    //    we would immediately have to refuse. The returned guard releases the
    //    slot on every close path via its `Drop`. These caps are per node.
    let conn_guard = state.ws_connections().try_open(user_id).ok_or_else(|| {
        tracing::debug!(%user_id, "rejecting WS upgrade: connection cap reached");
        ApiError::TooManyRequests("websocket connection limit reached".to_owned())
    })?;

    // 9. Apply the configured per-message size limit (#99) so the axum runtime
    //    rejects frames above the threshold before they reach the application,
    //    protecting memory against rogue large-frame attacks.
    let max_msg = state.ws_max_message_bytes();
    let upgrade = upgrade.max_message_size(max_msg);

    // 10. Echo the non-secret application-protocol identifier in the response.
    //
    //     `protocols(["mcs.v1"])` tells axum: if the client offered `mcs.v1`,
    //     select it; otherwise select nothing. We never echo `mcs.token.<jwt>`
    //     back — that would leak the credential in the upgrade response and in
    //     any logs that capture HTTP headers. Browsers require the server to
    //     select one of the offered subprotocols, so echoing `mcs.v1` satisfies
    //     both RFC 6455 and the browser's enforcement of the negotiation.
    let upgrade = upgrade.protocols([MCS_PROTOCOL]);

    // 11. Upgrade. From here the connection task owns the socket and the handle.
    Ok(upgrade.on_upgrade(move |socket| {
        run_connection(RunConnection {
            socket,
            handle,
            role,
            action_log,
            since_ply,
            state,
            game,
            table,
            conn_guard,
        })
    }))
}

// ---------------------------------------------------------------------------
// Cross-node spectator serving (#109)
// ---------------------------------------------------------------------------

/// Serves a **spectator** of a game this node does *not* own, without a redirect.
///
/// Spectating is read-only, so a watcher does not need the live actor (which
/// lives on the owning node). Instead this node:
///
/// 1. reserves a per-node WS connection slot and upgrades the socket;
/// 2. sends an opening [`ServerMessage::Snapshot`] built from a **read-only**
///    durable reconstruction — the action log replayed into a *transient*
///    session whose [`spectator_view`](mcs_core::GameSession::spectator_view) is
///    read; no live actor is inserted into this node's hub; and
/// 3. subscribes to the game's spectator topic on the shared event bus and
///    streams each published [`SpectatorFrame`] as a fresh
///    [`ServerMessage::Snapshot`].
///
/// Every published frame is a *full* spectator snapshot, so a watcher that
/// misses the bootstrap or skips a lagged frame still resynchronises on the next
/// one. Players never reach here (they were redirected to the owner); single-node
/// this is never reached because this node always owns the game.
async fn serve_remote_spectator(
    state: AppState,
    game_id: GameId,
    game: Game,
    upgrade: WebSocketUpgrade,
    user_id: UserId,
) -> Result<Response, ApiError> {
    // Connection cap (#100), per node, exactly as the owner path reserves it.
    let conn_guard = state.ws_connections().try_open(user_id).ok_or_else(|| {
        tracing::debug!(%user_id, "rejecting spectator WS upgrade: connection cap reached");
        ApiError::TooManyRequests("websocket connection limit reached".to_owned())
    })?;

    let upgrade = upgrade
        .max_message_size(state.ws_max_message_bytes())
        .protocols([MCS_PROTOCOL]);

    Ok(upgrade.on_upgrade(move |socket| {
        run_spectator_connection(socket, state, game_id, game, conn_guard)
    }))
}

/// Drives one read-only spectator socket for a game served off the owning node.
///
/// Sends an opening snapshot from a durable reconstruction, then forwards every
/// [`SpectatorFrame`] published on the game's bus topic as a
/// [`ServerMessage::Snapshot`]. The task ends when the client disconnects, a
/// socket write fails, or the bus stream closes; it never drives the game.
async fn run_spectator_connection(
    mut socket: WebSocket,
    state: AppState,
    game_id: GameId,
    game: Game,
    conn_guard: crate::limits::WsConnectionGuard,
) {
    // Hold the cap reservation and the active-connections gauge for the whole
    // task, released on every exit path by their `Drop`s.
    let _conn_guard = conn_guard;
    let _ws_guard = WsConnectionGuard::open();

    // Subscribe *before* the bootstrap snapshot so no frame published between the
    // two is missed; a duplicate frame is harmless (each is a full snapshot).
    let mut frames = match state.event_bus().subscribe(&spectator_topic(game_id)).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%game_id, %error, "failed to subscribe spectator to the event bus");
            return;
        }
    };

    // Bootstrap: a read-only spectator snapshot from the durable log. A failure
    // here is logged and the connection still proceeds — the first bus frame is
    // itself a full snapshot, so the watcher resynchronises on it.
    match spectator_snapshot_from_log(&state, &game).await {
        Ok(message) => {
            if socket.send(message.into_ws_message()).await.is_err() {
                return;
            }
        }
        Err(error) => {
            tracing::warn!(%game_id, %error, "failed to build spectator bootstrap snapshot");
        }
    }

    loop {
        tokio::select! {
            // A spectator frame published by the owner node (or this node's local
            // bus). Each is forwarded as a full snapshot.
            frame = frames.next() => match frame {
                Some(bytes) => match SpectatorFrame::from_bytes(&bytes) {
                    Ok(frame) => {
                        let message = spectator_frame_message(frame);
                        if socket.send(message.into_ws_message()).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        // A malformed frame is skipped, not fatal: the next full
                        // frame resynchronises the watcher.
                        tracing::warn!(%game_id, %error, "skipping a malformed spectator frame");
                    }
                },
                // The bus stream ended (every publisher gone / backend dropped).
                None => break,
            },

            // Frames from the client. A spectator may only observe; a submit is
            // rejected without closing, and a Close ends the task.
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(Message::Text(_))) => {
                    let reply = ServerMessage::error("spectators cannot submit actions".to_owned());
                    if socket.send(reply.into_ws_message()).await.is_err() {
                        break;
                    }
                }
                // Binary frames and pings need no spectator-side action.
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Builds the opening [`ServerMessage::Snapshot`] for a remote spectator from a
/// **read-only** durable reconstruction of the game.
///
/// It instantiates a transient [`GameSession`](mcs_core::GameSession) of the
/// game's variant, replays the recorded action log into it, and reads the
/// public [`spectator_view`](mcs_core::GameSession::spectator_view) — so a
/// hidden-information variant is redacted exactly as it is for a local spectator.
/// No live actor is inserted into this node's hub; the session is discarded once
/// the snapshot is built.
///
/// # Errors
///
/// Returns [`ApiError::Internal`] if the variant cannot be instantiated, the log
/// cannot be read, or a recorded action is rejected on replay (a divergent log).
async fn spectator_snapshot_from_log(
    state: &AppState,
    game: &Game,
) -> Result<ServerMessage, ApiError> {
    let mut session = state
        .variants()
        .new_game(&game.variant_id, &game.variant_options)
        .map_err(|error| {
            ApiError::Internal(format!("cannot rebuild spectator session: {error}"))
        })?;

    let recorded = state.action_log().list(game.id).await?;
    let ply = u32::try_from(recorded.len()).unwrap_or(u32::MAX);
    for action in &recorded {
        session
            .apply(action.player, &action.action)
            .map_err(|error| {
                ApiError::Internal(format!(
                    "spectator replay diverged at ply {}: {error}",
                    action.ply
                ))
            })?;
    }

    // The durable game snapshot carries the live clocks (if any); pair it with
    // the reconstructed public view, ply, and status for the opening frame.
    let clock = match (game.clock_white_ms, game.clock_black_ms) {
        (Some(white), Some(black)) => Some(ClockView {
            white_ms: white,
            black_ms: black,
        }),
        _ => None,
    };

    Ok(ServerMessage::Snapshot {
        protocol_version: PROTOCOL_VERSION,
        view: session.spectator_view(),
        status: session.status(),
        your_color: None,
        clock,
        ply,
        side_to_move: (!session.status().is_finished()).then(|| session.to_move()),
    })
}

/// Renders a received [`SpectatorFrame`] as a [`ServerMessage::Snapshot`].
///
/// A spectator frame is a full public snapshot, so it maps onto the snapshot
/// frame (not an `Update`): the watcher always has the complete current position
/// and resynchronises even after a skipped frame. `your_color` is always `None`
/// (a spectator plays no side); `side_to_move` is omitted because the frame
/// carries the full public position the client renders from directly.
fn spectator_frame_message(frame: SpectatorFrame) -> ServerMessage {
    ServerMessage::Snapshot {
        protocol_version: PROTOCOL_VERSION,
        view: frame.view,
        status: frame.status,
        your_color: None,
        clock: frame.clock.as_ref().map(ClockView::from_clock),
        ply: frame.ply,
        side_to_move: None,
    }
}

/// The fully-resolved inputs handed to [`run_connection`] once the handshake has
/// authenticated, routed, and resolved the role.
///
/// Bundled into one struct so the connection task's signature stays readable as
/// it grew the table side-channel, game record, and shared state needed by the
/// live rematch flow (#84). Every field is owned by the task for its lifetime.
struct RunConnection {
    /// The upgraded socket the task drives.
    socket: WebSocket,
    /// The live game actor handle.
    handle: GameHandle,
    /// How this connection participates (player color or spectator).
    role: Role,
    /// The action log, for `?since_ply` catch-up replay.
    action_log: Arc<dyn ActionLogRepo>,
    /// The optional catch-up cursor from the handshake query.
    since_ply: Option<u32>,
    /// Shared application state, used to create the rematch game on accept.
    state: AppState,
    /// The persisted record of *this* (the finished, on rematch) game, the source
    /// of the rematch's variant, time control, rated flag, and swapped colours.
    game: Game,
    /// This game's table side-channel: the live rematch event stream and the
    /// pending-offer state.
    table: Arc<TableChannel>,
    /// The reserved WebSocket connection-cap slot (#100). Held for the whole
    /// connection so its `Drop` releases the global/per-user count on every exit
    /// path; never read, just owned.
    conn_guard: crate::limits::WsConnectionGuard,
}

/// A scope guard that keeps the active-WebSocket-connections gauge accurate
/// (#88).
///
/// Constructing it via [`open`](WsConnectionGuard::open) increments
/// [`WS_CONNECTIONS_ACTIVE`](crate::metrics::WS_CONNECTIONS_ACTIVE); dropping it
/// decrements the gauge. Holding one for the lifetime of the connection task
/// means the gauge is released on *every* exit path — a clean close, a client
/// drop, the actor stopping, or an early-return error — without a decrement at
/// each branch.
struct WsConnectionGuard;

impl WsConnectionGuard {
    /// Records a newly opened connection and returns the guard that will record
    /// its close on drop.
    fn open() -> Self {
        crate::metrics::ws_connection_opened();
        Self
    }
}

impl Drop for WsConnectionGuard {
    fn drop(&mut self) {
        crate::metrics::ws_connection_closed();
    }
}

// ---------------------------------------------------------------------------
// Connection task
// ---------------------------------------------------------------------------

/// Drives one upgraded socket for its whole lifetime.
///
/// It first sends the opening [`ServerMessage::Snapshot`] (optionally followed
/// by `?since_ply` catch-up [`ServerMessage::Replay`] frames), then loops over
/// two concurrent sources with [`tokio::select!`]:
///
/// - **broadcast events** from [`GameHandle::subscribe`], each forwarded as a
///   per-player [`ServerMessage::Update`];
/// - **table events** from the game's table side-channel (#84), each forwarded
///   as the matching [`ServerMessage`] rematch frame; and
/// - **client frames**, dispatched by [`handle_client_message`].
///
/// The connection is purely an observer of the actor: it never drives the
/// game's lifecycle, so its ending — the client disconnects, the actor stops
/// (its broadcast channel closes), or a socket write fails — closes the socket
/// but leaves the game running untouched. A subscriber that lags past the
/// broadcast buffer is *resynced* with a fresh snapshot rather than dropped.
///
/// On exit, if this connection is the player who left a rematch offer
/// outstanding, that stale offer is cleared from the table so the opponent is
/// never left able to "accept" an offer whose maker has gone (#84).
async fn run_connection(conn: RunConnection) {
    let RunConnection {
        mut socket,
        handle,
        role,
        action_log,
        since_ply,
        state,
        game,
        table,
        conn_guard,
    } = conn;

    // Hold the connection-cap reservation (#100) for the whole task: its `Drop`
    // releases the global/per-user slot on every exit path. Bound to a named
    // local so it is not dropped early.
    let _conn_guard = conn_guard;

    // Track this socket on the active-connections gauge (#88). The guard's
    // `Drop` decrements it, so *every* exit path of this task — a snapshot send
    // failure, a client disconnect, an actor stop — releases the gauge exactly
    // once with no manual bookkeeping at each `return`/`break`.
    let _ws_guard = WsConnectionGuard::open();

    // Subscribe *before* taking the snapshot so no event applied between the two
    // can be missed; at worst the client sees a duplicate it can reconcile by
    // status. Subscribe to the table side-channel in the same spirit, so a
    // rematch event published during the handshake is delivered once we loop.
    let mut events = handle.subscribe();
    let mut table_events = table.subscribe();

    if let Err(error) = send_snapshot(&mut socket, &handle, role).await {
        tracing::debug!(%error, "failed to send game snapshot; closing socket");
        return;
    }

    // A reconnecting client may ask to be caught up on the moves it missed.
    if let Some(since_ply) = since_ply {
        if let Err(error) =
            send_catch_up(&mut socket, &handle, role, action_log.as_ref(), since_ply).await
        {
            tracing::debug!(%error, "failed to send catch-up replay; closing socket");
            return;
        }
    }

    loop {
        tokio::select! {
            // Live board updates from the actor.
            received = events.recv() => match received {
                Ok(event) => {
                    if forward_event(&mut socket, &handle, role, event).await.is_err() {
                        break;
                    }
                }
                // Slow consumer: rather than drop the gap (or the client), send a
                // fresh full snapshot so the client resyncs to the live state,
                // then resume streaming from the newest events.
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "ws subscriber lagged; resyncing with a snapshot");
                    if send_snapshot(&mut socket, &handle, role).await.is_err() {
                        break;
                    }
                }
                // The actor stopped; nothing more will ever arrive.
                Err(RecvError::Closed) => break,
            },

            // Live table updates (rematch offers/answers) from the side-channel.
            received = table_events.recv() => match received {
                Ok(event) => {
                    let message = ServerMessage::from(event);
                    if socket.send(message.into_ws_message()).await.is_err() {
                        break;
                    }
                }
                // A lagged table consumer simply drops the gap: table events are
                // self-describing (each fully states the new offer state), so the
                // next one resyncs the client without a special path.
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "ws table subscriber lagged; dropping the gap");
                }
                // The table channel was removed; nothing more will arrive here.
                Err(RecvError::Closed) => {}
            },

            // Frames from the client.
            incoming = socket.recv() => match incoming {
                Some(Ok(message)) => {
                    let ctx = ClientContext {
                        handle: &handle,
                        role,
                        state: &state,
                        game: &game,
                        table: &table,
                    };
                    if !handle_client_message(&mut socket, &ctx, message).await {
                        break;
                    }
                }
                // A receive error or a closed stream both mean the client is gone.
                Some(Err(_)) | None => break,
            },
        }
    }

    // Disconnect cleanup (#84): if this player left a rematch offer pending, clear
    // it so the opponent cannot accept an offer whose maker has gone. Clearing
    // *by this color* leaves any offer the opponent made untouched.
    if let Role::Player(color) = role {
        if table.clear_pending_offer_by(color) {
            tracing::debug!(%color, "cleared the disconnecting player's pending rematch offer");
        }
    }
}

/// Sends the opening (or resync) snapshot for `role`'s perspective.
///
/// Combines the connection's own [`PlayerView`] with the game-level
/// [`GameSnapshot`] read atomically from the actor, so the frame's view,
/// clocks, ply, and side to move are mutually consistent. If the actor has
/// stopped, the snapshot degrades gracefully to an empty view and a default
/// ongoing status rather than failing the connection.
async fn send_snapshot(
    socket: &mut WebSocket,
    handle: &GameHandle,
    role: Role,
) -> Result<(), axum::Error> {
    let view = view_for_role(handle, role).await;
    let snapshot = handle.snapshot().await.ok();
    let message = snapshot_message(view, role, snapshot.as_ref());
    socket.send(message.into_ws_message()).await
}

/// Builds the [`ServerMessage::Snapshot`] for a connection from its rendered
/// `view` and the game-level [`GameSnapshot`].
///
/// Pure and total: a `None` game snapshot (the actor stopped between the view
/// read and this call) degrades to an ongoing status with no clock, ply `0`, and
/// no side to move, so the connection still receives a well-formed frame. Both
/// the opening snapshot and the lag-resync go through here, so they are
/// guaranteed identical in shape.
fn snapshot_message(
    view: PlayerView,
    role: Role,
    snapshot: Option<&GameSnapshot>,
) -> ServerMessage {
    ServerMessage::Snapshot {
        protocol_version: PROTOCOL_VERSION,
        view,
        status: snapshot.map_or(GameStatus::Ongoing, |s| s.status.clone()),
        your_color: role.color(),
        clock: snapshot.and_then(|s| s.clock.as_ref().map(ClockView::from_clock)),
        ply: snapshot.map_or(0, |s| s.ply),
        side_to_move: snapshot.and_then(|s| s.side_to_move),
    }
}

/// Replays the actions recorded *after* `since_ply` to a reconnecting client.
///
/// For a **perfect-information** variant — one where this connection's
/// [`PlayerView`] is identical to the public spectator view — each missed
/// action is streamed as a [`ServerMessage::Replay`] frame, in ascending ply
/// order, so the client can re-apply just the moves it dropped. For a
/// **hidden-information** variant a raw action payload could reveal an
/// opponent's secret move, so nothing is replayed: the opening
/// [`ServerMessage::Snapshot`] (already sent, and rendered for this player's
/// own view) is the always-correct, leak-free resync.
///
/// A failure to read the log is logged and treated as "no catch-up": the client
/// still has the full snapshot, so the connection proceeds rather than closing.
async fn send_catch_up(
    socket: &mut WebSocket,
    handle: &GameHandle,
    role: Role,
    action_log: &dyn ActionLogRepo,
    since_ply: u32,
) -> Result<(), axum::Error> {
    // Only players have a private view to protect; a spectator already sees the
    // public view, so replay is always safe for them. For a player, replay only
    // when the variant is perfect-information for this game.
    if let Role::Player(_) = role {
        if !is_perfect_information(handle, role).await {
            tracing::debug!("skipping since_ply replay for a hidden-information variant");
            return Ok(());
        }
    }

    let actions = match action_log.list(handle.game_id()).await {
        Ok(actions) => actions,
        Err(error) => {
            tracing::warn!(%error, "failed to read action log for catch-up; relying on snapshot");
            return Ok(());
        }
    };

    for recorded in actions.into_iter().filter(|a| a.ply > since_ply) {
        let replay = ServerMessage::Replay {
            ply: recorded.ply,
            player: recorded.player,
            action: recorded.action,
        };
        socket.send(replay.into_ws_message()).await?;
    }

    Ok(())
}

/// Returns `true` when the game is perfect-information *for this connection*:
/// the player's own [`PlayerView`] equals the public spectator view, so no
/// hidden state exists that a raw action replay could leak.
///
/// This is the safe, variant-agnostic check the catch-up path uses: it asks the
/// live session itself rather than hard-coding a list of variants, so a new
/// hidden-information variant is protected automatically. A spectator is treated
/// as perfect-information (they already see the public view); a transient actor
/// error is treated as *not* perfect-information, the conservative default.
async fn is_perfect_information(handle: &GameHandle, role: Role) -> bool {
    match role.color() {
        Some(color) => {
            match (handle.view_for(color).await, handle.spectator_view().await) {
                (Ok(player_view), Ok(spectator_view)) => player_view == spectator_view,
                // If we cannot prove the views are equal, assume they differ.
                _ => false,
            }
        }
        None => true,
    }
}

/// Forwards one broadcast [`GameEvent`] as a per-player [`ServerMessage::Update`].
///
/// The view is re-fetched for this connection's role so that, in an
/// imperfect-information variant, each recipient only ever sees their own legal
/// view of the new position.
async fn forward_event(
    socket: &mut WebSocket,
    handle: &GameHandle,
    role: Role,
    event: GameEvent,
) -> Result<(), axum::Error> {
    let view = view_for_role(handle, role).await;
    let update = ServerMessage::Update { view, event };
    socket.send(update.into_ws_message()).await
}

/// The per-connection context a client frame is dispatched against.
///
/// Borrows everything a [`ClientMessage`] handler may need: the live actor
/// [`handle`](ClientContext::handle) for board actions, the connection's
/// [`role`](ClientContext::role), and — for the live rematch flow (#84) — the
/// shared [`state`](ClientContext::state) (to create the follow-up game), the
/// finished [`game`](ClientContext::game) record (its terms and colours), and
/// the [`table`](ClientContext::table) side-channel (the pending offer and the
/// publish endpoint). Grouped so the handlers take one parameter, not six.
struct ClientContext<'a> {
    /// The live game actor handle.
    handle: &'a GameHandle,
    /// How this connection participates (player color or spectator).
    role: Role,
    /// Shared application state, used to create the rematch game on accept.
    state: &'a AppState,
    /// The persisted record of this game (terms and colours for the rematch).
    game: &'a Game,
    /// This game's table side-channel (pending offer + publish endpoint).
    table: &'a TableChannel,
}

/// Handles one inbound client frame.
///
/// Returns `true` to keep the connection open, `false` to close it (the client
/// sent a `Close` frame). Application-level rejections (an illegal move, a
/// spectator trying to act, an unparsable text frame) are reported back as
/// [`ServerMessage::Error`] frames and keep the socket open.
async fn handle_client_message(
    socket: &mut WebSocket,
    ctx: &ClientContext<'_>,
    message: Message,
) -> bool {
    match message {
        Message::Text(text) => {
            match serde_json::from_str::<ClientMessage>(&text) {
                Ok(client_message) => {
                    if let Some(reply) = process_client_message(ctx, client_message).await {
                        // A failed write means the socket is gone; stop.
                        if socket.send(reply.into_ws_message()).await.is_err() {
                            return false;
                        }
                    }
                    true
                }
                Err(error) => {
                    let reply = ServerMessage::error(format!("malformed message: {error}"));
                    socket.send(reply.into_ws_message()).await.is_ok()
                }
            }
        }
        // Binary frames are not part of the protocol; ignore them but stay open.
        Message::Binary(_) => true,
        // Respond to pings to keep middleboxes happy; axum auto-replies, so just
        // continue. Pongs need no action.
        Message::Ping(_) | Message::Pong(_) => true,
        // The client asked to close; honour it.
        Message::Close(_) => false,
    }
}

/// Applies a parsed [`ClientMessage`], returning an optional reply frame.
///
/// `Submit` from a player is forwarded to the actor; its broadcast `Update` is
/// delivered through the subscription, so a successful submit yields no direct
/// reply (`None`). The rematch frames go through the table side-channel: a
/// successful offer/accept/decline publishes a [`TableEvent`] to *both* players
/// (so it returns `None` here — the publishing reaches this socket too), while a
/// rule violation yields an [`ServerMessage::Error`]. Errors and spectator
/// submits yield an [`ServerMessage::Error`]. `Chat` is acknowledged silently.
async fn process_client_message(
    ctx: &ClientContext<'_>,
    message: ClientMessage,
) -> Option<ServerMessage> {
    match message {
        ClientMessage::Submit { action } => match ctx.role {
            Role::Player(color) => match ctx.handle.submit_action(color, action).await {
                // The resulting Update reaches the client via the broadcast
                // subscription, so there is nothing to reply directly.
                Ok(_) => None,
                Err(error) => Some(ServerMessage::error(submit_error_message(&error))),
            },
            Role::Spectator => Some(ServerMessage::error(
                "spectators cannot submit actions".to_owned(),
            )),
        },
        // Chat is accepted; broadcasting it to the table is future work.
        ClientMessage::Chat { .. } => None,
        ClientMessage::RematchOffer => process_rematch_offer(ctx).await,
        ClientMessage::RematchAccept => process_rematch_accept(ctx).await,
        ClientMessage::RematchDecline => process_rematch_decline(ctx).await,
    }
}

/// Handles a [`ClientMessage::RematchOffer`].
///
/// Records the offer and publishes [`TableEvent::RematchOffered`] to both
/// players, returning `None` on success (the published event reaches this socket
/// too). Rejected — with an [`ServerMessage::Error`] — for a spectator or while
/// the game is not yet finished.
async fn process_rematch_offer(ctx: &ClientContext<'_>) -> Option<ServerMessage> {
    let color = match player_color(ctx.role) {
        Ok(color) => color,
        Err(reply) => return Some(*reply),
    };
    if let Err(reply) = ensure_finished(ctx.handle).await {
        return Some(*reply);
    }

    ctx.table.set_pending_offer(color);
    ctx.table.publish(TableEvent::RematchOffered { by: color });
    None
}

/// Handles a [`ClientMessage::RematchAccept`].
///
/// The accepter must be a player, the game must be finished, and there must be a
/// pending offer made by the *other* player (a player cannot accept their own
/// offer). On success it creates the swapped follow-up game, clears the offer,
/// and publishes [`TableEvent::RematchAccepted`] with the new id. Any rule
/// violation yields an [`ServerMessage::Error`].
async fn process_rematch_accept(ctx: &ClientContext<'_>) -> Option<ServerMessage> {
    let color = match player_color(ctx.role) {
        Ok(color) => color,
        Err(reply) => return Some(*reply),
    };
    if let Err(reply) = ensure_finished(ctx.handle).await {
        return Some(*reply);
    }

    match ctx.table.pending_offer() {
        // The offerer cannot accept their own offer.
        Some(offerer) if offerer == color => Some(ServerMessage::error(
            "you cannot accept your own rematch offer".to_owned(),
        )),
        Some(_) => {
            // Create the new game with colours swapped from the finished one,
            // reusing the REST rematch colour convention.
            let (white, black) = rematch_colors(ctx.game);
            match ctx
                .state
                .create_and_spawn_game(
                    white,
                    black,
                    &ctx.game.variant_id,
                    ctx.game.time_control.clone(),
                    ctx.game.rated,
                    ctx.game.variant_options.clone(),
                )
                .await
            {
                Ok(new_game) => {
                    ctx.table.clear_pending_offer();
                    ctx.table.publish(TableEvent::RematchAccepted {
                        game_id: new_game.id,
                    });
                    None
                }
                Err(error) => Some(ServerMessage::error(error.safe_detail().to_owned())),
            }
        }
        // Nothing to accept.
        None => Some(ServerMessage::error(
            "there is no pending rematch offer to accept".to_owned(),
        )),
    }
}

/// Handles a [`ClientMessage::RematchDecline`].
///
/// Clears any pending offer and publishes [`TableEvent::RematchDeclined`] to both
/// players. A decline from a spectator is rejected; a decline with no pending
/// offer is acknowledged silently (`None`).
async fn process_rematch_decline(ctx: &ClientContext<'_>) -> Option<ServerMessage> {
    let color = match player_color(ctx.role) {
        Ok(color) => color,
        Err(reply) => return Some(*reply),
    };

    if ctx.table.clear_pending_offer().is_some() {
        ctx.table.publish(TableEvent::RematchDeclined { by: color });
    }
    None
}

/// Resolves the connection's player [`Color`], or — for a spectator — the
/// rejection [`ServerMessage`] to send back: only the two players may take part
/// in a rematch (#84).
///
/// The error arm carries the reply as `Ok`/`Err` mirrored into the caller's
/// `Option<ServerMessage>` return; it is returned `Box`ed so the rarely-taken
/// error path does not bloat the common `Result` on the stack.
fn player_color(role: Role) -> Result<Color, Box<ServerMessage>> {
    match role {
        Role::Player(color) => Ok(color),
        Role::Spectator => Err(Box::new(ServerMessage::error(
            "spectators cannot offer or answer a rematch".to_owned(),
        ))),
    }
}

/// Confirms the game is [`Finished`](GameStatus::Finished), or returns the
/// rejection [`ServerMessage`] (boxed) to send back: a rematch is only valid once
/// the game is over (#84).
///
/// A transient actor error is treated conservatively as "not finished" so a
/// rematch is never offered against a game whose status cannot be confirmed.
async fn ensure_finished(handle: &GameHandle) -> Result<(), Box<ServerMessage>> {
    match handle.status().await {
        Ok(status) if status.is_finished() => Ok(()),
        Ok(_) => Err(Box::new(ServerMessage::error(
            "a rematch can only be offered once the game has finished".to_owned(),
        ))),
        Err(_) => Err(Box::new(ServerMessage::error(
            "the game status is unavailable; try again".to_owned(),
        ))),
    }
}

/// Fetches the [`PlayerView`] appropriate to `role`.
///
/// A player sees their own `view_for`; a spectator sees the public spectator
/// view. If the actor has stopped, an empty JSON view is returned rather than
/// failing the whole connection.
async fn view_for_role(handle: &GameHandle, role: Role) -> PlayerView {
    let view = match role {
        Role::Player(color) => handle.view_for(color).await,
        Role::Spectator => handle.spectator_view().await,
    };
    view.unwrap_or_else(|_| PlayerView::new(serde_json::Value::Null))
}

/// Maps a [`GameSessionError`] to a caller-safe message for an `Error` frame.
///
/// Reuses the crate's [`ApiError`] mapping so the wording matches the REST
/// surface, and never leaks internal detail (storage/serialization failures
/// collapse to a generic message).
fn submit_error_message(error: &GameSessionError) -> String {
    let api_error: ApiError = match error {
        GameSessionError::Game(game_error) => game_error.clone().into(),
        GameSessionError::Storage(_) => {
            ApiError::Internal("failed to persist game result".to_owned())
        }
        GameSessionError::ActorUnavailable => {
            ApiError::Conflict("the game is no longer active".to_owned())
        }
    };
    api_error.safe_detail().to_owned()
}

/// Truncates a remaining-time [`Duration`](std::time::Duration) to whole
/// milliseconds for the wire, saturating rather than overflowing on an absurdly
/// large budget. A clock should only ever round *down*, so the sub-millisecond
/// remainder is dropped.
fn whole_millis(remaining: std::time::Duration) -> u64 {
    u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mcs_core::Outcome;

    use super::*;

    fn sample_snapshot() -> GameSnapshot {
        GameSnapshot {
            status: GameStatus::Ongoing,
            clock: Some(Clock::with_times(
                Duration::from_millis(120_500),
                Duration::from_millis(90_000),
                None,
            )),
            ply: 7,
            side_to_move: Some(Color::Black),
        }
    }

    /// The resync/opening builder carries the enriched fields straight through:
    /// clocks (truncated to whole ms), ply, side to move, and the version.
    #[test]
    fn snapshot_message_carries_enriched_fields() {
        let view = PlayerView::new(serde_json::json!({ "fen": "startpos" }));
        let snapshot = sample_snapshot();
        let message = snapshot_message(view.clone(), Role::Player(Color::Black), Some(&snapshot));

        match message {
            ServerMessage::Snapshot {
                protocol_version,
                view: got_view,
                status,
                your_color,
                clock,
                ply,
                side_to_move,
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(got_view, view);
                assert_eq!(status, GameStatus::Ongoing);
                assert_eq!(your_color, Some(Color::Black));
                assert_eq!(
                    clock,
                    Some(ClockView {
                        white_ms: 120_500,
                        black_ms: 90_000,
                    })
                );
                assert_eq!(ply, 7);
                assert_eq!(side_to_move, Some(Color::Black));
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    /// A `None` game snapshot (the actor stopped) still yields a well-formed
    /// frame: ongoing status, no clock, ply 0, no side to move.
    #[test]
    fn snapshot_message_degrades_gracefully_without_a_game_snapshot() {
        let view = PlayerView::new(serde_json::Value::Null);
        let message = snapshot_message(view, Role::Spectator, None);

        match message {
            ServerMessage::Snapshot {
                status,
                your_color,
                clock,
                ply,
                side_to_move,
                ..
            } => {
                assert_eq!(status, GameStatus::Ongoing);
                assert_eq!(your_color, None);
                assert_eq!(clock, None);
                assert_eq!(ply, 0);
                assert_eq!(side_to_move, None);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    /// A finished game's snapshot reports the outcome and no side to move.
    #[test]
    fn snapshot_message_reflects_a_finished_game() {
        let outcome = Outcome::win(Color::White, mcs_core::EndReason::Checkmate);
        let snapshot = GameSnapshot {
            status: GameStatus::Finished(outcome.clone()),
            clock: None,
            ply: 42,
            side_to_move: None,
        };
        let view = PlayerView::new(serde_json::Value::Null);
        let message = snapshot_message(view, Role::Player(Color::White), Some(&snapshot));

        match message {
            ServerMessage::Snapshot {
                status,
                side_to_move,
                ply,
                ..
            } => {
                assert_eq!(status, GameStatus::Finished(outcome));
                assert_eq!(side_to_move, None);
                assert_eq!(ply, 42);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    #[test]
    fn clock_view_truncates_to_whole_millis() {
        let clock = Clock::with_times(
            Duration::from_micros(1_500), // 1.5 ms -> 1 ms (rounds down)
            Duration::from_millis(250),
            None,
        );
        let view = ClockView::from_clock(&clock);
        assert_eq!(view.white_ms, 1);
        assert_eq!(view.black_ms, 250);
    }
}
