//! Matchmaking seek aggregate.
//!
//! A [`Seek`] represents an open challenge posted by a user who wants to play a
//! game. It records their preferred variant, time control, and side preference.
//! The matchmaking layer consumes seeks and converts matching pairs into games.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{SeekId, UserId};
use crate::time_control::TimeControl;

/// A player's preference for which side they want to play.
///
/// When two seeks are matched, the server resolves colour conflicts according
/// to these preferences (e.g. two `Random` preferences flip a coin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorPreference {
    /// The seeker wants to play the white pieces.
    White,
    /// The seeker wants to play the black pieces.
    Black,
    /// The seeker accepts either colour; the server may assign either side.
    Random,
}

/// An open challenge posted to the matchmaking pool.
///
/// Seeks are immutable once created. A seek is consumed (removed) when it is
/// matched into a game or cancelled by the creator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seek {
    /// Stable identifier for this seek.
    pub id: SeekId,
    /// The user who created this challenge.
    pub creator: UserId,
    /// Identifies which chess variant this seek is for (e.g. `"standard"`).
    pub variant_id: String,
    /// The timing rules the challenger wants to play under.
    pub time_control: TimeControl,
    /// Which side the challenger prefers to play.
    pub color_preference: ColorPreference,
    /// Whether the challenger wants a rated game.
    ///
    /// A rated seek (`true`) only ever matches another rated seek, and a casual
    /// seek (`false`) only matches another casual seek, so both players always
    /// agree: the resulting game is rated exactly when both seeks were. A casual
    /// game is exempt from rating changes.
    #[serde(default = "default_rated")]
    pub rated: bool,
    /// When this seek was posted (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// The serde default for [`Seek::rated`]: seeks that predate the rated/casual
/// distinction deserialize as **rated**.
fn default_rated() -> bool {
    true
}

impl Seek {
    /// Creates a new [`Seek`] with a freshly generated [`SeekId`].
    ///
    /// # Arguments
    ///
    /// * `creator` – the user posting the challenge.
    /// * `variant_id` – the variant string identifier.
    /// * `time_control` – the desired time control.
    /// * `color_preference` – the desired side.
    /// * `rated` – whether the challenger wants a rated game (`true`) or a casual
    ///   one (`false`); the matchmaker only pairs seeks that agree on this.
    /// * `created_at` – creation timestamp; pass `OffsetDateTime::now_utc()`
    ///   in application code.
    #[must_use]
    pub fn new(
        creator: UserId,
        variant_id: String,
        time_control: TimeControl,
        color_preference: ColorPreference,
        rated: bool,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id: SeekId::new(),
            creator,
            variant_id,
            time_control,
            color_preference,
            rated,
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use time::OffsetDateTime;

    use super::*;

    fn sample_seek() -> Seek {
        Seek::new(
            UserId::new(),
            "standard".to_owned(),
            TimeControl::RealTime {
                initial: Duration::from_secs(300),
                increment: Duration::from_secs(5),
            },
            ColorPreference::Random,
            true,
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn new_generates_unique_ids() {
        let creator = UserId::new();
        let a = Seek::new(
            creator,
            "standard".to_owned(),
            TimeControl::Unlimited,
            ColorPreference::White,
            true,
            OffsetDateTime::UNIX_EPOCH,
        );
        let b = Seek::new(
            creator,
            "standard".to_owned(),
            TimeControl::Unlimited,
            ColorPreference::White,
            true,
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn rated_flag_survives_serde_round_trip() {
        for rated in [true, false] {
            let mut seek = sample_seek();
            seek.rated = rated;
            let json = serde_json::to_string(&seek).unwrap();
            let back: Seek = serde_json::from_str(&json).unwrap();
            assert_eq!(seek, back);
            assert_eq!(back.rated, rated);
        }
    }

    #[test]
    fn missing_rated_field_deserializes_as_rated() {
        let mut value = serde_json::to_value(sample_seek()).unwrap();
        value.as_object_mut().unwrap().remove("rated");
        let back: Seek = serde_json::from_value(value).unwrap();
        assert!(back.rated);
    }

    #[test]
    fn serde_round_trip() {
        let seek = sample_seek();
        let json = serde_json::to_string(&seek).unwrap();
        let back: Seek = serde_json::from_str(&json).unwrap();
        assert_eq!(seek, back);
    }

    #[test]
    fn color_preference_serde() {
        for pref in [
            ColorPreference::White,
            ColorPreference::Black,
            ColorPreference::Random,
        ] {
            let json = serde_json::to_string(&pref).unwrap();
            let back: ColorPreference = serde_json::from_str(&json).unwrap();
            assert_eq!(pref, back);
        }
    }
}
