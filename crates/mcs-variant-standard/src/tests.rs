//! End-to-end tests for the standard-chess and Chess960 variants.
//!
//! These drive games through the [`GameSession`] boundary using the type-erased
//! payloads, mirroring how the server uses the variant.

use mcs_core::{
    Action, Color, EndReason, GameError, GameSession, GameStatus, Outcome, PlayerView,
    VariantFactory, VariantOptions, VariantRegistry,
};

use crate::factory::{register, Chess960Variant, StandardVariant};
use crate::game::StandardGame;
use crate::wire::{StandardAction, StandardEvent, StandardView};
use crate::{CHESS960_VARIANT_ID, STANDARD_VARIANT_ID};

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

/// Helper: read back the typed view from a session.
fn view_of(game: &StandardGame) -> StandardView {
    game.spectator_view().to_typed().expect("typed view")
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
    assert_eq!(view_of(&game).draw_offer, None);
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
    // Black offers a draw, then ignores it by moving.
    game.apply(
        Color::Black,
        &Action::from_typed(&StandardAction::OfferDraw).unwrap(),
    )
    .unwrap();
    game.apply(Color::Black, &move_action("e7e5")).unwrap();
    // The draw offer should be gone now.
    assert_eq!(view_of(&game).draw_offer, None);
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
        view_of(&game).fen,
    );
}

#[test]
fn insufficient_material_is_a_draw() {
    // A lone black knight on e3 sits next to the White king on e2; capturing it
    // leaves king-versus-king, a dead position that cozy-chess itself keeps
    // `Ongoing` but which we terminate as an insufficient-material draw.
    let mut game = StandardGame::from_fen("4k3/8/8/8/8/4n3/4K3/8 w - - 0 1").unwrap();
    let effect = game.apply(Color::White, &move_action("e2e3")).unwrap();
    assert!(effect.status.is_finished());
    assert_eq!(
        game.outcome(),
        Some(Outcome::draw(EndReason::InsufficientMaterial)),
        "fen: {}",
        view_of(&game).fen
    );
    // The GameEnded event is emitted alongside the move.
    assert_eq!(effect.events.len(), 2);
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
fn view_fen_is_correct_after_opening_move() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["e2e4"]);
    assert_eq!(
        view_of(&game).fen,
        "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1"
    );
}

#[test]
fn view_reports_check() {
    let mut game = StandardGame::new();
    play_moves(&mut game, &["e2e4", "f7f5", "d1h5"]); // Qh5+ checks the Black king.
    assert!(view_of(&game).check, "Black should be in check after Qh5+");
}

// --- Castling: classic-UCI round-trip for the standard variant ---------------

