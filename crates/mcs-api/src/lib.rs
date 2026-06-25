//! # mcs-api
//!
//! HTTP API layer for the Modular Chess Server (MCS).
//!
//! This crate owns the **error** and **response** contract for the entire
//! HTTP surface. It defines:
//!
//! - [`error::ApiError`] — the single error type used by every handler,
//!   covering all HTTP failure modes (404, 409, 400, 401, 403, 422, 500).
//! - [`error::ApiError`] implements [`axum::response::IntoResponse`] and
//!   produces RFC 9457 `application/problem+json` responses.
//! - [`ApiResult<T>`] — a convenient alias for `Result<T, ApiError>`.
//! - [`From`] conversions from every domain-layer error type
//!   ([`mcs_storage::error::StorageError`], [`mcs_auth::AuthError`],
//!   [`mcs_domain::DomainError`], [`mcs_core::GameError`]) so handlers can
//!   propagate errors with `?`.
//!
//! ## Routers and endpoints
//!
//! [`router`] assembles the top-level [`axum::Router`] from per-area
//! sub-routers:
//!
//! | Method & path        | Handler |
//! |----------------------|---------|
//! | `GET /variants`      | list every registered variant ([`variants`]) |
//! | `GET /auth/nonce`    | issue a single-use SIWE challenge |
//! | `POST /auth/verify`  | verify the signed challenge, mint a session JWT |
//! | `POST /auth/logout`  | revoke the caller's current session token |
//! | `GET /ws/game/{id}`  | upgrade to the live-game WebSocket ([`ws`]) |
//! | `POST /seeks`        | post a seek; queue it or pair it into a game ([`rest`]) |
//! | `DELETE /seeks/{id}` | cancel one of the caller's own seeks ([`rest`]) |
//! | `POST /challenges`              | challenge a specific opponent ([`challenges`]) |
//! | `GET /challenges`               | list the caller's pending challenges ([`challenges`]) |
//! | `POST /challenges/{id}/accept`  | accept a challenge; create the game ([`challenges`]) |
//! | `POST /challenges/{id}/decline` | decline a challenge ([`challenges`]) |
//! | `DELETE /challenges/{id}`       | cancel one's own challenge ([`challenges`]) |
//! | `GET /games/{id}`         | fetch a single game by id ([`rest`]) |
//! | `GET /games`              | list recent games ([`rest`]) |
//! | `POST /games/{id}/rematch`| offer a rematch from a finished game ([`challenges`]) |
//! | `GET /games/{id}/moves`   | full action log for a game, ordered by ply ([`history`]) |
//! | `GET /games/{id}/pgn`     | PGN export for board-style variants ([`history`]) |
//! | `GET /leaderboard`        | top-rated players for a variant ([`rest`]) |
//! | `GET /users/{id}`         | a user's public profile ([`rest`]) |
//! | `GET /profile`            | the authenticated caller's profile ([`rest`]) |
//!
//! The WebSocket layer (#15, [`ws`]) streams a live game over a single socket,
//! authenticating with the session JWT passed as a `?token=` query parameter and
//! resolving the caller's [`Color`](mcs_core::Color) (or spectator) from the
//! game record in the shared [`GameHub`]. The REST game endpoints (#14, [`rest`])
//! create those games — pairing seeks, spawning actors, and registering them in
//! the same hub — and read them back over plain HTTP. Game creation is isolated
//! on [`rest::create_seek_router`] so the x402 payment middleware (#45) wraps
//! only it when an [`AppState`] carries a [`PaymentGate`](state::PaymentGate).
//! All HTTP handlers return [`ApiResult<T>`] so the error contract applies
//! everywhere.
//!
//! ## Authentication
//!
//! Login is Sign-In with Ethereum (see [`auth`]); authenticated routes take an
//! [`AuthUser`] argument, which validates the `Authorization: Bearer <jwt>`
//! header and yields the caller's [`UserId`](mcs_domain::UserId) and address.
//!
//! ## Security
//!
//! Internal errors (`ApiError::Internal`) log the real cause via
//! [`tracing::error!`] but replace it with a generic message in the HTTP
//! response body to avoid leaking server internals to callers.
#![doc(html_root_url = "https://docs.rs/mcs-api")]

pub mod auth;
pub mod challenges;
pub mod error;
pub mod extract;
pub mod history;
pub mod hub;
pub mod limits;
pub mod metrics;
pub mod presence;
pub mod rating;
pub mod ready;
pub mod rest;
pub mod state;
pub mod table;
pub mod variants;
pub mod ws;

use std::sync::Arc;

use axum::Router;
use mcs_payments::RequirePaymentLayer;

