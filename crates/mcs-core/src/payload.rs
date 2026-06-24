//! Type-erased, serde-serializable payloads used at the variant boundary.
//!
//! Each variant keeps strong, internal Rust types for its actions, views, and
//! events. At the boundary between a variant and the rest of the server those
//! types are erased into a [`serde_json::Value`] so that the core machinery,
//! the wire layer, and storage can handle every variant uniformly without
//! knowing its concrete types. The wire layer serializes to JSON anyway, so
//! erasing here costs nothing in the common path while keeping the
//! [`GameSession`](crate::GameSession) trait object-safe and variant-agnostic.
//!
//! Variants convert between their strong types and these newtypes with
//! [`from_typed`](Action::from_typed) and [`to_typed`](Action::to_typed).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::GameError;

/// Defines a transparent newtype around [`serde_json::Value`] together with the
/// `from_typed` / `to_typed` conversion helpers and an accessor for the inner
/// value. All three boundary payloads share the same shape, so they share one
/// definition.
macro_rules! erased_payload {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(serde_json::Value);

        impl $name {
            /// Wraps a raw JSON value directly.
            #[must_use]
            pub fn new(value: serde_json::Value) -> Self {
                Self(value)
            }

            /// Erases a strongly typed value into this payload by serializing
            /// it to JSON.
            ///
            /// # Errors
            ///
            /// Returns [`GameError::Serialization`] if `value` cannot be
            /// serialized.
            pub fn from_typed<T: Serialize>(value: &T) -> Result<Self, GameError> {
                serde_json::to_value(value)
                    .map(Self)
                    .map_err(|e| GameError::Serialization(e.to_string()))
            }

            /// Recovers a strongly typed value from this payload by
            /// deserializing the wrapped JSON.
            ///
            /// # Errors
            ///
            /// Returns [`GameError::Serialization`] if the wrapped value does
            /// not match the shape of `T`.
            pub fn to_typed<T: DeserializeOwned>(&self) -> Result<T, GameError> {
                T::deserialize(&self.0).map_err(|e| GameError::Serialization(e.to_string()))
            }

            /// Returns a reference to the wrapped JSON value.
            #[must_use]
            pub fn as_value(&self) -> &serde_json::Value {
                &self.0
            }

            /// Consumes the payload and returns the wrapped JSON value.
            #[must_use]
            pub fn into_value(self) -> serde_json::Value {
                self.0
            }
        }
    };
}

erased_payload! {
    /// A player-submitted action: a move, a sense, a resignation, a draw
    /// offer, and so on. The concrete shape is defined by each variant.
    Action
}

erased_payload! {
    /// What a single party is permitted to observe about a game.
    ///
    /// For a perfect-information variant this is the full position. For an
    /// imperfect-information variant it is a redacted, per-player view (e.g.
    /// only one's own pieces plus the result of one's latest sense).
    PlayerView
}

erased_payload! {
    /// Something that happened and should be broadcast to observers, such as a
    /// move being played or a game ending.
    Event
}
