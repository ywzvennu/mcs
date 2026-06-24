//! End-to-end tests for the Reconnaissance Blind Chess variant.
//!
//! These drive games through the [`GameSession`] boundary using the type-erased
//! payloads, mirroring how the server uses the variant. The emphasis is on the
//! two things that make RBC the motivating imperfect-information variant:
//!
//! 1. **phase and turn enforcement** — a player must sense before moving, may
//!    not sense twice, and may not act out of turn; and
//! 2. **the hidden-information guarantee** — `view_for` never leaks the
//!    opponent's piece locations, and the spectator view is redacted while the
//!    game is ongoing.

use mcs_core::{
    Action, Color, EndReason, GameError, GameSession, GameStatus, Outcome, PlayerView,
    VariantFactory, VariantOptions, VariantRegistry,
};

use crate::factory::{register, RbcVariant};
use crate::game::RbcGame;
use crate::wire::{RbcAction, RbcEvent, RbcFinalView, RbcSpectatorView, RbcView, TurnPhase};
use crate::RBC_VARIANT_ID;

/// Helper: a sense action payload centred on `square` (e.g. `"e4"`).
fn sense_action(square: &str) -> Action {
    Action::from_typed(&RbcAction::Sense {
        square: square.to_owned(),
    })
    .expect("serializable")
}

