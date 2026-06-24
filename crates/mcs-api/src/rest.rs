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

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use mcs_core::VariantOptions;
use mcs_domain::{
    ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Rating, Seek, SeekId, TimeControl,
    User, UserId,
};
use mcs_game::{Pairing, SubmitOutcome};

use crate::error::{ApiError, ApiResult};
use crate::extract::AuthUser;
use crate::state::AppState;

/// The default page size for `GET /games` when no `limit` is supplied.
const DEFAULT_GAMES_LIMIT: u32 = 20;

/// The largest page size `GET /games` will honour, clamping larger requests.
const MAX_GAMES_LIMIT: u32 = 100;

/// The default number of entries `GET /leaderboard` returns when no `limit` is
/// supplied.
const DEFAULT_LEADERBOARD_LIMIT: u32 = 20;

/// The largest leaderboard page `GET /leaderboard` will honour, clamping larger
/// requests.
const MAX_LEADERBOARD_LIMIT: u32 = 200;

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
    /// Whether the caller wants a **rated** game (the default) or a casual one.
    ///
    /// Defaults to `true` when omitted, so existing clients keep posting rated
    /// seeks. The matchmaker only pairs seeks that agree on this flag, so a
    /// rated seek never matches a casual one.
    #[serde(default = "default_rated")]
    pub rated: bool,
}

/// The serde default for [`CreateSeekRequest::rated`]: an absent `rated` field
/// means the caller wants a rated game.
fn default_rated() -> bool {
    true
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

/// A player's Glicko-2 rating, as exposed on the wire.
///
/// A thin projection of the domain [`Rating`]: the three Glicko-2 parameters,
/// flattened so a client can render "1500 ± 350" without reaching into a nested
/// object's internals.
#[derive(Debug, Clone, Serialize)]
pub struct RatingDto {
    /// Estimated playing strength (Glicko-1 / display scale, centred at 1500).
    pub value: f64,
    /// Rating deviation: the uncertainty around `value`. Smaller is more certain.
    pub deviation: f64,
    /// Volatility: how consistent the player's recent results have been.
    pub volatility: f64,
}

impl From<Rating> for RatingDto {
    fn from(rating: Rating) -> Self {
        Self {
            value: rating.value,
            deviation: rating.deviation,
            volatility: rating.volatility,
        }
    }
}

/// The public, serialized view of a [`Game`] record.
///
/// This is the wire shape returned by every game endpoint. It is a thin,
/// explicit projection of [`Game`] so the HTTP contract does not silently drift
/// when the domain type gains internal fields.
///
/// The two `*_rating` fields carry each player's current rating **for this
/// game's variant**. They are populated by the single-game lookup
/// (`GET /games/{id}`) and omitted (left `None`) by the bulk list endpoint,
/// which would otherwise issue two extra reads per row.
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
    /// White's current rating for this variant, if looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_rating: Option<RatingDto>,
    /// Black's current rating for this variant, if looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub black_rating: Option<RatingDto>,
    /// The game's server-side lifecycle state.
    pub lifecycle: GameLifecycle,
    /// The time control in force.
    pub time_control: TimeControl,
    /// Whether the game is rated (counts towards ratings) or casual (exempt).
    pub rated: bool,
    /// When the game record was created (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<Game> for GameDto {
    /// Projects a [`Game`] without ratings. Use
    /// [`with_ratings`](GameDto::with_ratings) to attach them for the
    /// single-game endpoint.
    fn from(game: Game) -> Self {
        Self {
            id: game.id,
            variant_id: game.variant_id,
            white: game.white,
            black: game.black,
            white_rating: None,
            black_rating: None,
            lifecycle: game.lifecycle,
            time_control: game.time_control,
            rated: game.rated,
            created_at: game.created_at,
        }
    }
}

