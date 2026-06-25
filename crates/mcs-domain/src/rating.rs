//! Player rating value object.
//!
//! This module provides a placeholder [`Rating`] struct whose fields match the
//! Glicko-2 model. The **rating algorithm itself is not implemented here**;
//! it will live in a dedicated crate once the rating subsystem is built.
//! This type exists to give every other domain object a well-typed, serialisable
//! place to store a player's current rating without coupling them to a future
//! algorithm crate.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{GameId, UserId};

/// A Glicko-2 rating record.
///
/// The three fields map directly to the standard Glicko-2 parameters:
///
/// | Field        | Glicko-2 symbol | Typical seed |
/// |--------------|-----------------|--------------|
/// | `value`      | μ (mu)          | 1500.0       |
/// | `deviation`  | φ (phi)         | 350.0        |
/// | `volatility` | σ (sigma)       | 0.06         |
///
/// # Note
///
/// The Glicko-2 update algorithm (computing new ratings from game results) is
/// intentionally absent. It will be implemented in a future rating-engine crate
/// and will consume/produce values of this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rating {
    /// The estimated playing strength. Higher is stronger; the scale is
    /// compatible with the traditional Elo scale when centred at 1500.
    pub value: f64,
    /// The uncertainty (standard deviation) around the `value` estimate.
    /// A freshly registered player starts at 350; it shrinks as more games
    /// are played.
    pub deviation: f64,
    /// A measure of how consistent the player's performance is. Low volatility
    /// means stable performance; high volatility means erratic results.
    pub volatility: f64,
}

impl Default for Rating {
    /// Returns the standard Glicko-2 seed rating for a newly registered player.
    ///
    /// | Field        | Seed value |
    /// |--------------|------------|
    /// | `value`      | 1500.0     |
    /// | `deviation`  | 350.0      |
    /// | `volatility` | 0.06       |
    fn default() -> Self {
        Self {
            value: 1500.0,
            deviation: 350.0,
            volatility: 0.06,
        }
    }
}

/// A single snapshot of a player's rating in a variant, recorded after a rated
/// game is scored.
///
/// One [`RatingHistoryEntry`] is appended for each player every time a rated
/// game finishes and their [`Rating`] is recomputed, so a player's history is
/// the ordered trail of their rating's `value`/`deviation` after each rated
/// game in a variant. It deliberately stores only the two display-relevant
/// Glicko-2 parameters (`value` and `deviation`); the full rating, including
/// `volatility`, lives in the current-rating record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatingHistoryEntry {
    /// The player whose rating this snapshot belongs to.
    pub user_id: UserId,
    /// The variant the rating is for (an opaque application-level string, e.g.
    /// `"standard"`).
    pub variant_id: String,
    /// The rating value (μ on the display scale) after the game was scored.
    pub value: f64,
    /// The rating deviation (φ) after the game was scored.
    pub deviation: f64,
    /// The game that produced this snapshot.
    pub game_id: GameId,
    /// When the snapshot was recorded (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_seed_values() {
        let r = Rating::default();
        assert_eq!(r.value, 1500.0);
        assert_eq!(r.deviation, 350.0);
        assert_eq!(r.volatility, 0.06);
    }

    #[test]
    fn serde_round_trip() {
        let r = Rating {
            value: 1632.5,
            deviation: 180.0,
            volatility: 0.05,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Rating = serde_json::from_str(&json).unwrap();
        assert_eq!(r.value, back.value);
        assert_eq!(r.deviation, back.deviation);
        assert_eq!(r.volatility, back.volatility);
    }

    #[test]
    fn serde_round_trip_default() {
        let r = Rating::default();
        let json = serde_json::to_string(&r).unwrap();
        let back: Rating = serde_json::from_str(&json).unwrap();
        assert_eq!(r.value, back.value);
        assert_eq!(r.deviation, back.deviation);
        assert_eq!(r.volatility, back.volatility);
    }

    #[test]
    fn history_entry_serde_round_trip() {
        let entry = RatingHistoryEntry {
            user_id: UserId::new(),
            variant_id: "standard".to_owned(),
            value: 1612.0,
            deviation: 90.0,
            game_id: GameId::new(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: RatingHistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }
}