/// Helper: a move action payload from a UCI string.
fn move_action(uci: &str) -> Action {
    Action::from_typed(&RbcAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// Helper: decode a player's view into the strongly typed [`RbcView`].
fn player_view(game: &RbcGame, player: Color) -> RbcView {
    game.view_for(player).to_typed().expect("rbc view decodes")
}

/// Helper: perform a full turn (sense then move) for `player`, asserting both
/// steps succeed.
fn sense_then_move(game: &mut RbcGame, player: Color, sense_sq: &str, uci: &str) {
    game.apply(player, &sense_action(sense_sq))
        .unwrap_or_else(|e| panic!("sense {sense_sq} should be legal: {e}"));
    game.apply(player, &move_action(uci))
        .unwrap_or_else(|e| panic!("move {uci} should be legal: {e}"));
}

// -----------------------------------------------------------------------------
// Registration & factory
// -----------------------------------------------------------------------------

#[test]
fn factory_identifies_the_variant() {
    let factory = RbcVariant;
    assert_eq!(factory.id(), "rbc");
    assert_eq!(factory.display_name(), "Reconnaissance Blind Chess");
}

#[test]
fn registers_and_creates_a_game() {
    let mut registry = VariantRegistry::new();
    register(&mut registry);

    let game = registry
        .new_game(RBC_VARIANT_ID, &VariantOptions::default())
        .expect("rbc registered");
    assert_eq!(game.variant_id(), "rbc");
    assert_eq!(game.to_move(), Color::White);
    assert_eq!(game.status(), GameStatus::Ongoing);
    assert!(game.outcome().is_none());
}

// -----------------------------------------------------------------------------
// Phase & turn enforcement
// -----------------------------------------------------------------------------

#[test]
fn fresh_turn_starts_in_the_sense_phase() {
    let game = RbcGame::new();
    let view = player_view(&game, Color::White);
    assert_eq!(view.phase, Some(TurnPhase::Sense));
    // In the sense phase, legal actions are senses (plus resign), not moves.
    let actions = game.legal_actions(Color::White);
    assert!(!actions.is_empty());
    let decoded: Vec<RbcAction> = actions.iter().map(|a| a.to_typed().unwrap()).collect();
    assert!(decoded
        .iter()
        .all(|a| matches!(a, RbcAction::Sense { .. } | RbcAction::Resign)));
    assert!(decoded.iter().any(|a| matches!(a, RbcAction::Sense { .. })));
}

#[test]
fn cannot_move_before_sensing() {
    let mut game = RbcGame::new();
    // Attempting a move while still owing a sense is rejected as illegal.
    let err = game.apply(Color::White, &move_action("e2e4")).unwrap_err();
    assert_eq!(err, GameError::IllegalAction);
}

#[test]
fn sensing_advances_to_the_move_phase() {
    let mut game = RbcGame::new();
    game.apply(Color::White, &sense_action("b7"))
        .expect("sense");
    let view = player_view(&game, Color::White);
    assert_eq!(view.phase, Some(TurnPhase::Move));

    // Now legal actions are moves (plus pass and resign), not senses.
    let decoded: Vec<RbcAction> = game
        .legal_actions(Color::White)
        .iter()
        .map(|a| a.to_typed().unwrap())
        .collect();
    assert!(decoded.iter().any(|a| matches!(a, RbcAction::Move { .. })));
    assert!(decoded.iter().any(|a| matches!(a, RbcAction::Pass)));
    assert!(decoded
        .iter()
        .all(|a| !matches!(a, RbcAction::Sense { .. })));
}

#[test]
fn cannot_sense_twice_in_one_turn() {
    let mut game = RbcGame::new();
    game.apply(Color::White, &sense_action("e4"))
        .expect("sense");
    // A second sense in the same turn is out of phase.
    let err = game.apply(Color::White, &sense_action("d4")).unwrap_err();
    assert_eq!(err, GameError::IllegalAction);
}

#[test]
fn cannot_act_out_of_turn() {
    let mut game = RbcGame::new();
    // White is to move; Black sensing or moving is rejected as not-their-turn.
    assert_eq!(
        game.apply(Color::Black, &sense_action("e4")).unwrap_err(),
        GameError::NotYourTurn
    );
    assert_eq!(
        game.apply(Color::Black, &move_action("e7e5")).unwrap_err(),
        GameError::NotYourTurn
    );
    // The off-turn player gets no legal actions.
    assert!(game.legal_actions(Color::Black).is_empty());
}

#[test]
fn turn_passes_to_opponent_after_a_move() {
    let mut game = RbcGame::new();
    sense_then_move(&mut game, Color::White, "e4", "e2e4");
    assert_eq!(game.to_move(), Color::Black);
    // Black now owes a sense.
    let view = player_view(&game, Color::Black);
    assert_eq!(view.phase, Some(TurnPhase::Sense));
}

#[test]
fn passing_the_move_also_advances_the_turn() {
    let mut game = RbcGame::new();
    game.apply(Color::White, &sense_action("e4"))
        .expect("sense");
    let pass = Action::from_typed(&RbcAction::Pass).unwrap();
    game.apply(Color::White, &pass).expect("pass is legal");
    assert_eq!(game.to_move(), Color::Black);
}

// -----------------------------------------------------------------------------
// Hidden-information guarantee — the core of the variant abstraction.
// -----------------------------------------------------------------------------

#[test]
fn view_for_white_never_reveals_black_piece_positions() {
    let game = RbcGame::new();
    let view = player_view(&game, Color::White);

    // The one-sided FEN must contain only white (upper-case) pieces; any
    // lower-case letter would be a black piece leaking through.
    assert!(
        !view.own_fen.chars().any(|c| c.is_ascii_lowercase()),
        "white's view leaked a black piece: {}",
        view.own_fen
    );
    // And it must contain white's pieces.
    assert!(view.own_fen.chars().any(|c| c.is_ascii_uppercase()));

    // The raw serialized bytes must not contain the standard black back rank or
    // pawn rank in any recognizable form beyond the redacted own_fen.
    let raw = serde_json::to_string(&game.view_for(Color::White)).unwrap();
    assert!(!raw.contains("rnbqkbnr"), "black back rank leaked: {raw}");
    assert!(view.last_sense.is_none());
}

#[test]
fn view_for_black_never_reveals_white_piece_positions() {
    let game = RbcGame::new();
    let view = player_view(&game, Color::Black);

    // Black's one-sided FEN must contain only black (lower-case) pieces.
    assert!(
        !view.own_fen.chars().any(|c| c.is_ascii_uppercase()),
        "black's view leaked a white piece: {}",
        view.own_fen
    );
    assert!(view.own_fen.chars().any(|c| c.is_ascii_lowercase()));

    let raw = serde_json::to_string(&game.view_for(Color::Black)).unwrap();
    assert!(!raw.contains("RNBQKBNR"), "white back rank leaked: {raw}");
}

#[test]
fn sensing_reveals_enemy_pieces_only_to_the_sensing_player() {
    let mut game = RbcGame::new();
    // White senses b7, which sits among black's pieces; the result is private.
    game.apply(Color::White, &sense_action("b7"))
        .expect("sense");

    // White's own view now carries the sense result, which includes black
    // pieces (this is the sanctioned channel).
    let white = player_view(&game, Color::White);
    let sense = white.last_sense.expect("white has a sense result");
    assert_eq!(sense.center, "b7");
    assert!(
        sense.squares.iter().any(|s| s
            .piece
            .as_deref()
            .is_some_and(|p| p.chars().all(|c| c.is_ascii_lowercase()))),
        "white's b7 sense should reveal some black pieces"
    );

    // Black's view must NOT carry white's sense result — it is private to white.
    let black = player_view(&game, Color::Black);
    assert!(
        black.last_sense.is_none(),
        "white's private sense leaked into black's view"
    );
    // And black's own view still hides white's pieces.
    assert!(!black.own_fen.chars().any(|c| c.is_ascii_uppercase()));
}

#[test]
fn spectator_view_is_redacted_while_ongoing() {
    let mut game = RbcGame::new();
    game.apply(Color::White, &sense_action("e4"))
        .expect("sense");

    let spectator: RbcSpectatorView = game
        .spectator_view()
        .to_typed()
        .expect("ongoing spectator view decodes to the redacted shape");
    assert_eq!(spectator.side_to_move, Color::White);
    assert_eq!(spectator.phase, TurnPhase::Move);
    assert_eq!(spectator.status, GameStatus::Ongoing);

    // No piece information whatsoever may appear in the serialized spectator
    // view while the game is ongoing.
    let raw = serde_json::to_string(&game.spectator_view()).unwrap();
    assert!(!raw.contains("fen"), "spectator view leaked a board: {raw}");
    assert!(!raw.contains("rnbqkbnr"));
    assert!(!raw.contains("RNBQKBNR"));
}

// -----------------------------------------------------------------------------
// Move outcomes & events
// -----------------------------------------------------------------------------

#[test]
fn sense_emits_a_redacted_sensed_event() {
    let mut game = RbcGame::new();
    let effect = game
        .apply(Color::White, &sense_action("e4"))
        .expect("sense");
    assert_eq!(effect.events.len(), 1);
    let event: RbcEvent = effect.events[0].to_typed().unwrap();
    // The broadcast event reveals only that white sensed, not what they saw.
    assert_eq!(event, RbcEvent::Sensed { by: Color::White });
}

#[test]
fn move_event_announces_capture_square_only() {
    // Set up a position where white can immediately capture by sensing then
    // moving. Drive a couple of normal turns and then a capturing move.
    let mut game = RbcGame::new();
    // 1. white e2e4
    sense_then_move(&mut game, Color::White, "e4", "e2e4");
    // 1... black d7d5
    sense_then_move(&mut game, Color::Black, "d5", "d7d5");
    // 2. white exd5 — a capture on d5.
    game.apply(Color::White, &sense_action("d5"))
        .expect("sense");
    let effect = game
        .apply(Color::White, &move_action("e4d5"))
        .expect("capture");
    let move_event = effect
        .events
        .iter()
        .find_map(|e| e.to_typed::<RbcEvent>().ok())
        .filter(|e| matches!(e, RbcEvent::MovePlayed { .. }))
        .expect("a MovePlayed event");
    match move_event {
        RbcEvent::MovePlayed {
            by,
            captured,
            capture_square,
        } => {
            assert_eq!(by, Color::White);
            assert!(captured);
            assert_eq!(capture_square.as_deref(), Some("d5"));
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------
// Terminal state: king capture maps to an Outcome.
// -----------------------------------------------------------------------------

#[test]
fn resignation_finishes_the_game_with_an_outcome() {
    let mut game = RbcGame::new();
    let resign = Action::from_typed(&RbcAction::Resign).unwrap();
    let effect = game.apply(Color::White, &resign).expect("resign is legal");
    assert!(effect.status.is_finished());

    // White resigning hands the win to black.
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Resignation))
    );

    // After the game ends, no further actions are legal and acting errors.
    assert!(game.legal_actions(Color::White).is_empty());
    let err = game.apply(Color::White, &resign).unwrap_err();
    assert_eq!(err, GameError::Finished);
}

#[test]
fn king_capture_maps_to_a_decisive_outcome() {
    // Drive a complete (if silly) game to a king capture purely through the
    // GameSession trait. Both sides bring out their queens; white then captures
    // black's king, which in RBC ends the game immediately.
    //
    // White: 1. Nc3, 2. e4 (opening lines), then a queen raid. We sense
    // harmlessly each turn since sensing is mandatory before moving.
    let mut game = RbcGame::new();
    // A line that walks the white queen to capture the black king on e8.
    // 1. e4 ; black passes its move each turn (still must sense) so the king
    // stays on e8 and pawns stay home enough for the queen's diagonal.
    sense_then_move(&mut game, Color::White, "a1", "e2e4"); // open queen diagonal
    sense_then_move(&mut game, Color::Black, "a8", "a7a6"); // harmless black move
    sense_then_move(&mut game, Color::White, "a1", "d1h5"); // Qh5
    sense_then_move(&mut game, Color::Black, "a8", "a6a5");
    sense_then_move(&mut game, Color::White, "a1", "h5e5"); // Qe5
    sense_then_move(&mut game, Color::Black, "a8", "a5a4");
    // Qxe7 — captures the black pawn on e7 (revised stop, no king yet)…
    sense_then_move(&mut game, Color::White, "a1", "e5e7");
    sense_then_move(&mut game, Color::Black, "a8", "a4a3");
    // …then Qxe8 captures the black king and ends the game.
    game.apply(Color::White, &sense_action("e8"))
        .expect("sense");
    let effect = game
        .apply(Color::White, &move_action("e7e8"))
        .expect("king capture");

    assert!(
        effect.status.is_finished(),
        "king capture should finish the game"
    );
    let outcome = game.outcome().expect("a finished game has an outcome");
    assert_eq!(outcome.winner, Some(Color::White));
    assert_eq!(outcome.reason, EndReason::Other("king_capture".to_owned()));

    // A GameEnded event accompanies the terminal move.
    assert!(effect
        .events
        .iter()
        .filter_map(|e| e.to_typed::<RbcEvent>().ok())
        .any(|e| matches!(e, RbcEvent::GameEnded { .. })));
}

#[test]
fn spectator_view_reveals_full_board_once_finished() {
    let mut game = RbcGame::new();
    let resign = Action::from_typed(&RbcAction::Resign).unwrap();
    game.apply(Color::White, &resign).expect("resign");

    // Now that the game is over, the spectator gets the full final board.
    let final_view: RbcFinalView = game
        .spectator_view()
        .to_typed()
        .expect("finished spectator view decodes to the full shape");
    assert!(final_view.status.is_finished());
    // The full FEN reveals both sides' pieces — there is nothing left to hide.
    assert!(final_view.fen.contains("rnbqkbnr"));
    assert!(final_view.fen.contains("RNBQKBNR"));
}

// -----------------------------------------------------------------------------
// Object safety: drive a game purely through `Box<dyn GameSession>`.
// -----------------------------------------------------------------------------

#[test]
fn box_dyn_game_session_is_usable() {
    let mut registry = VariantRegistry::new();
    register(&mut registry);
    let mut game: Box<dyn GameSession> = registry
        .new_game(RBC_VARIANT_ID, &VariantOptions::default())
        .expect("rbc game");

    assert_eq!(game.variant_id(), "rbc");
    assert_eq!(game.to_move(), Color::White);

    // All three view methods are obtainable through the trait object.
    let _: PlayerView = game.view_for(Color::White);
    let _: PlayerView = game.view_for(Color::Black);
    let _: PlayerView = game.spectator_view();

    // Sense, then move, through the boxed trait object.
    let senses = game.legal_actions(Color::White);
    assert!(!senses.is_empty());
    game.apply(Color::White, &sense_action("e4"))
        .expect("sense");
    let moves = game.legal_actions(Color::White);
    assert!(moves
        .iter()
        .any(|a| matches!(a.to_typed::<RbcAction>(), Ok(RbcAction::Move { .. }))));
    game.apply(Color::White, &move_action("e2e4"))
        .expect("move");
    assert_eq!(game.to_move(), Color::Black);
}
