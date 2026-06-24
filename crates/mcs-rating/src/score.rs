//! Game outcome from the perspective of one player.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The outcome of a single game from the perspective of the rated player.
///
/// Glicko-2 treats each result as a scalar in the range \[0, 1\]:
///
/// | Variant  | Numeric value |
/// |----------|---------------|
/// | [`Win`]  | 1.0           |
/// | [`Draw`] | 0.5           |
/// | [`Loss`] | 0.0           |
///
/// [`Win`]: Score::Win
/// [`Draw`]: Score::Draw
/// [`Loss`]: Score::Loss
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Score {
    /// The rated player won the game.
    Win,
    /// The game ended in a draw.
    Draw,
    /// The rated player lost the game.
    Loss,
}

impl Score {
    /// Returns the numeric Glicko-2 score value for this outcome.
    ///
    /// ```
    /// use mcs_rating::Score;
    /// assert_eq!(Score::Win.value(),  1.0);
    /// assert_eq!(Score::Draw.value(), 0.5);
    /// assert_eq!(Score::Loss.value(), 0.0);
    /// ```
    #[inline]
    pub fn value(self) -> f64 {
        match self {
            Score::Win => 1.0,
            Score::Draw => 0.5,
            Score::Loss => 0.0,
        }
    }
}
