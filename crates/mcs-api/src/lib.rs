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
//! | `GET /auth/nonce`    | issue a single-use SIWE challenge |
//! | `POST /auth/verify`  | verify the signed challenge, mint a session JWT |
//! | `GET /ws/game/{id}`  | upgrade to the live-game WebSocket ([`ws`]) |
//! | `POST /seeks`        | post a seek; queue it or pair it into a game ([`rest`]) |
//! | `DELETE /seeks/{id}` | cancel one of the caller's own seeks ([`rest`]) |
//! | `GET /games/{id}`    | fetch a single game by id ([`rest`]) |
//! | `GET /games`         | list recent games ([`rest`]) |
//! | `GET /users/{id}`    | a user's public profile ([`rest`]) |
//! | `GET /profile`       | the authenticated caller's profile ([`rest`]) |
//!
//! The WebSocket layer (#15, [`ws`]) streams a live game over a single socket,
//! authenticating with the session JWT passed as a `?token=` query parameter and
//! resolving the caller's [`Color`](mcs_core::Color) (or spectator) from the
//! game record in the shared [`GameHub`]. The REST game endpoints (#14, [`rest`])
//! create those games — pairing seeks, spawning actors, and registering them in
//! the same hub — and read them back over plain HTTP. Game creation is isolated
//! on [`rest::seek_router`] so a future x402 payment middleware can wrap only it.
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
pub mod error;
pub mod extract;
pub mod hub;
pub mod rest;
pub mod state;
pub mod ws;

use axum::Router;

pub use error::{ApiError, ApiResult};
pub use extract::AuthUser;
pub use hub::GameHub;
pub use rest::{
    CancelSeekResponse, CreateSeekRequest, CreateSeekResponse, GameDto, GameListResponse,
    ProfileDto,
};
pub use state::{AppState, SiweConfig};
pub use ws::{ClientMessage, ServerMessage, PROTOCOL_VERSION};

/// Builds the top-level HTTP router for the MCS API.
///
/// The supplied [`AppState`] is attached to the router so every handler and
/// extractor can reach the shared storage, session, and SIWE configuration.
/// Mount the result under a server with [`axum::serve`].
///
/// As later issues land, their sub-routers are merged in here; the auth routes
/// and the [`AuthUser`] extractor are unaffected by those additions.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(auth::auth_router())
        .merge(ws::ws_router())
        // Game creation (`POST /seeks`) is isolated on its own sub-router so a
        // future x402 payment middleware can wrap only it; see
        // [`rest::seek_router`].
        .merge(rest::seek_router())
        .merge(rest::read_router())
        .with_state(state)
}
