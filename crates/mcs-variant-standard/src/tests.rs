//! End-to-end tests for the standard-chess variant.
//!
//! These drive games through the [`GameSession`] boundary using the type-erased
//! payloads, mirroring how the server uses the variant.

use mcs_core::{
    Action, Color, EndReason, GameError, GameSession, GameStatus, Outcome, PlayerView,
    VariantFactory, VariantOptions, VariantRegistry,
};

use crate::factory::{register, StandardVariant};
use crate::game::StandardGame;
use crate::wire::{StandardAction, StandardEvent, StandardView};
use crate::STANDARD_VARIANT_ID;

/// Helper: wrap a UCI string into a move action payload.
fn move_action(uci: &str) -> Action {
    Action::from_typed(&StandardAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// Helper: play a sequence of UCI moves, asserting each one succeeds, with the
/// supplied colors alternating from White.
fn play_moves(game: &mut StandardGame, ucis: &[&str]) {
    let mut player = Color::White;
    for uci in ucis {
        game.apply(player, &move_action(uci))
            .unwrap_or_else(|e| panic!("move {uci} should be legal: {e}"));
        player = player.opposite();
    }
}

#[test]
fn fools_mate_is_a_black_checkmate_win() {
    let mut game = StandardGame::new();
    // Fool's mate: 1. f3 e5 2. g4 Qh4#
    play_moves(&mut game, &["f2f3", "e7e5", "g2g4", "d8h4"]);

    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
    assert!(game.status().is_finished());
}

#[test]
fn scholars_mate_is_a_white_checkmate_win() {
    let mut game = StandardGame::new();
    // Scholar's mate: 1. e4 e5 2. Bc4 Nc6 3. Qh5 Nf6?? 4. Qxf7#
    play_moves(
        &mut game,
        &["e2e4", "e7e5", "f1c4", "b8c6", "d1h5", "g8f6", "h5f7"],
    );

    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::White, EndReason::Checkmate))
    );
}

#[test]
fn checkmate_emits_move_and_game_ended_events() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["f2f3", "e7e5", "g2g4"]);

    // The mating move should yield both a MovePlayed and a GameEnded event.
    let effect = game.apply(Color::Black, &move_action("d8h4")).unwrap();
    assert!(effect.status.is_finished());
    assert_eq!(effect.events.len(), 2);

    let mate: StandardEvent = effect.events[0].to_typed().unwrap();
    match mate {
        StandardEvent::MovePlayed { uci, san, .. } => {
            assert_eq!(uci, "d8h4");
            assert_eq!(san, "Qh4#");
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
    let ended: StandardEvent = effect.events[1].to_typed().unwrap();
    assert!(matches!(ended, StandardEvent::GameEnded { .. }));
}

#[test]
fn illegal_move_is_rejected() {
    let mut game = StandardGame::new();
    // e2e5 is not a legal first move (pawn cannot jump three squares).
    let err = game.apply(Color::White, &move_action("e2e5")).unwrap_err();
    assert_eq!(err, GameError::IllegalAction);
    // The game is untouched and still ongoing.
    assert_eq!(game.status(), GameStatus::Ongoing);
    assert_eq!(game.to_move(), Color::White);
}

#[test]
fn malformed_uci_is_an_invalid_payload() {
    let mut game = StandardGame::new();
    let err = game
        .apply(Color::White, &move_action("not-a-move"))
        .unwrap_err();
    assert!(matches!(err, GameError::InvalidActionPayload(_)));
}

#[test]
fn out_of_turn_move_is_rejected() {
    let mut game = StandardGame::new();
    // Black tries to move first.
    let err = game.apply(Color::Black, &move_action("e7e5")).unwrap_err();
    assert_eq!(err, GameError::NotYourTurn);
}

#[test]
fn acting_after_finish_is_rejected() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["f2f3", "e7e5", "g2g4", "d8h4"]);
    // The game is over; any further action fails with Finished.
    let err = game.apply(Color::White, &move_action("a2a3")).unwrap_err();
    assert_eq!(err, GameError::Finished);
}

#[test]
fn resignation_hands_the_win_to_the_opponent() {
    let mut game = StandardGame::new();
    let resign = Action::from_typed(&StandardAction::Resign).unwrap();

    let effect = game.apply(Color::White, &resign).unwrap();
    assert!(effect.status.is_finished());
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Resignation))
    );
}

#[test]
fn a_player_may_resign_on_the_opponents_turn() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["e2e4"]); // Now it is Black's turn.
                                      // White resigns even though it is not their move.
    let resign = Action::from_typed(&StandardAction::Resign).unwrap();
    let effect = game.apply(Color::White, &resign).unwrap();
    assert!(effect.status.is_finished());
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Resignation))
    );
}

