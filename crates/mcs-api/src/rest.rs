//! REST endpoints for seeks, games, and public profiles.
//!
//! These handlers mirror the request/response shape of the lichess HTTP API:
//! a client posts a seek, the matchmaker either queues it or pairs it into a
//! live game, and the game is then read back over plain HTTP (`GET /games/{id}`,
//! `GET /games`) or streamed over the WebSocket endpoint (#15).
//!
//! | Method & path             | Auth | Purpose |
//! |---------------------------|------|---------|
//! | `POST /seeks`             | yes  | Post a seek; queue it or pair it into a game. |
//! | `GET /seeks`              | no   | Browse the open-seek lobby. |
//! | `POST /seeks/{id}/accept` | yes  | Join an open seek directly, creating the game. |
//! | `DELETE /seeks/{id}`      | yes  | Cancel one of the caller's own open seeks. |
//! | `GET /games/{id}`         | no   | Fetch a single game record by id. |
//! | `GET /games`              | no   | List the most recently created games. |
//! | `GET /users/{id}`         | no   | Public profile for a user. |
//! | `GET /users/{id}/ratings` | no   | A user's per-variant ratings. |
//! | `GET /users/{id}/rating-history` | no | A user's rating trail for a variant. |
//! | `GET /users/{id}/stats`   | no   | A user's W/L/D record and performance rating. |
//! | `GET /profile`            | yes  | Public profile for the authenticated caller. |
//! | `PUT /profile`            | yes  | Edit the authenticated caller's username. |
//!
//! # Seek lobby (#77)
//!
//! Alongside auto-matching (`POST /seeks`), a seek can be **browsed** and
//! **joined directly**: `GET /seeks` lists the open pool and
//! `POST /seeks/{id}/accept` lets a second player take a specific seek,
//! bypassing the matchmaker. The accept path atomically claims the seek (so two
//! simultaneous accepts cannot both create a game — see
//! [`SeekRepo::claim`](mcs_storage::SeekRepo::claim)) and then spawns the game
//! through the same [`AppState::create_and_spawn_game`] helper a paired seek
//! uses.
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
use utoipa::ToSchema;

use mcs_core::{Color, VariantOptions};
use mcs_domain::{
    ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Rating, Seek, SeekId, TimeClass,
    TimeControl, UserId,
};
use mcs_game::{Pairing, SubmitOutcome};
use mcs_storage::ClaimOutcome;

use crate::error::{ApiError, ApiResult};
use crate::extract::AuthUser;
use crate::state::AppState;

/// The default page size for `GET /games` when no `limit` is supplied.
const DEFAULT_GAMES_LIMIT: u32 = 20;

/// The largest page size `GET /games` will honour, clamping larger requests.
const MAX_GAMES_LIMIT: u32 = 100;

/// The default number of entries `GET /leaderboard` returns when no `limit` is
/// supplied.
const DEFAULT_LEADERBOARD_LIMIT: u32 = 50;

/// The largest leaderboard page `GET /leaderboard` will honour, clamping larger
/// requests.
const MAX_LEADERBOARD_LIMIT: u32 = 200;

/// The time class `GET /leaderboard` and `GET /games/{id}` rating lookups assume
/// when the caller does not specify one.
///
/// Blitz is the most-played pace, so it is the most useful default leaderboard.
/// Full per-`(variant, time_class)` selection and pagination on the leaderboard
/// is a follow-up; today the endpoint always returns a single bucket, defaulting
/// here when `time_class` is omitted.
const DEFAULT_TIME_CLASS: TimeClass = TimeClass::Blitz;

/// The minimum length of a username (inclusive).
const USERNAME_MIN_LEN: usize = 3;

/// The maximum length of a username (inclusive).
const USERNAME_MAX_LEN: usize = 20;

/// The default number of entries `GET /users/{id}/rating-history` returns when
/// no `limit` is supplied.
const DEFAULT_HISTORY_LIMIT: u32 = 50;

/// The largest rating-history page that endpoint will honour, clamping larger
/// requests.
const MAX_HISTORY_LIMIT: u32 = 200;

