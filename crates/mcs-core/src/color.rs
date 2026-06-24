//! Player colors.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The side a player is playing.
///
/// Every variant is two-sided, with [`Color::White`] conventionally moving
/// first. Variants that are not literally "chess" can still map their two
/// parties onto these colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    /// The side that conventionally moves first.
    White,
    /// The side that conventionally moves second.
    Black,
}

impl Color {
    /// Returns the opposing color.
    ///
    /// ```
    /// use mcs_core::Color;
    /// assert_eq!(Color::White.opposite(), Color::Black);
    /// assert_eq!(Color::Black.opposite(), Color::White);
    /// ```
    #[must_use]
    pub const fn opposite(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Color::White => "white",
            Color::Black => "black",
        };
        f.write_str(name)
    }
}
