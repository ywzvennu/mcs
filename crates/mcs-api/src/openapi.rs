//! OpenAPI 3.1 document and the Scalar docs UI (#127).
//!
//! This module assembles a single [`utoipa::OpenApi`] document
//! ([`ApiDoc`]) covering the entire HTTP surface and exposes it two ways:
//!
//! | Method & path     | Purpose |
//! |-------------------|---------|
//! | `GET /openapi.json` | The machine-readable OpenAPI 3.1 document. |
//! | `GET /docs`         | An interactive [Scalar](https://scalar.com) docs UI. |
//!
//! Both routes are unauthenticated reads and are merged into the top-level
//! router by [`crate::router`].
//!
//! # How the schema stays accurate
//!
//! Every request/response DTO in this crate derives [`utoipa::ToSchema`]
//! directly, so its OpenAPI schema is generated from the *same* struct the
//! handler serialises — the field names and `#[serde(...)]` renames can never
//! drift from the wire contract.
//!
//! The DTOs reference value objects that live in `mcs-domain` / `mcs-core`
//! (e.g. [`UserId`](mcs_domain::UserId), [`TimeControl`](mcs_domain::TimeControl),
//! [`GameLifecycle`](mcs_domain::GameLifecycle)). Those crates are intentionally
//! **not** modified to depend on `utoipa`, so this module provides faithful
//! schema *mirrors* for them ([`schema`]) and the DTO fields point at the mirror
//! with `#[schema(value_type = ...)]`. Each mirror reproduces the exact serde
//! representation of the domain type (verified against the domain type's own
//! `#[serde(...)]` attributes), so the documented shape matches what the server
//! actually emits and accepts.
//!
//! # WebSocket
//!
//! The live-game WebSocket (`GET /ws/game/{id}`) is a bidirectional protocol,
//! not a REST resource, so it is described in the document's top-level
//! description rather than forced into a path with a misleading request/response
//! body. Clients should consult [`crate::ws`] for its frame protocol.

use axum::routing::get;
use axum::{Json, Router};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_scalar::{Scalar, Servable};

use crate::state::AppState;

use crate::auth::{ChallengeFields, NonceResponse, VerifyRequest, VerifyResponse};
use crate::challenges::{ChallengeDto, ChallengeListResponse, CreateChallengeRequest};
use crate::history::{MoveEntry, MovesResponse};
use crate::rest::{
    CancelSeekResponse, CreateSeekRequest, CreateSeekResponse, GameDto, GameListResponse,
    LeaderboardEntry, LeaderboardResponse, ProfileDto, RatingDto, RatingHistoryEntryDto,
    RatingHistoryResponse, SeekCreatorDto, SeekDto, SeekListResponse, UpdateProfileRequest,
    UserRatingDto, UserRatingsResponse, UserStatusResponse,
};
use crate::variants::{VariantDto, VariantListResponse};

/// Schema mirrors for the `mcs-domain` / `mcs-core` value objects.
///
/// These types exist only to feed `utoipa` an accurate OpenAPI schema for the
/// foreign value objects the DTOs embed, without making the domain crates depend
/// on `utoipa`. Each mirror reproduces the wire representation of its domain
/// counterpart exactly; the DTOs reference them via `#[schema(value_type = ...)]`.
pub mod schema {
    use serde::Serialize;
    use utoipa::ToSchema;

    /// Mirror of [`mcs_domain::ColorPreference`] (`#[serde(rename_all = "snake_case")]`).
    #[derive(Debug, Serialize, ToSchema)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code)]
    pub enum ColorPreference {
        /// Wants the white pieces.
        White,
        /// Wants the black pieces.
        Black,
        /// Accepts either side.
        Random,
    }

    /// Mirror of [`mcs_core::Color`] (`#[serde(rename_all = "lowercase")]`).
    #[derive(Debug, Serialize, ToSchema)]
    #[serde(rename_all = "lowercase")]
    #[allow(dead_code)]
    pub enum Color {
        /// The side that moves first.
        White,
        /// The side that moves second.
        Black,
    }

    /// Mirror of [`mcs_domain::GameLifecycle`] (`#[serde(rename_all = "snake_case")]`).
    #[derive(Debug, Serialize, ToSchema)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code)]
    pub enum GameLifecycle {
        /// Record created; play not started.
        Created,
        /// Play in progress.
        Active,
        /// Game ended.
        Finished,
    }

