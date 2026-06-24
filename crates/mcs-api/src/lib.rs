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
//! Routers for individual resource collections (games, seeks, users, auth) are
//! added in later issues (#13, #14, #15) and will live as submodules of this
//! crate. They return [`ApiResult<T>`] so that the error contract here applies
//! everywhere.
//!
//! ## Security
//!
//! Internal errors (`ApiError::Internal`) log the real cause via
//! [`tracing::error!`] but replace it with a generic message in the HTTP
//! response body to avoid leaking server internals to callers.
#![doc(html_root_url = "https://docs.rs/mcs-api")]

pub mod error;

pub use error::{ApiError, ApiResult};
