//! # mcs-storage
//!
//! Persistence-agnostic repository traits for the Modular Chess Server.
//!
//! This crate defines the **storage boundary**: the set of async traits that
//! the game, auth, and API layers depend on to read and write domain state, and
//! it ships a concrete [`sqlx`]-backed implementation ([`SqlxStorage`]) that
//! speaks either SQLite or PostgreSQL depending on the active crate feature.
//!
//! The trait layer carries no driver knowledge, so upper layers can be tested
//! against lightweight in-memory implementations without a real database.
//!
//! ## Crate contents
//!
//! | Module           | Contents |
//! |------------------|----------|
//! | [`error`]        | [`StorageError`] and [`StorageResult`] |
//! | [`user`]         | [`UserRepo`] trait |
//! | [`game`]         | [`GameRepo`] trait |
//! | [`action_log`]   | [`ActionLogRepo`] trait and [`RecordedAction`] |
//! | [`seek`]         | [`SeekRepo`] trait |
//! | [`challenge`]    | [`ChallengeRepo`] trait |
//! | [`session`]      | [`SessionRepo`] trait |
//! | [`revoked_token`]| [`RevokedTokenRepo`] trait |
//! | [`rating`]       | [`RatingRepo`] trait |
//! | [`rating_history`] | [`RatingHistoryRepo`] trait |
//! | [`payment`]      | [`PaymentStore`] re-export (x402 idempotency, #108) |
//! | [`repositories`] | [`Repositories`] aggregate trait |
//! | [`sqlx_store`]   | [`SqlxStorage`] sqlx-backed implementation |
//!
//! ## Backends
//!
//! Exactly one driver feature should be active: `sqlite` (the default) or
//! `postgres`. Both compile; the SQL is portable across them. See
//! [`sqlx_store`] for the encoding conventions and the "no compile-time query
//! macro" decision (CI builds offline, with no database).
//!
//! ## Usage pattern
//!
//! Application layers receive a `&dyn Repositories` (or an `Arc<dyn
//! Repositories>`) and call through it to the individual repos:
//!
//! ```rust,ignore
//! async fn handle_login(repos: &dyn Repositories, addr: &EvmAddress) {
//!     let user = repos.users().upsert_by_address(addr).await?;
//!     // ...
//! }
//! ```

#![doc(html_root_url = "https://docs.rs/mcs-storage")]

pub mod action_log;
pub mod challenge;
pub mod error;
pub mod game;
pub mod payment;
pub mod rating;
pub mod rating_history;
pub mod repositories;
pub mod revoked_token;
pub mod seek;
pub mod session;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub mod sqlx_store;
pub mod user;

pub use action_log::{ActionLogRepo, RecordedAction};
pub use challenge::ChallengeRepo;
pub use error::{StorageError, StorageResult};
pub use game::GameRepo;
pub use payment::{PaymentRecord, PaymentStore, PaymentStoreError};
pub use rating::RatingRepo;
pub use rating_history::RatingHistoryRepo;
pub use repositories::Repositories;
pub use revoked_token::RevokedTokenRepo;
pub use seek::{ClaimOutcome, SeekRepo};
pub use session::SessionRepo;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use sqlx_store::{PoolConfig, SqlxStorage};
pub use user::UserRepo;

#[cfg(test)]
mod tests;
