//! REST endpoints for direct challenges and game rematches.
//!
//! A direct challenge invites a **specific** opponent to a game on agreed terms,
//! in contrast to a [`Seek`](mcs_domain::Seek), which floats in an open pool and
//! is paired against an unknown opponent by the matchmaker. Accepting a challenge
//! creates the game directly — there is no matchmaking step — through the shared
//! [`AppState::create_and_spawn_game`] helper, the very same path a paired seek
//! uses.
//!
//! | Method & path                  | Auth | Purpose |
//! |--------------------------------|------|---------|
//! | `POST /challenges`             | yes  | Invite a specific opponent by address. |
//! | `GET /challenges`              | yes  | List the caller's pending incoming/outgoing challenges. |
//! | `POST /challenges/{id}/accept` | yes  | Accept (challenged only); creates the game. |
//! | `POST /challenges/{id}/decline`| yes  | Decline (challenged only). |
//! | `DELETE /challenges/{id}`      | yes  | Cancel (challenger only). |
//! | `POST /games/{id}/rematch`     | yes  | Offer a rematch from a finished game. |
//!
//! # Colour assignment
//!
//! The **challenger** is honoured: their [`ColorPreference`] decides their side
//! when the challenge is accepted, and the challenged player takes the other
//! side. A [`ColorPreference::Random`] is resolved deterministically from the
//! challenge id (its first byte's low bit) so the same challenge always yields
//! the same colours — this needs no RNG and is reproducible in tests, while
//! still being effectively unpredictable to the players.
//!
//! # Rematch colour convention
//!
//! `POST /games/{id}/rematch` creates a pre-filled challenge with
//! `color_preference` set to the **opposite** of the side the caller just played:
//! if the caller was White, `color_preference` is `Black` (so — when the opponent
//! accepts — the caller will play Black in the next game, swapping sides). This
//! matches the lichess convention: rematches automatically alternate colours.

use axum::extract::{Path, State};
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;

use mcs_core::{Color, VariantOptions};
use mcs_domain::{
    Challenge, ChallengeId, ChallengeStatus, ColorPreference, EvmAddress, Game, GameId,
    GameLifecycle, TimeControl, UserId,
};