/// The Glicko-style rating-deviation threshold above which a rating is reported
/// as **provisional**.
///
/// A freshly registered player starts at the Glicko-2 seed deviation of `350`
/// and it shrinks as games are recorded. A deviation still above this threshold
/// means too few rated games are on record for the rating to be considered
/// reliable, so it is flagged provisional. `110` is a common Glicko-style cutoff
/// (a player needs a handful of rated games before their deviation drops under
/// it).
const PROVISIONAL_DEVIATION_THRESHOLD: f64 = 110.0;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /seeks`.
///
/// The fields reuse the domain value objects directly, so an invalid time
/// control or colour preference is rejected at deserialization time with a
/// **422 Unprocessable Entity** before the handler runs.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateSeekRequest {
    /// The variant the caller wants to play (e.g. `"standard"`).
    pub variant_id: String,
    /// The time control the caller wants to play under.
    #[schema(value_type = crate::openapi::schema::TimeControl)]
    pub time_control: TimeControl,
    /// Which side the caller would prefer.
    #[schema(value_type = crate::openapi::schema::ColorPreference)]
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
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CreateSeekResponse {
    /// No compatible seek was waiting; this one is now in the pool. The client
    /// can later cancel it with `DELETE /seeks/{id}`.
    Queued {
        /// The id of the freshly queued seek.
        #[schema(value_type = String, format = Uuid)]
        seek_id: SeekId,
    },
    /// A compatible seek was found and a live game was created. The client
    /// should open the game socket at `/ws/game/{game.id}`.
    Paired {
        /// The created game record.
        game: GameDto,
    },
}

/// The creator of an open seek, as exposed in the lobby listing.
///
/// The `user_id` is always present; the `address` is resolved best-effort over
/// the user store and omitted (rather than failing the whole listing) when the
/// account cannot be looked up — mirroring how [`LeaderboardEntry`] treats a
/// missing account.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeekCreatorDto {
    /// The creator's stable identifier.
    #[schema(value_type = String, format = Uuid)]
    pub user_id: UserId,
    /// The creator's Ethereum address, if it could be resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub address: Option<EvmAddress>,
}

/// One open seek in the lobby, as returned by `GET /seeks`.
///
/// A thin, explicit projection of the domain [`Seek`]: the creator is expanded
/// into a [`SeekCreatorDto`] (id plus best-effort address) so a client can
/// render the lobby without a second round-trip per row.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeekDto {
    /// The seek's stable identifier; join it with `POST /seeks/{id}/accept`.
    #[schema(value_type = String, format = Uuid)]
    pub seek_id: SeekId,
    /// The player who posted the seek.
    pub creator: SeekCreatorDto,
    /// The variant on offer (e.g. `"standard"`).
    pub variant_id: String,
    /// The time control on offer.
    #[schema(value_type = crate::openapi::schema::TimeControl)]
    pub time_control: TimeControl,
    /// Whether the resulting game would be rated.
    pub rated: bool,
    /// The creator's colour preference (honoured on accept).
    #[schema(value_type = crate::openapi::schema::ColorPreference)]
    pub color_preference: ColorPreference,
    /// When the seek was posted (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

/// Response body for `GET /seeks`: the open-seek lobby.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeekListResponse {
    /// The seeks currently awaiting an opponent, in no guaranteed order.
    pub seeks: Vec<SeekDto>,
}

/// A player's Glicko-2 rating, as exposed on the wire.
///
/// A thin projection of the domain [`Rating`]: the three Glicko-2 parameters,
/// flattened so a client can render "1500 ± 350" without reaching into a nested
/// object's internals.
#[derive(Debug, Clone, Serialize, ToSchema)]
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
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GameDto {
    /// The game's stable identifier; open the socket at `/ws/game/{id}`.
    #[schema(value_type = String, format = Uuid)]
    pub id: GameId,
    /// The variant being played.
    pub variant_id: String,
    /// The user playing White.
    #[schema(value_type = String, format = Uuid)]
    pub white: UserId,
    /// The user playing Black.
    #[schema(value_type = String, format = Uuid)]
    pub black: UserId,
    /// White's current rating for this variant, if looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_rating: Option<RatingDto>,
    /// Black's current rating for this variant, if looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub black_rating: Option<RatingDto>,
    /// The game's server-side lifecycle state.
    #[schema(value_type = crate::openapi::schema::GameLifecycle)]
    pub lifecycle: GameLifecycle,
    /// The time control in force.
    #[schema(value_type = crate::openapi::schema::TimeControl)]
    pub time_control: TimeControl,
    /// Whether the game is rated (counts towards ratings) or casual (exempt).
    pub rated: bool,
    /// When the game record was created (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
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
#[derive(Debug, Clone, Serialize, ToSchema)]
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
    /// The time class whose leaderboard to return (e.g. `"blitz"`). Optional;
    /// defaults to [`DEFAULT_TIME_CLASS`] when absent so existing callers that
    /// only pass `variant` keep getting a populated board.
    pub time_class: Option<String>,
    /// Zero-based offset into the full ranking. Defaults to `0` when absent so
    /// the first page is returned without an explicit `offset` param.
    pub offset: Option<u32>,
    /// Maximum number of entries to return. Clamped to
    /// [`MAX_LEADERBOARD_LIMIT`]; defaults to [`DEFAULT_LEADERBOARD_LIMIT`] when
    /// absent.
    pub limit: Option<u32>,
}

/// One ranked entry in a variant's leaderboard.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LeaderboardEntry {
    /// The 1-based rank within the full leaderboard. The first entry on page 1
    /// has `rank = 1`; the first entry on a page fetched with `offset = 50`
    /// has `rank = 51`.
    pub rank: u64,
    /// The ranked player's stable identifier.
    #[schema(value_type = String, format = Uuid)]
    pub user_id: UserId,
    /// The player's Ethereum address, if their account could be resolved.
    /// Omitted rather than failing the whole listing if a lookup misses.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub address: Option<EvmAddress>,
    /// The player's current rating for the requested variant.
    pub rating: RatingDto,
}

/// Response body for `GET /leaderboard`: a paginated slice of the top players.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LeaderboardResponse {
    /// The variant this leaderboard is for, echoed back from the request.
    pub variant: String,
    /// The time class this leaderboard is for (the resolved value, which is
    /// [`DEFAULT_TIME_CLASS`] when the request omitted it).
    #[schema(value_type = String)]
    pub time_class: TimeClass,
    /// The ranked players on this page, ordered by rating descending.
    pub entries: Vec<LeaderboardEntry>,
    /// The zero-based offset supplied (or defaulted to `0`), echoed back.
    pub offset: u32,
    /// The effective page size (the clamped `limit` value), echoed back.
    pub limit: u32,
    /// The total number of players with a rating in this `(variant,
    /// time_class)`. Use this together with `offset` and `limit` to compute
    /// whether a next page exists: `offset + limit < total`.
    pub total: u64,
}

/// A user's **public** profile.
///
/// Deliberately a narrow projection of [`User`]: it exposes only the address,
/// the optional username, the creation time, and the real-time `online` flag
/// (see [`crate::presence::PresenceTracker`]). No session, nonce, or other
/// sensitive state is ever included.
///
/// # Online flag
///
/// `online` is `true` when the user made an authenticated REST or WebSocket
/// request within the configured TTL on **this** node. In a multi-node
/// deployment a user may appear offline here while being active on another
/// node; a Redis-backed [`PresenceTracker`](crate::presence::PresenceTracker)
/// is the cross-node upgrade path.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProfileDto {
    /// The user's stable identifier.
    #[schema(value_type = String, format = Uuid)]
    pub id: UserId,
    /// The user's Ethereum address (lowercase, `0x`-prefixed).
    #[schema(value_type = String)]
    pub address: EvmAddress,
    /// The user's optional display name.
    pub username: Option<String>,
    /// When the account was created (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    /// Whether the user is currently online (seen within the configured TTL).
    pub online: bool,
}

/// Response body for `GET /users/{id}/status`.
///
/// Reports whether a user is currently online and when they were last seen.
/// `online` is derived from the configured TTL (see
/// [`AppState::online_ttl`](crate::state::AppState::online_ttl)).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserStatusResponse {
    /// `true` when the user was seen within the online TTL on this node.
    pub online: bool,
    /// The most recent instant the user made an authenticated request on this
    /// node. `null` if this user has never been seen.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_seen: Option<OffsetDateTime>,
}

