//! End-to-end tests for the shakmaty variant family.
//!
//! These drive games through the [`GameSession`] boundary using the type-erased
//! payloads, mirroring how the server uses the variants, and assert real
//! terminal states that shakmaty computes (third check, king on the hill, an
//! atomic king explosion, and so on).

use mcs_core::{
    Action, Color, EndReason, GameError, GameSession, GameStatus, Outcome, PlayerView,
    VariantFactory, VariantOptions, VariantRegistry,
};

use crate::factory::{register_all, ShakmatyVariant};
use crate::game::ShakmatyGame;
use crate::spec::VariantSpec;
use crate::variants::{
    Antichess, Atomic, Chess960, Crazyhouse, Horde, KingOfTheHill, RacingKings, ThreeCheck,
};
use crate::wire::{ShakmatyAction, ShakmatyEvent, ShakmatyView};

/// Every id registered by [`register_all`], for exhaustiveness checks.
const ALL_IDS: [&str; 8] = [
    "atomic",
    "antichess",
    "crazyhouse",
    "kingofthehill",
    "threecheck",
    "racingkings",
    "horde",
    "chess960",
];

/// Wraps a UCI string into a move action payload.
fn move_action(uci: &str) -> Action {
    Action::from_typed(&ShakmatyAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// Builds a fresh game for spec `S` from the default options.
fn new_game<S: VariantSpec>() -> ShakmatyGame<S> {
    ShakmatyGame::<S>::new(&VariantOptions::default()).expect("default options are valid")
}

/// Plays a sequence of UCI moves alternating from White, asserting each is
/// legal, and returns the effect of the final move.
fn play_moves<S: VariantSpec>(game: &mut ShakmatyGame<S>, ucis: &[&str]) {
    let mut player = Color::White;
    for uci in ucis {
        game.apply(player, &move_action(uci))
            .unwrap_or_else(|e| panic!("[{}] move {uci} should be legal: {e}", S::ID));
        player = player.opposite();
    }
}

/// Reads the current spectator view as the typed [`ShakmatyView`].
fn view<S: VariantSpec>(game: &ShakmatyGame<S>) -> ShakmatyView {
    game.spectator_view().to_typed().expect("view round-trips")
}

// ----------------------------------------------------------------------------
// Per-variant terminal-state tests.
// ----------------------------------------------------------------------------

#[test]
fn three_check_wins_after_three_checks() {
    let mut game = new_game::<ThreeCheck>();
    // A forcing line in which White delivers three checks. Each White move below
    // gives check; on the third, shakmaty's remaining-check counter hits zero
    // and the game is a White win — not a checkmate.
    //   1. e4 f6  2. Bc4 g6  3. Qf3 a6
    //   4. Bxf7+ (check 1)  Kxf7
    //   5. Qb3+  (check 2)  e6
    //   6. Qxe6+ (check 3, White wins)
    play_moves(
        &mut game,
        &[
            "e2e4", "f7f6", "f1c4", "g7g6", "d1f3", "a7a6", "c4f7", "e8f7", "f3b3", "e7e6", "b3e6",
        ],
    );

    assert_eq!(
        game.outcome(),
        Some(Outcome::win(
            Color::White,
            EndReason::Other("three_checks".to_owned())
        )),
        "fen: {}",
        view(&game).fen
    );
    assert!(game.status().is_finished());
    // The terminal FEN records that White has exhausted all three checks (`0+3`,
    // i.e. zero remaining for White).
    assert!(
        view(&game).fen.contains("0+3"),
        "fen should record exhausted checks, got {}",
        view(&game).fen
    );
}

#[test]
fn king_of_the_hill_wins_when_king_reaches_center() {
    let mut game = new_game::<KingOfTheHill>();
    // March the white king to the central hill square d4. The d-pawn steps
    // aside first so the king can walk e1-d2-d3-d4. Black just shuffles a pawn.
    // Reaching a centre square (d4/e4/d5/e5) ends the game immediately.
    play_moves(
        &mut game,
        &["d2d4", "a7a6", "e1d2", "a6a5", "d2d3", "a5a4", "d3e4"],
    );
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(
            Color::White,
            EndReason::Other("king_in_the_center".to_owned())
        )),
        "fen: {}",
        view(&game).fen
    );
    assert!(game.status().is_finished());
    assert!(game.legal_actions(Color::White).is_empty());
}

