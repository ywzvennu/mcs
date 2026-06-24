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
//! | `GET /ws/game/{id}`  | upgrade to the live-game WebSocket ([`ws`]) |
//! | `POST /seeks`        | post a seek; queue it or pair it into a game ([`rest`]) |
//! | `DELETE /seeks/{id}` | cancel one of the caller's own seeks ([`rest`]) |
//! | `GET /games/{id}`         | fetch a single game by id ([`rest`]) |
//! | `GET /games`              | list recent games ([`rest`]) |
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
pub mod error;
pub mod extract;
pub mod history;
pub mod hub;
pub mod rating;
pub mod rest;
pub mod state;
pub mod variants;
pub mod ws;

use std::sync::Arc;

use axum::Router;
use mcs_payments::RequirePaymentLayer;

pub use error::{ApiError, ApiResult};
pub use extract::AuthUser;
pub use history::{MoveEntry, MovesResponse};
pub use hub::GameHub;
pub use rating::RatingUpdateHook;
pub use rest::{
    CancelSeekResponse, CreateSeekRequest, CreateSeekResponse, GameDto, GameListResponse,
    LeaderboardEntry, LeaderboardQuery, LeaderboardResponse, ProfileDto, RatingDto,
};
pub use state::{AppState, PaymentGate, SiweConfig};
pub use variants::{VariantDto, VariantListResponse};
pub use ws::{ClientMessage, ServerMessage, PROTOCOL_VERSION};

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
pub fn router(state: AppState) -> Router {
    // Game creation is gated when (and only when) a payment gate is configured.
    // The layer wraps the one-route `create_seek_router` so cancellation, reads,
    // auth, and the WebSocket all stay free.
    let create_seeks = match state.payment_gate() {
        Some(gate) => rest::create_seek_router().layer(RequirePaymentLayer::new(
            gate.requirements().to_vec(),
            Arc::clone(gate.verifier()),
        )),
        None => rest::create_seek_router(),
    };

    Router::new()
        .merge(variants::variants_router())
        .merge(auth::auth_router())
        .merge(ws::ws_router())
        .merge(create_seeks)
        .merge(rest::cancel_seek_router())
        .merge(rest::read_router())
        .merge(history::history_router())
        .with_state(state)
}