    /// Mirror of [`mcs_domain::ChallengeStatus`] (`#[serde(rename_all = "snake_case")]`).
    #[derive(Debug, Serialize, ToSchema)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code)]
    pub enum ChallengeStatus {
        /// Awaiting the challenged player's response.
        Pending,
        /// Accepted; a game was created.
        Accepted,
        /// Declined by the challenged player.
        Declined,
        /// Withdrawn by the challenger.
        Canceled,
    }

    /// Mirror of [`mcs_domain::TimeControl`]
    /// (`#[serde(tag = "type", rename_all = "snake_case")]`).
    ///
    /// ```json
    /// { "type": "real_time", "initial_secs": 300, "increment_secs": 5 }
    /// { "type": "correspondence", "days_per_move": 3 }
    /// { "type": "unlimited" }
    /// ```
    #[derive(Debug, Serialize, ToSchema)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[allow(dead_code)]
    pub enum TimeControl {
        /// A shared real-time clock with optional per-move increment.
        RealTime {
            /// Starting time on each clock, in whole seconds.
            initial_secs: u64,
            /// Seconds added after each move.
            increment_secs: u64,
        },
        /// Days-per-move correspondence pacing.
        Correspondence {
            /// Maximum calendar days per move.
            days_per_move: u32,
        },
        /// No time limit at all.
        Unlimited,
    }

    /// Mirror of a recorded action payload ([`mcs_core::Action`]).
    ///
    /// The action is a type-erased, variant-defined JSON object (e.g.
    /// `{ "type": "move", "uci": "e2e4" }` for board variants, or a `sense`
    /// action for RBC), so it is documented as a free-form object.
    #[derive(Debug, Serialize, ToSchema)]
    #[schema(value_type = Object, example = json!({ "type": "move", "uci": "e2e4" }))]
    #[allow(dead_code)]
    pub struct Action(serde_json::Value);
}

/// The RFC 9457 `application/problem+json` error body returned by every handler.
///
/// This documents the shape produced by
/// [`ApiError::into_response`](crate::error::ApiError). It is registered as a
/// component schema and referenced by the error responses on the paths below.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub struct ProblemDetails {
    /// A URI reference for the problem type; `"about:blank"` when the title is
    /// just the HTTP status phrase.
    #[serde(rename = "type")]
    #[schema(example = "about:blank")]
    pub problem_type: String,
    /// Short, human-readable summary of the problem type (the HTTP reason phrase).
    #[schema(example = "Not Found")]
    pub title: String,
    /// The HTTP status code.
    #[schema(example = 404)]
    pub status: u16,
    /// A human-readable explanation specific to this occurrence.
    #[schema(example = "no game: 0b6e…")]
    pub detail: String,
}

/// Adds the `bearerAuth` security scheme to the generated document.
///
/// Authenticated endpoints reference it via `security(("bearerAuth" = []))` in
/// their `#[utoipa::path]` annotation; the scheme itself is an HTTP `Bearer`
/// (JWT) credential presented as `Authorization: Bearer <token>`.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::new);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "Session JWT from `POST /auth/verify`, sent as \
                         `Authorization: Bearer <token>`.",
                    ))
                    .build(),
            ),
        );
    }
}

