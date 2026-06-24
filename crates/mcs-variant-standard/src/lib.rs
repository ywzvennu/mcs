//! # mcs-variant-standard
//!
//! Standard chess and Chess960 as MCS variants.
//!
//! This crate implements the [`GameSession`](mcs_core::GameSession) and
//! [`VariantFactory`](mcs_core::VariantFactory) abstractions from `mcs-core`
//! for ordinary FIDE chess and for Chess960 (Fischer Random Chess). All move
//! generation and rule enforcement is delegated to the permissively licensed
//! [`cozy_chess`] crate (MIT); this crate only adapts cozy-chess to the
//! variant-agnostic boundary types and adds the non-board game mechanics the
//! server needs (resignation and draw offers).
//!
//! Both variants are **perfect-information**: both players and any spectator
//! observe the same complete board at all times. See [`wire`] for the exact JSON
//! shapes of actions, views, and events, and [`StandardGame`] for the session
//! implementation.
//!
//! ## Castling on the wire
//!
//! cozy-chess represents castling internally as *king-captures-own-rook*
//! (Fischer-random style). The two variants differ only in how this is spelled
//! on the wire:
//!
//! - **`standard`** uses **classic UCI** castling (`e1g1`, `e1c1`, `e8g8`,
//!   `e8c8`), translated to/from cozy-chess's internal form at the boundary so
//!   existing clients are unaffected.
//! - **`chess960`** uses **UCI_960** (king-to-rook) castling, e.g. `e1h1`,
//!   because the rook's starting file is not fixed.
//!
//! ## Usage
//!
//! ```
//! use mcs_core::{Color, VariantOptions, VariantRegistry};
//! use mcs_variant_standard::{register, CHESS960_VARIANT_ID, STANDARD_VARIANT_ID};
//!
//! let mut registry = VariantRegistry::new();
//! register(&mut registry);
//!
//! let mut game = registry
//!     .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
//!     .expect("standard variant is registered");
//! assert_eq!(game.to_move(), Color::White);
//!
//! // Chess960 is registered by the same call. A `position` option selects a
//! // Scharnagl-numbered start layout (518 is the classical setup).
//! let opts = VariantOptions::new(serde_json::json!({ "position": 518 }));
//! let game960 = registry
//!     .new_game(CHESS960_VARIANT_ID, &opts)
//!     .expect("chess960 variant is registered");
//! assert_eq!(game960.variant_id(), CHESS960_VARIANT_ID);
//! ```
#![doc(html_root_url = "https://docs.rs/mcs-variant-standard")]

mod factory;
mod game;
pub mod wire;

pub use factory::{register, Chess960Variant, StandardVariant};
pub use game::StandardGame;

/// The stable identifier of the standard-chess variant: `"standard"`.
pub const STANDARD_VARIANT_ID: &str = game::VARIANT_ID;

/// The stable identifier of the Chess960 variant: `"chess960"`.
pub const CHESS960_VARIANT_ID: &str = game::CHESS960_VARIANT_ID;

#[cfg(test)]
mod tests;
