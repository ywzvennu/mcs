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
//! ## Scope: the whole catalog
//!
//! [`register`] walks mcr's whole catalog ([`mcr::VariantRef::all`]) and registers
//! a factory for **every** variant — mcr redacts each one's per-player views, so
//! there is no longer any variant this adapter must defer. Highlights:
//!
//! - the **phased** variants Duck (whose two-part move is a single combined UCI,
//!   `e2e4,e5`), Placement, and Sittuyin (whose setup phases are alternating
//!   *open* drops driven through the ordinary move seam) — all single-action,
//!   with no hidden information (#156);
//! - the **hidden-information** variants **Fog of War** (Dark Chess) and **jieqi**
//!   (hidden Xiangqi), whose per-player views mcr **redacts** — Fog of War shows a
//!   side only its own pieces and the squares they attack, and seeded jieqi keeps
//!   every unflipped piece a generic `Dark` token and never leaks its reveal seed.
//!   This adapter computes none of that redaction: it delegates to mcr's
//!   [`view_for`](mcr::Game::view_for) (#163). jieqi games are created with a
//!   per-game random reveal seed (folded into the persisted options for recovery),
//!   so their concealed identities are genuinely hidden rather than the
//!   deterministic home-role baseline.
//!
//! Since #155 this includes `standard` (ordinary FIDE chess) and `chess960`
//! (Fischer Random): the cozy-chess-backed `mcs-variant-standard` crate has been
//! retired, making mcr the single gameplay engine. The history-dependent FIDE
//! draws that adapter hand-rolled — threefold / fivefold repetition and the
//! fifty-move claim — are preserved here (see [`McrGame`]).
//!
//! See [`wire`] for the exact JSON shapes of actions, views, and events, and
//! [`McrGame`] for the session implementation.
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
