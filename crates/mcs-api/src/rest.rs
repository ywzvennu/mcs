//! REST endpoints for seeks, games, and public profiles.
//!
//! These handlers mirror the request/response shape of the lichess HTTP API:
//! a client posts a seek, the matchmaker either queues it or pairs it into a
//! live game, and the game is then read back over plain HTTP (`GET /games/{id}`,
//! `GET /games`) or streamed over the WebSocket endpoint (#15).
//!
//! | Method & path        | Auth | Purpose |
//! |----------------------|------|---------|
//! | `POST /seeks`        | yes  | Post a seek; queue it or pair it into a game. |
//! | `DELETE /seeks/{id}` | yes  | Cancel one of the caller's own open seeks. |
//! | `GET /games/{id}`    | no   | Fetch a single game record by id. |
//! | `GET /games`         | no   | List the most recently created games. |
//! | `GET /users/{id}`    | no   | Public profile for a user. |
//! | `GET /profile`       | yes  | Public profile for the authenticated caller. |
//!
//! # Payment middleware (x402)
//!
//! Game creation is the natural place to charge for play. The route layout
//! keeps `POST /seeks` (and the game spawn it triggers) on its own
//! [`seek_router`], so a future x402 payment layer can wrap *only* that
//! sub-router — see the comment on [`seek_router`] — without touching the read
//! endpoints or the auth/WS routers.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mcs_core::VariantOptions;
use mcs_domain::{
    ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Seek, SeekId, TimeControl, User,
    UserId,
};
use mcs_game::{GameActor, Pairing, SubmitOutcome};

use crate::error::{ApiError, ApiResult};
use crate::extract::AuthUser;
use crate::state::AppState;

/// The default page size for `GET /games` when no `limit` is supplied.
const DEFAULT_GAMES_LIMIT: u32 = 20;

/// The largest page size `GET /games` will honour, clamping larger requests.
const MAX_GAMES_LIMIT: u32 = 100;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /seeks`.
///
/// The fields reuse the domain value objects directly, so an invalid time
/// control or colour preference is rejected at deserialization time with a
/// **422 Unprocessable Entity** before the handler runs.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSeekRequest {
    /// The variant the caller wants to play (e.g. `"standard"`).
    pub variant_id: String,
    /// The time control the caller wants to play under.
    pub time_control: TimeControl,
    /// Which side the caller would prefer.
    pub color_preference: ColorPreference,
}

/// The two outcomes of `POST /seeks`, tagged on `"status"`.
///
/// ```json
/// { "status": "queued", "seek_id": "…" }
/// { "status": "paired", "game": { … } }
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CreateSeekResponse {
    /// No compatible seek was waiting; this one is now in the pool. The client
    /// can later cancel it with `DELETE /seeks/{id}`.
    Queued {
        /// The id of the freshly queued seek.
        seek_id: SeekId,
    },
    /// A compatible seek was found and a live game was created. The client
    /// should open the game socket at `/ws/game/{game.id}`.
    Paired {
        /// The created game record.
        game: GameDto,
    },
}

/// The public, serialized view of a [`Game`] record.
///
/// This is the wire shape returned by every game endpoint. It is a thin,
/// explicit projection of [`Game`] so the HTTP contract does not silently drift
/// when the domain type gains internal fields.
#[derive(Debug, Clone, Serialize)]
pub struct GameDto {
    /// The game's stable identifier; open the socket at `/ws/game/{id}`.
    pub id: GameId,
    /// The variant being played.
    pub variant_id: String,
    /// The user playing White.
    pub white: UserId,
    /// The user playing Black.
    pub black: UserId,
    /// The game's server-side lifecycle state.
    pub lifecycle: GameLifecycle,
    /// The time control in force.
    pub time_control: TimeControl,
    /// When the game record was created (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<Game> for GameDto {
    fn from(game: Game) -> Self {
        Self {
            id: game.id,
            variant_id: game.variant_id,
            white: game.white,
            black: game.black,
            lifecycle: game.lifecycle,
            time_control: game.time_control,
            created_at: game.created_at,
        }
    }
}

