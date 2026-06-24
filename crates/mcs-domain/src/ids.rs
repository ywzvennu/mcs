//! Strongly-typed identifier newtypes.
//!
//! Each ID is a [`uuid::Uuid`] newtype with a random-v4 constructor, `Display`,
//! `FromStr`, and full serde support (as a UUID string). Using distinct types
//! for each entity prevents accidentally mixing up a [`UserId`] where a
//! [`GameId`] is expected at compile time.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::DomainError;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Creates a new random v4 identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Returns the underlying [`Uuid`].
            #[must_use]
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|_| DomainError::InvalidId(s.to_owned()))
            }
        }
    };
}

define_id!(
    /// Identifies a registered user.
    UserId
);

define_id!(
    /// Identifies a game instance.
    GameId
);

define_id!(
    /// Identifies a matchmaking seek.
    SeekId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_id_new_is_unique() {
        let a = UserId::new();
        let b = UserId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn user_id_display_and_from_str_round_trip() {
        let id = UserId::new();
        let s = id.to_string();
        let back: UserId = s.parse().expect("valid uuid string");
        assert_eq!(id, back);
    }

    #[test]
    fn game_id_from_str_rejects_non_uuid() {
        let err = "not-a-uuid".parse::<GameId>().unwrap_err();
        assert!(matches!(err, DomainError::InvalidId(_)));
    }

    #[test]
    fn seek_id_serde_round_trip() {
        let id = SeekId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: SeekId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn as_uuid_returns_inner_value() {
        let uuid = Uuid::new_v4();
        let id = UserId(uuid);
        assert_eq!(id.as_uuid(), uuid);
    }
}
