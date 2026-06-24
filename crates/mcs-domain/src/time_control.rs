//! Time-control variants for chess games.
//!
//! [`TimeControl`] describes the pace of a game without tracking any per-move
//! state — that responsibility belongs to [`crate::clock::Clock`].

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The timing rules that govern how quickly players must move.
///
/// # Serde
///
/// All variants serialise as a tagged JSON object, e.g.:
///
/// ```json
/// { "type": "real_time", "initial_secs": 300, "increment_secs": 5 }
/// { "type": "correspondence", "days_per_move": 3 }
/// { "type": "unlimited" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimeControl {
    /// Both players share a clock; each move may add increment seconds.
    ///
    /// Standard examples: 1+0 (bullet), 3+2 (blitz), 10+5 (rapid).
    RealTime {
        /// Starting time on each player's clock, in whole seconds.
        #[serde(rename = "initial_secs", with = "duration_secs")]
        initial: Duration,
        /// Seconds added to the mover's clock after each move.
        #[serde(rename = "increment_secs", with = "duration_secs")]
        increment: Duration,
    },
    /// Players have multiple days per move; no shared wall-clock pressure.
    Correspondence {
        /// Maximum calendar days a player may take per move.
        days_per_move: u32,
    },
    /// No time limit at all.
    Unlimited,
}

impl TimeControl {
    /// Returns `true` if this is a [`TimeControl::RealTime`] variant.
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcs_domain::TimeControl;
    ///
    /// assert!(TimeControl::RealTime { initial: Duration::from_secs(180), increment: Duration::ZERO }.is_real_time());
    /// assert!(!TimeControl::Unlimited.is_real_time());
    /// ```
    #[must_use]
    pub fn is_real_time(&self) -> bool {
        matches!(self, Self::RealTime { .. })
    }

    /// Returns `true` if this is a [`TimeControl::Correspondence`] variant.
    #[must_use]
    pub fn is_correspondence(&self) -> bool {
        matches!(self, Self::Correspondence { .. })
    }

    /// Returns `true` if this is [`TimeControl::Unlimited`].
    #[must_use]
    pub fn is_unlimited(&self) -> bool {
        matches!(self, Self::Unlimited)
    }
}

/// Serde helper that round-trips a [`Duration`] as a `u64` number of whole
/// seconds, which is compact and unambiguous in JSON.
mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(d: &Duration, ser: S) -> Result<S::Ok, S::Error> {
        d.as_secs().serialize(ser)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(de)?;
        Ok(Duration::from_secs(secs))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn real_time_helpers() {
        let tc = TimeControl::RealTime {
            initial: Duration::from_secs(300),
            increment: Duration::from_secs(5),
        };
        assert!(tc.is_real_time());
        assert!(!tc.is_correspondence());
        assert!(!tc.is_unlimited());
    }

    #[test]
    fn correspondence_helpers() {
        let tc = TimeControl::Correspondence { days_per_move: 3 };
        assert!(!tc.is_real_time());
        assert!(tc.is_correspondence());
        assert!(!tc.is_unlimited());
    }

    #[test]
    fn unlimited_helpers() {
        assert!(TimeControl::Unlimited.is_unlimited());
        assert!(!TimeControl::Unlimited.is_real_time());
    }

    #[test]
    fn real_time_serde_round_trip() {
        let tc = TimeControl::RealTime {
            initial: Duration::from_secs(180),
            increment: Duration::from_secs(2),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let back: TimeControl = serde_json::from_str(&json).unwrap();
        assert_eq!(tc, back);
    }

    #[test]
    fn correspondence_serde_round_trip() {
        let tc = TimeControl::Correspondence { days_per_move: 7 };
        let json = serde_json::to_string(&tc).unwrap();
        let back: TimeControl = serde_json::from_str(&json).unwrap();
        assert_eq!(tc, back);
    }

    #[test]
    fn unlimited_serde_round_trip() {
        let tc = TimeControl::Unlimited;
        let json = serde_json::to_string(&tc).unwrap();
        let back: TimeControl = serde_json::from_str(&json).unwrap();
        assert_eq!(tc, back);
    }
}