#[test]
fn draw_offer_and_accept_ends_in_a_draw() {
    let mut game = StandardGame::new();
    let offer = Action::from_typed(&StandardAction::OfferDraw).unwrap();
    let accept = Action::from_typed(&StandardAction::AcceptDraw).unwrap();

    // White offers a draw.
    let effect = game.apply(Color::White, &offer).unwrap();
    assert!(!effect.status.is_finished());
    let event: StandardEvent = effect.events[0].to_typed().unwrap();
    assert!(matches!(event, StandardEvent::DrawOffered { by } if by == Color::White));

    // Black accepts.
    let effect = game.apply(Color::Black, &accept).unwrap();
    assert!(effect.status.is_finished());
    assert_eq!(
        game.outcome(),
        Some(Outcome::draw(EndReason::DrawAgreement))
    );
}

#[test]
fn draw_offer_can_be_declined_and_game_continues() {
    let mut game = StandardGame::new();
    let offer = Action::from_typed(&StandardAction::OfferDraw).unwrap();
    let decline = Action::from_typed(&StandardAction::DeclineDraw).unwrap();

    game.apply(Color::White, &offer).unwrap();
    let effect = game.apply(Color::Black, &decline).unwrap();
    assert!(!effect.status.is_finished());
    assert_eq!(game.status(), GameStatus::Ongoing);

    // The offer is cleared, so the view shows no pending offer.
    let view: StandardView = game.spectator_view().to_typed().unwrap();
    assert_eq!(view.draw_offer, None);
}

#[test]
fn accepting_a_nonexistent_offer_is_illegal() {
    let mut game = StandardGame::new();
    let accept = Action::from_typed(&StandardAction::AcceptDraw).unwrap();
    let err = game.apply(Color::Black, &accept).unwrap_err();
    assert_eq!(err, GameError::IllegalAction);
}

#[test]
fn a_move_clears_a_pending_draw_offer() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["e2e4"]);
    // Black offers a draw, then White ignores it by moving.
    game.apply(
        Color::Black,
        &Action::from_typed(&StandardAction::OfferDraw).unwrap(),
    )
    .unwrap();
    play_moves_from(&mut game, Color::Black, &["e7e5"]);
    // The draw offer should be gone now.
    let view: StandardView = game.spectator_view().to_typed().unwrap();
    assert_eq!(view.draw_offer, None);
}

/// Variant of [`play_moves`] starting from an arbitrary side.
fn play_moves_from(game: &mut StandardGame, first: Color, ucis: &[&str]) {
    let mut player = first.opposite(); // the side that just moved was `first`
    for uci in ucis {
        game.apply(player.opposite(), &move_action(uci))
            .unwrap_or_else(|e| panic!("move {uci} should be legal: {e}"));
        player = player.opposite();
    }
}

#[test]
fn stalemate_is_a_draw() {
    // A classic stalemate sequence reaching the position where Black is to move
    // with no legal moves and is not in check.
    let mut game = StandardGame::new();
    play_moves(
        &mut game,
        &[
            "e2e3", "a7a5", "d1h5", "a8a6", "h5a5", "h7h5", "a5c7", "a6h6", "h2h4", "f7f6", "c7d7",
            "e8f7", "d7b7", "d8d3", "b7b8", "d3h7", "b8c8", "f7g6", "c8e6",
        ],
    );

    assert_eq!(
        game.outcome(),
        Some(Outcome::draw(EndReason::Stalemate)),
        "expected stalemate, got {:?} (fen: {})",
        game.outcome(),
        match game.spectator_view().to_typed::<StandardView>() {
            Ok(v) => v.fen,
            Err(_) => "<unavailable>".to_owned(),
        }
    );
}

#[test]
fn view_is_perfect_information_and_round_trips() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["e2e4"]);

    let white = game.view_for(Color::White);
    let black = game.view_for(Color::Black);
    let spectator = game.spectator_view();

    // Perfect information: all three views are identical.
    assert_eq!(white, black);
    assert_eq!(white, spectator);

    let view: StandardView = white.to_typed().unwrap();
    assert_eq!(view.side_to_move, Color::Black);
    assert!(!view.check);
    assert_eq!(view.status, GameStatus::Ongoing);
    assert!(view.legal_moves_uci.contains(&"e7e5".to_owned()));

    // The PlayerView newtype round-trips through JSON unchanged.
    let json = serde_json::to_string(&white).unwrap();
    let back: PlayerView = serde_json::from_str(&json).unwrap();
    assert_eq!(white, back);
    assert_eq!(back.to_typed::<StandardView>().unwrap(), view);
}