/// Request body for `PUT /profile`: the new display name for the caller.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateProfileRequest {
    /// The desired username. Validated for length (3–20) and an
    /// `[A-Za-z0-9_-]` character set; uniqueness is enforced case-insensitively
    /// by the store.
    pub username: String,
}

/// One `(variant, time_class)` rating in the per-user ratings listing.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserRatingDto {
    /// The variant this rating is for (e.g. `"standard"`).
    pub variant_id: String,
    /// The time class this rating is for (e.g. `"blitz"`). Ratings are kept per
    /// `(variant, time_class)`, so a player may hold several ratings per variant.
    #[schema(value_type = String)]
    pub time_class: TimeClass,
    /// The Glicko-2 rating itself.
    pub rating: RatingDto,
    /// Whether the rating is **provisional** — its deviation is still above the
    /// [`PROVISIONAL_DEVIATION_THRESHOLD`], so too few rated games are on record
    /// for it to be considered reliable.
    pub provisional: bool,
}

/// Response body for `GET /users/{id}/ratings`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserRatingsResponse {
    /// The user these ratings belong to.
    #[schema(value_type = String, format = Uuid)]
    pub user_id: UserId,
    /// One entry per `(variant, time_class)` the user has a rating in, ordered
    /// by variant id then time class.
    pub ratings: Vec<UserRatingDto>,
}

/// Query parameters for `GET /users/{id}/rating-history`.
#[derive(Debug, Clone, Deserialize)]
pub struct RatingHistoryQuery {
    /// The variant whose history to return (e.g. `"standard"`). Required.
    pub variant: String,
    /// The time class whose history to return (e.g. `"blitz"`). Defaults to
    /// [`DEFAULT_TIME_CLASS`] when absent, so existing callers keep working.
    pub time_class: Option<String>,
    /// Maximum number of snapshots to return. Clamped to [`MAX_HISTORY_LIMIT`];
    /// defaults to [`DEFAULT_HISTORY_LIMIT`] when absent.
    pub limit: Option<u32>,
}

/// One snapshot in a user's rating history.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RatingHistoryEntryDto {
    /// The rating value after the game was scored.
    pub value: f64,
    /// The rating deviation after the game was scored.
    pub deviation: f64,
    /// The game that produced this snapshot.
    #[schema(value_type = String, format = Uuid)]
    pub game_id: GameId,
    /// When the snapshot was recorded (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

/// Response body for `GET /users/{id}/rating-history`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RatingHistoryResponse {
    /// The user this history belongs to.
    #[schema(value_type = String, format = Uuid)]
    pub user_id: UserId,
    /// The variant the history is for, echoed back from the request.
    pub variant_id: String,
    /// The time class the history is for, echoed back (the resolved value, which
    /// is [`DEFAULT_TIME_CLASS`] when the request omitted it).
    #[schema(value_type = String)]
    pub time_class: TimeClass,
    /// The snapshots, most-recent-first.
    pub entries: Vec<RatingHistoryEntryDto>,
}

/// The Elo-style spread applied per net win in the **performance rating**.
///
/// The formula is `avg(opponent_rating) + PERFORMANCE_SPREAD * (wins - losses)
/// / games`, the standard tournament "performance rating" approximation: a
/// player who scores 50% performs at the average of their opponents, and each
/// net win (or loss) shifts that estimate by a fixed Elo margin. `400` is the
/// conventional Elo constant.
const PERFORMANCE_SPREAD: f64 = 400.0;

/// Query parameters for `GET /users/{id}/stats`.
#[derive(Debug, Clone, Deserialize)]
pub struct UserStatsQuery {
    /// Optional variant filter (e.g. `"standard"`). When present, only matching
    /// categories are returned; the `overall` tally always spans every game.
    pub variant: Option<String>,
    /// Optional time-class filter (e.g. `"blitz"`). An unknown value is a
    /// **422 Unprocessable Entity**. When present, only matching categories are
    /// returned.
    pub time_class: Option<String>,
}

/// A win/loss/draw tally with a derived total.
#[derive(Debug, Clone, Copy, Default, Serialize, ToSchema)]
pub struct RecordDto {
    /// Games the user won (the outcome's winner is the user's colour).
    pub wins: u32,
    /// Games the user lost (the winner is the opposing colour).
    pub losses: u32,
    /// Games drawn (the outcome has no winner).
    pub draws: u32,
    /// `wins + losses + draws`. Aborted / no-outcome games are excluded
    /// entirely, so they never count here.
    pub total: u32,
}

/// One `(variant, time_class)` category in a user's statistics.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StatsCategoryDto {
    /// The variant this category is for (e.g. `"standard"`).
    pub variant_id: String,
    /// The time class this category is for (e.g. `"blitz"`), derived from each
    /// game's time control.
    #[schema(value_type = String)]
    pub time_class: TimeClass,
    /// Games the user won in this category.
    pub wins: u32,
    /// Games the user lost in this category.
    pub losses: u32,
    /// Games drawn in this category.
    pub draws: u32,
    /// `wins + losses + draws` for this category.
    pub total: u32,
    /// The **performance rating** for this category, rounded to an integer, or
    /// `null` when the category has no rated games to compute it from.
    ///
    /// Computed as `round(avg(opponent_rating) + 400 * (wins - losses) /
    /// games)` over this category's **rated** games. Each opponent's rating is
    /// their **current** `(variant, time_class)` rating (the live value at the
    /// time of the request, defaulting to the Glicko-2 seed when the opponent
    /// has never been rated in this bucket) — a deliberate, cheap approximation
    /// over reconstructing the historical rating each game was played at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance_rating: Option<i64>,
}

/// Response body for `GET /users/{id}/stats`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserStatsResponse {
    /// The user these statistics belong to.
    #[schema(value_type = String, format = Uuid)]
    pub user_id: UserId,
    /// The aggregate win/loss/draw tally across **all** of the user's finished
    /// games, irrespective of any `variant`/`time_class` filter.
    pub overall: RecordDto,
    /// One entry per `(variant, time_class)` the user has a finished game in,
    /// narrowed by the optional `variant`/`time_class` filters, ordered by
    /// variant id then time class.
    pub categories: Vec<StatsCategoryDto>,
}

