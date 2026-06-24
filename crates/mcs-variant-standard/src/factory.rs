//! The [`VariantFactory`] implementations for standard chess and Chess960, plus
//! registry wiring.

use std::sync::Arc;

use mcs_core::{GameError, GameSession, VariantFactory, VariantOptions, VariantRegistry};
use serde::Deserialize;

use crate::game::{StandardGame, CHESS960_VARIANT_ID, VARIANT_ID};

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

/// Options accepted by the [`Chess960Variant`] factory.
///
/// Both fields are optional and mutually exclusive in practice; `position` takes
/// precedence when both are given. With neither (the default), the game starts
/// from the classical chess setup but plays under Chess960 castling rules
/// (king-to-rook UCI). The JSON shape is:
///
/// ```json
/// { "position": 518 }
/// ```
/// or
/// ```json
/// { "fen": "nrbbqknr/pppppppp/8/8/8/8/PPPPPPPP/NRBBQKNR w KQkq - 0 1" }
/// ```
#[derive(Debug, Default, Deserialize)]
struct Chess960Options {
    /// A Scharnagl start-position number in `0..=959` (518 is the classical
    /// setup).
    position: Option<u32>,
    /// A starting FEN, used when `position` is absent.
    fen: Option<String>,
}

/// The factory that creates Chess960 (Fischer Random) games.
///
/// Registered under the id [`CHESS960_VARIANT_ID`] (`"chess960"`).
///
/// # Castling on the wire
///
/// Unlike [`StandardVariant`], Chess960 spells castling in **UCI_960**
/// (king-to-rook) form, e.g. `e1h1` rather than `e1g1`, because the rook's
/// starting file is not fixed. Clients must use this convention for Chess960
/// games. Everything else — promotions, en passant, check, and the FEN in the
/// view — is identical to standard chess.
#[derive(Debug, Default, Clone, Copy)]
pub struct Chess960Variant;

impl VariantFactory for Chess960Variant {
    fn id(&self) -> &'static str {
        CHESS960_VARIANT_ID
    }

    fn display_name(&self) -> &str {
        "Chess960"
    }

    /// Creates a fresh Chess960 game.
    ///
    /// Accepts `{ "position": 0..=959 }` to start from a Scharnagl-numbered
    /// layout, or `{ "fen": "..." }` to start from an explicit position. With no
    /// (or `null`) options the classical setup is used.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::InvalidActionPayload`] if the options are malformed,
    /// `position` is out of range, or `fen` is not a valid position.
    fn new_game(&self, options: &VariantOptions) -> Result<Box<dyn GameSession>, GameError> {
        // An absent/null options value deserializes to the all-`None` default.
        let opts: Chess960Options = if options.as_value().is_null() {
            Chess960Options::default()
        } else {
            options.to_typed()?
        };

        let game = match (opts.position, opts.fen) {
            (Some(position), _) => StandardGame::chess960(position)?,
            (None, Some(fen)) => StandardGame::from_fen(&fen)?,
            (None, None) => StandardGame::new_chess960(),
        };
        Ok(Box::new(game))
    }
}

/// Registers the standard-chess and Chess960 variants with `registry`.
///
/// This is the entry point used at server startup; it makes both `"standard"`
/// and `"chess960"` available via [`VariantRegistry::new_game`]. The name
/// [`register`] is kept (rather than `register_all`) to match the call site the
/// composition root already uses.
pub fn register(registry: &mut VariantRegistry) {
    registry.register(Arc::new(StandardVariant));
    registry.register(Arc::new(Chess960Variant));
}
