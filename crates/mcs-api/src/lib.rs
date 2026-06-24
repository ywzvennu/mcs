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
//! sub-routers. Today it mounts the **auth** endpoints ([`auth::auth_router`]):
//!
//! | Method & path       | Handler |
//! |---------------------|---------|
//! | `GET /auth/nonce`   | issue a single-use SIWE challenge |
//! | `POST /auth/verify` | verify the signed challenge, mint a session JWT |
//!
//! The REST game endpoints (#14) and the WebSocket layer (#15) add their own
//! sub-routers here later; both will protect routes with the [`AuthUser`]
//! extractor and extend [`AppState`] with a live-game hub. All handlers return
//! [`ApiResult<T>`] so the error contract applies everywhere.
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
pub mod state;

use axum::Router;

pub use error::{ApiError, ApiResult};
pub use extract::AuthUser;
pub use state::{AppState, SiweConfig};

/// Builds the top-level HTTP router for the MCS API.
///
/// The supplied [`AppState`] is attached to the router so every handler and
/// extractor can reach the shared storage, session, and SIWE configuration.
/// Mount the result under a server with [`axum::serve`].
///
/// As later issues land, their sub-routers are merged in here; the auth routes
/// and the [`AuthUser`] extractor are unaffected by those additions.
pub fn router(state: AppState) -> Router {
    Router::new().merge(auth::auth_router()).with_state(state)
}