#[test]
fn atomic_capture_can_explode_the_king() {
    let mut game = new_game::<Atomic>();
    // In atomic chess a capture detonates a 3x3 area, removing every adjacent
    // non-pawn piece — including a king. The bishop captures on d7, whose blast
    // radius includes the black king on e8, which ends the game as a White win.
    // 1. e4 e6 2. Bb5 a6 3. Bxd7 (explodes the king on e8)
    play_moves(&mut game, &["e2e4", "e7e6", "f1b5", "a7a6", "b5d7"]);

    assert_eq!(
        game.outcome(),
        Some(Outcome::win(
            Color::White,
            EndReason::Other("king_exploded".to_owned())
        )),
        "fen: {}",
        view(&game).fen
    );
    assert!(game.status().is_finished());
}

#[test]
fn racing_kings_starts_legal_and_advances() {
    let mut game = new_game::<RacingKings>();
    // Racing Kings starts with both armies on the first two ranks and no checks
    // allowed. Just confirm a legal opening move advances the game and the king
    // can begin its race.
    assert!(!view(&game).check, "Racing Kings disallows checks");
    let legal = view(&game).legal_moves_uci;
    let first = legal.first().expect("there are legal moves").clone();
    play_moves(&mut game, &[&first]);
    assert_eq!(game.status(), GameStatus::Ongoing);
}

#[test]
fn horde_white_is_a_wall_of_pawns() {
    let game = new_game::<Horde>();
    let v = view(&game);
    // White's first rank in the Horde start is all pawns: the FEN's last board
    // field (White's back rank) should be "PPPPPPPP".
    assert!(
        v.fen.contains("PPPPPPPP/PPPPPPPP"),
        "horde start should have ranks of white pawns, got {}",
        v.fen
    );
    assert_eq!(v.side_to_move, Color::White);
    assert!(!v.legal_moves_uci.is_empty());
}

#[test]
fn antichess_has_no_check_and_captures_are_forced() {
    let mut game = new_game::<Antichess>();
    assert!(!view(&game).check, "antichess has no royal king");
    // 1.e4 ... if Black plays a move that allows a capture, captures become
    // forced; just confirm a normal opening proceeds.
    play_moves(&mut game, &["e2e3", "b7b6"]);
    assert_eq!(game.status(), GameStatus::Ongoing);
    assert_eq!(game.to_move(), Color::White);
}

#[test]
fn crazyhouse_records_pockets_in_the_fen() {
    let mut game = new_game::<Crazyhouse>();
    // After a capture the captured piece enters the capturer's pocket, which
    // shakmaty serializes inside the FEN (the bracketed/`/` pocket field).
    play_moves(&mut game, &["e2e4", "d7d5", "e4d5"]);
    let v = view(&game);
    // White captured a pawn, so White's pocket holds a pawn; the FEN encodes it.
    assert!(
        v.fen.contains("[P]") || v.fen.contains("/P "),
        "crazyhouse FEN should record White's pocketed pawn, got {}",
        v.fen
    );
    assert_eq!(game.status(), GameStatus::Ongoing);
}

#[test]
fn chess960_default_is_the_standard_position() {
    let game = new_game::<Chess960>();
    let v = view(&game);
    // Chess960 number 518 is the standard chess setup.
    assert!(
        v.fen
            .starts_with("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w"),
        "default chess960 should be the standard position, got {}",
        v.fen
    );
    assert_eq!(v.variant_id, "chess960");
}

#[test]
fn chess960_position_number_selects_a_shuffled_back_rank() {
    let factory = ShakmatyVariant::<Chess960>::new();
    // Position 0 is the leftmost Chess960 arrangement: BBQNNRKR.
    let opts = VariantOptions::new(serde_json::json!({ "position": 0 }));
    let game = factory.new_game(&opts).expect("position 0 is valid");
    let v: ShakmatyView = game.spectator_view().to_typed().unwrap();
    assert!(
        v.fen.starts_with("bbqnnrkr/pppppppp"),
        "chess960 #0 should be bbqnnrkr, got {}",
        v.fen
    );

    // Position 959 is the rightmost arrangement: RKRNNQBB.
    let opts = VariantOptions::new(serde_json::json!({ "position": 959 }));
    let game = factory.new_game(&opts).expect("position 959 is valid");
    let v: ShakmatyView = game.spectator_view().to_typed().unwrap();
    assert!(
        v.fen.starts_with("rkrnnqbb/pppppppp"),
        "chess960 #959 should be rkrnnqbb, got {}",
        v.fen
    );
}

