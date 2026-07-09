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
//! ## Scope: almost the whole catalog
//!
//! [`register`] walks mcr's whole catalog ([`mcr::VariantRef::all`]) and
//! registers a factory for every variant **except Jieqi** (dark chess), whose
//! stochastic per-piece hidden identity mcr's [`Game`](mcr::Game) seam does not
//! surface — see [`register`] for the rationale. Everything else is served:
//!
//! - the **phased** variants Duck (whose two-part move is a single combined UCI,
//!   `e2e4,e5`), Placement, and Sittuyin (whose setup phases are alternating
//!   *open* drops driven through the ordinary move seam) — all single-action,
//!   with no hidden information (#156);
//! - **Fog of War** (Dark Chess), the flagship hidden-information variant, whose
//!   per-player views are **redacted** so a side sees only its own pieces and the
//!   squares they attack (see [`McrGame`] and the [`fog`](mod@fog) module).
//!
//! Since #155 this includes `standard` (ordinary FIDE chess) and `chess960`
//! (Fischer Random): the cozy-chess-backed `mcs-variant-standard` crate has been
//! retired, making mcr the single gameplay engine. The history-dependent FIDE
//! draws that adapter hand-rolled — threefold / fivefold repetition and the
//! fifty-move claim — are preserved here (see [`McrGame`]).
//!
//! Every registered variant but Fog of War is **perfect-information**: both
//! players and any spectator observe the same complete board. See [`wire`] for
//! the exact JSON shapes of actions, views, and events, and [`McrGame`] for the
//! session implementation.
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
mod fog;
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
