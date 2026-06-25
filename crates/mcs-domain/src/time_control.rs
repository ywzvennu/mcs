//! Time-control variants for chess games.
//!
//! [`TimeControl`] describes the pace of a game without tracking any per-move
//! state — that responsibility belongs to [`crate::clock::Clock`].

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// The broad pace category a [`TimeControl`] falls into.
///
/// Ratings are kept **per `(variant, time_class)`** rather than per variant
/// alone, so a player's bullet strength is tracked separately from their
/// classical strength. [`TimeControl::time_class`] maps a concrete time control
/// onto one of these buckets.
///
/// # Serde
///
/// Serialises as a lowercase `snake_case` string (`"bullet"`, `"blitz"`,
/// `"rapid"`, `"classical"`, `"correspondence"`). The same spelling is used as
/// the storage key, so the serde and [`Display`](fmt::Display)/[`FromStr`]
/// representations are deliberately identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeClass {
    /// Very fast games — an estimated duration under [`BULLET_MAX_SECS`] seconds.
    Bullet,
    /// Fast games — estimated duration under [`BLITZ_MAX_SECS`] seconds.
    Blitz,
    /// Medium-paced games — estimated duration under [`RAPID_MAX_SECS`] seconds.
    Rapid,
    /// Slow games — estimated duration at or above [`RAPID_MAX_SECS`] seconds.
    Classical,
    /// Untimed or days-per-move games (correspondence and unlimited).
    Correspondence,
}

/// Estimated-duration upper bound (exclusive) for [`TimeClass::Bullet`], in
/// seconds. An estimate `< 179` is bullet; e.g. 2+1 → `120 + 40 = 160`.
pub const BULLET_MAX_SECS: u64 = 179;
/// Estimated-duration upper bound (exclusive) for [`TimeClass::Blitz`], in
/// seconds. An estimate `< 479` (and `>= BULLET_MAX_SECS`) is blitz; e.g.
/// 5+3 → `300 + 120 = 420`.
pub const BLITZ_MAX_SECS: u64 = 479;
/// Estimated-duration upper bound (exclusive) for [`TimeClass::Rapid`], in
/// seconds. An estimate `< 1499` (and `>= BLITZ_MAX_SECS`) is rapid; e.g.
/// 10+0 → `600`. At or above this bound the game is [`TimeClass::Classical`].
pub const RAPID_MAX_SECS: u64 = 1499;

impl TimeClass {
    /// The lowercase `snake_case` spelling used on the wire and as a storage key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bullet => "bullet",
            Self::Blitz => "blitz",
            Self::Rapid => "rapid",
            Self::Classical => "classical",
            Self::Correspondence => "correspondence",
        }
    }
}

impl fmt::Display for TimeClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TimeClass {
    type Err = DomainError;

