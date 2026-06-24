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
//! # Authentication
//!
//! Browsers cannot set an `Authorization` header on a `WebSocket` handshake, so
//! the session JWT is supplied as the `token` query parameter:
//! `GET /ws/game/{id}?token=<jwt>`. It is validated with
//! [`verify_session`](mcs_auth::verify_session) — the same check the
//! [`AuthUser`](crate::AuthUser) extractor performs for REST routes — *before*
//! the socket is upgraded, so an unauthenticated or invalid request is rejected
//! with **401 Unauthorized** and never reaches the streaming task. The verified
//! [`UserId`](mcs_domain::UserId) is then matched against the game's players to
//! resolve the connection's [`Role`]: White, Black, or a read-only Spectator.
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
//! - **Another node owns it** → the handler does **not** upgrade. It answers with
//!   **421 Misdirected Request** and a small JSON body naming the owner and the
//!   exact WebSocket URL to reconnect to (the original `token`/`since_ply` query
//!   is preserved), plus a `Location` header. A smart load balancer can route by
//!   game id and never hit this; a plain client simply reconnects to the URL.
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

use std::sync::Arc;

use mcs_auth::verify_session;
use mcs_cluster::NodeInfo;
use mcs_core::{Action, Color, GameStatus, PlayerView};
use mcs_domain::{Clock, GameId};
use mcs_game::{GameEvent, GameHandle, GameSessionError, GameSnapshot};
use mcs_storage::ActionLogRepo;

use crate::error::ApiError;
use crate::state::AppState;

/// The version of the WebSocket game protocol implemented by this module.
///
/// It is included in the opening [`ServerMessage::Snapshot`] so a client can
/// detect a server it does not understand and refuse to proceed. Bump it on any
/// breaking change to the [`ClientMessage`] / [`ServerMessage`] schema.
///
/// Version `2` enriches the opening snapshot for reconnect/resync — it now
/// carries `clock`, `ply`, and `side_to_move` alongside the original
/// `view`/`status`/`your_color` — and adds the `?since_ply=N` catch-up
/// mechanism with its [`ServerMessage::Replay`] frames.
pub const PROTOCOL_VERSION: u32 = 2;

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

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

/// The query string of the WebSocket handshake:
/// `?token=<jwt>[&since_ply=<n>]`.
#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    /// The session JWT, validated exactly like the `Authorization: Bearer`
    /// token of a REST request. Supplied in the query because browsers cannot
    /// set request headers on a WebSocket handshake.
    token: String,
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

