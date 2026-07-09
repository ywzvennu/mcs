//! The [`VariantFactory`] for mcr-backed variants and the catalog registration.

use std::sync::Arc;

use mcr::VariantRef;
use mcs_core::{
    GameError, GameSession, VariantFactory, VariantMetadata, VariantOptions, VariantRegistry,
};
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

    /// Reads render-oriented metadata from the variant's engine-derived
    /// [`mcr::VariantRules`].
    ///
    /// - `board_width` / `board_height` come from `rules.board.{width,height}`
    ///   (so large- or small-board variants report their true geometry).
    /// - `has_hand` comes from `rules.mechanics.has_hand` (the hand/drop mechanic
    ///   of Crazyhouse and the shogi family).
    /// - `start_fen` is `rules.board.start_fen` (mcr's FEN dialect).
    ///
    /// `family` is left `None`: mcr's `VariantRules` carries board, army, and
    /// per-mechanic flags but no family / piece-set taxonomy to map from.
    fn metadata(&self) -> VariantMetadata {
        let rules = self.variant.rules();
        VariantMetadata {
            board_width: u32::from(rules.board.width),
            board_height: u32::from(rules.board.height),
            has_hand: rules.mechanics.has_hand,
            family: None,
            start_fen: Some(rules.board.start_fen),
        }
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
/// Since #156 this adapter serves the whole of mcr's catalog — including the
/// once-deferred phased variants (Duck, Placement, Sittuyin) and the flagship
/// hidden-information variant Fog of War (whose per-player views are redacted;
/// see [`McrGame`](crate::McrGame)) — with a **single** remaining exclusion:
///
/// - **Jieqi** (dark chess). mcr's [`Game`](mcr::Game) seam models Jieqi's reveal
///   *deterministically* (a face-down piece reveals as the Xiangqi piece native
///   to its home square) and its FEN exposes only a **generic** face-down marker
///   (`=D`/`=d`) for every concealed piece — never the stochastic per-piece
///   identity that makes real Jieqi a hidden-information game (the seeded reveal
///   pool is a separate layer, not wired into `Game`). The adapter therefore
///   cannot show a player its own concealed identities, nor drive a true reveal,
///   without fabricating hidden state mcr does not expose. Rather than ship a
///   misleading "deterministic Jieqi", it stays excluded until the seam surfaces
///   the reveal pool (tracked under #156's follow-up).
///
/// Duck, Placement, and Sittuyin need no exclusion: Duck's two-part move is a
/// single combined UCI (`e2e4,e5`) mcr emits directly, and the setup phases of
/// Placement and Sittuyin are alternating **open** drops (`N@a1`) driven through
/// the ordinary [`legal_ucis`](mcr::Game::legal_ucis) / [`play_uci`](mcr::Game::play_uci)
/// seam — all fully expressible as single actions with no hidden information.
fn is_excluded(variant: VariantRef) -> bool {
    // Jieqi remains deferred: the seam exposes only a generic face-down marker,
    // not the stochastic per-piece hidden identity (see the doc comment). Every
    // other variant — Fog of War (redacted views), Duck, Placement, Sittuyin — is
    // now registered.
    variant.name() == "jieqi"
}

/// Registers mcr's variant catalog with `registry`, one factory per variant,
/// keyed by the variant's canonical mcr name.
///
/// Every variant in [`VariantRef::all`] is registered **except** those filtered
/// by [`is_excluded`] (since #156, only Jieqi — see that function for the
/// rationale). This includes `standard` and `chess960` (#155), so mcr is the
/// single gameplay engine for ordinary chess as well, and Fog of War, whose
/// per-player views are redacted by [`McrGame`](crate::McrGame).
pub fn register(registry: &mut VariantRegistry) {
    for variant in VariantRef::all() {
        if is_excluded(variant) {
            continue;
        }
        registry.register(Arc::new(McrVariant::new(variant)));
    }
}
