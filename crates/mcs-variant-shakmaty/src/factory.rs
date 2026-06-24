//! The generic [`VariantFactory`] and registry wiring for the family.

use std::marker::PhantomData;
use std::sync::Arc;

use mcs_core::{GameError, GameSession, VariantFactory, VariantOptions, VariantRegistry};

use crate::game::ShakmatyGame;
use crate::spec::VariantSpec;
use crate::variants::{
    Antichess, Atomic, Chess960, Crazyhouse, Horde, KingOfTheHill, RacingKings, ThreeCheck,
};

/// A [`VariantFactory`] that creates [`ShakmatyGame`]s for the variant `S`.
///
/// One generic factory type serves the whole family; the variant is selected by
/// the [`VariantSpec`] type parameter. Construct one with [`ShakmatyVariant::new`]
/// (or [`Default`]) and register it, or use [`register_all`] to register them
/// all at once.
pub struct ShakmatyVariant<S: VariantSpec>(PhantomData<fn() -> S>);

impl<S: VariantSpec> ShakmatyVariant<S> {
    /// Creates the factory for variant `S`.
    #[must_use]
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<S: VariantSpec> Default for ShakmatyVariant<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: VariantSpec> std::fmt::Debug for ShakmatyVariant<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShakmatyVariant")
            .field("id", &S::ID)
            .finish()
    }
}

impl<S: VariantSpec> VariantFactory for ShakmatyVariant<S> {
    fn id(&self) -> &'static str {
        S::ID
    }

    fn display_name(&self) -> &str {
        S::DISPLAY_NAME
    }

    fn new_game(&self, options: &VariantOptions) -> Result<Box<dyn GameSession>, GameError> {
        Ok(Box::new(ShakmatyGame::<S>::new(options)?))
    }
}

/// Registers every shakmaty-family variant with `registry`.
///
/// Registers, by id: `atomic`, `antichess`, `crazyhouse`, `kingofthehill`,
/// `threecheck`, `racingkings`, `horde`, and `chess960`. Call this at server
/// startup to make them all available via
/// [`VariantRegistry::new_game`](mcs_core::VariantRegistry::new_game).
pub fn register_all(registry: &mut VariantRegistry) {
    registry.register(Arc::new(ShakmatyVariant::<Atomic>::new()));
    registry.register(Arc::new(ShakmatyVariant::<Antichess>::new()));
    registry.register(Arc::new(ShakmatyVariant::<Crazyhouse>::new()));
    registry.register(Arc::new(ShakmatyVariant::<KingOfTheHill>::new()));
    registry.register(Arc::new(ShakmatyVariant::<ThreeCheck>::new()));
    registry.register(Arc::new(ShakmatyVariant::<RacingKings>::new()));
    registry.register(Arc::new(ShakmatyVariant::<Horde>::new()));
    registry.register(Arc::new(ShakmatyVariant::<Chess960>::new()));
}
