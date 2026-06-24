//! # mcs-core
//!
//! The variant-agnostic core of the Modular Chess Server (MCS).
//!
//! This crate defines the abstractions that every chess variant implements so
//! that the rest of the server (storage, matchmaking, transport) can treat all
//! variants uniformly. The abstraction is deliberately general enough to cover
//! both:
//!
//! - **perfect-information** variants such as standard chess, where a turn is a
//!   single move and both players observe the full board; and
//! - **imperfect-information** variants such as Reconnaissance Blind Chess,
//!   where a turn includes a "sense" action and each player observes only a
//!   partial, private view of the position.
//!
//! The concrete trait and registry are implemented incrementally; see the
//! `mcs-core` tracking issue for the current design.
#![doc(html_root_url = "https://docs.rs/mcs-core")]
