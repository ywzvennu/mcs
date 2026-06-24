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
//! 1. On connect the server sends exactly one [`ServerMessage::Snapshot`] with
//!    the player's current view, the game status, and the connection's color.
//! 2. Thereafter every applied action produces a [`ServerMessage::Update`]
//!    carrying the per-player [`PlayerView`] and the broadcast [`GameEvent`].
//! 3. A client submits play with [`ClientMessage::Submit`]; a rejected action
//!    (illegal, out of turn, finished, or sent by a spectator) comes back as a
//!    [`ServerMessage::Error`] without closing the socket.

use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use mcs_auth::verify_session;
use mcs_core::{Action, Color, GameStatus, PlayerView};
use mcs_domain::GameId;
use mcs_game::{GameEvent, GameHandle, GameSessionError};

use crate::error::ApiError;
use crate::state::AppState;

/// The version of the WebSocket game protocol implemented by this module.
///
/// It is included in the opening [`ServerMessage::Snapshot`] so a client can
/// detect a server it does not understand and refuse to proceed. Bump it on any
/// breaking change to the [`ClientMessage`] / [`ServerMessage`] schema.
pub const PROTOCOL_VERSION: u32 = 1;

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

/// A message sent **from the server to a client** over the game socket.
///
/// JSON, internally tagged on `"type"`. The first frame is always a
/// [`ServerMessage::Snapshot`]; subsequent frames are [`ServerMessage::Update`]
/// or [`ServerMessage::Error`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The opening frame: the current game state from the connection's
    /// perspective.
    Snapshot {
        /// The protocol version the server speaks (see [`PROTOCOL_VERSION`]).
        protocol_version: u32,
        /// The connection's player view of the current position. For a
        /// spectator this is the public spectator view.
        view: PlayerView,
        /// The game's lifecycle status at connection time.
        status: GameStatus,
        /// The color this connection plays, or `None` for a spectator.
        your_color: Option<Color>,
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

/// The query string of the WebSocket handshake: `?token=<jwt>`.
#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    /// The session JWT, validated exactly like the `Authorization: Bearer`
    /// token of a REST request. Supplied in the query because browsers cannot
    /// set request headers on a WebSocket handshake.
    token: String,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Builds the WebSocket sub-router: `GET /ws/game/{id}`.
pub fn ws_router() -> axum::Router<AppState> {
    use axum::routing::get;
    axum::Router::new().route("/ws/game/{id}", get(game_socket))
}

/// The `GET /ws/game/{id}` handler: authenticate, resolve the role, then upgrade.
///
/// Authentication and role resolution happen *before* the upgrade so a failure
/// is a normal HTTP error response (401/404) the client can read, rather than a
/// dropped socket. Only once the caller is known to be a valid player or
/// spectator of an existing, live game is the connection handed to
/// [`run_connection`].
async fn game_socket(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ConnectQuery>,
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

    // 3. Resolve the live actor, reviving it from the durable log if this node
    //    has no in-memory handle for it (a cold node, or a game evicted after a
    //    restart). An unknown or already-finished game has no live actor and is
    //    a 404 just as before.
    let handle = state
        .get_or_recover(game_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no live game: {game_id}")))?;

    // 4. Resolve the caller's role from the persisted game record. A user who is
    //    neither player connects as a spectator.
    let game = state.storage().games().get(game_id).await?;
    let role = if game.white == user_id {
        Role::Player(Color::White)
    } else if game.black == user_id {
        Role::Player(Color::Black)
    } else {
        Role::Spectator
    };

    // 5. Upgrade. From here the connection task owns the socket and the handle.
    Ok(upgrade.on_upgrade(move |socket| run_connection(socket, handle, role)))
}

// ---------------------------------------------------------------------------
// Connection task
// ---------------------------------------------------------------------------

/// Drives one upgraded socket for its whole lifetime.
///
/// It first sends the opening [`ServerMessage::Snapshot`], then loops over two
/// concurrent sources with [`tokio::select!`]:
///
/// - **broadcast events** from [`GameHandle::subscribe`], each forwarded as a
///   per-player [`ServerMessage::Update`]; and
/// - **client frames**, dispatched by [`handle_client_message`].
///
/// The task returns — closing the socket — as soon as either side ends: the
/// client disconnects, the actor stops (its broadcast channel closes), or a
/// socket write fails.
async fn run_connection(mut socket: WebSocket, handle: GameHandle, role: Role) {
    // Subscribe *before* taking the snapshot so no event applied between the two
    // can be missed; at worst the client sees a duplicate it can reconcile by
    // status.
    let mut events = handle.subscribe();

    if let Err(error) = send_snapshot(&mut socket, &handle, role).await {
        tracing::debug!(%error, "failed to send game snapshot; closing socket");
        return;
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
                // Slow consumer: skip the gap and keep streaming the latest.
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "ws subscriber lagged; continuing from newest");
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

/// Sends the opening snapshot for `role`'s perspective.
async fn send_snapshot(
    socket: &mut WebSocket,
    handle: &GameHandle,
    role: Role,
) -> Result<(), axum::Error> {
    let view = view_for_role(handle, role).await;
    let status = handle.status().await.unwrap_or(GameStatus::Ongoing);
    let snapshot = ServerMessage::Snapshot {
        protocol_version: PROTOCOL_VERSION,
        view,
        status,
        your_color: role.color(),
    };
    socket.send(snapshot.into_ws_message()).await
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
