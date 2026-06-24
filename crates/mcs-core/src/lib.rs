//! # mcs-core
//!
//! The variant-agnostic core of the Modular Chess Server (MCS).
//!
//! This crate defines the abstractions that every chess variant implements so
//! that the rest of the server (storage, matchmaking, transport) can treat all
//! variants uniformly. The abstraction is deliberately general enough to cover
//! both:
//!
//! - **perfect-information** variants such as standard chess, where a turn is a
//!   single move and both players observe the full board; and
//! - **imperfect-information** variants such as Reconnaissance Blind Chess,
//!   where a turn includes a "sense" action and each player observes only a
//!   partial, private view of the position.
//!
//! ## Design
//!
//! The center of the crate is the object-safe [`GameSession`] trait. Each
//! variant keeps its own strong, internal types but exposes them across the
//! trait boundary as type-erased, serde-serializable payloads — [`Action`],
//! [`PlayerView`], and [`Event`] — so that the core machinery and the wire
//! layer can handle every variant without knowing its concrete types. See the
//! [`GameSession`] documentation for how perfect- and imperfect-information
//! variants are both expressed through the same methods.
//!
//! Variants are instantiated through a [`VariantFactory`], and the server holds
//! them in a [`VariantRegistry`].
//!
//! This crate has no dependency on HTTP, storage, or async runtimes; it is pure
//! game logic and data definitions.
#![doc(html_root_url = "https://docs.rs/mcs-core")]

mod color;
mod error;
mod payload;
mod registry;
mod session;
mod status;