impl GameDto {
    /// Attaches both players' current ratings (for the game's variant) to this
    /// DTO, replacing whatever was there.
    #[must_use]
    fn with_ratings(mut self, white: Rating, black: Rating) -> Self {
        self.white_rating = Some(white.into());
        self.black_rating = Some(black.into());
        self
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

/// Query parameters for `GET /leaderboard`.
#[derive(Debug, Clone, Deserialize)]
pub struct LeaderboardQuery {
    /// The variant whose leaderboard to return (e.g. `"standard"`). Required.
    pub variant: String,
    /// Maximum number of entries to return. Clamped to
    /// [`MAX_LEADERBOARD_LIMIT`]; defaults to [`DEFAULT_LEADERBOARD_LIMIT`] when
    /// absent.
    pub limit: Option<u32>,
}

/// One ranked entry in a variant's leaderboard.
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardEntry {
    /// The ranked player's stable identifier.
    pub user_id: UserId,
    /// The player's Ethereum address, if their account could be resolved.
    /// Omitted rather than failing the whole listing if a lookup misses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<EvmAddress>,
    /// The player's current rating for the requested variant.
    pub rating: RatingDto,
}

/// Response body for `GET /leaderboard`: the top players, highest-rated first.
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardResponse {
    /// The variant this leaderboard is for, echoed back from the request.
    pub variant: String,
    /// The ranked players, ordered by rating descending.
    pub entries: Vec<LeaderboardEntry>,
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

/// Builds the seek **creation** sub-router: the single `POST /seeks` route.
///
/// # Payment middleware (x402, #45)
///
/// `POST /seeks` is the request that spawns a paid game, so it is isolated on
/// its own one-route sub-router precisely so an x402 payment middleware can wrap
/// *only* it — e.g. `create_seek_router().layer(RequirePaymentLayer::new(..))`.
/// [`crate::router`] does exactly that when the [`AppState`] carries a
/// [`PaymentGate`](crate::state::PaymentGate); otherwise this router is merged
/// in untouched and creation stays free. The cancel route is deliberately kept
/// out of this router (see [`cancel_seek_router`]) so the gate scopes to game
/// creation alone.
pub fn create_seek_router() -> Router<AppState> {
    Router::new().route("/seeks", post(create_seek))
}

/// Builds the seek **cancellation** sub-router: the single `DELETE /seeks/{id}`
/// route.
///
/// Kept separate from [`create_seek_router`] so the x402 payment layer (#45)
/// gates creation only: cancelling one's own open seek is never charged.
pub fn cancel_seek_router() -> Router<AppState> {
    Router::new().route("/seeks/{id}", delete(cancel_seek))
}

/// Builds the read sub-router: game lookups, the game list, and profiles.
///
/// These endpoints are unauthenticated reads, except `GET /profile`, which
/// requires a session to identify "the caller".
pub fn read_router() -> Router<AppState> {
    Router::new()
        .route("/games/{id}", get(get_game))
        .route("/games", get(list_games))
        .route("/leaderboard", get(leaderboard))
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
        body.rated,
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
/// A thin adapter over the shared
/// [`AppState::create_and_spawn_game`](crate::state::AppState::create_and_spawn_game)
/// helper: it forwards the pairing's players and terms. Seeks do not yet carry
/// per-game options, so the variant's own defaults
/// ([`VariantOptions::default`]) are used. Returns the persisted [`Game`]
/// record (already [`GameLifecycle::Active`]); an unknown variant surfaces as a
/// **400 Bad Request**.
async fn create_paired_game(state: &AppState, pairing: Pairing) -> ApiResult<Game> {
    state
        .create_and_spawn_game(
            pairing.white,
            pairing.black,
            &pairing.variant_id,
            pairing.time_control,
            pairing.rated,
            VariantOptions::default(),
        )
        .await
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

    // Attach each player's current rating for this game's variant. An unrated
    // player (no row yet) is reported at the Glicko-2 seed, matching what the
    // rating-update hook would seed them with.
    let ratings = state.storage().ratings();
    let white = ratings
        .get(game.white, &game.variant_id)
        .await?
        .unwrap_or_default();
    let black = ratings
        .get(game.black, &game.variant_id)
        .await?
        .unwrap_or_default();

    Ok(Json(GameDto::from(game).with_ratings(white, black)))
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

/// `GET /leaderboard?variant=&limit=` — the top-rated players for a variant.
///
/// Returns players ordered by rating descending. `limit` is clamped to
/// [`MAX_LEADERBOARD_LIMIT`] and defaults to [`DEFAULT_LEADERBOARD_LIMIT`]. Each
/// entry carries the player's id, current rating, and — where it can be resolved
/// over the same store — their address. A user lookup that misses leaves
/// `address` absent rather than failing the whole listing, so a stale rating row
/// cannot 500 the endpoint.
async fn leaderboard(
    State(state): State<AppState>,
    Query(query): Query<LeaderboardQuery>,
) -> ApiResult<Json<LeaderboardResponse>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LEADERBOARD_LIMIT)
        .min(MAX_LEADERBOARD_LIMIT);

    let ranked = state
        .storage()
        .ratings()
        .leaderboard(&query.variant, limit)
        .await?;

    let mut entries = Vec::with_capacity(ranked.len());
    for (user_id, rating) in ranked {
        // Resolve the address best-effort: a missing user (a rating row with no
        // surviving account) simply omits the address.
        let address = state
            .storage()
            .users()
            .get(user_id)
            .await
            .ok()
            .map(|u| u.address);
        entries.push(LeaderboardEntry {
            user_id,
            address,
            rating: rating.into(),
        });
    }

    Ok(Json(LeaderboardResponse {
        variant: query.variant,
        entries,
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
