//! # mcs-rating
//!
//! A pure, allocation-light Glicko-2 rating engine.
//!
//! ## Quick start
//!
//! ```rust
//! use mcs_rating::{Glicko2Rating, Score, DEFAULT_TAU};
//!
//! // Glickman (2012) canonical example player: r=1500, RD=200, σ=0.06.
//! let player = Glicko2Rating { rating: 1500.0, deviation: 200.0, volatility: 0.06 };
//!
//! let opponent_a = Glicko2Rating { rating: 1400.0, deviation: 30.0,  volatility: 0.06 };
//! let opponent_b = Glicko2Rating { rating: 1550.0, deviation: 100.0, volatility: 0.06 };
//! let opponent_c = Glicko2Rating { rating: 1700.0, deviation: 300.0, volatility: 0.06 };
//!
//! let results = [
//!     (opponent_a, Score::Win),
//!     (opponent_b, Score::Loss),
//!     (opponent_c, Score::Loss),
//! ];
//!
//! let updated = mcs_rating::update(player, &results, DEFAULT_TAU);
//!
//! // Expect ≈ 1464.06 / 151.52 / 0.05999 (Glickman 2012 example).
//! assert!((updated.rating - 1464.06).abs() < 0.1);
//! assert!((updated.deviation - 151.52).abs() < 0.1);
//! assert!((updated.volatility - 0.05999).abs() < 1e-4);
//! ```
//!
//! ## Algorithm
//!
//! The implementation follows the reference algorithm described in:
//!
//! > Glickman, M. E. (2012). *Example of the Glicko-2 system.*
//! > <http://www.glicko.net/glicko/glicko2.pdf>
//!
//! Steps in brief:
//!
//! 1. Convert ratings to the internal scale (μ = (r − 1500) / 173.7178,
//!    φ = RD / 173.7178).
//! 2. Compute the *g* and *E* functions for each opponent.
//! 3. Estimate the rating variance *v*.
//! 4. Compute the performance delta Δ.
//! 5. Update volatility σ′ with the Illinois / regula-falsi root-finding
//!    algorithm (convergence ε = 1 × 10⁻⁶).
//! 6. Compute the new φ′ and μ′.
//! 7. Convert back to the Glicko-1 scale.
//!
//! ## Feature flags
//!
//! | Flag     | Effect |
//! |----------|--------|
//! | `serde`  | Derives `serde::Serialize` / `serde::Deserialize` on public types. |
//! | `domain` | Enables `From`/`Into` conversions between [`Glicko2Rating`] and `mcs_domain::Rating`. Implies `serde`. |

#![doc(html_root_url = "https://docs.rs/mcs-rating")]

mod algorithm;
mod convert;
mod score;

pub use algorithm::{update, update_single};
pub use convert::{from_internal, to_internal, InternalRating};
pub use score::Score;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The default system constant τ, which controls the expected change in
/// volatility over one rating period.  A value of **0.5** is the conservative
/// recommendation in Glickman (2012) and the value used by lichess.
///
/// Increase τ if you expect player strength to fluctuate rapidly; decrease it
/// for a more stable, sluggish update.
pub const DEFAULT_TAU: f64 = 0.5;

/// A Glicko-2 rating triple.
///
/// Stores a player's estimated strength, the uncertainty around that estimate,
/// and a measure of performance consistency.
///
/// | Field        | Glicko-2 symbol | Default (new player) |
/// |--------------|-----------------|----------------------|
/// | `rating`     | r               | 1500.0               |
/// | `deviation`  | RD              | 350.0                |
/// | `volatility` | σ               | 0.06                 |
///
/// The `rating` and `deviation` fields use the **Glicko-1 / display scale**
/// (centred at 1500), not the internal μ/φ scale.  Conversion helpers live in
/// [`to_internal`] and [`from_internal`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Glicko2Rating {
    /// Estimated playing strength on the Glicko-1 scale (higher is stronger).
    pub rating: f64,
    /// Rating deviation — the standard deviation of the strength estimate.
    /// Smaller means a more certain estimate.
    pub deviation: f64,
    /// Volatility — a measure of performance consistency.  Typical values are
    /// close to 0.06; higher means more erratic results.
    pub volatility: f64,
}

impl Default for Glicko2Rating {
    /// Returns the standard Glicko-2 seed for a newly registered player:
    /// rating = 1500, deviation = 350, volatility = 0.06.
    fn default() -> Self {
        Self {
            rating: 1500.0,
            deviation: 350.0,
            volatility: 0.06,
        }
    }
}

// Optional conversions to/from mcs-domain's `Rating`.
#[cfg(feature = "domain")]
mod domain_convert {
    use super::Glicko2Rating;
    use mcs_domain::Rating;

    impl From<Rating> for Glicko2Rating {
        fn from(r: Rating) -> Self {
            Self {
                rating: r.value,
                deviation: r.deviation,
                volatility: r.volatility,
            }
        }
    }

    impl From<Glicko2Rating> for Rating {
        fn from(g: Glicko2Rating) -> Self {
            Self {
                value: g.rating,
                deviation: g.deviation,
                volatility: g.volatility,
            }
        }
    }
}