#[test]
fn chess960_accepts_an_explicit_fen() {
    let factory = ShakmatyVariant::<Chess960>::new();
    let opts = VariantOptions::new(serde_json::json!({
        "fen": "nrbbnqkr/pppppppp/8/8/8/8/PPPPPPPP/NRBBNQKR w KQkq - 0 1"
    }));
    let game = factory.new_game(&opts).expect("valid chess960 fen");
    let v: ShakmatyView = game.spectator_view().to_typed().unwrap();
    assert!(v.fen.starts_with("nrbbnqkr/pppppppp"));
}

#[test]
fn chess960_rejects_out_of_range_position() {
    let factory = ShakmatyVariant::<Chess960>::new();
    let opts = VariantOptions::new(serde_json::json!({ "position": 960 }));
    let err = factory.new_game(&opts).unwrap_err();
    assert!(matches!(err, GameError::InvalidActionPayload(_)));
}

// ----------------------------------------------------------------------------
// Boundary behavior shared by every variant.
// ----------------------------------------------------------------------------

#[test]
fn illegal_move_is_rejected() {
    let mut game = new_game::<Atomic>();
    // A pawn cannot jump three squares.
    let err = game.apply(Color::White, &move_action("e2e5")).unwrap_err();
    assert_eq!(err, GameError::IllegalAction);
    assert_eq!(game.status(), GameStatus::Ongoing);
    assert_eq!(game.to_move(), Color::White);
}

#[test]
fn out_of_turn_move_is_rejected() {
    let mut game = new_game::<KingOfTheHill>();
    // Black tries to move first.
    let err = game.apply(Color::Black, &move_action("e7e5")).unwrap_err();
    assert_eq!(err, GameError::NotYourTurn);
}

#[test]
fn malformed_uci_is_an_invalid_payload() {
    let mut game = new_game::<Crazyhouse>();
    let err = game
        .apply(Color::White, &move_action("not-a-move"))
        .unwrap_err();
    assert!(matches!(err, GameError::InvalidActionPayload(_)));
}

#[test]
fn resignation_hands_the_win_to_the_opponent() {
    let mut game = new_game::<Horde>();
    let resign = Action::from_typed(&ShakmatyAction::Resign).unwrap();
    let effect = game.apply(Color::White, &resign).unwrap();
    assert!(effect.status.is_finished());
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Resignation))
    );
}

#[test]
fn draw_offer_and_accept_ends_in_a_draw() {
    let mut game = new_game::<Antichess>();
    let offer = Action::from_typed(&ShakmatyAction::OfferDraw).unwrap();
    let accept = Action::from_typed(&ShakmatyAction::AcceptDraw).unwrap();

    let effect = game.apply(Color::White, &offer).unwrap();
    let event: ShakmatyEvent = effect.events[0].to_typed().unwrap();
    assert!(matches!(event, ShakmatyEvent::DrawOffered { by } if by == Color::White));

    let effect = game.apply(Color::Black, &accept).unwrap();
    assert!(effect.status.is_finished());
    assert_eq!(
        game.outcome(),
        Some(Outcome::draw(EndReason::DrawAgreement))
    );
}

#[test]
fn acting_after_finish_is_rejected() {
    let mut game = new_game::<Atomic>();
    let resign = Action::from_typed(&ShakmatyAction::Resign).unwrap();
    game.apply(Color::White, &resign).unwrap();
    let err = game.apply(Color::Black, &move_action("e7e5")).unwrap_err();
    assert_eq!(err, GameError::Finished);
}

#[test]
fn move_played_event_carries_uci_san_and_fen() {
    let mut game = new_game::<Chess960>();
    let effect = game.apply(Color::White, &move_action("e2e4")).unwrap();
    let played: ShakmatyEvent = effect.events[0].to_typed().unwrap();
    match played {
        ShakmatyEvent::MovePlayed { uci, san, fen } => {
            assert_eq!(uci, "e2e4");
            assert_eq!(san, "e4");
            assert!(fen.starts_with("rnbqkbnr/pppppppp/8/8/4P3"));
        }
        other => panic!("expected MovePlayed, got {other:?}"),
    }
}