pub use color::Color;
pub use error::GameError;
pub use payload::{Action, Event, PlayerView};
pub use registry::{VariantFactory, VariantOptions, VariantRegistry};
pub use session::GameSession;
pub use status::{ActionEffect, EndReason, GameStatus, Outcome};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde::{Deserialize, Serialize};

    use super::*;

    // -------------------------------------------------------------------------
    // A trivial toy variant used to exercise the abstraction end to end.
    //
    // Rules: whoever acts first wins. White moves first; the only legal action
    // is `ToyAction::Play`. Applying it finishes the game with the actor as the
    // winner.
    // -------------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    enum ToyAction {
        Play,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct ToyView {
        to_move: Color,
        finished: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    enum ToyEvent {
        Won { winner: Color },
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct ToyOptions {
        // A no-op option just to exercise typed options round-tripping.
        #[serde(default)]
        label: Option<String>,
    }

    #[derive(Debug)]
    struct ToyGame {
        status: GameStatus,
    }

    impl GameSession for ToyGame {
        fn variant_id(&self) -> &'static str {
            "toy"
        }

        fn to_move(&self) -> Color {
            Color::White
        }

        fn status(&self) -> GameStatus {
            self.status.clone()
        }

        fn legal_actions(&self, player: Color) -> Vec<Action> {
            if self.status.is_finished() || player != self.to_move() {
                return Vec::new();
            }
            vec![Action::from_typed(&ToyAction::Play).expect("serializable")]
        }

        fn apply(&mut self, player: Color, action: &Action) -> Result<ActionEffect, GameError> {
            if self.status.is_finished() {
                return Err(GameError::Finished);
            }
            if player != self.to_move() {
                return Err(GameError::NotYourTurn);
            }
            let action: ToyAction = action
                .to_typed()
                .map_err(|e| GameError::InvalidActionPayload(e.to_string()))?;
            match action {
                ToyAction::Play => {
                    let outcome = Outcome::win(player, EndReason::Other("first to act".into()));
                    self.status = GameStatus::Finished(outcome);
                    let event = Event::from_typed(&ToyEvent::Won { winner: player })?;
                    Ok(ActionEffect {
                        status: self.status.clone(),
                        events: vec![event],
                    })
                }
            }
        }

        fn view_for(&self, player: Color) -> PlayerView {
            let _ = player; // perfect-information toy: everyone sees the same view
            PlayerView::from_typed(&ToyView {
                to_move: self.to_move(),
                finished: self.status.is_finished(),
            })
            .expect("serializable")
        }

        fn spectator_view(&self) -> PlayerView {
            self.view_for(Color::White)
        }

        fn outcome(&self) -> Option<Outcome> {
            match &self.status {
                GameStatus::Finished(o) => Some(o.clone()),
                GameStatus::Ongoing => None,
            }
        }
    }

    #[derive(Debug)]
    struct ToyFactory;

    impl VariantFactory for ToyFactory {
        fn id(&self) -> &'static str {
            "toy"
        }

        fn display_name(&self) -> &str {
            "Toy Variant"
        }

        fn new_game(&self, options: &VariantOptions) -> Result<Box<dyn GameSession>, GameError> {
            // Decode options to prove the typed path works, even though we
            // ignore them. A null value (the default) means "no options", so we
            // treat it as the default rather than a decode error.
            let _opts: ToyOptions = if options.as_value().is_null() {
                ToyOptions::default()
            } else {
                options.to_typed()?
            };
            Ok(Box::new(ToyGame {
                status: GameStatus::Ongoing,
            }))
        }
    }

    // -------------------------------------------------------------------------
    // Color
    // -------------------------------------------------------------------------

    #[test]
    fn color_opposite_and_display() {
        assert_eq!(Color::White.opposite(), Color::Black);
        assert_eq!(Color::Black.opposite(), Color::White);
        assert_eq!(Color::White.to_string(), "white");
        assert_eq!(Color::Black.to_string(), "black");
    }

    // -------------------------------------------------------------------------
    // serde round-trips
    // -------------------------------------------------------------------------

    #[test]
    fn action_payload_round_trips() {
        let action = Action::from_typed(&ToyAction::Play).unwrap();
        let decoded: ToyAction = action.to_typed().unwrap();
        assert_eq!(decoded, ToyAction::Play);

        // The newtype itself round-trips through JSON unchanged.
        let json = serde_json::to_string(&action).unwrap();
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(action, back);
    }

    #[test]
    fn player_view_round_trips() {
        let view = ToyView {
            to_move: Color::Black,
            finished: true,
        };
        let payload = PlayerView::from_typed(&view).unwrap();
        let json = serde_json::to_string(&payload).unwrap();
        let back: PlayerView = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        assert_eq!(back.to_typed::<ToyView>().unwrap(), view);
    }

    #[test]
    fn outcome_and_status_round_trip() {
        let outcome = Outcome::win(Color::White, EndReason::Checkmate);
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(serde_json::from_str::<Outcome>(&json).unwrap(), outcome);

        let draw = Outcome::draw(EndReason::Stalemate);
        assert!(draw.winner.is_none());

        let status = GameStatus::Finished(outcome);
        assert!(status.is_finished());
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<GameStatus>(&json).unwrap(), status);
        assert!(!GameStatus::Ongoing.is_finished());
    }

    #[test]
    fn end_reason_other_round_trips() {
        let reason = EndReason::Other("forfeit".into());
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(serde_json::from_str::<EndReason>(&json).unwrap(), reason);
    }

    // -------------------------------------------------------------------------
    // to_typed / from_typed happy and error paths
    // -------------------------------------------------------------------------

    #[test]
    fn to_typed_reports_serialization_error_on_mismatch() {
        // A view is shaped like ToyView, not like ToyAction; decoding it as a
        // ToyAction must fail with a Serialization error.
        let payload = PlayerView::from_typed(&ToyView {
            to_move: Color::White,
            finished: false,
        })
        .unwrap();
        let err = payload.to_typed::<ToyAction>().unwrap_err();
        assert!(matches!(err, GameError::Serialization(_)));
    }

    // -------------------------------------------------------------------------
    // Registry
    // -------------------------------------------------------------------------

    #[test]
    fn registry_register_get_and_ids() {
        let mut registry = VariantRegistry::new();
        assert!(registry.get("toy").is_none());

        let prev = registry.register(Arc::new(ToyFactory));
        assert!(prev.is_none());

        let factory = registry.get("toy").expect("registered");
        assert_eq!(factory.id(), "toy");
        assert_eq!(factory.display_name(), "Toy Variant");
        assert_eq!(registry.ids(), vec!["toy"]);

        // Registering the same id again returns the previous factory.
        let prev = registry.register(Arc::new(ToyFactory));
        assert!(prev.is_some());
    }

    #[test]
    fn registry_new_game_unknown_variant() {
        let registry = VariantRegistry::new();
        let err = registry
            .new_game("does-not-exist", &VariantOptions::default())
            .unwrap_err();
        assert_eq!(err, GameError::UnknownVariant("does-not-exist".to_owned()));
    }

    #[test]
    fn registry_default_and_debug() {
        let registry = VariantRegistry::default();
        // Debug should not panic and should mention the type.
        assert!(format!("{registry:?}").contains("VariantRegistry"));
    }

    // -------------------------------------------------------------------------
    // Object safety: drive a game purely through `Box<dyn GameSession>`.
    // -------------------------------------------------------------------------

    #[test]
    fn box_dyn_game_session_is_usable() {
        let mut registry = VariantRegistry::new();
        registry.register(Arc::new(ToyFactory));

        let mut game: Box<dyn GameSession> = registry
            .new_game("toy", &VariantOptions::default())
            .expect("toy game");

        assert_eq!(game.variant_id(), "toy");
        assert_eq!(game.to_move(), Color::White);
        assert_eq!(game.status(), GameStatus::Ongoing);
        assert!(game.outcome().is_none());

        // Both player views and the spectator view are obtainable.
        let _ = game.view_for(Color::White);
        let _ = game.view_for(Color::Black);
        let _ = game.spectator_view();

        // The only legal action is to play.
        let actions = game.legal_actions(Color::White);
        assert_eq!(actions.len(), 1);
        assert!(game.legal_actions(Color::Black).is_empty());

        // Acting out of turn is rejected.
        let out_of_turn = game.apply(Color::Black, &actions[0]).unwrap_err();
        assert_eq!(out_of_turn, GameError::NotYourTurn);

        // White plays and wins.
        let effect = game.apply(Color::White, &actions[0]).expect("legal");
        assert!(effect.status.is_finished());
        assert_eq!(effect.events.len(), 1);
        assert_eq!(game.outcome().map(|o| o.winner), Some(Some(Color::White)),);

        // Acting after the game is over is rejected.
        let after = game.apply(Color::White, &actions[0]).unwrap_err();
        assert_eq!(after, GameError::Finished);
    }

    #[test]
    fn typed_options_round_trip() {
        let opts = VariantOptions::from_typed(&ToyOptions {
            label: Some("rated".into()),
        })
        .unwrap();
        let back: ToyOptions = opts.to_typed().unwrap();
        assert_eq!(back.label.as_deref(), Some("rated"));
    }
}
