//! End-to-end tests for the mcr-backed variants.
//!
//! These drive games through the [`GameSession`] boundary using the type-erased
//! payloads, mirroring how the server uses the variant. Two variants exercise
//! the two things this adapter must get right: a standard-army fairy variant
//! (`kingofthehill`) for creation, move legality, and terminal detection, and a
//! drop variant (`minishogi`) for hand / drop UCIs surfacing in the view.

use mcs_core::{
    Action, Color, EndReason, GameError, GameSession, GameStatus, Outcome, VariantOptions,
    VariantRegistry,
};

use crate::factory::register;
use crate::wire::{McrAction, McrView};

/// The number of variants this adapter registers (mcr's full catalog minus the
/// seven excluded ids: `standard`, `chess960`, `fogofwar`, `jieqi`, `duck`,
/// `placement`, `sittuyin`).
const EXPECTED_REGISTERED: usize = 110;

/// Helper: a move action payload from a UCI string.
fn move_action(uci: &str) -> Action {
    Action::from_typed(&McrAction::Move {
        uci: uci.to_owned(),
    })
    .expect("serializable")
}

/// Helper: decode a session's (perfect-information) view into an [`McrView`].
fn view(game: &dyn GameSession, player: Color) -> McrView {
    game.view_for(player)
        .to_typed::<McrView>()
        .expect("view decodes")
}

/// Helper: build a fresh registry with the mcr catalog registered.
fn registry() -> VariantRegistry {
    let mut registry = VariantRegistry::new();
    register(&mut registry);
    registry
}

/// Helper: a new game of `variant` from its default start position.
fn new_game(variant: &str) -> Box<dyn GameSession> {
    registry()
        .new_game(variant, &VariantOptions::default())
        .expect("variant is registered")
}

#[test]
fn registers_the_expected_catalog() {
    let registry = registry();
    let ids = registry.ids();
    assert_eq!(ids.len(), EXPECTED_REGISTERED);

    // A representative sample of the catalog is present: an 8x8 fairy, a hand
    // variant, a large-board variant, and a small board.
    for present in [
        "kingofthehill",
        "crazyhouse",
        "shogi",
        "minishogi",
        "xiangqi",
        "atomic",
    ] {
        assert!(
            registry.get(present).is_some(),
            "{present} should be registered"
        );
    }

    // The excluded ids are absent: cozy-owned, hidden-information, and phased.
    for absent in [
        "standard",
        "chess960",
        "fogofwar",
        "jieqi",
        "duck",
        "placement",
        "sittuyin",
    ] {
        assert!(
            registry.get(absent).is_none(),
            "{absent} should be excluded"
        );
    }
}

#[test]
fn create_and_apply_a_legal_move() {
    let mut game = new_game("kingofthehill");
    assert_eq!(game.variant_id(), "kingofthehill");
    assert_eq!(game.to_move(), Color::White);
    assert_eq!(game.status(), GameStatus::Ongoing);
    assert!(game.outcome().is_none());

    let before = view(game.as_ref(), Color::White);
    assert_eq!(before.side_to_move, Color::White);
    assert_eq!(before.legal_moves_uci.len(), 20);
    assert!(before.legal_moves_uci.contains(&"e2e4".to_owned()));
    assert!(!before.check);

    let effect = game
        .apply(Color::White, &move_action("e2e4"))
        .expect("e2e4 is legal");
    assert_eq!(effect.status, GameStatus::Ongoing);
    assert_eq!(game.to_move(), Color::Black);

    let after = view(game.as_ref(), Color::Black);
    assert_ne!(after.fen, before.fen);
    assert_eq!(after.side_to_move, Color::Black);
}

#[test]
fn rejects_illegal_wrong_turn_and_malformed_moves() {
    let mut game = new_game("kingofthehill");

    // A well-formed but illegal move (pawns cannot leap three squares).
    assert_eq!(
        game.apply(Color::White, &move_action("e2e5")),
        Err(GameError::IllegalAction)
    );
    // A malformed UCI string names no legal move either.
    assert_eq!(
        game.apply(Color::White, &move_action("zzzz")),
        Err(GameError::IllegalAction)
    );
    // Black cannot move on White's turn.
    assert_eq!(
        game.apply(Color::Black, &move_action("e7e5")),
        Err(GameError::NotYourTurn)
    );

    // None of the rejected attempts advanced the game.
    assert_eq!(game.to_move(), Color::White);
    assert!(game.outcome().is_none());
}

#[test]
fn reports_a_checkmate_terminal() {
    let mut game = new_game("kingofthehill");

    // Fool's mate: 1. f3 e5 2. g4 Qh4#.
    for (player, uci) in [
        (Color::White, "f2f3"),
        (Color::Black, "e7e5"),
        (Color::White, "g2g4"),
        (Color::Black, "d8h4"),
    ] {
        game.apply(player, &move_action(uci)).expect("legal move");
    }

    let expected = Outcome::win(Color::Black, EndReason::Checkmate);
    assert_eq!(game.outcome(), Some(expected.clone()));
    assert_eq!(game.status(), GameStatus::Finished(expected));

    // A finished game admits no further actions.
    assert!(game.legal_actions(Color::White).is_empty());
    assert_eq!(
        game.apply(Color::White, &move_action("a2a3")),
        Err(GameError::Finished)
    );
    // The final view offers no moves.
    assert!(view(game.as_ref(), Color::White).legal_moves_uci.is_empty());
}

#[test]
fn resignation_hands_the_win_to_the_opponent() {
    let mut game = new_game("kingofthehill");
    let resign = Action::from_typed(&McrAction::Resign).expect("serializable");

    game.apply(Color::White, &resign)
        .expect("resignation is legal");
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Resignation))
    );
}

#[test]
fn draw_offer_and_acceptance_end_the_game() {
    let mut game = new_game("kingofthehill");
    let offer = Action::from_typed(&McrAction::OfferDraw).expect("serializable");
    let accept = Action::from_typed(&McrAction::AcceptDraw).expect("serializable");

    game.apply(Color::White, &offer).expect("offer is legal");
    assert_eq!(
        view(game.as_ref(), Color::White).draw_offer,
        Some(Color::White)
    );

    game.apply(Color::Black, &accept)
        .expect("acceptance is legal");
    assert_eq!(
        game.outcome(),
        Some(Outcome::draw(EndReason::DrawAgreement))
    );
}

#[test]
fn hand_drops_surface_in_the_view_and_apply() {
    // A minishogi position where White holds a pawn in hand (the `[P]`), so drops
    // onto the empty a-file are legal. Built through the factory's `fen` option.
    let opts = VariantOptions::new(serde_json::json!({
        "fen": "rbsgk/4p/5/5/KGSBR[P] w - - 0 1"
    }));
    let mut game = registry()
        .new_game("minishogi", &opts)
        .expect("minishogi is registered");

    assert_eq!(game.to_move(), Color::White);
    let before = view(game.as_ref(), Color::White);
    // The hand is carried in the FEN, and drop UCIs (spelled `P@<square>`) appear
    // alongside board moves.
    assert!(before.fen.contains("[P]"));
    let drop = "P@a4";
    assert!(
        before.legal_moves_uci.contains(&drop.to_owned()),
        "expected a drop in {:?}",
        before.legal_moves_uci
    );

    game.apply(Color::White, &move_action(drop))
        .expect("the drop is legal");

    // After dropping the pawn the hand is empty and it is Black's turn.
    let after = view(game.as_ref(), Color::Black);
    assert!(after.fen.contains("[]"));
    assert_eq!(game.to_move(), Color::Black);
}
