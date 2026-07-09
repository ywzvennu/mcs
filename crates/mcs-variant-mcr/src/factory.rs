//! The [`VariantFactory`] for mcr-backed variants and the catalog registration.

use std::sync::Arc;

use mcr::geometry::VariantFamily;
use mcr::VariantRef;
use mcs_core::{
    GameError, GameSession, VariantFactory, VariantMetadata, VariantOptions, VariantRegistry,
};

/// The stable snake_case label for an mcr [`VariantFamily`] — a coarse piece-set
/// hint a client uses to pick a glyph set / group variants. Kept in lockstep
/// with the enum (exhaustive, so a new mcr family fails to compile until mapped).
fn family_label(family: VariantFamily) -> &'static str {
    match family {
        VariantFamily::Chess => "chess",
        VariantFamily::Capablanca => "capablanca",
        VariantFamily::Xiangqi => "xiangqi",
        VariantFamily::Janggi => "janggi",
        VariantFamily::Shogi => "shogi",
        VariantFamily::Makruk => "makruk",
        VariantFamily::Fairy => "fairy",
    }
}
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

/// The canonical mcr catalog name of jieqi (hidden Xiangqi), the one variant this
/// adapter resolves a per-game reveal seed for (see
/// [`prepare_new_game_options`](McrVariant::prepare_new_game_options)).
const JIEQI_ID: &str = "jieqi";

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
    /// `family` is mapped from mcr's `VariantRules::family` taxonomy (mcr#611) —
    /// a coarse piece-set hint (chess / capablanca / xiangqi / janggi / shogi /
    /// makruk / fairy) the client uses to select a glyph set and group variants.
    fn metadata(&self) -> VariantMetadata {
        let rules = self.variant.rules();
        VariantMetadata {
            board_width: u32::from(rules.board.width),
            board_height: u32::from(rules.board.height),
            has_hand: rules.mechanics.has_hand,
            family: Some(family_label(rules.family).to_owned()),
            start_fen: Some(rules.board.start_fen),
        }
    }

    /// Resolves the durable options a fresh game will be created and persisted
    /// with, giving jieqi its per-game hidden-reveal seed.
    ///
    /// jieqi is a genuine hidden-information game only when it carries a reveal
    /// seed: without one, every concealed piece reveals to the Xiangqi piece
    /// native to its home square (the deterministic home-role baseline), which any
    /// observer can predict — so there is no hidden information. This method gives
    /// each fresh jieqi game a per-game random `u64` seed, folded into the starting
    /// FEN as mcr's optional trailing seventh field, so its concealed identities
    /// are genuinely secret (a seed-derived shuffle of each army).
    ///
    /// Baking the seed into the returned options — which the server persists and
    /// later replays through [`new_game`](Self::new_game) on recovery — is what
    /// makes recovery **reproduce the same assignment**: the seed is generated once
    /// here, never in [`new_game`](Self::new_game), which recovery re-runs. An
    /// explicit `fen` (a caller-chosen position, or a recovered one that already
    /// carries its seed) is honored untouched, so a seed is never re-randomized.
    ///
    /// Every other variant, and jieqi given an explicit `fen`, returns its options
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::InvalidActionPayload`] (via
    /// [`GameError::Serialization`]) if the options are malformed.
    fn prepare_new_game_options(
        &self,
        options: &VariantOptions,
    ) -> Result<VariantOptions, GameError> {
        // Only jieqi resolves a seed; every other variant is a pure function of
        // its (possibly default) options already.
        if self.variant.name() != JIEQI_ID {
            return Ok(options.clone());
        }

        let opts: McrOptions = if options.as_value().is_null() {
            McrOptions::default()
        } else {
            options.to_typed()?
        };
        // An explicit starting position is authoritative: honor it as-is (this is
        // also the recovery path, whose persisted FEN already carries the seed) and
        // never re-randomize.
        if opts.fen.is_some() {
            return Ok(options.clone());
        }

        // Fold a fresh per-game seed into the variant's start FEN as mcr's optional
        // trailing seventh field (a plain `u64`). mcr's start FEN is the six-field
        // all-dark baseline; appending the seed opts this game into the stochastic
        // reveal.
        let seed = rand::random::<u64>();
        let start_fen = self.variant.rules().board.start_fen;
        let seeded_fen = format!("{start_fen} {seed}");
        Ok(VariantOptions::new(
            serde_json::json!({ "fen": seeded_fen }),
        ))
    }

    /// Creates a fresh game of this variant.
    ///
    /// Accepts `{ "fen": "..." }` to start from an explicit position; with no
    /// (or `null`) options the variant's standard start position is used. This is
    /// a pure function of `options`: any per-game randomness (jieqi's reveal seed)
    /// is resolved once in
    /// [`prepare_new_game_options`](Self::prepare_new_game_options) and arrives
    /// here already baked into the `fen`, so recovery — which replays through this
    /// method on the persisted options — rebuilds the identical game.
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

/// Registers mcr's variant catalog with `registry`, one factory per variant,
/// keyed by the variant's canonical mcr name.
///
/// **Every** variant in [`VariantRef::all`] is registered — this adapter no longer
/// defers any of them. This includes `standard` and `chess960` (#155), so mcr is
/// the single gameplay engine for ordinary chess as well, and the
/// hidden-information variants **Fog of War** and **jieqi**, whose per-player views
/// mcr redacts (delegated by [`McrGame`](crate::McrGame), #163). jieqi is registered
/// as a genuine hidden-information game: each fresh game is given a per-game reveal
/// seed by [`prepare_new_game_options`](McrVariant::prepare_new_game_options).
pub fn register(registry: &mut VariantRegistry) {
    for variant in VariantRef::all() {
        registry.register(Arc::new(McrVariant::new(variant)));
    }
}
