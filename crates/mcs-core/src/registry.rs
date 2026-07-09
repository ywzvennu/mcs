//! Variant factories and the registry that owns them.

use std::collections::HashMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::GameError;
use crate::session::GameSession;

/// Type-erased options passed when creating a new game.
///
/// Like the other boundary payloads this wraps a [`serde_json::Value`] so that
/// each variant can define its own strongly typed options (time control,
/// starting position, and so on) while the registry stays variant-agnostic.
/// An empty/default value selects the variant's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VariantOptions(serde_json::Value);

impl VariantOptions {
    /// Wraps a raw JSON value directly.
    #[must_use]
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Erases strongly typed options into this payload.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::Serialization`] if `value` cannot be serialized.
    pub fn from_typed<T: Serialize>(value: &T) -> Result<Self, GameError> {
        serde_json::to_value(value)
            .map(Self)
            .map_err(|e| GameError::Serialization(e.to_string()))
    }

    /// Recovers strongly typed options from this payload.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::Serialization`] if the wrapped value does not match
    /// the shape of `T`.
    pub fn to_typed<T: DeserializeOwned>(&self) -> Result<T, GameError> {
        T::deserialize(&self.0).map_err(|e| GameError::Serialization(e.to_string()))
    }

    /// Returns a reference to the wrapped JSON value.
    #[must_use]
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

/// Static, render-oriented facts about one variant: its board geometry and the
/// capabilities a client needs to draw it.
///
/// This is the metadata seam through which the API layer learns enough to render
/// any variant (board dimensions, whether there is a hand / piece drops, the
/// starting position) **without** the API crate depending on the rules engine.
/// Each [`VariantFactory`] fills it in from its own rules source.
///
/// The registry stays IO-free, so this is plain owned data. It is serde-friendly
/// (no `utoipa` dependency); the API layer mirrors it into its own wire DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantMetadata {
    /// The number of files (board width), in squares.
    pub board_width: u32,
    /// The number of ranks (board height), in squares.
    pub board_height: u32,
    /// Whether the variant has a persistent hand and piece drops (Crazyhouse and
    /// the shogi family). A client renders a pocket / drop control when this is
    /// set; for every drop-less variant it is `false`.
    pub has_hand: bool,
    /// A coarse family / rule grouping (e.g. a piece-set hint the client uses to
    /// pick sprites), when the variant's rules source exposes one. `None` when the
    /// source has no such taxonomy.
    pub family: Option<String>,
    /// The starting position in the variant's own FEN dialect, when known — enough
    /// for a client to draw the initial board.
    pub start_fen: Option<String>,
}

impl Default for VariantMetadata {
    /// The neutral fallback: an ordinary 8x8 board with no hand, no family hint,
    /// and no start FEN. This is what the default [`VariantFactory::metadata`]
    /// returns, so a factory that does not override it still reports a sane board.
    fn default() -> Self {
        Self {
            board_width: 8,
            board_height: 8,
            has_hand: false,
            family: None,
            start_fen: None,
        }
    }
}

/// Creates fresh [`GameSession`]s for one variant.
///
/// A factory is the entry point through which the server instantiates games of
/// a given variant without naming its concrete session type.
pub trait VariantFactory: Send + Sync {
    /// The stable, machine-facing identifier of this variant (e.g. `"standard"`
    /// or `"rbc"`). This is the key the variant is registered under.
    fn id(&self) -> &'static str;

    /// A human-facing name for this variant (e.g. `"Standard Chess"`).
    fn display_name(&self) -> &str;

    /// Static, render-oriented [`VariantMetadata`] for this variant.
    ///
    /// Surfaced through `GET /variants` so a client can render the board without
    /// knowing the concrete rules engine. The default returns
    /// [`VariantMetadata::default`] (a plain 8x8, hand-less board); factories with
    /// a different geometry or a hand should override it.
    fn metadata(&self) -> VariantMetadata {
        VariantMetadata::default()
    }

    /// Creates a new game of this variant configured by `options`.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::InvalidActionPayload`] (or another suitable case)
    /// if `options` cannot be interpreted by the variant.
    fn new_game(&self, options: &VariantOptions) -> Result<Box<dyn GameSession>, GameError>;
}

impl std::fmt::Debug for dyn VariantFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VariantFactory")
            .field("id", &self.id())
            .field("display_name", &self.display_name())
            .finish()
    }
}

/// A registry mapping variant ids to their factories.
///
/// The server registers every supported variant once at startup, then creates
/// games by id via [`new_game`](VariantRegistry::new_game).
#[derive(Default)]
pub struct VariantRegistry {
    factories: HashMap<&'static str, Arc<dyn VariantFactory>>,
}

impl VariantRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `factory`, returning any factory previously registered under
    /// the same id.
    pub fn register(
        &mut self,
        factory: Arc<dyn VariantFactory>,
    ) -> Option<Arc<dyn VariantFactory>> {
        self.factories.insert(factory.id(), factory)
    }

    /// Looks up the factory registered under `id`, if any.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn VariantFactory>> {
        self.factories.get(id).cloned()
    }

    /// Returns the ids of all registered variants, in unspecified order.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.factories.keys().copied().collect()
    }

    /// Creates a new game of the variant registered under `id`.
    ///
    /// # Errors
    ///
    /// Returns [`GameError::UnknownVariant`] if no variant is registered under
    /// `id`, or whatever error the variant's factory returns.
    pub fn new_game(
        &self,
        id: &str,
        options: &VariantOptions,
    ) -> Result<Box<dyn GameSession>, GameError> {
        let factory = self
            .get(id)
            .ok_or_else(|| GameError::UnknownVariant(id.to_owned()))?;
        factory.new_game(options)
    }
}

impl std::fmt::Debug for VariantRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Sort ids so the debug output is deterministic regardless of the
        // underlying hash map ordering.
        let mut ids = self.ids();
        ids.sort_unstable();
        f.debug_struct("VariantRegistry")
            .field("variants", &ids)
            .finish()
    }
}