/// Response body for `GET /games`.
#[derive(Debug, Clone, Serialize)]
pub struct GameListResponse {
    /// The most recently created games, newest first.
    pub games: Vec<GameDto>,
}

/// Query parameters for `GET /games`.
#[derive(Debug, Clone, Deserialize)]
pub struct ListGamesQuery {
    /// Maximum number of games to return. Clamped to [`MAX_GAMES_LIMIT`];
    /// defaults to [`DEFAULT_GAMES_LIMIT`] when absent.
    pub limit: Option<u32>,
}

/// A user's **public** profile.
///
/// Deliberately a narrow projection of [`User`]: it exposes only the address,
/// the optional username, and the creation time. No session, nonce, or other
/// sensitive state is ever included.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileDto {
    /// The user's stable identifier.
    pub id: UserId,
    /// The user's Ethereum address (lowercase, `0x`-prefixed).
    pub address: EvmAddress,
    /// The user's optional display name.
    pub username: Option<String>,
    /// When the account was created (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<User> for ProfileDto {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            address: user.address,
            username: user.username,
            created_at: user.created_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Routers
// ---------------------------------------------------------------------------

/// Builds the seek sub-router: the write endpoints that create or cancel seeks.
///
/// # Payment middleware (x402)
///
/// `POST /seeks` is the request that spawns a paid game. It is isolated on this
/// sub-router precisely so an x402 payment middleware can be attached here —
/// e.g. `seek_router().layer(x402_layer)` — to gate game creation behind a
/// settled payment, without affecting the read-only game/profile endpoints or
/// the auth and WebSocket routers. The cancel route lives here too because it is
/// the natural inverse of the create route; a payment layer would scope itself
/// to the `POST` method only.
pub fn seek_router() -> Router<AppState> {
    Router::new()
        .route("/seeks", post(create_seek))
        .route("/seeks/{id}", delete(cancel_seek))
}

/// Builds the read sub-router: game lookups, the game list, and profiles.
///
/// These endpoints are unauthenticated reads, except `GET /profile`, which
/// requires a session to identify "the caller".
pub fn read_router() -> Router<AppState> {
    Router::new()
        .route("/games/{id}", get(get_game))
        .route("/games", get(list_games))
        .route("/users/{id}", get(get_profile))
        .route("/profile", get(my_profile))
}

// ---------------------------------------------------------------------------
// Handlers — seeks
// ---------------------------------------------------------------------------

/// `POST /seeks` — post a seek and either queue it or create a paired game.
///
/// Builds a [`Seek`] for the authenticated caller and submits it to the
/// matchmaker. If no compatible seek is waiting, the seek is queued and its id
/// returned. If a compatible seek is found, a live game is created from the
/// resulting [`Pairing`]: a fresh session is instantiated from the variant
/// registry, a [`Game`] record is persisted, a [`GameActor`] is spawned, its
/// handle is registered in the [`GameHub`](crate::GameHub), and the created game
/// is returned so the client can open the socket at `/ws/game/{id}`.
async fn create_seek(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateSeekRequest>,
) -> ApiResult<Json<CreateSeekResponse>> {
    let seek = Seek::new(
        user.user_id,
        body.variant_id,
        body.time_control,
        body.color_preference,
        OffsetDateTime::now_utc(),
    );

    match state.matchmaker().submit(seek).await? {
        SubmitOutcome::Queued(seek_id) => Ok(Json(CreateSeekResponse::Queued { seek_id })),
        SubmitOutcome::Paired(pairing) => {
            let game = create_paired_game(&state, pairing).await?;
            Ok(Json(CreateSeekResponse::Paired { game: game.into() }))
        }
    }
}