    /// Parses a lowercase `snake_case` time-class string, the inverse of
    /// [`TimeClass::as_str`].
    ///
    /// An unrecognised value is a [`DomainError::Validation`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bullet" => Ok(Self::Bullet),
            "blitz" => Ok(Self::Blitz),
            "rapid" => Ok(Self::Rapid),
            "classical" => Ok(Self::Classical),
            "correspondence" => Ok(Self::Correspondence),
            other => Err(DomainError::Validation(format!(
                "unknown time class: {other:?}"
            ))),
        }
    }
}

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

    /// Maps this time control onto its broad [`TimeClass`] bucket, used to key
    /// ratings per `(variant, time_class)`.
    ///
    /// For a [`TimeControl::RealTime`] control the estimated game duration is
    /// `initial_secs + 40 * increment_secs` (the conventional "40 expected
    /// moves" estimate). That estimate is bucketed:
    ///
    /// | Estimated seconds            | Class                        |
    /// |------------------------------|------------------------------|
    /// | `< BULLET_MAX_SECS` (179)    | [`TimeClass::Bullet`]        |
    /// | `< BLITZ_MAX_SECS` (479)     | [`TimeClass::Blitz`]         |
    /// | `< RAPID_MAX_SECS` (1499)    | [`TimeClass::Rapid`]         |
    /// | otherwise                    | [`TimeClass::Classical`]     |
    ///
    /// [`TimeControl::Correspondence`] and [`TimeControl::Unlimited`] both map to
    /// [`TimeClass::Correspondence`] (an untimed/days-per-move bucket).
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcs_domain::{TimeClass, TimeControl};
    ///
    /// let blitz = TimeControl::RealTime {
    ///     initial: Duration::from_secs(300),
    ///     increment: Duration::from_secs(0),
    /// };
    /// assert_eq!(blitz.time_class(), TimeClass::Blitz);
    /// assert_eq!(TimeControl::Unlimited.time_class(), TimeClass::Correspondence);
    /// ```
    #[must_use]
    pub fn time_class(&self) -> TimeClass {
        match self {
            Self::RealTime { initial, increment } => {
                let estimated = initial.as_secs().saturating_add(40 * increment.as_secs());
                if estimated < BULLET_MAX_SECS {
                    TimeClass::Bullet
                } else if estimated < BLITZ_MAX_SECS {
                    TimeClass::Blitz
                } else if estimated < RAPID_MAX_SECS {
                    TimeClass::Rapid
                } else {
                    TimeClass::Classical
                }
            }
            Self::Correspondence { .. } | Self::Unlimited => TimeClass::Correspondence,
        }
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

    /// Builds a `RealTime` control from whole seconds for the boundary tests.
    fn real_time(initial: u64, increment: u64) -> TimeControl {
        TimeControl::RealTime {
            initial: Duration::from_secs(initial),
            increment: Duration::from_secs(increment),
        }
    }

    #[test]
    fn time_class_bullet_boundaries() {
        // 0+0 is the fastest possible: bullet.
        assert_eq!(real_time(0, 0).time_class(), TimeClass::Bullet);
        // estimated = 178 (< 179): still bullet.
        assert_eq!(real_time(178, 0).time_class(), TimeClass::Bullet);
        // increment counts 40x: 138 + 40*1 = 178 → bullet.
        assert_eq!(real_time(138, 1).time_class(), TimeClass::Bullet);
    }

    #[test]
    fn time_class_blitz_boundaries() {
        // estimated = 179 is the first blitz value (>= 179, < 479).
        assert_eq!(real_time(179, 0).time_class(), TimeClass::Blitz);
        // 3+0 → 180: blitz.
        assert_eq!(real_time(180, 0).time_class(), TimeClass::Blitz);
        // 5+0 → 300: blitz.
        assert_eq!(real_time(300, 0).time_class(), TimeClass::Blitz);
        // estimated = 478 (< 479): still blitz.
        assert_eq!(real_time(478, 0).time_class(), TimeClass::Blitz);
        // 3+2 → 180 + 80 = 260: blitz.
        assert_eq!(real_time(180, 2).time_class(), TimeClass::Blitz);
    }

    #[test]
    fn time_class_rapid_boundaries() {
        // estimated = 479 is the first rapid value (>= 479, < 1499).
        assert_eq!(real_time(479, 0).time_class(), TimeClass::Rapid);
        // 10+0 → 600: rapid.
        assert_eq!(real_time(600, 0).time_class(), TimeClass::Rapid);
        // estimated = 1498 (< 1499): still rapid.
        assert_eq!(real_time(1498, 0).time_class(), TimeClass::Rapid);
        // 10+5 → 600 + 200 = 800: rapid.
        assert_eq!(real_time(600, 5).time_class(), TimeClass::Rapid);
    }

    #[test]
    fn time_class_classical_boundaries() {
        // estimated = 1499 is the first classical value (>= 1499).
        assert_eq!(real_time(1499, 0).time_class(), TimeClass::Classical);
        // 30+0 → 1800: classical.
        assert_eq!(real_time(1800, 0).time_class(), TimeClass::Classical);
        // 15+10 → 900 + 400 = 1300 is rapid, but 25+10 → 1500 + 400 = 1900 is classical.
        assert_eq!(real_time(1500, 10).time_class(), TimeClass::Classical);
    }

    #[test]
    fn time_class_correspondence_and_unlimited() {
        assert_eq!(
            TimeControl::Correspondence { days_per_move: 1 }.time_class(),
            TimeClass::Correspondence
        );
        assert_eq!(
            TimeControl::Correspondence { days_per_move: 14 }.time_class(),
            TimeClass::Correspondence
        );
        assert_eq!(
            TimeControl::Unlimited.time_class(),
            TimeClass::Correspondence
        );
    }

    #[test]
    fn time_class_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&TimeClass::Bullet).unwrap(),
            "\"bullet\""
        );
        assert_eq!(
            serde_json::to_string(&TimeClass::Correspondence).unwrap(),
            "\"correspondence\""
        );
        let back: TimeClass = serde_json::from_str("\"rapid\"").unwrap();
        assert_eq!(back, TimeClass::Rapid);
    }

    #[test]
    fn time_class_as_str_and_from_str_round_trip() {
        for tc in [
            TimeClass::Bullet,
            TimeClass::Blitz,
            TimeClass::Rapid,
            TimeClass::Classical,
            TimeClass::Correspondence,
        ] {
            assert_eq!(tc.as_str().parse::<TimeClass>().unwrap(), tc);
            assert_eq!(tc.to_string(), tc.as_str());
        }
    }

    #[test]
    fn time_class_from_str_rejects_unknown() {
        assert!("hyperbullet".parse::<TimeClass>().is_err());
        // Casing matters: the canonical spelling is lowercase snake_case.
        assert!("Blitz".parse::<TimeClass>().is_err());
    }
}
