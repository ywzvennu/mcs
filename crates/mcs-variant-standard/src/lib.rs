//! # mcs-variant-standard
//!
//! Standard chess as an MCS variant.
//!
//! This crate implements the [`GameSession`](mcs_core::GameSession) and
//! [`VariantFactory`](mcs_core::VariantFactory) abstractions from `mcs-core`
//! for ordinary FIDE chess. All move generation and rule enforcement is
//! delegated to the [`shakmaty`] crate; this crate only adapts shakmaty to the
//! variant-agnostic boundary types and adds the non-board game mechanics the
//! server needs (resignation and draw offers).
//!
//! Standard chess is a **perfect-information** variant: both players and any
//! spectator observe the same complete board at all times. See [`wire`] for the
//! exact JSON shapes of actions, views, and events, and [`StandardGame`] for
//! the session implementation.
//!
//! ## Usage
//!
//! ```
//! use mcs_core::{Color, VariantOptions, VariantRegistry};
//! use mcs_variant_standard::{register, STANDARD_VARIANT_ID};
//!
//! let mut registry = VariantRegistry::new();
//! register(&mut registry);
//!
//! let mut game = registry
//!     .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
//!     .expect("standard variant is registered");
//! assert_eq!(game.to_move(), Color::White);
//! ```
#![doc(html_root_url = "https://docs.rs/mcs-variant-standard")]

mod factory;
mod game;
pub mod wire;

pub use factory::{register, StandardVariant};
pub use game::StandardGame;

/// The stable identifier of the standard-chess variant: `"standard"`.
pub const STANDARD_VARIANT_ID: &str = game::VARIANT_ID;

#[cfg(test)]
mod tests;
