//! Glicko-2 update algorithm.
//!
//! Implements the full rating update described in:
//!
//! > Glickman, M. E. (2012). *Example of the Glicko-2 system.*
//! > <http://www.glicko.net/glicko/glicko2.pdf>
//!
//! The step numbers in the comments below refer directly to the steps in that
//! document.

use crate::{
    convert::{from_internal, to_internal},
    Glicko2Rating, Score,
};

use std::f64::consts::PI;

/// Convergence threshold ε for the Illinois/regula-falsi volatility iteration.
const EPSILON: f64 = 1e-6;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// The *g* function from the Glicko-2 paper (step 3a).
///
/// ```text
/// g(φ) = 1 / sqrt(1 + 3φ²/π²)
/// ```
#[inline]
fn g(phi: f64) -> f64 {
    1.0 / (1.0 + 3.0 * phi * phi / (PI * PI)).sqrt()
}

/// The expected score *E* for the player against one opponent (step 3b).
///
/// ```text
/// E(μ, μⱼ, φⱼ) = 1 / (1 + exp(−g(φⱼ) · (μ − μⱼ)))
/// ```
#[inline]
fn expected_score(mu: f64, mu_j: f64, phi_j: f64) -> f64 {
    1.0 / (1.0 + (-g(phi_j) * (mu - mu_j)).exp())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Computes a new Glicko-2 rating after a set of games in one rating period.
///
/// # Arguments
///
/// * `player`  — The player's current rating.
/// * `results` — A slice of `(opponent_rating, score)` pairs representing each
///   game played during this rating period.
/// * `tau`     — The system constant τ ∈ (0.3, 1.2).  Controls how quickly
///   volatility can change.  Use [`DEFAULT_TAU`] (0.5) when in doubt.
///
/// # No-games case
///
/// When `results` is empty the algorithm still applies: volatility is unchanged
/// and the deviation increases according to the player's current volatility
/// (as described in step 6 of the paper).  The rating itself does not change.
///
/// # Example
///
/// ```rust
/// use mcs_rating::{Glicko2Rating, Score, DEFAULT_TAU, update};
///
/// let player = Glicko2Rating { rating: 1500.0, deviation: 200.0, volatility: 0.06 };
/// let opponents = [
///     (Glicko2Rating { rating: 1400.0, deviation: 30.0,  volatility: 0.06 }, Score::Win),
///     (Glicko2Rating { rating: 1550.0, deviation: 100.0, volatility: 0.06 }, Score::Loss),
///     (Glicko2Rating { rating: 1700.0, deviation: 300.0, volatility: 0.06 }, Score::Loss),
/// ];
/// let new_rating = update(player, &opponents, DEFAULT_TAU);
/// assert!((new_rating.rating   - 1464.06).abs() < 0.1);
/// assert!((new_rating.deviation - 151.52).abs() < 0.1);
/// assert!((new_rating.volatility - 0.05999).abs() < 1e-4);
/// ```
///
/// [`DEFAULT_TAU`]: crate::DEFAULT_TAU
pub fn update(
    player: Glicko2Rating,
    results: &[(Glicko2Rating, Score)],
    tau: f64,
) -> Glicko2Rating {
    let p = to_internal(player);

    // Step 6 (no-games case): deviation widens by volatility; rating unchanged.
    if results.is_empty() {
        let phi_star = (p.phi * p.phi + p.volatility * p.volatility).sqrt();
        return from_internal(crate::convert::InternalRating {
            mu: p.mu,
            phi: phi_star,
            volatility: p.volatility,
        });
    }

    // Step 1 (already done): p.mu, p.phi, p.volatility hold the internal values.

    // Step 3 — Compute estimated variance v and the performance delta Δ.
    let mut inv_v: f64 = 0.0; // accumulates 1/v
    let mut delta_sum: f64 = 0.0; // accumulates Δ/v (before multiply)

    for (opp, score) in results {
        let o = to_internal(*opp);
        let g_j = g(o.phi);
        let e_j = expected_score(p.mu, o.mu, o.phi);
        inv_v += g_j * g_j * e_j * (1.0 - e_j);
        delta_sum += g_j * (score.value() - e_j);
    }

    let v = 1.0 / inv_v; // estimated variance (step 3)
    let delta = v * delta_sum; // performance delta (step 4)

    // Step 5 — Update volatility via Illinois/regula-falsi root-finding.
    let new_volatility = update_volatility(p.phi, p.volatility, delta, v, tau);

    // Step 6 — Update deviation to pre-rating period value φ*.
    let phi_star = (p.phi * p.phi + new_volatility * new_volatility).sqrt();

    // Step 7 — Compute new φ′ and μ′.
    let phi_prime = 1.0 / (1.0 / (phi_star * phi_star) + 1.0 / v).sqrt();
    let mu_prime = p.mu + phi_prime * phi_prime * delta_sum;

    from_internal(crate::convert::InternalRating {
        mu: mu_prime,
        phi: phi_prime,
        volatility: new_volatility,
    })
}

/// Convenience wrapper for a single game.
///
/// Equivalent to calling [`update`] with a one-element slice.
///
/// # Example
///
/// ```rust
/// use mcs_rating::{Glicko2Rating, Score, DEFAULT_TAU, update_single};
///
/// let player   = Glicko2Rating::default();
/// let opponent = Glicko2Rating { rating: 1600.0, deviation: 150.0, volatility: 0.06 };
/// let result   = update_single(player, opponent, Score::Win, DEFAULT_TAU);
/// // Beating a higher-rated opponent should raise the rating.
/// assert!(result.rating > player.rating);
/// ```
pub fn update_single(
    player: Glicko2Rating,
    opponent: Glicko2Rating,
    score: Score,
    tau: f64,
) -> Glicko2Rating {
    update(player, &[(opponent, score)], tau)
}

// ---------------------------------------------------------------------------
// Volatility update (step 5 of the Glicko-2 algorithm)
// ---------------------------------------------------------------------------

/// Runs the Illinois / regula-falsi algorithm to find the new volatility σ′.
///
/// This is step 5 of the Glickman (2012) reference algorithm.  It seeks the
/// root of the function `f` defined in the paper, where the argument `x`
/// represents `ln(σ²)` (not `ln(σ)`).
fn update_volatility(phi: f64, sigma: f64, delta: f64, v: f64, tau: f64) -> f64 {
    let phi2 = phi * phi;
    let delta2 = delta * delta;
    // ln(σ²): the working variable throughout the algorithm is x = ln(σ²).
    let ln_sigma2 = sigma.powi(2).ln();

    // f(x) as defined in the paper.  x = ln(σ²), e^x = σ².
    let f = |x: f64| -> f64 {
        let ex = x.exp(); // e^x = σ²
        let tmp = phi2 + v + ex;
        let num = ex * (delta2 - phi2 - v - ex);
        let den = 2.0 * tmp * tmp;
        num / den - (x - ln_sigma2) / (tau * tau)
    };

    // Step 5.1 — Establish initial interval [A, B] where A = ln(σ²).
    let a = ln_sigma2;
    let mut b = if delta2 > phi2 + v {
        (delta2 - phi2 - v).ln()
    } else {
        // Expand downward until f(B) < 0.
        let mut k = 1.0_f64;
        while f(a - k * tau) < 0.0 {
            k += 1.0;
        }
        a - k * tau
    };

    // Step 5.2
    let mut fa = f(a);
    let mut fb = f(b);
    let mut a_cur = a;

    // Step 5.3 — Illinois / regula-falsi iteration.
    while (b - a_cur).abs() > EPSILON {
        // Regula-falsi step.
        let c = a_cur + (a_cur - b) * fa / (fb - fa);
        let fc = f(c);

        if fc * fb < 0.0 {
            a_cur = b;
            fa = fb;
        } else {
            // Illinois adjustment: halve fa to force the bracket to move.
            fa /= 2.0;
        }
        b = c;
        fb = fc;
    }

    // Step 5.4 — New volatility: a_cur = ln(σ'²), so σ' = exp(a_cur / 2).
    (a_cur / 2.0).exp()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_TAU;

    /// Canonical Glickman (2012) worked example.
    ///
    /// Player: r = 1500, RD = 200, σ = 0.06.
    /// Opponents (r, RD, σ) with results:
    ///   (1400, 30, 0.06)  — Win
    ///   (1550, 100, 0.06) — Loss
    ///   (1700, 300, 0.06) — Loss
    ///
    /// Expected output (from the paper): r′ ≈ 1464.06, RD′ ≈ 151.52, σ′ ≈ 0.05999.
    #[test]
    fn glickman_canonical_example() {
        let player = Glicko2Rating {
            rating: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let opponents = [
            (
                Glicko2Rating {
                    rating: 1400.0,
                    deviation: 30.0,
                    volatility: 0.06,
                },
                Score::Win,
            ),
            (
                Glicko2Rating {
                    rating: 1550.0,
                    deviation: 100.0,
                    volatility: 0.06,
                },
                Score::Loss,
            ),
            (
                Glicko2Rating {
                    rating: 1700.0,
                    deviation: 300.0,
                    volatility: 0.06,
                },
                Score::Loss,
            ),
        ];

        let result = update(player, &opponents, DEFAULT_TAU);

        assert!(
            (result.rating - 1464.06).abs() < 0.1,
            "rating: expected ≈1464.06, got {}",
            result.rating
        );
        assert!(
            (result.deviation - 151.52).abs() < 0.1,
            "deviation: expected ≈151.52, got {}",
            result.deviation
        );
        assert!(
            (result.volatility - 0.05999).abs() < 1e-4,
            "volatility: expected ≈0.05999, got {}",
            result.volatility
        );
    }

    /// Beating a higher-rated opponent must raise the player's rating.
    #[test]
    fn win_against_higher_rated_raises_rating() {
        let player = Glicko2Rating::default(); // 1500
        let stronger = Glicko2Rating {
            rating: 1700.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let result = update_single(player, stronger, Score::Win, DEFAULT_TAU);
        assert!(
            result.rating > player.rating,
            "Expected rating to increase after beating a stronger opponent; got {}",
            result.rating
        );
    }

    /// Losing to a lower-rated opponent must decrease the player's rating.
    #[test]
    fn loss_against_weaker_rated_lowers_rating() {
        let player = Glicko2Rating::default(); // 1500
        let weaker = Glicko2Rating {
            rating: 1300.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let result = update_single(player, weaker, Score::Loss, DEFAULT_TAU);
        assert!(
            result.rating < player.rating,
            "Expected rating to decrease after losing to a weaker opponent; got {}",
            result.rating
        );
    }

    /// Playing games must reduce the rating deviation (uncertainty shrinks with more data).
    #[test]
    fn deviation_shrinks_after_games() {
        let player = Glicko2Rating::default(); // deviation = 350
        let opponent = Glicko2Rating {
            rating: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let result = update_single(player, opponent, Score::Draw, DEFAULT_TAU);
        assert!(
            result.deviation < player.deviation,
            "Expected deviation to shrink after playing a game; got {}",
            result.deviation
        );
    }

    /// With no games played the deviation must increase (uncertainty grows with inactivity).
    #[test]
    fn no_games_increases_deviation() {
        let player = Glicko2Rating {
            rating: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let result = update(player, &[], DEFAULT_TAU);
        assert!(
            result.deviation > player.deviation,
            "Expected deviation to increase when no games are played; got {}",
            result.deviation
        );
        // Rating must not change.
        assert!(
            (result.rating - player.rating).abs() < 1e-10,
            "Expected rating to be unchanged; got {}",
            result.rating
        );
        // Volatility must not change.
        assert!(
            (result.volatility - player.volatility).abs() < 1e-10,
            "Expected volatility to be unchanged; got {}",
            result.volatility
        );
    }

    /// Drawing against an equal opponent should leave the rating nearly unchanged.
    #[test]
    fn draw_against_equal_rating_stable() {
        let player = Glicko2Rating {
            rating: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let equal = Glicko2Rating {
            rating: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let result = update_single(player, equal, Score::Draw, DEFAULT_TAU);
        // Rating should stay very close to 1500.
        assert!(
            (result.rating - 1500.0).abs() < 1.0,
            "Expected rating near 1500 after drawing vs equal; got {}",
            result.rating
        );
    }

    /// Multiple wins in sequence must yield a higher final rating than a single win.
    #[test]
    fn more_wins_yield_higher_rating() {
        let player = Glicko2Rating::default();
        let opponent = Glicko2Rating {
            rating: 1600.0,
            deviation: 150.0,
            volatility: 0.06,
        };

        let single_win = update(player, &[(opponent, Score::Win)], DEFAULT_TAU);
        let two_wins = update(
            player,
            &[(opponent, Score::Win), (opponent, Score::Win)],
            DEFAULT_TAU,
        );

        assert!(
            two_wins.rating > single_win.rating,
            "Two wins should yield higher rating than one; single={}, two={}",
            single_win.rating,
            two_wins.rating
        );
    }

    /// `update_single` must give the same result as `update` with one element.
    #[test]
    fn update_single_consistent_with_update() {
        let player = Glicko2Rating {
            rating: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        };
        let opp = Glicko2Rating {
            rating: 1400.0,
            deviation: 30.0,
            volatility: 0.06,
        };

        let via_single = update_single(player, opp, Score::Win, DEFAULT_TAU);
        let via_slice = update(player, &[(opp, Score::Win)], DEFAULT_TAU);

        assert!((via_single.rating - via_slice.rating).abs() < 1e-12);
        assert!((via_single.deviation - via_slice.deviation).abs() < 1e-12);
        assert!((via_single.volatility - via_slice.volatility).abs() < 1e-12);
    }
}
