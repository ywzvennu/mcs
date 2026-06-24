//! User aggregate.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::evm_address::EvmAddress;
use crate::ids::UserId;

/// A registered user of the server.
///
/// A user is identified by their Ethereum address ([`EvmAddress`]) and
/// assigned a random [`UserId`] on creation. The `username` field is optional
/// and may be set later through a profile-update flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    /// Stable internal identifier.
    pub id: UserId,
    /// The Ethereum address that authenticated this user.
    pub address: EvmAddress,
    /// An optional human-readable display name.
    pub username: Option<String>,
    /// When the account was first created (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl User {
    /// Creates a new [`User`] with a freshly generated [`UserId`] and the
    /// supplied creation timestamp.
    ///
    /// # Arguments
    ///
    /// * `address` – validated Ethereum address.
    /// * `username` – optional display name.
    /// * `created_at` – creation timestamp; pass `OffsetDateTime::now_utc()`
    ///   in application code.
    #[must_use]
    pub fn new(address: EvmAddress, username: Option<String>, created_at: OffsetDateTime) -> Self {
        Self {
            id: UserId::new(),
            address,
            username,
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;

    fn sample_address() -> EvmAddress {
        "0xabcdef1234567890abcdef1234567890abcdef12"
            .parse()
            .unwrap()
    }

    #[test]
    fn new_assigns_unique_ids() {
        let addr = sample_address();
        let now = OffsetDateTime::UNIX_EPOCH;
        let a = User::new(addr.clone(), None, now);
        let b = User::new(addr, None, now);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn serde_round_trip() {
        let user = User::new(
            sample_address(),
            Some("alice".to_owned()),
            OffsetDateTime::UNIX_EPOCH,
        );
        let json = serde_json::to_string(&user).unwrap();
        let back: User = serde_json::from_str(&json).unwrap();
        assert_eq!(user, back);
    }

    #[test]
    fn serde_round_trip_no_username() {
        let user = User::new(sample_address(), None, OffsetDateTime::UNIX_EPOCH);
        let json = serde_json::to_string(&user).unwrap();
        let back: User = serde_json::from_str(&json).unwrap();
        assert_eq!(user, back);
    }
}
