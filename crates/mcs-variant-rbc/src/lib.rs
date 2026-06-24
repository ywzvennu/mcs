//! # mcs-variant-rbc
//!
//! Reconnaissance Blind Chess (RBC) as an MCS variant.
//!
//! This crate implements the [`GameSession`](mcs_core::GameSession) and
//! [`VariantFactory`](mcs_core::VariantFactory) abstractions from `mcs-core`
//! for Reconnaissance Blind Chess. All rules, move resolution, sensing, and
//! result adjudication are delegated to the [`rbc_rs`] crate; this crate adapts
//! `rbc-rs` to the variant-agnostic boundary types and, crucially, enforces the
//! per-player information redaction that the boundary requires.
//!
//! ## Why RBC matters here
//!
//! RBC is the motivating **imperfect-information** variant for the MCS variant
//! abstraction. Unlike standard chess, neither player ever sees the full board.
//! A turn has two phases performed by the same player:
//!
//! 1. **Sense** — privately inspect a 3×3 window and learn which pieces sit
//!    there (and only there);
//! 2. **Move** — play a move that the engine may silently revise or reject, and
//!    whose outcome the player observes only partially.
//!
//! This crate enforces that sense-then-move ordering through the
//! [`GameSession`](mcs_core::GameSession) trait and guarantees that
//! [`view_for`](mcs_core::GameSession::view_for) reveals a player only their own
//! pieces plus their own latest sense — never the opponent's hidden positions —
//! while [`spectator_view`](mcs_core::GameSession::spectator_view) is redacted
//! until the game ends. See [`wire`] for the exact JSON shapes and [`RbcGame`]
//! for the session implementation.
//!
//! ## Usage
//!
//! ```
//! use mcs_core::{Color, VariantOptions, VariantRegistry};
//! use mcs_variant_rbc::{register, RBC_VARIANT_ID};
//!
//! let mut registry = VariantRegistry::new();
//! register(&mut registry);
//!
//! let game = registry
//!     .new_game(RBC_VARIANT_ID, &VariantOptions::default())
//!     .expect("rbc variant is registered");
//! assert_eq!(game.to_move(), Color::White);
//! ```
#![doc(html_root_url = "https://docs.rs/mcs-variant-rbc")]

mod convert;
mod factory;
mod game;
pub mod wire;

pub use factory::{register, RbcVariant};
pub use game::RbcGame;

/// The stable identifier of the Reconnaissance Blind Chess variant: `"rbc"`.
pub const RBC_VARIANT_ID: &str = game::VARIANT_ID;

#[cfg(test)]
mod tests;