#[test]
fn standard_kingside_castle_uses_classic_uci() {
    let mut game = StandardGame::new();
    // Clear the kingside for White: 1.e4 e5 2.Nf3 Nc6 3.Bc4 Bc5.
    play_moves(&mut game, &["e2e4", "e7e5", "g1f3", "b8c6", "f1c4", "f8c5"]);

    // The legal-move list offers the castle in classic UCI (`e1g1`), NOT the
    // cozy-chess king-to-rook form (`e1h1`).
    let moves = view_of(&game).legal_moves_uci;
    assert!(moves.contains(&"e1g1".to_owned()), "got: {moves:?}");
    assert!(!moves.contains(&"e1h1".to_owned()), "got: {moves:?}");

    // Playing `e1g1` actually castles: king to g1, rook to f1, and the emitted
    // SAN is `O-O`.
    let effect = game.apply(Color::White, &move_action("e1g1")).unwrap();
    let event: StandardEvent = effect.events[0].to_typed().unwrap();
    match event {
        StandardEvent::MovePlayed { uci, san, fen } => {
            assert_eq!(uci, "e1g1");
            assert_eq!(san, "O-O");
            assert!(
                fen.starts_with("r1bqk1nr/pppp1ppp/2n5/2b1p3/2B1P3/5N2/PPPP1PPP/RNBQ1RK1"),
                "fen after O-O: {fen}"
            );
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
}

#[test]
fn standard_queenside_castle_uses_classic_uci() {
    // Reach a position where White can castle queenside (b1, c1, d1 cleared) in
    // a fresh standard game, then castle with the classic UCI `e1c1`.
    let mut game = StandardGame::new();
    play_moves(
        &mut game,
        &[
            "d2d4", "d7d5", "b1c3", "b8c6", "c1f4", "c8f5", "d1d2", "d8d7",
        ],
    );

    // The queenside castle is offered as classic UCI `e1c1`, not `e1a1`.
    let moves = view_of(&game).legal_moves_uci;
    assert!(moves.contains(&"e1c1".to_owned()), "got: {moves:?}");
    assert!(!moves.contains(&"e1a1".to_owned()), "got: {moves:?}");

    let effect = game.apply(Color::White, &move_action("e1c1")).unwrap();
    let event: StandardEvent = effect.events[0].to_typed().unwrap();
    match event {
        StandardEvent::MovePlayed { uci, san, fen } => {
            assert_eq!(uci, "e1c1");
            assert_eq!(san, "O-O-O");
            // King to c1, rook to d1.
            assert!(
                fen.starts_with("r3kbnr/pppqpppp/2n5/3p1b2/3P1B2/2N5/PPPQPPPP/2KR1BNR"),
                "fen after O-O-O: {fen}"
            );
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
    assert_eq!(game.to_move(), Color::Black);
}

#[test]
fn promotion_to_queen_is_played_and_rendered() {
    // White pawn on b7 promotes by capturing the rook on a8.
    let mut game = StandardGame::from_fen("r3k3/1P6/8/8/8/8/8/4K3 w - - 0 1").unwrap();
    let effect = game.apply(Color::White, &move_action("b7a8q")).unwrap();
    let event: StandardEvent = effect.events[0].to_typed().unwrap();
    match event {
        StandardEvent::MovePlayed { uci, san, fen } => {
            assert_eq!(uci, "b7a8q");
            assert_eq!(san, "bxa8=Q+");
            assert!(fen.starts_with("Q3k3/8/8/8/8/8/8/4K3"), "fen: {fen}");
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
}

#[test]
fn en_passant_capture_is_legal_and_applied() {
    // 1.e4 a6 2.e5 d5 — now White can take d5 en passant via e5d6.
    let mut game = StandardGame::new();
    play_moves(&mut game, &["e2e4", "a7a6", "e4e5", "d7d5"]);

    // The en-passant square is recorded in the view's FEN.
    assert!(
        view_of(&game).fen.contains(" d6 "),
        "fen should record the ep square: {}",
        view_of(&game).fen
    );
    // `e5d6` is offered and captures the d5 pawn.
    let moves = view_of(&game).legal_moves_uci;
    assert!(moves.contains(&"e5d6".to_owned()), "got: {moves:?}");

    let effect = game.apply(Color::White, &move_action("e5d6")).unwrap();
    let event: StandardEvent = effect.events[0].to_typed().unwrap();
    match event {
        StandardEvent::MovePlayed { san, fen, .. } => {
            assert_eq!(san, "exd6");
            // The captured pawn is gone: no black pawn remains on d5.
            assert!(fen.starts_with("rnbqkbnr/1pp1pppp/p2P4/8"), "fen: {fen}");
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
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

    let factory = Chess960Variant;
    assert_eq!(factory.id(), CHESS960_VARIANT_ID);
    assert_eq!(factory.id(), "chess960");
    assert_eq!(factory.display_name(), "Chess960");
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
    assert!(registry.ids().contains(&CHESS960_VARIANT_ID));

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

// --- Chess960 ----------------------------------------------------------------

#[test]
fn chess960_factory_builds_from_scharnagl_position() {
    let factory = Chess960Variant;
    let opts = VariantOptions::new(serde_json::json!({ "position": 518 }));
    let game = factory.new_game(&opts).expect("position 518 is valid");
    // Position 518 is the classical setup.
    assert_eq!(game.variant_id(), "chess960");
    assert_eq!(game.to_move(), Color::White);
    let view: StandardView = game.spectator_view().to_typed().unwrap();
    assert!(
        view.fen
            .starts_with("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR"),
        "fen: {}",
        view.fen
    );
}

#[test]
fn chess960_rejects_out_of_range_position() {
    let factory = Chess960Variant;
    let opts = VariantOptions::new(serde_json::json!({ "position": 960 }));
    let err = factory.new_game(&opts).unwrap_err();
    assert!(matches!(err, GameError::InvalidActionPayload(_)));
}

#[test]
fn chess960_default_options_use_classical_setup() {
    let factory = Chess960Variant;
    let game = factory
        .new_game(&VariantOptions::default())
        .expect("default options build a game");
    assert_eq!(game.variant_id(), "chess960");
    assert_eq!(game.to_move(), Color::White);
}

#[test]
fn chess960_castles_with_king_to_rook_uci() {
    // A position with the king on e1 and rooks on a1/h1, both castles available.
    // In Chess960 the castle UCI targets the *rook's* square — `e1a1` queenside
    // and `e1h1` kingside — which is exactly how it differs from the classic
    // `e1c1` / `e1g1` the standard variant uses.
    let game =
        StandardGame::from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w HAha - 0 1").unwrap();
    assert_eq!(game.variant_id(), "chess960");

    let moves = view_of(&game).legal_moves_uci;
    assert!(
        moves.contains(&"e1a1".to_owned()) && moves.contains(&"e1h1".to_owned()),
        "expected king-to-rook castles e1a1 and e1h1; got: {moves:?}"
    );
    // The classic-UCI spellings are NOT used by the Chess960 variant.
    assert!(!moves.contains(&"e1c1".to_owned()), "got: {moves:?}");
    assert!(!moves.contains(&"e1g1".to_owned()), "got: {moves:?}");

    // Castle queenside (king-to-a-rook) and confirm king→c1, rook→d1.
    let mut game = game;
    let effect = game.apply(Color::White, &move_action("e1a1")).unwrap();
    let event: StandardEvent = effect.events[0].to_typed().unwrap();
    match event {
        StandardEvent::MovePlayed { uci, san, fen } => {
            assert_eq!(uci, "e1a1");
            assert_eq!(san, "O-O-O");
            assert!(
                fen.starts_with("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/2KR3R"),
                "fen after 960 O-O-O: {fen}"
            );
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
}

#[test]
fn chess960_full_game_reaches_checkmate() {
    // Play Fool's-mate-style moves from the classical 960 layout (position 518),
    // which behaves exactly like standard chess, to reach a terminal state.
    let mut game = StandardGame::chess960(518).unwrap();
    play_moves(&mut game, &["f2f3", "e7e5", "g2g4", "d8h4"]);
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
    assert!(game.status().is_finished());
}

#[test]
fn chess960_from_fen_via_factory() {
    let factory = Chess960Variant;
    // Shredder FEN (rook files in the castling field) is accepted for the
    // off-centre rook placements Chess960 allows.
    let opts = VariantOptions::new(serde_json::json!({
        "fen": "nrbbqknr/pppppppp/8/8/8/8/PPPPPPPP/NRBBQKNR w HBhb - 0 1"
    }));
    let mut game = factory.new_game(&opts).expect("valid 960 fen");
    assert_eq!(game.variant_id(), "chess960");
    // A simple knight move from the corner is legal.
    let effect = game.apply(Color::White, &move_action("a1b3")).unwrap();
    assert!(!effect.status.is_finished());
    assert_eq!(game.to_move(), Color::Black);
}