/// Validates a requested username, returning the trimmed value on success.
///
/// The rules: 3–20 characters, each one of `[A-Za-z0-9_-]`. A violation is a
/// **422 Unprocessable Entity** so a client can correct its input.
fn validate_username(raw: &str) -> ApiResult<&str> {
    let name = raw.trim();
    let len = name.chars().count();
    if !(USERNAME_MIN_LEN..=USERNAME_MAX_LEN).contains(&len) {
        return Err(ApiError::UnprocessableEntity(format!(
            "username must be between {USERNAME_MIN_LEN} and {USERNAME_MAX_LEN} characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ApiError::UnprocessableEntity(
            "username may contain only letters, digits, '_' and '-'".to_owned(),
        ));
    }
    Ok(name)
}

/// Reports whether a rating is provisional given its deviation.
fn is_provisional(deviation: f64) -> bool {
    deviation > PROVISIONAL_DEVIATION_THRESHOLD
}

/// Resolves an optional `time_class` query value to a [`TimeClass`].
///
/// `None` (the parameter was omitted) maps to [`DEFAULT_TIME_CLASS`], keeping
/// callers that only pass `variant` working. A present-but-unrecognised value is
/// a **422 Unprocessable Entity** so the client can correct it.
fn resolve_time_class(raw: Option<&str>) -> ApiResult<TimeClass> {
    match raw {
        None => Ok(DEFAULT_TIME_CLASS),
        Some(s) => s.parse::<TimeClass>().map_err(|_| {
            ApiError::UnprocessableEntity(format!(
                "unknown time class {s:?}; expected one of bullet, blitz, rapid, \
                 classical, correspondence"
            ))
        }),
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

/// Builds the seek **accept** sub-router: the single `POST /seeks/{id}/accept`
/// route.
///
/// Kept out of [`create_seek_router`] on purpose: the x402 payment layer (#45)
/// gates *seek creation* (`POST /seeks`), and a direct join is a distinct action
/// the gate should not double-charge. Accepting is authenticated (the handler
/// takes an [`AuthUser`]) but free, exactly like cancellation.
pub fn accept_seek_router() -> Router<AppState> {
    Router::new().route("/seeks/{id}/accept", post(accept_seek))
}

/// Builds the read sub-router: the seek lobby, game lookups, the game list,
/// profiles, and the user-status endpoint.
///
/// These endpoints are unauthenticated reads, except `GET /profile`, which
/// requires a session to identify "the caller".
pub fn read_router() -> Router<AppState> {
    Router::new()
        .route("/seeks", get(list_seeks))
        .route("/games/{id}", get(get_game))
        .route("/games", get(list_games))
        .route("/leaderboard", get(leaderboard))
        .route("/users/{id}", get(get_profile))
        .route("/users/{id}/status", get(get_user_status))
        .route("/users/{id}/ratings", get(get_user_ratings))
        .route("/users/{id}/rating-history", get(get_user_rating_history))
        .route("/users/{id}/stats", get(get_user_stats))
        .route("/profile", get(my_profile).put(update_profile))
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
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CancelSeekResponse {
    /// The id of the seek that was cancelled.
    #[schema(value_type = String, format = Uuid)]
    pub cancelled: SeekId,
}

/// `GET /seeks` — browse the open-seek lobby (public read).
///
/// Returns every seek currently awaiting an opponent, projected to [`SeekDto`].
/// Each creator's address is resolved best-effort: a seek whose creator account
/// can no longer be read simply omits the address rather than failing the whole
/// listing — the same robustness `GET /leaderboard` applies.
async fn list_seeks(State(state): State<AppState>) -> ApiResult<Json<SeekListResponse>> {
    let open = state.storage().seeks().list_open().await?;

    let mut seeks = Vec::with_capacity(open.len());
    for seek in open {
        // Best-effort address resolution; a missing account omits the address.
        let address = state
            .storage()
            .users()
            .get(seek.creator)
            .await
            .ok()
            .map(|u| u.address);
        seeks.push(SeekDto {
            seek_id: seek.id,
            creator: SeekCreatorDto {
                user_id: seek.creator,
                address,
            },
            variant_id: seek.variant_id,
            time_control: seek.time_control,
            rated: seek.rated,
            color_preference: seek.color_preference,
            created_at: seek.created_at,
        });
    }

    Ok(Json(SeekListResponse { seeks }))
}

/// `POST /seeks/{id}/accept` — join an open seek directly, creating the game.
///
/// This is the lobby's direct-join path: the matchmaker is bypassed and the
/// accepter takes a *specific* seek, with the game's variant, time control, and
/// rated flag fixed by that seek. The sequence is:
///
/// 1. Load the seek; a missing one is **404 Not Found**.
/// 2. Reject the creator accepting their own seek with **400 Bad Request** —
///    there would be no opponent.
/// 3. **Atomically claim** the seek via [`SeekRepo::claim`](mcs_storage::SeekRepo::claim).
///    When several callers race to accept the same seek, exactly one wins the
///    claim and proceeds; every loser gets **409 Conflict**. This also covers an
///    already-taken seek (matched, cancelled, or claimed): the claim reports it
///    absent, so the caller is told it is gone.
/// 4. Resolve colours from the *creator's* preference (the creator keeps their
///    preferred side; the accepter takes the other; see [`resolve_seek_color`]).
/// 5. Create the game through the shared
///    [`AppState::create_and_spawn_game`](crate::state::AppState::create_and_spawn_game)
///    helper — the same path a paired seek takes — and return it, so the client
///    can open the socket at `/ws/game/{id}`.
///
/// The 404 / claim ordering is deliberate: a clear "no such seek" is reported
/// before the claim, while the claim itself collapses the *racing* "someone else
/// just took it" into a 409.
async fn accept_seek(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<SeekId>,
) -> ApiResult<Json<GameDto>> {
    let seeks = state.storage().seeks();

    // 1. The seek must exist. A clean not-found is friendlier than forcing the
    //    claim to disambiguate "never existed" from "just taken".
    let seek = seeks
        .get(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no open seek: {id}")))?;

    // 2. A creator cannot accept their own seek — there would be no opponent.
    if seek.creator == user.user_id {
        return Err(ApiError::BadRequest(
            "you cannot accept your own seek".to_owned(),
        ));
    }

    // 3. Atomically claim it. Exactly one of any concurrent accepters wins; the
    //    rest (and any accept of an already-taken seek) get 409.
    if state.storage().seeks().claim(id).await? == ClaimOutcome::AlreadyClaimed {
        return Err(ApiError::Conflict(format!(
            "seek {id} has already been taken"
        )));
    }

    // 4. Resolve colours: the creator keeps their preferred side; the accepter
    //    takes the other.
    let creator_color = resolve_seek_color(seek.color_preference, seek.id);
    let (white, black) = match creator_color {
        Color::White => (seek.creator, user.user_id),
        Color::Black => (user.user_id, seek.creator),
    };

    // 5. Create the game through the shared helper — identical to the paired-seek
    //    path. Seeks carry no per-game options yet, so use the variant defaults.
    let game = state
        .create_and_spawn_game(
            white,
            black,
            &seek.variant_id,
            seek.time_control,
            seek.rated,
            VariantOptions::default(),
        )
        .await?;

    Ok(Json(game.into()))
}

/// Resolves a seek creator's [`ColorPreference`] into a concrete [`Color`].
///
/// [`White`](ColorPreference::White) and [`Black`](ColorPreference::Black) map
/// directly. [`Random`](ColorPreference::Random) is resolved deterministically
/// from the seek id — the low bit of its first byte — so the same seek always
/// yields the same colours: no RNG, reproducible in tests, yet effectively
/// unpredictable to the players. This matches how direct challenges
/// (`crate::challenges`) assign colours.
fn resolve_seek_color(pref: ColorPreference, id: SeekId) -> Color {
    match pref {
        ColorPreference::White => Color::White,
        ColorPreference::Black => Color::Black,
        ColorPreference::Random => {
            if id.as_uuid().as_bytes()[0] & 1 == 0 {
                Color::White
            } else {
                Color::Black
            }
        }
    }
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

    // Attach each player's current rating for this game's (variant, time_class).
    // An unrated player (no row yet) is reported at the Glicko-2 seed, matching
    // what the rating-update hook would seed them with.
    let time_class = game.time_control.time_class();
    let ratings = state.storage().ratings();
    let white = ratings
        .get(game.white, &game.variant_id, time_class)
        .await?
        .unwrap_or_default();
    let black = ratings
        .get(game.black, &game.variant_id, time_class)
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

/// `GET /leaderboard?variant=&time_class=&offset=&limit=` — a paginated slice
/// of the top-rated players for a `(variant, time_class)`.
///
/// Returns players ordered by rating descending. `time_class` is optional and
/// defaults to [`DEFAULT_TIME_CLASS`] (an unknown value is a **422**). `offset`
/// is the zero-based starting position in the full ranking (default `0`).
/// `limit` is clamped to [`MAX_LEADERBOARD_LIMIT`] and defaults to
/// [`DEFAULT_LEADERBOARD_LIMIT`]. Each entry carries the player's 1-based rank
/// (continuing from `offset`), their id, current rating, and — where it can be
/// resolved over the same store — their address. A user lookup that misses
/// leaves `address` absent rather than failing the whole listing. The response
/// also includes `total` (the full count of ranked players in the bucket) and
/// the effective `offset`/`limit` for the caller to compute next-page existence
/// (`offset + limit < total`).
async fn leaderboard(
    State(state): State<AppState>,
    Query(query): Query<LeaderboardQuery>,
) -> ApiResult<Json<LeaderboardResponse>> {
    let offset = query.offset.unwrap_or(0);
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LEADERBOARD_LIMIT)
        .min(MAX_LEADERBOARD_LIMIT);
    let time_class = resolve_time_class(query.time_class.as_deref())?;

    let ratings = state.storage().ratings();

    // Issue the page query and the total count in sequence. The count query is
    // cheap (an indexed aggregate on a small table) and avoids a second
    // round-trip to the store for pages that are already empty.
    let ranked = ratings
        .leaderboard(&query.variant, time_class, offset, limit)
        .await?;
    let total = ratings
        .leaderboard_count(&query.variant, time_class)
        .await?;

    let mut entries = Vec::with_capacity(ranked.len());
    for (i, (user_id, rating)) in ranked.into_iter().enumerate() {
        // Rank is 1-based and continues from the requested offset.
        let rank = u64::from(offset) + i as u64 + 1;
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
            rank,
            user_id,
            address,
            rating: rating.into(),
        });
    }

    Ok(Json(LeaderboardResponse {
        variant: query.variant,
        time_class,
        entries,
        offset,
        limit,
        total,
    }))
}

// ---------------------------------------------------------------------------
// Handlers — profiles
// ---------------------------------------------------------------------------

/// `GET /users/{id}` — the public profile for a user.
///
/// Returns only public fields (see [`ProfileDto`]); a missing user is a **404
/// Not Found**. The `online` flag is derived from the node-local presence
/// tracker and reflects activity on **this** node only.
async fn get_profile(
    State(state): State<AppState>,
    Path(id): Path<UserId>,
) -> ApiResult<Json<ProfileDto>> {
    let user = state.storage().users().get(id).await?;
    let online = state.presence().is_online(user.id, state.online_ttl());
    Ok(Json(ProfileDto {
        id: user.id,
        address: user.address,
        username: user.username,
        created_at: user.created_at,
        online,
    }))
}

/// `GET /users/{id}/status` — the real-time presence status for a user.
///
/// Returns `{ "online": bool, "last_seen": Option<rfc3339> }`.
///
/// `online` is `true` when the user made an authenticated request within the
/// configured online TTL on this node. `last_seen` is the RFC 3339 timestamp
/// of the most recent such request, or `null` if the user has never been seen
/// on this node.
async fn get_user_status(
    State(state): State<AppState>,
    Path(id): Path<UserId>,
) -> ApiResult<Json<UserStatusResponse>> {
    // Verify the user exists so we return 404 for unknown ids rather than just
    // an "online: false" that could be confused with a valid but offline user.
    let _user = state.storage().users().get(id).await?;
    let online = state.presence().is_online(id, state.online_ttl());
    let last_seen = state.presence().last_seen(id);
    Ok(Json(UserStatusResponse { online, last_seen }))
}

/// `GET /profile` — the public profile of the authenticated caller.
///
/// A convenience for "me": the [`AuthUser`] extractor resolves the caller, and
/// the same public projection is returned (including the `online` flag, which
/// will always be `true` here since the request itself stamped the user as
/// active).
async fn my_profile(State(state): State<AppState>, user: AuthUser) -> ApiResult<Json<ProfileDto>> {
    let stored = state.storage().users().get(user.user_id).await?;
    let online = state.presence().is_online(stored.id, state.online_ttl());
    Ok(Json(ProfileDto {
        id: stored.id,
        address: stored.address,
        username: stored.username,
        created_at: stored.created_at,
        online,
    }))
}

/// `PUT /profile` — set or change the authenticated caller's username.
///
/// The body is `{ "username": "<name>" }`. The name is validated for length
/// (3–20 characters) and an `[A-Za-z0-9_-]` character set; a violation is a
/// **422 Unprocessable Entity**. Uniqueness is enforced **case-insensitively**
/// by the store: a name already held by another user (in any casing) is a
/// **409 Conflict**. On success the updated [`ProfileDto`] is returned.
async fn update_profile(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<UpdateProfileRequest>,
) -> ApiResult<Json<ProfileDto>> {
    let name = validate_username(&body.username)?;

    // The store maps a case-insensitive clash to `StorageError::Conflict`, which
    // the standard `From` conversion turns into `ApiError::Conflict` (409).
    state
        .storage()
        .users()
        .set_username(user.user_id, name)
        .await?;

    let stored = state.storage().users().get(user.user_id).await?;
    let online = state.presence().is_online(stored.id, state.online_ttl());
    Ok(Json(ProfileDto {
        id: stored.id,
        address: stored.address,
        username: stored.username,
        created_at: stored.created_at,
        online,
    }))
}

/// `GET /users/{id}/ratings` — every `(variant, time_class)` rating the user
/// holds.
///
/// Returns `{ user_id, ratings: [{ variant_id, time_class, rating, provisional }] }`,
/// where `provisional` is `true` when the rating's deviation is still above the
/// [`PROVISIONAL_DEVIATION_THRESHOLD`]. A user with no rated games yields an
/// empty `ratings` list. A missing user is a **404 Not Found**.
async fn get_user_ratings(
    State(state): State<AppState>,
    Path(id): Path<UserId>,
) -> ApiResult<Json<UserRatingsResponse>> {
    // Confirm the user exists so an unknown id is a clean 404 rather than an
    // empty list that could be confused with a rated-but-empty account.
    let _user = state.storage().users().get(id).await?;

    let ratings = state.storage().ratings().list_for_user(id).await?;
    let ratings = ratings
        .into_iter()
        .map(|(variant_id, time_class, rating)| UserRatingDto {
            provisional: is_provisional(rating.deviation),
            variant_id,
            time_class,
            rating: rating.into(),
        })
        .collect();

    Ok(Json(UserRatingsResponse {
        user_id: id,
        ratings,
    }))
}

/// `GET /users/{id}/rating-history?variant=&time_class=&limit=` — a user's
/// rating trail for a `(variant, time_class)`, most-recent-first.
///
/// `time_class` is optional and defaults to [`DEFAULT_TIME_CLASS`] (an unknown
/// value is a **422**). `limit` is clamped to [`MAX_HISTORY_LIMIT`] and defaults
/// to [`DEFAULT_HISTORY_LIMIT`]. A missing user is a **404 Not Found**; a
/// `(variant, time_class)` the user has no history in yields an empty list.
async fn get_user_rating_history(
    State(state): State<AppState>,
    Path(id): Path<UserId>,
    Query(query): Query<RatingHistoryQuery>,
) -> ApiResult<Json<RatingHistoryResponse>> {
    // Confirm the user exists so an unknown id is a clean 404.
    let _user = state.storage().users().get(id).await?;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .min(MAX_HISTORY_LIMIT);
    let time_class = resolve_time_class(query.time_class.as_deref())?;

    let history = state
        .storage()
        .rating_history()
        .list(id, &query.variant, time_class, limit)
        .await?;

    let entries = history
        .into_iter()
        .map(|e| RatingHistoryEntryDto {
            value: e.value,
            deviation: e.deviation,
            game_id: e.game_id,
            created_at: e.created_at,
        })
        .collect();

    Ok(Json(RatingHistoryResponse {
        user_id: id,
        variant_id: query.variant,
        time_class,
        entries,
    }))
}

/// `GET /users/{id}/stats?variant=&time_class=` — a user's win/loss/draw record
/// and performance rating, aggregated per `(variant, time_class)`.
///
/// The handler folds the user's **finished** games (over
/// [`GameRepo::finished_games_for_user`](mcs_storage::GameRepo::finished_games_for_user))
/// into per-category tallies plus an `overall` total:
///
/// - **win** — the outcome's winner is the user's colour;
/// - **loss** — the winner is the opposing colour;
/// - **draw** — the outcome has no winner.
///
/// Games with no outcome (aborted / no result recorded) are skipped entirely:
/// they never count towards any tally. The `time_class` of each game is derived
/// from its time control.
///
/// The **performance rating** of a category is
/// `round(avg(opponent_rating) + 400 * (wins - losses) / games)` over that
/// category's **rated** games. Each opponent contributes their *current*
/// `(variant, time_class)` rating (defaulting to the Glicko-2 seed when they
/// have none) — see [`StatsCategoryDto::performance_rating`]. A category with no
/// rated games omits the field (`null`).
///
/// The optional `variant`/`time_class` query parameters narrow the returned
/// `categories` to matching ones; `overall` always spans every finished game. A
/// missing user is a **404 Not Found**; an unknown `time_class` is a **422**.
async fn get_user_stats(
    State(state): State<AppState>,
    Path(id): Path<UserId>,
    Query(query): Query<UserStatsQuery>,
) -> ApiResult<Json<UserStatsResponse>> {
    // Confirm the user exists so an unknown id is a clean 404 rather than an
    // empty-but-successful body.
    let _user = state.storage().users().get(id).await?;

    // Validate the optional time-class filter up front (an unknown value is a
    // 422, mirroring the leaderboard/history endpoints).
    let time_class_filter = match query.time_class.as_deref() {
        None => None,
        Some(s) => Some(s.parse::<TimeClass>().map_err(|_| {
            ApiError::UnprocessableEntity(format!(
                "unknown time class {s:?}; expected one of bullet, blitz, rapid, \
                 classical, correspondence"
            ))
        })?),
    };

    let games = state.storage().games().finished_games_for_user(id).await?;

    // Accumulate per-(variant, time_class) tallies plus the running opponent
    // total over rated games (the performance-rating numerator).
    let mut overall = RecordDto::default();
    let mut buckets: std::collections::HashMap<(String, TimeClass), StatsBucket> =
        std::collections::HashMap::new();

    for game in games {
        // Determine the user's colour and their opponent. A finished game always
        // has the user on one side (the repo filtered for it).
        let (user_color, opponent) = if game.white == id {
            (Color::White, game.black)
        } else {
            (Color::Black, game.white)
        };

        // Classify the result against the outcome's winner. No outcome → skip.
        let Some(outcome) = game.outcome.as_ref() else {
            continue;
        };
        let result = match outcome.winner {
            None => GameResult::Draw,
            Some(winner) if winner == user_color => GameResult::Win,
            Some(_) => GameResult::Loss,
        };

        let time_class = game.time_control.time_class();
        overall.record(result);

        let bucket = buckets
            .entry((game.variant_id.clone(), time_class))
            .or_default();
        bucket.record.record(result);

        // The performance rating draws only on rated games. Defer the opponent
        // rating lookup to after the fold so each opponent is read once per
        // bucket-game (rather than holding the rating store across the loop).
        if game.rated {
            bucket.rated_games.push(RatedGame {
                opponent,
                variant_id: game.variant_id.clone(),
                time_class,
            });
        }
    }

    // A stable category order — by variant id then time class — so the response
    // is deterministic regardless of `HashMap` iteration order.
    let mut buckets: Vec<((String, TimeClass), StatsBucket)> = buckets.into_iter().collect();
    buckets.sort_by(|((va, ta), _), ((vb, tb), _)| {
        va.cmp(vb).then_with(|| ta.as_str().cmp(tb.as_str()))
    });

    // Resolve performance ratings: for each bucket with rated games, average the
    // opponents' *current* ratings and apply the Elo-style net-result spread.
    let ratings = state.storage().ratings();
    let mut categories = Vec::with_capacity(buckets.len());
    for ((variant_id, time_class), bucket) in buckets {
        // Apply the optional filters: a category is kept only if it matches both
        // the variant filter (when present) and the time-class filter.
        if let Some(ref want) = query.variant {
            if &variant_id != want {
                continue;
            }
        }
        if let Some(want_tc) = time_class_filter {
            if time_class != want_tc {
                continue;
            }
        }

        let performance_rating = if bucket.rated_games.is_empty() {
            None
        } else {
            let games = bucket.rated_games.len() as f64;
            let mut opponent_sum = 0.0_f64;
            for rated in &bucket.rated_games {
                // Each opponent's *current* rating in this (variant, time_class);
                // a never-rated opponent contributes the Glicko-2 seed.
                let rating = ratings
                    .get(rated.opponent, &rated.variant_id, rated.time_class)
                    .await?
                    .unwrap_or_default();
                opponent_sum += rating.value;
            }
            let wins = f64::from(bucket.record.wins);
            let losses = f64::from(bucket.record.losses);
            let perf = opponent_sum / games + PERFORMANCE_SPREAD * (wins - losses) / games;
            Some(perf.round() as i64)
        };

        categories.push(StatsCategoryDto {
            variant_id,
            time_class,
            wins: bucket.record.wins,
            losses: bucket.record.losses,
            draws: bucket.record.draws,
            total: bucket.record.total,
            performance_rating,
        });
    }

    Ok(Json(UserStatsResponse {
        user_id: id,
        overall,
        categories,
    }))
}

/// The result of a finished game from one player's perspective.
#[derive(Debug, Clone, Copy)]
enum GameResult {
    Win,
    Loss,
    Draw,
}

impl RecordDto {
    /// Folds one game result into this tally, bumping the matching counter and
    /// the total.
    fn record(&mut self, result: GameResult) {
        match result {
            GameResult::Win => self.wins += 1,
            GameResult::Loss => self.losses += 1,
            GameResult::Draw => self.draws += 1,
        }
        self.total += 1;
    }
}

/// One rated game's performance-rating inputs: the opponent and the
/// `(variant, time_class)` whose *current* opponent rating to fetch.
#[derive(Debug, Clone)]
struct RatedGame {
    opponent: UserId,
    variant_id: String,
    time_class: TimeClass,
}

/// Per-`(variant, time_class)` accumulator while folding a user's finished
/// games: the running W/L/D tally and the rated games that feed the performance
/// rating.
#[derive(Debug, Default)]
struct StatsBucket {
    record: RecordDto,
    rated_games: Vec<RatedGame>,
}

// ---------------------------------------------------------------------------
// OpenAPI path documentation (#127)
//
// These marker functions are never routed; they exist only to carry the
// `#[utoipa::path]` metadata for the handlers above, which `crate::openapi`
// collects into the aggregated document. Keeping them beside the handlers keeps
// the documented contract close to the code that implements it.
// ---------------------------------------------------------------------------

/// `POST /seeks` — post a seek; queue it or pair it into a game.
#[utoipa::path(
    post,
    path = "/seeks",
    tag = "seeks",
    security(("bearerAuth" = [])),
    request_body = CreateSeekRequest,
    responses(
        (status = 200, description = "Seek queued, or paired into a new game.", body = CreateSeekResponse),
        (status = 401, description = "Unauthenticated.", body = crate::openapi::ProblemDetails),
        (status = 402, description = "Payment required (only when the x402 gate is enabled)."),
        (status = 422, description = "Invalid time control or colour preference.", body = crate::openapi::ProblemDetails),
        (status = 429, description = "Rate limited.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn create_seek_doc() {}

/// `GET /seeks` — browse the open-seek lobby (public).
#[utoipa::path(
    get,
    path = "/seeks",
    tag = "seeks",
    responses(
        (status = 200, description = "The open-seek lobby.", body = SeekListResponse),
    ),
)]
#[allow(dead_code)]
pub(crate) fn list_seeks_doc() {}

/// `POST /seeks/{id}/accept` — join an open seek directly, creating the game.
#[utoipa::path(
    post,
    path = "/seeks/{id}/accept",
    tag = "seeks",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Seek id (UUID).")),
    responses(
        (status = 200, description = "The created game.", body = GameDto),
        (status = 400, description = "Cannot accept your own seek.", body = crate::openapi::ProblemDetails),
        (status = 401, description = "Unauthenticated.", body = crate::openapi::ProblemDetails),
        (status = 404, description = "No such open seek.", body = crate::openapi::ProblemDetails),
        (status = 409, description = "The seek has already been taken.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn accept_seek_doc() {}

/// `DELETE /seeks/{id}` — cancel one of the caller's own open seeks.
#[utoipa::path(
    delete,
    path = "/seeks/{id}",
    tag = "seeks",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Seek id (UUID).")),
    responses(
        (status = 200, description = "The seek was cancelled.", body = CancelSeekResponse),
        (status = 401, description = "Unauthenticated.", body = crate::openapi::ProblemDetails),
        (status = 403, description = "Only the creator may cancel.", body = crate::openapi::ProblemDetails),
        (status = 404, description = "No such open seek.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn cancel_seek_doc() {}

/// `GET /games/{id}` — fetch a single game record by id (public).
#[utoipa::path(
    get,
    path = "/games/{id}",
    tag = "games",
    params(("id" = String, Path, description = "Game id (UUID).")),
    responses(
        (status = 200, description = "The game, with both players' current ratings.", body = GameDto),
        (status = 404, description = "No such game.", body = crate::openapi::ProblemDetails),
        (status = 422, description = "Malformed game id.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn get_game_doc() {}

/// `GET /games` — list the most recently created games (public).
#[utoipa::path(
    get,
    path = "/games",
    tag = "games",
    params(
        ("limit" = Option<u32>, Query, description = "Max games to return (clamped to 100; default 20)."),
    ),
    responses(
        (status = 200, description = "Recent games, newest first.", body = GameListResponse),
    ),
)]
#[allow(dead_code)]
pub(crate) fn list_games_doc() {}

/// `GET /leaderboard` — paginated leaderboard for a `(variant, time_class)` (public).
#[utoipa::path(
    get,
    path = "/leaderboard",
    tag = "leaderboard",
    params(
        ("variant" = String, Query, description = "The variant to rank (e.g. \"standard\"). Required."),
        ("time_class" = Option<String>, Query, description = "Time class to rank (bullet|blitz|rapid|classical|correspondence; default blitz)."),
        ("offset" = Option<u32>, Query, description = "Zero-based starting rank (default 0)."),
        ("limit" = Option<u32>, Query, description = "Max entries per page (clamped to 200; default 50)."),
    ),
    responses(
        (status = 200, description = "One page of ranked players, highest-rated first, with total count.", body = LeaderboardResponse),
        (status = 422, description = "Unknown time class.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn leaderboard_doc() {}

/// `GET /profile` — the authenticated caller's own profile.
#[utoipa::path(
    get,
    path = "/profile",
    tag = "profile",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The caller's profile.", body = ProfileDto),
        (status = 401, description = "Unauthenticated.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn my_profile_doc() {}

/// `PUT /profile` — set or change the caller's username.
#[utoipa::path(
    put,
    path = "/profile",
    tag = "profile",
    security(("bearerAuth" = [])),
    request_body = UpdateProfileRequest,
    responses(
        (status = 200, description = "The updated profile.", body = ProfileDto),
        (status = 401, description = "Unauthenticated.", body = crate::openapi::ProblemDetails),
        (status = 409, description = "Username already taken (case-insensitive).", body = crate::openapi::ProblemDetails),
        (status = 422, description = "Username fails length/charset validation.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn update_profile_doc() {}

/// `GET /users/{id}` — a user's public profile.
#[utoipa::path(
    get,
    path = "/users/{id}",
    tag = "profile",
    params(("id" = String, Path, description = "User id (UUID).")),
    responses(
        (status = 200, description = "The user's public profile.", body = ProfileDto),
        (status = 404, description = "No such user.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn get_profile_doc() {}

/// `GET /users/{id}/status` — a user's real-time presence status.
#[utoipa::path(
    get,
    path = "/users/{id}/status",
    tag = "profile",
    params(("id" = String, Path, description = "User id (UUID).")),
    responses(
        (status = 200, description = "Online flag and last-seen timestamp.", body = UserStatusResponse),
        (status = 404, description = "No such user.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn get_user_status_doc() {}

/// `GET /users/{id}/ratings` — a user's per-variant ratings.
#[utoipa::path(
    get,
    path = "/users/{id}/ratings",
    tag = "profile",
    params(("id" = String, Path, description = "User id (UUID).")),
    responses(
        (status = 200, description = "One rating per variant the user has played.", body = UserRatingsResponse),
        (status = 404, description = "No such user.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn get_user_ratings_doc() {}

/// `GET /users/{id}/rating-history` — a user's rating trail for a variant.
#[utoipa::path(
    get,
    path = "/users/{id}/rating-history",
    tag = "profile",
    params(
        ("id" = String, Path, description = "User id (UUID)."),
        ("variant" = String, Query, description = "The variant to report (e.g. \"standard\"). Required."),
        ("time_class" = Option<String>, Query, description = "Time class to report (bullet|blitz|rapid|classical|correspondence; default blitz)."),
        ("limit" = Option<u32>, Query, description = "Max snapshots (clamped to 200; default 50)."),
    ),
    responses(
        (status = 200, description = "Rating snapshots, most-recent-first.", body = RatingHistoryResponse),
        (status = 404, description = "No such user.", body = crate::openapi::ProblemDetails),
        (status = 422, description = "Unknown time class.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn get_user_rating_history_doc() {}

/// `GET /users/{id}/stats` — a user's win/loss/draw record and performance
/// rating, per `(variant, time_class)`.
#[utoipa::path(
    get,
    path = "/users/{id}/stats",
    tag = "profile",
    params(
        ("id" = String, Path, description = "User id (UUID)."),
        ("variant" = Option<String>, Query, description = "Optional variant filter (e.g. \"standard\"). Narrows the returned categories."),
        ("time_class" = Option<String>, Query, description = "Optional time-class filter (bullet|blitz|rapid|classical|correspondence). Narrows the returned categories."),
    ),
    responses(
        (status = 200, description = "Overall W/L/D plus per-category tallies and performance ratings.", body = UserStatsResponse),
        (status = 404, description = "No such user.", body = crate::openapi::ProblemDetails),
        (status = 422, description = "Unknown time class.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn get_user_stats_doc() {}
