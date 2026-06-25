//! # mcs-domain
//!
//! Pure domain entities and value objects for the Modular Chess Server.
//!
//! This crate is deliberately free of I/O, async, HTTP, and database concerns.
//! It contains only types, validation logic, and serde implementations that
//! model the business domain. Higher-level crates (`mcs-game`, `mcs-api`,
//! `mcs-storage`) depend on this crate; it depends only on `mcs-core` for the
//! shared [`mcs_core::Color`] and [`mcs_core::Outcome`] types.
//!
//! ## Module overview
//!
//! | Module            | Contents |
//! |-------------------|----------|
//! | [`ids`]           | Strongly-typed [`UserId`], [`GameId`], [`SeekId`], [`ChallengeId`] newtypes |
//! | [`evm_address`]   | Validated [`EvmAddress`] value object |
//! | [`user`]          | [`User`] aggregate |
//! | [`time_control`]  | [`TimeControl`] enum (real-time, correspondence, unlimited) |
//! | [`clock`]         | [`Clock`] per-player time snapshot |
//! | [`rating`]        | Glicko-2 [`Rating`] placeholder |
//! | [`seek`]          | [`Seek`] matchmaking aggregate and [`ColorPreference`] |
//! | [`challenge`]     | [`Challenge`] direct-challenge aggregate and [`ChallengeStatus`] |
//! | [`game`]          | [`Game`] aggregate and [`GameLifecycle`] |
//! | [`error`]         | [`DomainError`] validation error type |
#![doc(html_root_url = "https://docs.rs/mcs-domain")]

pub mod challenge;
pub mod clock;
pub mod error;
pub mod evm_address;
pub mod game;
pub mod ids;
pub mod rating;
pub mod seek;
pub mod time_control;
pub mod user;

pub use challenge::{Challenge, ChallengeStatus};
pub use clock::Clock;
pub use error::DomainError;
pub use evm_address::EvmAddress;
pub use game::{Game, GameLifecycle};
pub use ids::{ChallengeId, GameId, SeekId, UserId};
pub use rating::{Rating, RatingHistoryEntry};
pub use seek::{ColorPreference, Seek};
pub use time_control::{TimeClass, TimeControl};
pub use user::User;