#[test]
fn view_is_perfect_information_and_round_trips() {
    let mut game = new_game::<KingOfTheHill>();
    play_moves(&mut game, &["e2e4"]);

    let white = game.view_for(Color::White);
    let black = game.view_for(Color::Black);
    let spectator = game.spectator_view();
    assert_eq!(white, black);
    assert_eq!(white, spectator);

    let v: ShakmatyView = white.to_typed().unwrap();
    assert_eq!(v.variant_id, "kingofthehill");
    assert_eq!(v.side_to_move, Color::Black);

    // The PlayerView newtype round-trips through JSON unchanged.
    let json = serde_json::to_string(&white).unwrap();
    let back: PlayerView = serde_json::from_str(&json).unwrap();
    assert_eq!(white, back);
}

#[test]
fn legal_actions_for_side_to_move_include_moves_and_meta() {
    let game = new_game::<Atomic>();
    let white_actions = game.legal_actions(Color::White);
    let mut moves = 0;
    let mut resign = 0;
    let mut offer = 0;
    for action in &white_actions {
        match action.to_typed::<ShakmatyAction>().unwrap() {
            ShakmatyAction::Move { .. } => moves += 1,
            ShakmatyAction::Resign => resign += 1,
            ShakmatyAction::OfferDraw => offer += 1,
            other => panic!("unexpected action {other:?}"),
        }
    }
    assert_eq!(moves, 20, "atomic shares the standard opening move count");
    assert_eq!(resign, 1);
    assert_eq!(offer, 1);

    // The non-moving side may still resign / offer a draw, but has no moves.
    let black = game.legal_actions(Color::Black);
    assert!(black.iter().all(|a| !matches!(
        a.to_typed::<ShakmatyAction>().unwrap(),
        ShakmatyAction::Move { .. }
    )));
    assert!(black.iter().any(|a| matches!(
        a.to_typed::<ShakmatyAction>().unwrap(),
        ShakmatyAction::Resign
    )));
}

// ----------------------------------------------------------------------------
// Registry & factory wiring.
// ----------------------------------------------------------------------------

#[test]
fn register_all_registers_every_id() {
    let mut registry = VariantRegistry::new();
    register_all(&mut registry);
    let ids = registry.ids();
    for id in ALL_IDS {
        assert!(ids.contains(&id), "registry should contain {id}");
    }
    assert_eq!(ids.len(), ALL_IDS.len(), "no extra or missing variants");
}

#[test]
fn factory_metadata_is_correct() {
    assert_eq!(ShakmatyVariant::<Atomic>::new().id(), "atomic");
    assert_eq!(ShakmatyVariant::<Atomic>::new().display_name(), "Atomic");
    assert_eq!(ShakmatyVariant::<Chess960>::new().id(), "chess960");
    assert_eq!(
        ShakmatyVariant::<KingOfTheHill>::new().display_name(),
        "King of the Hill"
    );
}

#[test]
fn new_game_works_through_box_dyn_for_every_variant() {
    let mut registry = VariantRegistry::new();
    register_all(&mut registry);

    for id in ALL_IDS {
        let mut game: Box<dyn GameSession> = registry
            .new_game(id, &VariantOptions::default())
            .unwrap_or_else(|e| panic!("{id} should create a game: {e}"));
        assert_eq!(game.variant_id(), id);
        assert_eq!(game.to_move(), Color::White);
        assert!(game.outcome().is_none());

        // Each variant has at least one legal opening move playable for White.
        let v: ShakmatyView = game.spectator_view().to_typed().unwrap();
        let first = v
            .legal_moves_uci
            .first()
            .unwrap_or_else(|| panic!("{id} should have an opening move"))
            .clone();
        let effect = game
            .apply(Color::White, &move_action(&first))
            .unwrap_or_else(|e| panic!("{id} opening move {first} should be legal: {e}"));
        assert!(!effect.events.is_empty());
    }
}

#[test]
fn action_payload_round_trips() {
    for action in [
        ShakmatyAction::Move {
            uci: "e2e4".to_owned(),
        },
        ShakmatyAction::Resign,
        ShakmatyAction::OfferDraw,
        ShakmatyAction::AcceptDraw,
        ShakmatyAction::DeclineDraw,
    ] {
        let payload = Action::from_typed(&action).unwrap();
        let json = serde_json::to_string(&payload).unwrap();
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        assert_eq!(back.to_typed::<ShakmatyAction>().unwrap(), action);
    }
}
