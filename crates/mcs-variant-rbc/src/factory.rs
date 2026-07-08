//! The [`VariantFactory`] for Reconnaissance Blind Chess and registry wiring.

use std::sync::Arc;

use mcs_core::{
    GameError, GameSession, VariantFactory, VariantMetadata, VariantOptions, VariantRegistry,
};

use crate::game::{RbcGame, VARIANT_ID};

/// The factory that creates Reconnaissance Blind Chess games.
///
/// Registered under the id [`VARIANT_ID`] (`"rbc"`).
#[derive(Debug, Default, Clone, Copy)]
pub struct RbcVariant;

impl VariantFactory for RbcVariant {
    fn id(&self) -> &'static str {
        VARIANT_ID
    }

    fn display_name(&self) -> &str {
        "Reconnaissance Blind Chess"
    }

    /// RBC is played on a standard 8x8 board with the ordinary chess army and no
    /// hand / drops, so it reports the standard geometry and starting position
    /// rather than relying on the trait default.
    fn metadata(&self) -> VariantMetadata {
        VariantMetadata {
            board_width: 8,
            board_height: 8,
            has_hand: false,
            family: None,
            start_fen: Some("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_owned()),
        }
    }

    /// Creates a fresh game from the standard RBC starting position.
    ///
    /// RBC takes no configuration here, so any options are accepted: an
    /// empty/`null` value (the default) selects the standard start, and any
    /// other value is ignored rather than rejected, keeping the variant
    /// forgiving of extra fields a caller might pass.
    fn new_game(&self, options: &VariantOptions) -> Result<Box<dyn GameSession>, GameError> {
        let _ = options;
        Ok(Box::new(RbcGame::new()))
    }
}

/// Registers the Reconnaissance Blind Chess variant with `registry`.
///
/// Call this at server startup to make `"rbc"` available via
/// [`VariantRegistry::new_game`].
pub fn register(registry: &mut VariantRegistry) {
    registry.register(Arc::new(RbcVariant));
}
