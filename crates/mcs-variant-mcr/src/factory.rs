//! The [`VariantFactory`] for mcr-backed variants and the catalog registration.

use std::sync::Arc;

use mcr::VariantRef;
use mcs_core::{GameError, GameSession, VariantFactory, VariantOptions, VariantRegistry};
use serde::Deserialize;

use crate::game::McrGame;

/// A factory that creates games of one mcr variant, identified by its
/// [`VariantRef`].
///
/// One factory is registered per non-excluded variant in mcr's catalog (see
/// [`register`]); each reports the variant's canonical mcr name as its id.
#[derive(Debug, Clone, Copy)]
pub struct McrVariant {
    /// The mcr catalog key this factory builds games for.
    variant: VariantRef,
}

impl McrVariant {
    /// Wraps `variant` in a factory.
    #[must_use]
    pub fn new(variant: VariantRef) -> Self {
        Self { variant }
    }
}

/// Options accepted by the [`McrVariant`] factory.
///
/// The only field is an optional starting FEN, in mcr's dialect. With no (or
/// `null`) options the variant's standard starting position is used.
///
/// ```json
/// { "fen": "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1" }
/// ```
#[derive(Debug, Default, Deserialize)]
struct McrOptions {
    /// A starting FEN, used instead of the variant's default start position.
    fen: Option<String>,
}

impl VariantFactory for McrVariant {
    fn id(&self) -> &'static str {
        self.variant.name()
    }

    fn display_name(&self) -> &str {
        // mcr exposes only the canonical (machine) name; enriching this with a
        // human-facing label is deferred to the `/variants` work (#157).
        self.variant.name()
    }

    /// Creates a fresh game of this variant.
    ///
    /// Accepts `{ "fen": "..." }` to start from an explicit position; with no
    /// (or `null`) options the variant's standard start position is used.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::InvalidActionPayload`] if the options are malformed
    /// or the supplied `fen` is not valid for this variant.
    fn new_game(&self, options: &VariantOptions) -> Result<Box<dyn GameSession>, GameError> {
        // An absent/null options value deserializes to the all-`None` default.
        let opts: McrOptions = if options.as_value().is_null() {
            McrOptions::default()
        } else {
            options.to_typed()?
        };

        let game = match opts.fen {
            Some(fen) => McrGame::from_fen(self.variant, &fen)?,
            None => McrGame::new(self.variant),
        };
        Ok(Box::new(game))
    }
}

/// Whether `variant` is deliberately **not** registered by this adapter.
///
/// This adapter serves only the *perfect-information, single-move* slice of
/// mcr's catalog. The following are excluded:
///
/// - **`standard` and `chess960`** — owned by the cozy-chess-backed
///   `mcs-variant-standard` until that crate is retired (#155). Registering them
///   here too would collide on the registry key.
/// - **Fog of War and Jieqi** — hidden-information variants (no full-board
///   visibility / concealed "dark" pieces) whose views must be redacted
///   per player; deferred to the redaction work (#156).
/// - **Duck chess** — a two-part move (piece move, then duck placement) that the
///   single-action seam here cannot express; deferred to #156.
/// - **Placement / Sittuyin** — variants with a setup (piece-deployment) phase
///   before normal play, likewise deferred to #156. These are matched by mcr's
///   `has_placement` mechanic flag so any future placement variant is excluded
///   automatically.
fn is_excluded(variant: VariantRef) -> bool {
    // (a) Owned by `mcs-variant-standard` until #155.
    if matches!(variant.name(), "standard" | "chess960") {
        return true;
    }
    // (b) Hidden-information variants needing per-player redaction (#156).
    if matches!(variant.name(), "fogofwar" | "jieqi") {
        return true;
    }
    // (b) Phased variants: a two-part move (duck) or a setup/placement phase,
    // neither of which the single-action seam expresses. Deferred to #156.
    let mechanics = variant.rules().mechanics;
    mechanics.has_duck || mechanics.has_placement
}

/// Registers mcr's variant catalog with `registry`, one factory per variant,
/// keyed by the variant's canonical mcr name.
///
/// Every variant in [`VariantRef::all`] is registered **except** those filtered
/// by [`is_excluded`] (standard / chess960, the hidden-information variants, and
/// the phased variants — see that function for the rationale). This keeps the PR
/// additive: the exclusion of `standard` / `chess960` prevents any key collision
/// with the still-present `mcs-variant-standard`.
pub fn register(registry: &mut VariantRegistry) {
    for variant in VariantRef::all() {
        if is_excluded(variant) {
            continue;
        }
        registry.register(Arc::new(McrVariant::new(variant)));
    }
}
