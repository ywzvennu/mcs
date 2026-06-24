//! Per-player remaining time snapshot.
//!
//! [`Clock`] is a **data value object** that represents a point-in-time
//! snapshot of how much time each player has remaining. It does *not*
//! implement tick logic, decrement time, or detect flags — that live-update
//! behaviour belongs in `mcs-game`, which owns the game-loop event loop.
//!
//! Consumers of this type should treat it as a read-only record of the last
//! persisted clock state.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A snapshot of the remaining clock time for both players.
///
/// `turn_started_at` is `Some` while a turn is in progress and `None` when no
/// turn has started yet (e.g. the game just transitioned to a new move and the
/// timestamp has not been recorded) or after a game ends.
///
/// # Note
///
/// This type intentionally has no `tick`, `decrement`, or `flag` methods.
/// Time-pressure logic belongs in the `mcs-game` crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    /// Time remaining on White's clock.
    #[serde(with = "duration_secs")]
    pub white_remaining: Duration,
    /// Time remaining on Black's clock.
    #[serde(with = "duration_secs")]
    pub black_remaining: Duration,
    /// Wall-clock instant at which the current turn began, if known.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_rfc3339"
    )]
    pub turn_started_at: Option<OffsetDateTime>,
}

impl Clock {
    /// Creates a new clock with identical starting time for both players and
    /// no active turn.
    ///
    /// # Arguments
    ///
    /// * `initial` – the starting time budget for each side.
    #[must_use]
    pub fn new(initial: Duration) -> Self {
        Self {
            white_remaining: initial,
            black_remaining: initial,
            turn_started_at: None,
        }
    }

    /// Creates a clock with independently specified remaining times and an
    /// optional turn-start timestamp.
    #[must_use]
    pub fn with_times(
        white_remaining: Duration,
        black_remaining: Duration,
        turn_started_at: Option<OffsetDateTime>,
    ) -> Self {
        Self {
            white_remaining,
            black_remaining,
            turn_started_at,
        }
    }

    /// Returns the remaining time for White.
    #[must_use]
    pub fn white_remaining(&self) -> Duration {
        self.white_remaining
    }

    /// Returns the remaining time for Black.
    #[must_use]
    pub fn black_remaining(&self) -> Duration {
        self.black_remaining
    }

    /// Returns the timestamp at which the current turn started, if known.
    #[must_use]
    pub fn turn_started_at(&self) -> Option<OffsetDateTime> {
        self.turn_started_at
    }
}

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

mod optional_rfc3339 {
    use serde::{Deserializer, Serializer};
    use time::OffsetDateTime;

    pub(super) fn serialize<S: Serializer>(
        opt: &Option<OffsetDateTime>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match opt {
            Some(dt) => time::serde::rfc3339::serialize(dt, ser),
            None => ser.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        time::serde::rfc3339::option::deserialize(de)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use time::OffsetDateTime;

    use super::*;

    #[test]
    fn new_sets_equal_times_and_no_turn() {
        let initial = Duration::from_secs(300);
        let clock = Clock::new(initial);
        assert_eq!(clock.white_remaining(), initial);
        assert_eq!(clock.black_remaining(), initial);
        assert!(clock.turn_started_at().is_none());
    }

    #[test]
    fn with_times_constructor() {
        let w = Duration::from_secs(120);
        let b = Duration::from_secs(180);
        let ts = OffsetDateTime::UNIX_EPOCH;
        let clock = Clock::with_times(w, b, Some(ts));
        assert_eq!(clock.white_remaining(), w);
        assert_eq!(clock.black_remaining(), b);
        assert_eq!(clock.turn_started_at(), Some(ts));
    }

    #[test]
    fn serde_round_trip_without_turn() {
        let clock = Clock::new(Duration::from_secs(600));
        let json = serde_json::to_string(&clock).unwrap();
        let back: Clock = serde_json::from_str(&json).unwrap();
        assert_eq!(clock, back);
    }

    #[test]
    fn serde_round_trip_with_turn() {
        let clock = Clock::with_times(
            Duration::from_secs(250),
            Duration::from_secs(300),
            Some(OffsetDateTime::UNIX_EPOCH),
        );
        let json = serde_json::to_string(&clock).unwrap();
        let back: Clock = serde_json::from_str(&json).unwrap();
        assert_eq!(clock, back);
    }

    #[test]
    fn turn_started_at_absent_when_none() {
        let clock = Clock::new(Duration::from_secs(60));
        let json = serde_json::to_string(&clock).unwrap();
        // turn_started_at should be omitted from the serialised form.
        assert!(!json.contains("turn_started_at"));
    }
}
