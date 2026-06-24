//! # mcs-variant-shakmaty
//!
//! The shakmaty *variant family* as MCS variants: Atomic, Antichess,
//! Crazyhouse, King of the Hill, Three-check, Racing Kings, Horde, and
//! Chess960.
//!
//! All move generation and rule enforcement is delegated to the [`shakmaty`]
//! crate (built with its `variant` feature). This crate implements the
//! [`GameSession`](mcs_core::GameSession) and
//! [`VariantFactory`](mcs_core::VariantFactory) abstractions from `mcs-core`
//! **once**, via a generic adapter
//! [`ShakmatyGame<S>`](crate::ShakmatyGame) parameterized over a
//! [`VariantSpec`]. Each concrete variant is then just a small spec type
//! ([`Atomic`], [`ThreeCheck`], …) selecting a shakmaty position and describing
//! how its endings map onto [`Outcome`](mcs_core::Outcome) /
//! [`EndReason`](mcs_core::EndReason).
//!
//! Every variant here is **perfect information**: both players and any spectator
//! observe the same complete board. The wire protocol — UCI moves plus
//! resign/draw meta-actions — matches `mcs-variant-standard` so a client speaks
//! one protocol across the whole family. See [`wire`] for the exact JSON shapes.
//!
//! ## Chess960
//!
//! Chess960 is not a distinct shakmaty position type; it is ordinary `Chess`
//! played with [`CastlingMode::Chess960`](shakmaty::CastlingMode::Chess960)
//! from a shuffled back rank. It is exposed as the [`Chess960`] spec, whose
//! starting position is chosen from the variant options (a Scharnagl position
//! number or an explicit FEN), defaulting to the standard setup (number 518).
//!
//! ## Usage
//!
//! ```
//! use mcs_core::{VariantOptions, VariantRegistry};
//! use mcs_variant_shakmaty::register_all;
//!
//! let mut registry = VariantRegistry::new();
//! register_all(&mut registry);
//!
//! let game = registry
//!     .new_game("atomic", &VariantOptions::default())
//!     .expect("atomic is registered");
//! assert_eq!(game.variant_id(), "atomic");
//! ```
#![doc(html_root_url = "https://docs.rs/mcs-variant-shakmaty")]

mod factory;
mod game;
mod spec;
mod variants;
pub mod wire;

pub use factory::{register_all, ShakmatyVariant};
pub use game::ShakmatyGame;
pub use spec::VariantSpec;
pub use variants::{
    Antichess, Atomic, Chess960, Crazyhouse, Horde, KingOfTheHill, RacingKings, ThreeCheck,
};

#[cfg(test)]
mod tests;