use crate::error::{ApiError, ApiResult};
use crate::extract::AuthUser;
use crate::rest::GameDto;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /challenges`.
///
/// The opponent is named by Ethereum address; the server resolves (or creates)
/// the corresponding account. The `rated` and `color` fields default so a
/// minimal request — just an opponent, a variant, and a time control — issues a
/// rated challenge with a random colour for the challenger.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateChallengeRequest {
    /// The opponent's Ethereum address (any casing; validated and lowercased).
    pub opponent_address: String,
    /// The variant to play (e.g. `"standard"`).
    pub variant_id: String,
    /// The time control to play under.
    #[schema(value_type = crate::openapi::schema::TimeControl)]
    pub time_control: TimeControl,
    /// Whether the game should be **rated** (the default) or casual.
    #[serde(default = "default_rated")]
    pub rated: bool,
    /// Which side the *challenger* wants. Defaults to
    /// [`ColorPreference::Random`] when omitted.
    #[serde(default = "default_color")]
    #[schema(value_type = crate::openapi::schema::ColorPreference)]
    pub color: ColorPreference,
}

/// The serde default for [`CreateChallengeRequest::rated`]: an absent field
/// means the challenger wants a rated game.
fn default_rated() -> bool {
    true
}

/// The serde default for [`CreateChallengeRequest::color`]: an absent field
/// means the challenger does not mind which side they play.
fn default_color() -> ColorPreference {
    ColorPreference::Random
}

/// The public, serialized view of a [`Challenge`].
///
/// A thin, explicit projection of the domain [`Challenge`] so the HTTP contract
/// does not silently drift when the domain type changes.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ChallengeDto {
    /// The challenge's stable identifier.
    #[schema(value_type = String, format = Uuid)]
    pub id: ChallengeId,
    /// The user who issued the challenge.
    #[schema(value_type = String, format = Uuid)]
    pub challenger: UserId,
    /// The user the challenge was issued to.
    #[schema(value_type = String, format = Uuid)]
    pub challenged: UserId,
    /// The variant to be played.
    pub variant_id: String,
    /// The proposed time control.
    #[schema(value_type = crate::openapi::schema::TimeControl)]
    pub time_control: TimeControl,
    /// Whether the proposed game is rated.
    pub rated: bool,
    /// The challenger's colour preference.
    #[schema(value_type = crate::openapi::schema::ColorPreference)]
    pub color_preference: ColorPreference,
    /// The current lifecycle status.
    #[schema(value_type = crate::openapi::schema::ChallengeStatus)]
    pub status: ChallengeStatus,
    /// The game created on acceptance, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = Uuid)]
    pub game_id: Option<GameId>,
    /// When the challenge was created (RFC 3339, UTC).
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<Challenge> for ChallengeDto {
    fn from(c: Challenge) -> Self {
        Self {
            id: c.id,
            challenger: c.challenger,
            challenged: c.challenged,
            variant_id: c.variant_id,
            time_control: c.time_control,
            rated: c.rated,
            color_preference: c.color_preference,
            status: c.status,
            game_id: c.game_id,
            created_at: c.created_at,
        }
    }
}

/// Response body for `GET /challenges`: the caller's pending challenges, split
/// by direction.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ChallengeListResponse {
    /// Pending challenges issued *to* the caller (awaiting their response).
    pub incoming: Vec<ChallengeDto>,
    /// Pending challenges the caller issued (awaiting the opponent's response).
    pub outgoing: Vec<ChallengeDto>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Builds the challenge **creation** sub-router: the single
/// `POST /challenges` route.
///
/// Isolated from the rest of the challenge lifecycle so [`crate::router`] can
/// wrap *only* this creation route in the per-IP rate-limit layer (#100), the
/// same way `POST /seeks` is rate-limited. `GET /challenges` and the
/// accept/decline/cancel routes stay on [`challenges_router`] and are not
/// throttled by the creation tier.
pub fn create_challenge_router() -> Router<AppState> {
    Router::new().route("/challenges", post(create_challenge))
}

/// Builds the challenges sub-router for everything except creation.
///
/// Every route is authenticated (each handler takes an [`AuthUser`]). The
/// router is merged into [`crate::router`] alongside the seek and read routers.
/// `POST /challenges` lives on its own [`create_challenge_router`] so the
/// creation rate limit (#100) scopes to it alone.
pub fn challenges_router() -> Router<AppState> {
    Router::new()
        .route("/challenges", axum::routing::get(list_challenges))
        .route("/challenges/{id}/accept", post(accept_challenge))
        .route("/challenges/{id}/decline", post(decline_challenge))
        .route("/challenges/{id}", delete(cancel_challenge))
}

/// Builds the rematch sub-router: the single `POST /games/{id}/rematch` route.
///
/// Kept on its own sub-router so it can be merged next to the other game routes
/// in [`crate::router`] without mixing concerns with the challenge lifecycle
/// routes above.
pub fn rematch_game_router() -> Router<AppState> {
    Router::new().route("/games/{id}/rematch", post(rematch_game))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /challenges` — invite a specific opponent to a game.