pub use challenges::{ChallengeDto, ChallengeListResponse, CreateChallengeRequest};
pub use error::{ApiError, ApiResult};
pub use extract::AuthUser;
pub use history::{MoveEntry, MovesResponse};
pub use hub::GameHub;
pub use limits::{
    LimitsConfig, LiveGameRegistry, RateDecision, RateLimitTier, RateLimiter, WsConnectionRegistry,
};
pub use metrics::{
    GAMES_CREATED_TOTAL, GAMES_LIVE, HTTP_REQUESTS_TOTAL, HTTP_REQUEST_DURATION,
    RATING_UPDATES_TOTAL, WS_CONNECTIONS_ACTIVE,
};
pub use presence::{InProcessPresence, PresenceTracker};
pub use rating::RatingUpdateHook;
pub use ready::ready_router;
pub use rest::{
    CancelSeekResponse, CreateSeekRequest, CreateSeekResponse, GameDto, GameListResponse,
    LeaderboardEntry, LeaderboardQuery, LeaderboardResponse, ProfileDto, RatingDto,
    UserStatusResponse,
};
pub use state::{AppState, Cluster, PaymentGate, SiweConfig, DEFAULT_ONLINE_TTL};
pub use table::{TableChannel, TableEvent, TableHub};
pub use variants::{VariantDto, VariantListResponse};
pub use ws::{ClientMessage, OwnerInfo, RedirectBody, ServerMessage, PROTOCOL_VERSION};

/// Builds the top-level HTTP router for the MCS API.
///
/// The supplied [`AppState`] is attached to the router so every handler and
/// extractor can reach the shared storage, session, and SIWE configuration.
/// Mount the result under a server with [`axum::serve`].
///
/// As later issues land, their sub-routers are merged in here; the auth routes
/// and the [`AuthUser`] extractor are unaffected by those additions.
///
/// # x402 payment gate (#45)
///
/// When the [`AppState`] carries a [`PaymentGate`](state::PaymentGate) (set via
/// [`AppState::with_payment`]), the `POST /seeks` creation route — and only that
/// route — is wrapped in a
/// [`RequirePaymentLayer`](mcs_payments::RequirePaymentLayer): an unpaid request
/// receives `402 Payment Required` with the advertised terms, while a request
/// carrying a valid `X-PAYMENT` header proceeds to the handler. When no gate is
/// configured (the default), creation is free and this router is byte-for-byte
/// the one that shipped before payments existed.
///
/// # Observability (#88)
///
/// Every route is wrapped in the [`metrics::http_metrics`] middleware, which
/// records a request counter and latency histogram labelled by method, the
/// matched **route template** (so ids never inflate cardinality), and status.
/// A `GET /ready` readiness probe ([`ready`]) is merged in: it verifies the
/// database (and, in a cluster, Redis) are reachable before reporting `200`.
/// The Prometheus recorder and the `GET /metrics` scrape endpoint are installed
/// by the composition root (`mcs-server`), which owns the exporter.
pub fn router(state: AppState) -> Router {
    use axum::middleware::from_fn_with_state;

    // Game creation is gated when (and only when) a payment gate is configured.
    // The layer wraps the one-route `create_seek_router` so cancellation, reads,
    // auth, and the WebSocket all stay free.
    let create_seeks = match state.payment_gate() {
        Some(gate) => rest::create_seek_router().layer(RequirePaymentLayer::new(
            gate.requirements().to_vec(),
            Arc::clone(gate.verifier()),
            // Inject the settled-payment store so a duplicated paid request is
            // served from the prior settlement (idempotent — no second charge,
            // no second settle) and each settlement persists (#108).
            Arc::clone(state.payment_store()),
        )),
        None => rest::create_seek_router(),
    };

    // Per-IP rate limiting on the abuse-prone routes (#100). Each tier wraps only
    // its own route(s): the auth nonce/verify routes and the game-creation routes
    // (`POST /seeks`, `POST /challenges`). The layer runs before the route's auth
    // extractor so an over-limit caller is throttled cheaply, without a DB read.
    // All limits are **per node** — see [`limits`].
    let nonce =
        auth::nonce_router().layer(from_fn_with_state(state.clone(), limits::rate_limit_nonce));
    let verify =
        auth::verify_router().layer(from_fn_with_state(state.clone(), limits::rate_limit_verify));
    let create_seeks =
        create_seeks.layer(from_fn_with_state(state.clone(), limits::rate_limit_create));
    let create_challenges = challenges::create_challenge_router()
        .layer(from_fn_with_state(state.clone(), limits::rate_limit_create));

    Router::new()
        .merge(variants::variants_router())
        .merge(nonce)
        .merge(verify)
        // Logout (#101) requires auth and revokes the caller's own token, so it
        // is not behind the per-IP login rate limiter; merge it directly.
        .merge(auth::logout_router())
        .merge(ws::ws_router())
        .merge(create_seeks)
        .merge(create_challenges)
        .merge(rest::accept_seek_router())
        .merge(rest::cancel_seek_router())
        .merge(challenges::challenges_router())
        .merge(challenges::rematch_game_router())
        .merge(rest::read_router())
        .merge(history::history_router())
        .merge(ready::ready_router())
        // Record per-request metrics for every route above. Applied as the
        // outermost API layer so the matched route template is already resolved
        // when the middleware reads it for the low-cardinality `path` label.
        .layer(axum::middleware::from_fn(metrics::http_metrics))
        .with_state(state)
}
