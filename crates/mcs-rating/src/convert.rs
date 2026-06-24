//! Scale conversions between the Glicko-1 display scale and the Glicko-2
//! internal (μ / φ) scale.
//!
//! The Glicko-2 paper defines the internal scale via:
//!
//! ```text
//! μ = (r − 1500) / Q        where Q = 173.7178
//! φ = RD / Q
//! ```
//!
//! All internal algorithm steps use μ and φ; conversions back to the display
//! scale are applied only to produce the final [`Glicko2Rating`].

use crate::Glicko2Rating;

/// The scale factor Q = ln(10) / 400 × 1000 ≈ 173.7178 used by Glicko-2.
///
/// Computed as `400.0 / ln(10)`, which is the reciprocal of how Elo maps
/// probability differences to rating differences.
pub(crate) const GLICKO2_SCALE: f64 = 173.717_82_f64;

/// A rating represented on the internal Glicko-2 scale.
///
/// `mu` and `phi` are the internal counterparts of `rating` and `deviation`
/// from [`Glicko2Rating`].  `volatility` is scale-independent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InternalRating {
    /// Internal mean μ = (r − 1500) / Q.
    pub mu: f64,
    /// Internal deviation φ = RD / Q.
    pub phi: f64,
    /// Volatility σ (scale-independent).
    pub volatility: f64,
}

/// Converts a [`Glicko2Rating`] to the Glicko-2 internal scale.
///
/// ```
/// use mcs_rating::{Glicko2Rating, to_internal};
///
/// let r = Glicko2Rating::default(); // 1500 / 350 / 0.06
/// let internal = to_internal(r);
/// // μ = (1500 − 1500) / Q = 0
/// assert!((internal.mu - 0.0).abs() < 1e-12);
/// // φ = 350 / 173.7178 ≈ 2.0148
/// assert!((internal.phi - 2.014_764).abs() < 1e-5);
/// assert_eq!(internal.volatility, 0.06);
/// ```
#[inline]
pub fn to_internal(r: Glicko2Rating) -> InternalRating {
    InternalRating {
        mu: (r.rating - 1500.0) / GLICKO2_SCALE,
        phi: r.deviation / GLICKO2_SCALE,
        volatility: r.volatility,
    }
}

/// Converts an [`InternalRating`] back to the display scale.
///
/// ```
/// use mcs_rating::{Glicko2Rating, to_internal, from_internal};
///
/// let original = Glicko2Rating { rating: 1632.5, deviation: 180.0, volatility: 0.055 };
/// let roundtripped = from_internal(to_internal(original));
/// assert!((roundtripped.rating    - original.rating).abs()    < 1e-10);
/// assert!((roundtripped.deviation - original.deviation).abs() < 1e-10);
/// assert_eq!(roundtripped.volatility, original.volatility);
/// ```
#[inline]
pub fn from_internal(i: InternalRating) -> Glicko2Rating {
    Glicko2Rating {
        rating: i.mu * GLICKO2_SCALE + 1500.0,
        deviation: i.phi * GLICKO2_SCALE,
        volatility: i.volatility,
    }
}
