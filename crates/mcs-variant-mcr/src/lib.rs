//! # mcs-variant-mcr
//!
//! The mcr fairy-variant catalog as MCS variants.
//!
//! This crate implements the [`GameSession`](mcs_core::GameSession) and
//! [`VariantFactory`](mcs_core::VariantFactory) abstractions from `mcs-core` on
//! top of the [`mcr`] rules library — a clean-room, permissively licensed
//! (MIT OR Apache-2.0) move-generation and rules engine covering standard chess,
//! Chess960, and 100+ fairy variants (Shogi, Xiangqi, Makruk, Capablanca, Chu
//! Shogi, and more). All move generation and rule enforcement is delegated to
//! mcr's uniform [`Game`](mcr::Game) driver; this crate only adapts it to the
//! variant-agnostic boundary types and adds the non-board mechanics the server
//! needs (resignation and draw offers).
//!
//! ## Scope: perfect-information, single-move variants only
//!
//! [`register`] walks mcr's whole catalog ([`mcr::VariantRef::all`]) and
//! registers a factory for every variant **except**:
//!
//! - `standard` and `chess960`, which remain owned by the cozy-chess-backed
//!   `mcs-variant-standard` (until #155) — excluding them keeps this PR additive
//!   and avoids a registry-key collision;
//! - the hidden-information variants (Fog of War, Jieqi), whose views must be
//!   redacted per player (deferred to #156);
//! - the phased variants — Duck (a two-part move) and the setup-phase variants
//!   (Placement, Sittuyin) — which the single-action seam cannot express
//!   (deferred to #156).
//!
//! Every registered variant is therefore **perfect-information**: both players
//! and any spectator observe the same complete board at all times. See [`wire`]
//! for the exact JSON shapes of actions, views, and events, and [`McrGame`] for
//! the session implementation.
//!
//! ## Usage
//!
//! ```
//! use mcs_core::{VariantOptions, VariantRegistry};
//! use mcs_variant_mcr::register;
//!
//! let mut registry = VariantRegistry::new();
//! register(&mut registry);
//!
//! // Fairy variants are addressed by their canonical mcr name.
//! let game = registry
//!     .new_game("kingofthehill", &VariantOptions::default())
//!     .expect("kingofthehill is registered");
//! assert_eq!(game.variant_id(), "kingofthehill");
//! ```
#![doc(html_root_url = "https://docs.rs/mcs-variant-mcr")]

mod factory;
mod game;
pub mod wire;

pub use factory::{register, McrVariant};
pub use game::McrGame;

#[cfg(test)]
mod tests;