/// Creates, persists, spawns, and registers the game for a matched pairing.
///
/// Returns the persisted [`Game`] record (in the [`GameLifecycle::Active`]
/// state) on success. The session is built from the variant registry; an
/// unknown variant surfaces as a **400 Bad Request** via the
/// [`GameError`](mcs_core::GameError) mapping.
async fn create_paired_game(state: &AppState, pairing: Pairing) -> ApiResult<Game> {
    // Instantiate a fresh session for the agreed variant. The matchmaker only
    // pairs seeks of the same `variant_id`, so this resolves the one both
    // players asked for.
    let session = state
        .variants()
        .new_game(&pairing.variant_id, &VariantOptions::default())?;

    // Build and persist the durable record. Play starts immediately on pairing,
    // so the record is created already `Active` rather than `Created`.
    let mut game = Game::new(
        pairing.variant_id,
        pairing.white,
        pairing.black,
        pairing.time_control.clone(),
        OffsetDateTime::now_utc(),
    );
    game.lifecycle = GameLifecycle::Active;
    state.game_repo().create(&game).await?;

    // Spawn the actor over the same backing store and register its handle so the
    // WebSocket endpoint can find the live game by id.
    let repo: Arc<dyn mcs_storage::GameRepo> = state.game_repo().clone();
    let handle = GameActor::spawn(game.id, session, repo, pairing.time_control);
    state.game_hub().insert(game.id, handle);

    Ok(game)
}

/// `DELETE /seeks/{id}` — cancel one of the caller's own open seeks.
///
/// Only the seek's creator may cancel it: the handler loads the seek and
/// rejects a mismatched caller with **403 Forbidden** before removing it, so a
/// user cannot cancel someone else's seek. A seek that no longer exists (already
/// matched or cancelled) is reported as **404 Not Found**. Cancellation itself
/// is idempotent at the matchmaker level.
async fn cancel_seek(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<SeekId>,
) -> ApiResult<Json<CancelSeekResponse>> {
    // Authorize against the stored seek: the caller must be its creator.
    let seek = state
        .storage()
        .seeks()
        .get(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no open seek: {id}")))?;
    if seek.creator != user.user_id {
        return Err(ApiError::Forbidden(
            "only the seek's creator may cancel it".to_owned(),
        ));
    }

    state.matchmaker().cancel(id).await?;
    Ok(Json(CancelSeekResponse { cancelled: id }))
}

/// Response body for a successful `DELETE /seeks/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct CancelSeekResponse {
    /// The id of the seek that was cancelled.
    pub cancelled: SeekId,
}

// ---------------------------------------------------------------------------
// Handlers — games
// ---------------------------------------------------------------------------

/// `GET /games/{id}` — fetch a single game record by id.
///
/// A malformed id is a **422 Unprocessable Entity** (rejected during path
/// extraction); a well-formed id with no matching game is a **404 Not Found**.
async fn get_game(
    State(state): State<AppState>,
    Path(id): Path<GameId>,
) -> ApiResult<Json<GameDto>> {
    let game = state.storage().games().get(id).await?;
    Ok(Json(game.into()))
}

/// `GET /games?limit=` — list the most recently created games, newest first.
///
/// `limit` is clamped to [`MAX_GAMES_LIMIT`] and defaults to
/// [`DEFAULT_GAMES_LIMIT`].
async fn list_games(
    State(state): State<AppState>,
    Query(query): Query<ListGamesQuery>,
) -> ApiResult<Json<GameListResponse>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_GAMES_LIMIT)
        .min(MAX_GAMES_LIMIT);
    let games = state.storage().games().list_recent(limit).await?;
    Ok(Json(GameListResponse {
        games: games.into_iter().map(GameDto::from).collect(),
    }))
}

// ---------------------------------------------------------------------------
// Handlers — profiles
// ---------------------------------------------------------------------------

/// `GET /users/{id}` — the public profile for a user.
///
/// Returns only public fields (see [`ProfileDto`]); a missing user is a **404
/// Not Found**.
async fn get_profile(
    State(state): State<AppState>,
    Path(id): Path<UserId>,
) -> ApiResult<Json<ProfileDto>> {
    let user = state.storage().users().get(id).await?;
    Ok(Json(user.into()))
}

/// `GET /profile` — the public profile of the authenticated caller.
///
/// A convenience for "me": the [`AuthUser`] extractor resolves the caller, and
/// the same public projection is returned.
async fn my_profile(State(state): State<AppState>, user: AuthUser) -> ApiResult<Json<ProfileDto>> {
    let user = state.storage().users().get(user.user_id).await?;
    Ok(Json(user.into()))
}