/// The `GET /ws/game/{id}` handler: authenticate, route, resolve the role, then
/// upgrade.
///
/// Authentication, cluster routing, and role resolution all happen *before* the
/// upgrade so a failure is a normal HTTP error response (401/404/421) the client
/// can read, rather than a dropped socket. Only once the caller is known to be a
/// valid player or spectator of an existing, live game **that this node owns** is
/// the connection handed to [`run_connection`].
///
/// The raw query string is taken alongside the parsed [`ConnectQuery`] so that,
/// on a cluster redirect, the token and any `since_ply` can be preserved verbatim
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

    // 2. Verify the session token from the query string. Any failure is a 401
    //    with a single generic message, matching the `AuthUser` extractor.
    let claims = verify_session(state.session_config(), &query.token)?;
    let user_id = claims.sub;

    // 3. Cluster routing (#68): does *this* node own the game? Ownership is the
    //    rendezvous owner of the game id over the live membership set. Single-node
    //    the live set is just this node, so this is always true and no redirect is
    //    ever emitted — byte-for-byte the pre-cluster behaviour. If another node
    //    owns it, redirect (do NOT upgrade), preserving the original query.
    let cluster = state.cluster();
    let nodes = cluster.registry().live_nodes().await.map_err(|error| {
        tracing::error!(%game_id, %error, "failed to read cluster membership");
        ApiError::Internal(format!("failed to read cluster membership: {error}"))
    })?;
    if let Some(owner) = mcs_cluster::owner(&game_id.to_string(), &nodes) {
        if owner.id != cluster.this_node().id {
            tracing::debug!(%game_id, owner = %owner.id, "redirecting WS to the owning node");
            return Ok(redirect_to_owner(
                owner,
                game_id,
                raw_query.as_deref().unwrap_or(""),
            ));
        }
    }
    // An empty live set (no owner resolvable) cannot happen with the local
    // default, and for a real registry it means membership is momentarily empty;
    // we fall through and serve locally rather than reject, since this node is, by
    // construction, a live member able to recover the game from the durable log.

    // 4. Resolve the live actor, reviving it from the durable log if this node
    //    has no in-memory handle for it (a cold node, or a game evicted after a
    //    restart). An unknown or already-finished game has no live actor and is
    //    a 404 just as before.
    let handle = state
        .get_or_recover(game_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no live game: {game_id}")))?;

    // 5. Resolve the caller's role from the persisted game record. A user who is
    //    neither player connects as a spectator.
    let game = state.storage().games().get(game_id).await?;
    let role = if game.white == user_id {
        Role::Player(Color::White)
    } else if game.black == user_id {
        Role::Player(Color::Black)
    } else {
        Role::Spectator
    };

    // 6. The action-log repo lets a `?since_ply` reconnect replay the moves the
    //    client missed. Cloned out of the state so the connection task owns it.
    let action_log = state.action_log().clone();
    let since_ply = query.since_ply;

    // 7. Upgrade. From here the connection task owns the socket and the handle.
    Ok(upgrade
        .on_upgrade(move |socket| run_connection(socket, handle, role, action_log, since_ply)))
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
///   per-player [`ServerMessage::Update`]; and
/// - **client frames**, dispatched by [`handle_client_message`].
///
/// The connection is purely an observer of the actor: it never drives the
/// game's lifecycle, so its ending — the client disconnects, the actor stops
/// (its broadcast channel closes), or a socket write fails — closes the socket
/// but leaves the game running untouched. A subscriber that lags past the
/// broadcast buffer is *resynced* with a fresh snapshot rather than dropped.
async fn run_connection(
    mut socket: WebSocket,
    handle: GameHandle,
    role: Role,
    action_log: Arc<dyn ActionLogRepo>,
    since_ply: Option<u32>,
) {
    // Subscribe *before* taking the snapshot so no event applied between the two
    // can be missed; at worst the client sees a duplicate it can reconcile by
    // status.
    let mut events = handle.subscribe();

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
            // Live updates from the actor.
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

            // Frames from the client.
            incoming = socket.recv() => match incoming {
                Some(Ok(message)) => {
                    if !handle_client_message(&mut socket, &handle, role, message).await {
                        break;
                    }
                }
                // A receive error or a closed stream both mean the client is gone.
                Some(Err(_)) | None => break,
            },
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

/// Handles one inbound client frame.
///
/// Returns `true` to keep the connection open, `false` to close it (the client
/// sent a `Close` frame). Application-level rejections (an illegal move, a
/// spectator trying to act, an unparsable text frame) are reported back as
/// [`ServerMessage::Error`] frames and keep the socket open.
async fn handle_client_message(
    socket: &mut WebSocket,
    handle: &GameHandle,
    role: Role,
    message: Message,
) -> bool {
    match message {
        Message::Text(text) => {
            match serde_json::from_str::<ClientMessage>(&text) {
                Ok(client_message) => {
                    if let Some(reply) = process_client_message(handle, role, client_message).await
                    {
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
/// reply (`None`). Errors and spectator submits yield an
/// [`ServerMessage::Error`]. `Chat` is acknowledged silently for now.
async fn process_client_message(
    handle: &GameHandle,
    role: Role,
    message: ClientMessage,
) -> Option<ServerMessage> {
    match message {
        ClientMessage::Submit { action } => match role {
            Role::Player(color) => match handle.submit_action(color, action).await {
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