///
/// Validates the opponent's address, resolves (or creates) their account, and
/// records a [`ChallengeStatus::Pending`] challenge from the caller. A caller
/// who challenges their own address is rejected with **400 Bad Request**.
async fn create_challenge(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateChallengeRequest>,
) -> ApiResult<Json<ChallengeDto>> {
    // Reject an unknown variant up front (400) rather than recording a challenge
    // that could never spawn a game when accepted.
    state.ensure_known_variant(&body.variant_id)?;

    // Validate the opponent address; a malformed address is a 422 via the
    // `From<DomainError>` mapping.
    let opponent_address: EvmAddress = body.opponent_address.parse()?;

    // Resolve (or create) the opponent account by address.
    let opponent = state
        .storage()
        .users()
        .upsert_by_address(&opponent_address)
        .await?;

    // A self-challenge is meaningless; reject it before anything is persisted.
    if opponent.id == user.user_id {
        return Err(ApiError::BadRequest(
            "you cannot challenge yourself".to_owned(),
        ));
    }

    let challenge = Challenge::new(
        user.user_id,
        opponent.id,
        body.variant_id,
        body.time_control,
        body.rated,
        body.color,
        OffsetDateTime::now_utc(),
    );
    state.storage().challenges().create(&challenge).await?;

    Ok(Json(challenge.into()))
}

/// `GET /challenges` — list the caller's pending incoming and outgoing
/// challenges.
async fn list_challenges(
    State(state): State<AppState>,
    user: AuthUser,
) -> ApiResult<Json<ChallengeListResponse>> {
    let challenges = state.storage().challenges();
    let incoming = challenges.list_incoming(user.user_id).await?;
    let outgoing = challenges.list_outgoing(user.user_id).await?;

    Ok(Json(ChallengeListResponse {
        incoming: incoming.into_iter().map(ChallengeDto::from).collect(),
        outgoing: outgoing.into_iter().map(ChallengeDto::from).collect(),
    }))
}

/// `POST /challenges/{id}/accept` — accept a challenge and create the game.
///
/// Only the **challenged** player may accept (else **403 Forbidden**), and only
/// while the challenge is [`Pending`](ChallengeStatus::Pending) (else **409
/// Conflict**). Colours are resolved from the *challenger's* preference (see the
/// module docs), the game is created through the shared
/// [`AppState::create_and_spawn_game`] path, and the challenge is marked
/// [`Accepted`](ChallengeStatus::Accepted) with the new game's id. The created
/// game is returned so the client can open the socket at `/ws/game/{id}`.
async fn accept_challenge(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<ChallengeId>,
) -> ApiResult<Json<GameDto>> {
    let mut challenge = load_challenge(&state, id).await?;

    // Only the challenged party may accept.
    if challenge.challenged != user.user_id {
        return Err(ApiError::Forbidden(
            "only the challenged player may accept this challenge".to_owned(),
        ));
    }
    require_pending(&challenge)?;

    // Resolve colours: the challenger gets their preferred side; the challenged
    // player takes the other.
    let challenger_color = resolve_color(challenge.color_preference, challenge.id);
    let (white, black) = match challenger_color {
        Color::White => (challenge.challenger, challenge.challenged),
        Color::Black => (challenge.challenged, challenge.challenger),
    };

    // Create the game through the shared helper — the identical path a paired
    // seek takes. Challenges carry no per-game options yet, so use the defaults.
    let game = state
        .create_and_spawn_game(
            white,
            black,
            &challenge.variant_id,
            challenge.time_control.clone(),
            challenge.rated,
            VariantOptions::default(),
        )
        .await?;

    // Record the acceptance and the game it produced. The in-memory transition
    // only fires from `Pending`, which `require_pending` already guaranteed.
    challenge.accept(game.id);
    state.storage().challenges().update(&challenge).await?;

    Ok(Json(game.into()))
}

/// `POST /challenges/{id}/decline` — decline a pending challenge.
///
/// Only the **challenged** player may decline (else **403 Forbidden**), and only
/// while the challenge is [`Pending`](ChallengeStatus::Pending) (else **409
/// Conflict**).
async fn decline_challenge(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<ChallengeId>,
) -> ApiResult<Json<ChallengeDto>> {
    let mut challenge = load_challenge(&state, id).await?;
    if challenge.challenged != user.user_id {
        return Err(ApiError::Forbidden(
            "only the challenged player may decline this challenge".to_owned(),
        ));
    }
    require_pending(&challenge)?;

    challenge.decline();
    state.storage().challenges().update(&challenge).await?;
    Ok(Json(challenge.into()))
}

