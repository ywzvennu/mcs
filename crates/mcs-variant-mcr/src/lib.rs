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
//! - the hidden-information variants (Fog of War, Jieqi), whose views must be
//!   redacted per player (deferred to #156);
//! - the phased variants — Duck (a two-part move) and the setup-phase variants
//!   (Placement, Sittuyin) — which the single-action seam cannot express
//!   (deferred to #156).
//!
//! Since #155 this includes `standard` (ordinary FIDE chess) and `chess960`
//! (Fischer Random): the cozy-chess-backed `mcs-variant-standard` crate has been
//! retired, making mcr the single gameplay engine. The history-dependent FIDE
//! draws that adapter hand-rolled — threefold / fivefold repetition and the
//! fifty-move claim — are preserved here (see [`McrGame`]).
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

/// The canonical mcr catalog name of standard (FIDE) chess — the marquee variant
/// this adapter owns since #155 retired the cozy-chess-backed crate.
pub const STANDARD_VARIANT_ID: &str = "standard";

/// The canonical mcr catalog name of Chess960 (Fischer Random Chess), served by
/// this adapter since #155.
pub const CHESS960_VARIANT_ID: &str = "chess960";

#[cfg(test)]
mod tests;