#[test]
fn view_reports_check() {
    let mut game = StandardGame::new();
    // 1. e4 e5 2. Bc4 Nc6 3. Qh5 ... now Black faces threats but is not in
    // check; deliver a check instead via a fast line.
    play_moves(&mut game, &["e2e4", "f7f5", "d1h5"]); // Qh5+ checks the Black king.
    let view: StandardView = game.spectator_view().to_typed().unwrap();
    assert!(view.check, "Black should be in check after Qh5+");
}

#[test]
fn legal_actions_for_side_to_move_include_moves_and_meta() {
    let game = StandardGame::new();
    let white_actions = game.legal_actions(Color::White);

    // White (to move) gets the 20 opening moves plus Resign and OfferDraw.
    let mut moves = 0;
    let mut resign = 0;
    let mut offer = 0;
    for action in &white_actions {
        let parsed: StandardAction = action.to_typed().unwrap();
        match parsed {
            StandardAction::Move { .. } => moves += 1,
            StandardAction::Resign => resign += 1,
            StandardAction::OfferDraw => offer += 1,
            other => panic!("unexpected action {other:?}"),
        }
    }
    assert_eq!(moves, 20);
    assert_eq!(resign, 1);
    assert_eq!(offer, 1);

    // Black (not to move) may still resign or offer a draw, but has no moves.
    let black_actions = game.legal_actions(Color::Black);
    for action in &black_actions {
        let parsed: StandardAction = action.to_typed().unwrap();
        assert!(
            !matches!(parsed, StandardAction::Move { .. }),
            "the non-moving side should have no moves"
        );
    }
    assert!(black_actions.iter().any(|a| matches!(
        a.to_typed::<StandardAction>().unwrap(),
        StandardAction::Resign
    )));
}

#[test]
fn legal_actions_are_empty_once_finished() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["f2f3", "e7e5", "g2g4", "d8h4"]);
    assert!(game.legal_actions(Color::White).is_empty());
    assert!(game.legal_actions(Color::Black).is_empty());
}

#[test]
fn action_payload_round_trips() {
    for action in [
        StandardAction::Move {
            uci: "e2e4".to_owned(),
        },
        StandardAction::Resign,
        StandardAction::OfferDraw,
        StandardAction::AcceptDraw,
        StandardAction::DeclineDraw,
    ] {
        let payload = Action::from_typed(&action).unwrap();
        let json = serde_json::to_string(&payload).unwrap();
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        assert_eq!(back.to_typed::<StandardAction>().unwrap(), action);
    }
}

#[test]
fn move_action_json_shape_is_tagged() {
    let payload = move_action("e2e4");
    // The on-the-wire JSON is the documented `{ "type": "move", "uci": ... }`.
    assert_eq!(
        payload.as_value(),
        &serde_json::json!({ "type": "move", "uci": "e2e4" })
    );
}

#[test]
fn factory_metadata_is_correct() {
    let factory = StandardVariant;
    assert_eq!(factory.id(), STANDARD_VARIANT_ID);
    assert_eq!(factory.id(), "standard");
    assert_eq!(factory.display_name(), "Standard Chess");
}

#[test]
fn factory_ignores_options() {
    let factory = StandardVariant;
    // A non-null options value is accepted rather than rejected.
    let opts = VariantOptions::new(serde_json::json!({ "rated": true }));
    let game = factory.new_game(&opts).expect("options are ignored");
    assert_eq!(game.variant_id(), "standard");
    assert_eq!(game.to_move(), Color::White);
}

#[test]
fn registry_integration_drives_a_move_through_box_dyn() {
    let mut registry = VariantRegistry::new();
    register(&mut registry);

    assert!(registry.ids().contains(&STANDARD_VARIANT_ID));

    let mut game: Box<dyn GameSession> = registry
        .new_game(STANDARD_VARIANT_ID, &VariantOptions::default())
        .expect("standard variant registered");

    assert_eq!(game.variant_id(), "standard");
    assert_eq!(game.to_move(), Color::White);
    assert!(game.outcome().is_none());

    // Drive a move through the trait object.
    let effect = game.apply(Color::White, &move_action("e2e4")).unwrap();
    assert!(!effect.status.is_finished());
    assert_eq!(game.to_move(), Color::Black);

    // The view obtained through the boxed session reflects the move.
    let view: StandardView = game.spectator_view().to_typed().unwrap();
    assert_eq!(view.side_to_move, Color::Black);
    assert!(view.fen.starts_with("rnbqkbnr/pppppppp/8/8/4P3"));
}
