//! # mcs-storage
//!
//! Persistence-agnostic repository traits for the Modular Chess Server.
//!
//! This crate defines the **storage boundary**: the set of async traits that
//! the game, auth, and API layers depend on to read and write domain state.
//! No concrete database driver is used or referenced here — that lives in a
//! separate crate (e.g. `mcs-storage-sqlite`). Keeping this crate free of
//! driver code means the upper layers can be tested against lightweight
//! in-memory implementations without a real database.
//!
//! ## Crate contents
//!
//! | Module           | Contents |
//! |------------------|----------|
//! | [`error`]        | [`StorageError`] and [`StorageResult`] |
//! | [`user`]         | [`UserRepo`] trait |
//! | [`game`]         | [`GameRepo`] trait |
//! | [`seek`]         | [`SeekRepo`] trait |
//! | [`session`]      | [`SessionRepo`] trait |
//! | [`repositories`] | [`Repositories`] aggregate trait |
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

pub mod error;
pub mod game;
pub mod repositories;
pub mod seek;
pub mod session;
pub mod user;

pub use error::{StorageError, StorageResult};
pub use game::GameRepo;
pub use repositories::Repositories;
pub use seek::SeekRepo;
pub use session::SessionRepo;
pub use user::UserRepo;

#[cfg(test)]
mod tests;