/// `DELETE /challenges/{id}` — cancel a pending challenge.
///
/// Only the **challenger** may cancel (else **403 Forbidden**), and only while
/// the challenge is [`Pending`](ChallengeStatus::Pending) (else **409
/// Conflict**).
async fn cancel_challenge(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<ChallengeId>,
) -> ApiResult<Json<ChallengeDto>> {
    let mut challenge = load_challenge(&state, id).await?;
    if challenge.challenger != user.user_id {
        return Err(ApiError::Forbidden(
            "only the challenger may cancel this challenge".to_owned(),
        ));
    }
    require_pending(&challenge)?;

    challenge.cancel();
    state.storage().challenges().update(&challenge).await?;
    Ok(Json(challenge.into()))
}

/// `POST /games/{id}/rematch` — offer a rematch from a finished game.
///
/// Creates a [`Pending`](ChallengeStatus::Pending) [`Challenge`] pre-filled
/// from the finished game's terms. The caller becomes the challenger; the other
/// player in the original game is the challenged. The `color_preference` is set
/// to the **opposite** of the side the caller played, so — if the opponent
/// accepts — the colours automatically alternate from the previous game.
///
/// # Errors
///
/// - **404 Not Found** — no game with the given id.
/// - **403 Forbidden** — the caller was not a player in that game.
/// - **409 Conflict** — the game has not yet finished
///   (`lifecycle != Finished`).
async fn rematch_game(
    State(state): State<AppState>,
    user: AuthUser,
    Path(game_id): Path<GameId>,
) -> ApiResult<Json<ChallengeDto>> {
    // Load the game; a missing id is 404.
    let game = state
        .storage()
        .games()
        .get(game_id)
        .await
        .map_err(|err| match err {
            mcs_storage::StorageError::NotFound => {
                ApiError::NotFound(format!("no game: {game_id}"))
            }
            other => other.into(),
        })?;

    // Only a player in the original game may offer a rematch.
    if user.user_id != game.white && user.user_id != game.black {
        return Err(ApiError::Forbidden(
            "only a player of the original game may offer a rematch".to_owned(),
        ));
    }

    // A rematch only makes sense once the game has concluded.
    if game.lifecycle != GameLifecycle::Finished {
        return Err(ApiError::Conflict(format!(
            "game {game_id} has not yet finished (lifecycle: {:?})",
            game.lifecycle
        )));
    }

    // Determine the opponent and the caller's color preference for the rematch.
    // The convention: the caller requests the *opposite* side they just played,
    // so colours automatically swap when the opponent accepts.
    let (opponent, color_preference) = rematch_color(&game, user.user_id);

    let challenge = Challenge::new(
        user.user_id,
        opponent,
        game.variant_id,
        game.time_control,
        game.rated,
        color_preference,
        OffsetDateTime::now_utc(),
    );
    state.storage().challenges().create(&challenge).await?;

    Ok(Json(challenge.into()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolves the opponent and the caller's colour preference for a rematch.
///
/// The caller requests the **opposite** side they just played:
///
/// - Caller was White → requests `Black` (the opponent was Black → `opponent = black`).
/// - Caller was Black → requests `White` (the opponent was White → `opponent = white`).
///
/// This means that when the opponent accepts, colours automatically swap vs. the
/// original game — exactly the lichess rematch convention.
///
/// # Panics
///
/// Never; the caller guarantees `user_id` is either `game.white` or `game.black`
/// before calling this (enforced by the 403 check in [`rematch_game`]).
fn rematch_color(game: &Game, user_id: UserId) -> (UserId, ColorPreference) {
    if user_id == game.white {
        // Caller was White last time; they want Black this time.
        (game.black, ColorPreference::Black)
    } else {
        // Caller was Black last time; they want White this time.
        (game.white, ColorPreference::White)
    }
}

/// Resolves the `(white, black)` players for a rematch with the colours swapped
/// from `game`.
///
/// This is the lichess rematch convention — the two players trade sides — applied
/// directly to produce the *new* game's seating: the player who was Black becomes
/// White and vice versa. It is the single source of truth for the swap, shared by
/// the offline REST path (via [`rematch_color`], which expresses the same
/// convention as the *challenger's* colour preference) and the live WebSocket
/// rematch path ([`crate::ws`]), which has both players present and so can create
/// the swapped game immediately.
#[must_use]
pub(crate) fn rematch_colors(game: &Game) -> (UserId, UserId) {
    // Swap: last game's Black plays White now, last game's White plays Black.
    (game.black, game.white)
}

/// Loads a challenge by id, mapping a missing one to a **404 Not Found** with an
/// id-bearing detail (rather than the generic storage not-found message).
async fn load_challenge(state: &AppState, id: ChallengeId) -> ApiResult<Challenge> {
    state
        .storage()
        .challenges()
        .get(id)
        .await
        .map_err(|err| match err {
            mcs_storage::StorageError::NotFound => {
                ApiError::NotFound(format!("no challenge: {id}"))
            }
            other => other.into(),
        })
}

/// Rejects a non-pending challenge with **409 Conflict**.
///
/// A challenge that has already been accepted, declined, or canceled cannot be
/// acted on again, so the second actor is told the resource is in a conflicting
/// state.
fn require_pending(challenge: &Challenge) -> ApiResult<()> {
    if challenge.is_pending() {
        Ok(())
    } else {
        Err(ApiError::Conflict(format!(
            "challenge is not pending (status: {:?})",
            challenge.status
        )))
    }
}

/// Resolves a [`ColorPreference`] into a concrete [`Color`] for the challenger.
///
/// [`White`](ColorPreference::White) and [`Black`](ColorPreference::Black) map
/// directly. [`Random`](ColorPreference::Random) is resolved deterministically
/// from the challenge id — the low bit of the id's first byte — so the outcome
/// is reproducible (no RNG, stable across retries and in tests) while remaining
/// effectively unpredictable to the players.
fn resolve_color(pref: ColorPreference, id: ChallengeId) -> Color {
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
// OpenAPI path documentation (#127)
//
// These `#[utoipa::path]` markers carry the OpenAPI metadata for each route.
// They are never routed — only their generated path metadata is collected into
// `crate::openapi::ApiDoc`. Keeping them next to the handlers keeps the docs in
// step with the real request/response DTOs.
// ---------------------------------------------------------------------------

/// `POST /challenges` — invite a specific opponent to a game.
#[utoipa::path(
    post,
    path = "/challenges",
    tag = "challenges",
    security(("bearerAuth" = [])),
    request_body = CreateChallengeRequest,
    responses(
        (status = 200, description = "Challenge created.", body = ChallengeDto),
        (status = 400, description = "You cannot challenge yourself.", body = crate::openapi::ProblemDetails),
        (status = 401, description = "Missing or invalid bearer token.", body = crate::openapi::ProblemDetails),
        (status = 422, description = "Malformed opponent address.", body = crate::openapi::ProblemDetails),
        (status = 429, description = "Rate limited.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn create_challenge_doc() {}

/// `GET /challenges` — list the caller's pending incoming and outgoing
/// challenges.
#[utoipa::path(
    get,
    path = "/challenges",
    tag = "challenges",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The caller's pending challenges, split by direction.", body = ChallengeListResponse),
        (status = 401, description = "Missing or invalid bearer token.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn list_challenges_doc() {}

/// `POST /challenges/{id}/accept` — accept a challenge and create the game.
#[utoipa::path(
    post,
    path = "/challenges/{id}/accept",
    tag = "challenges",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "The challenge id (UUID)."),
    ),
    responses(
        (status = 200, description = "The challenge was accepted; the created game is returned.", body = crate::rest::GameDto),
        (status = 401, description = "Missing or invalid bearer token.", body = crate::openapi::ProblemDetails),
        (status = 403, description = "Only the challenged player may accept.", body = crate::openapi::ProblemDetails),
        (status = 404, description = "No such challenge.", body = crate::openapi::ProblemDetails),
        (status = 409, description = "The challenge is not pending.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn accept_challenge_doc() {}

/// `POST /challenges/{id}/decline` — decline a pending challenge.
#[utoipa::path(
    post,
    path = "/challenges/{id}/decline",
    tag = "challenges",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "The challenge id (UUID)."),
    ),
    responses(
        (status = 200, description = "The challenge was declined.", body = ChallengeDto),
        (status = 401, description = "Missing or invalid bearer token.", body = crate::openapi::ProblemDetails),
        (status = 403, description = "Only the challenged player may decline.", body = crate::openapi::ProblemDetails),
        (status = 404, description = "No such challenge.", body = crate::openapi::ProblemDetails),
        (status = 409, description = "The challenge is not pending.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn decline_challenge_doc() {}

/// `DELETE /challenges/{id}` — cancel a pending challenge.
#[utoipa::path(
    delete,
    path = "/challenges/{id}",
    tag = "challenges",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "The challenge id (UUID)."),
    ),
    responses(
        (status = 200, description = "The challenge was cancelled.", body = ChallengeDto),
        (status = 401, description = "Missing or invalid bearer token.", body = crate::openapi::ProblemDetails),
        (status = 403, description = "Only the challenger may cancel.", body = crate::openapi::ProblemDetails),
        (status = 404, description = "No such challenge.", body = crate::openapi::ProblemDetails),
        (status = 409, description = "The challenge is not pending.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn cancel_challenge_doc() {}

/// `POST /games/{id}/rematch` — offer a rematch from a finished game.
#[utoipa::path(
    post,
    path = "/games/{id}/rematch",
    tag = "challenges",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "The id (UUID) of the finished game to rematch."),
    ),
    responses(
        (status = 200, description = "A pre-filled rematch challenge was created.", body = ChallengeDto),
        (status = 401, description = "Missing or invalid bearer token.", body = crate::openapi::ProblemDetails),
        (status = 403, description = "The caller was not a player in that game.", body = crate::openapi::ProblemDetails),
        (status = 404, description = "No such game.", body = crate::openapi::ProblemDetails),
        (status = 409, description = "The game has not yet finished.", body = crate::openapi::ProblemDetails),
    ),
)]
#[allow(dead_code)]
pub(crate) fn rematch_game_doc() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_color_honours_explicit_preferences() {
        let id = ChallengeId::new();
        assert_eq!(resolve_color(ColorPreference::White, id), Color::White);
        assert_eq!(resolve_color(ColorPreference::Black, id), Color::Black);
    }

    #[test]
    fn resolve_color_random_is_deterministic_for_an_id() {
        let id = ChallengeId::new();
        // Same id always resolves to the same colour.
        assert_eq!(
            resolve_color(ColorPreference::Random, id),
            resolve_color(ColorPreference::Random, id)
        );
    }

    #[test]
    fn require_pending_rejects_terminal_states() {
        let mut challenge = Challenge::new(
            UserId::new(),
            UserId::new(),
            "standard".to_owned(),
            TimeControl::Unlimited,
            true,
            ColorPreference::White,
            OffsetDateTime::UNIX_EPOCH,
        );
        assert!(require_pending(&challenge).is_ok());
        challenge.decline();
        assert!(matches!(
            require_pending(&challenge),
            Err(ApiError::Conflict(_))
        ));
    }
}