/// The aggregated OpenAPI 3.1 document for the whole MCS HTTP API.
#[derive(Debug, OpenApi)]
#[openapi(
    info(
        title = "Modular Chess Server API",
        description = "HTTP API for the Modular Chess Server (MCS): SIWE authentication, \
                       matchmaking seeks, direct challenges, games, move history/PGN, \
                       leaderboards, profiles and ratings.\n\n\
                       Errors are returned as RFC 9457 `application/problem+json` \
                       (see the `ProblemDetails` schema).\n\n\
                       The live-game WebSocket (`GET /ws/game/{id}`) is a bidirectional \
                       protocol rather than a REST resource and is therefore documented \
                       separately, not as an OpenAPI path.",
        license(name = "MIT OR Apache-2.0"),
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "ops", description = "Liveness, readiness and metrics probes."),
        (name = "variants", description = "Registered game variants."),
        (name = "auth", description = "Sign-In with Ethereum authentication."),
        (name = "seeks", description = "Matchmaking seeks and the open-seek lobby."),
        (name = "challenges", description = "Direct challenges and rematches."),
        (name = "games", description = "Game records, move history and PGN export."),
        (name = "leaderboard", description = "Per-variant leaderboards."),
        (name = "profile", description = "User profiles, status and ratings."),
    ),
    paths(
        // ops
        crate::openapi::health_doc,
        crate::ready::ready_doc,
        crate::openapi::metrics_doc,
        // variants
        crate::variants::list_variants_doc,
        // auth
        crate::auth::nonce_doc,
        crate::auth::verify_doc,
        crate::auth::logout_doc,
        // seeks
        crate::rest::create_seek_doc,
        crate::rest::list_seeks_doc,
        crate::rest::accept_seek_doc,
        crate::rest::cancel_seek_doc,
        // challenges
        crate::challenges::create_challenge_doc,
        crate::challenges::list_challenges_doc,
        crate::challenges::accept_challenge_doc,
        crate::challenges::decline_challenge_doc,
        crate::challenges::cancel_challenge_doc,
        crate::challenges::rematch_game_doc,
        // games
        crate::rest::get_game_doc,
        crate::rest::list_games_doc,
        crate::history::get_moves_doc,
        crate::history::get_pgn_doc,
        // leaderboard
        crate::rest::leaderboard_doc,
        // profile
        crate::rest::my_profile_doc,
        crate::rest::update_profile_doc,
        crate::rest::get_profile_doc,
        crate::rest::get_user_status_doc,
        crate::rest::get_user_ratings_doc,
        crate::rest::get_user_rating_history_doc,
    ),
    components(schemas(
        // error contract
        ProblemDetails,
        // domain value-object mirrors
        schema::ColorPreference,
        schema::Color,
        schema::GameLifecycle,
        schema::ChallengeStatus,
        schema::TimeControl,
        schema::Action,
        // ops
        crate::openapi::HealthDoc,
        crate::ready::ReadyDoc,
        crate::ready::NotReadyDoc,
        // variants
        VariantDto,
        VariantListResponse,
        // auth
        ChallengeFields,
        NonceResponse,
        VerifyRequest,
        VerifyResponse,
        // seeks
        CreateSeekRequest,
        CreateSeekResponse,
        SeekCreatorDto,
        SeekDto,
        SeekListResponse,
        CancelSeekResponse,
        // challenges
        CreateChallengeRequest,
        ChallengeDto,
        ChallengeListResponse,
        // games
        RatingDto,
        GameDto,
        GameListResponse,
        MoveEntry,
        MovesResponse,
        // leaderboard
        LeaderboardEntry,
        LeaderboardResponse,
        // profile
        ProfileDto,
        UserStatusResponse,
        UpdateProfileRequest,
        UserRatingDto,
        UserRatingsResponse,
        RatingHistoryEntryDto,
        RatingHistoryResponse,
    )),
)]
pub struct ApiDoc;

// ---------------------------------------------------------------------------
// Path documentation for the operational endpoints served outside `mcs-api`.
//
// `GET /health` and `GET /metrics` are mounted by the composition root
// (`mcs-server`), so their handlers do not live here. To keep the document
// complete and self-contained — and to avoid forcing `mcs-server` to depend on
// `utoipa` — their `#[utoipa::path]` metadata is declared on no-op marker
// functions in this crate. They are never routed; only their generated path
// metadata is collected into [`ApiDoc`].
// ---------------------------------------------------------------------------

/// Liveness response mirror for `GET /health` (`{"status":"ok"}`).
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[allow(dead_code)]
pub struct HealthDoc {
    /// Always `"ok"` while the process is serving.
    #[schema(example = "ok")]
    pub status: String,
}

/// `GET /health` — liveness probe (documentation only; routed by `mcs-server`).
#[utoipa::path(
    get,
    path = "/health",
    tag = "ops",
    responses(
        (status = 200, description = "The process is up and serving.", body = HealthDoc),
    ),
)]
#[allow(dead_code)]
pub(crate) fn health_doc() {}

/// `GET /metrics` — Prometheus exposition (documentation only; routed by
/// `mcs-server`).
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "ops",
    responses(
        (
            status = 200,
            description = "Prometheus exposition text (`text/plain; version=0.0.4`).",
            content_type = "text/plain",
            body = String,
        ),
    ),
)]
#[allow(dead_code)]
pub(crate) fn metrics_doc() {}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /openapi.json` — the generated OpenAPI 3.1 document as JSON.
///
/// The document is built once per call from [`ApiDoc`]; it is small and the
/// route is rarely hit, so there is no need to cache it.
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// Builds the documentation sub-router: `GET /openapi.json` and the Scalar docs
/// UI at `GET /docs`.
///
/// Both routes are public reads. The Scalar page renders the served document
/// in-browser; its JavaScript is loaded from a CDN at runtime, so no third-party
/// web assets are vendored into this binary.
pub fn docs_router() -> Router<AppState> {
    let scalar: Router<AppState> = Scalar::with_url("/docs", ApiDoc::openapi()).into();
    Router::new()
        .route("/openapi.json", get(openapi_json))
        .merge(scalar)
}
