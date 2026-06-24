//! The [`VariantFactory`] for standard chess and registry wiring.

use std::sync::Arc;

use mcs_core::{GameError, GameSession, VariantFactory, VariantOptions, VariantRegistry};

use crate::game::{StandardGame, VARIANT_ID};

/// The factory that creates standard-chess games.
///
/// Registered under the id [`VARIANT_ID`] (`"standard"`).
#[derive(Debug, Default, Clone, Copy)]
pub struct StandardVariant;

impl VariantFactory for StandardVariant {
    fn id(&self) -> &'static str {
        VARIANT_ID
    }

    fn display_name(&self) -> &str {
        "Standard Chess"
    }

    /// Creates a fresh game from the initial position.
    ///
    /// Standard chess takes no configuration, so any options are accepted: an
    /// empty/`null` value (the default) selects the standard start, and any
    /// other value is ignored rather than rejected, keeping the variant
    /// forgiving of extra fields a caller might pass.
    fn new_game(&self, options: &VariantOptions) -> Result<Box<dyn GameSession>, GameError> {
        let _ = options;
        Ok(Box::new(StandardGame::new()))
    }
}

/// Registers the standard-chess variant with `registry`.
///
/// Call this at server startup to make `"standard"` available via
/// [`VariantRegistry::new_game`].
pub fn register(registry: &mut VariantRegistry) {
    registry.register(Arc::new(StandardVariant));
}
