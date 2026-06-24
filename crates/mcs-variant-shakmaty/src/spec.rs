//! The [`VariantSpec`] trait that parameterizes the generic adapter.
//!
//! A [`VariantSpec`] is the single place where one shakmaty variant differs from
//! the others. It pins down the concrete [`Position`] type, the stable id and
//! display name, how a fresh starting position is built (from caller options),
//! and — crucially — how a *decisive* board ending is described as an
//! [`EndReason`], since each variant wins for its own reason (a king reaching
//! the centre, the third check, an exploded king, …).
//!
//! Everything else — move generation, legality, UCI parsing, view building,
//! resign/draw handling, termination detection via [`Position::outcome`] — is
//! shared by [`ShakmatyGame`](crate::game::ShakmatyGame) and written once.

use mcs_core::{Color, EndReason, GameError, VariantOptions};
use shakmaty::Position;

/// Specification of a single shakmaty variant.
///
/// Implementors are zero-sized marker types (e.g. [`Atomic`](crate::Atomic))
/// that select a concrete shakmaty [`Position`] and describe the variant-level
/// metadata the generic adapter cannot infer on its own.
pub trait VariantSpec: Send + Sync + 'static {
    /// The concrete shakmaty position type backing this variant.
    ///
    /// It must be `Clone` (the adapter clones it to build SAN and FENs without
    /// consuming the live position) and `Debug` (sessions are `Debug`).
    type Position: Position + Clone + std::fmt::Debug + Send + Sync;

    /// The stable, machine-facing identifier (e.g. `"atomic"`).
    const ID: &'static str;

    /// A human-facing name (e.g. `"Atomic"`).
    const DISPLAY_NAME: &'static str;

    /// Builds the starting position for a new game from `options`.
    ///
    /// Most variants have a single fixed start and ignore `options`; Chess960
    /// reads a starting-position number or FEN from it (see
    /// [`crate::Chess960`]).
    ///
    /// # Errors
    ///
    /// Returns a [`GameError`] if `options` cannot be interpreted by the
    /// variant (for example, an out-of-range Chess960 position number).
    fn starting_position(options: &VariantOptions) -> Result<Self::Position, GameError>;

    /// Describes a **decisive** board ending — one where shakmaty reports a
    /// winner — as an [`EndReason`].
    ///
    /// `winner` is the side shakmaty declared victorious, and `position` is the
    /// terminal position so a variant can tell *how* the game was won: shakmaty
    /// reports special wins (a king on the hill, the third check, an exploded
    /// king, a finished race) through
    /// [`Position::variant_outcome`](shakmaty::Position::variant_outcome), and an
    /// ordinary mate through the base rules. Implementations typically branch on
    /// `position.variant_outcome().is_some()`.
    ///
    /// The default reports [`EndReason::Checkmate`], which is correct for the
    /// variants whose only decisive board ending is mate. Variants that can win
    /// by a special rule override this to return the precise reason, using
    /// [`EndReason::Other`] where no exact enum case exists.
    #[must_use]
    fn decisive_reason(_winner: Color, _position: &Self::Position) -> EndReason {
        EndReason::Checkmate
    }
}
