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
/// five excluded ids: `fogofwar`, `jieqi`, `duck`, `placement`, `sittuyin`).
/// Since #155 this includes `standard` and `chess960`.
const EXPECTED_REGISTERED: usize = 112;

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

    // A representative sample of the catalog is present: ordinary chess and
    // Chess960 (now mcr-owned, #155), an 8x8 fairy, a hand variant, a
    // large-board variant, and a small board.
    for present in [
        "standard",
        "chess960",
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

    // The excluded ids are absent: hidden-information and phased variants (#156).
    for absent in ["fogofwar", "jieqi", "duck", "placement", "sittuyin"] {
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

// ---------------------------------------------------------------------------
// Standard chess routed through mcr (#155): the FIDE draw rules the retired
// cozy-chess adapter provided must survive.
// ---------------------------------------------------------------------------

/// Helper: a game of `variant` created from an explicit starting FEN.
fn game_from_fen(variant: &str, fen: &str) -> Box<dyn GameSession> {
    let opts = VariantOptions::new(serde_json::json!({ "fen": fen }));
    registry()
        .new_game(variant, &opts)
        .expect("variant is registered")
}

/// Helper: the `claim_draw` action payload.
fn claim_draw_action() -> Action {
    Action::from_typed(&McrAction::ClaimDraw).expect("serializable")
}

#[test]
fn standard_runs_through_mcr() {
    let mut game = new_game("standard");
    assert_eq!(game.variant_id(), "standard");
    assert_eq!(game.to_move(), Color::White);

    let before = view(game.as_ref(), Color::White);
    assert_eq!(before.legal_moves_uci.len(), 20);
    assert!(before.legal_moves_uci.contains(&"e2e4".to_owned()));
    assert!(!before.can_claim_draw);

    game.apply(Color::White, &move_action("e2e4"))
        .expect("e2e4 is legal in standard chess");
    assert_eq!(game.to_move(), Color::Black);
    assert!(view(game.as_ref(), Color::Black)
        .fen
        .starts_with("rnbqkbnr/pppppppp/8/8/4P3"));
}

#[test]
fn standard_reports_checkmate() {
    let mut game = new_game("standard");
    // Fool's mate: 1. f3 e5 2. g4 Qh4#.
    for (player, uci) in [
        (Color::White, "f2f3"),
        (Color::Black, "e7e5"),
        (Color::White, "g2g4"),
        (Color::Black, "d8h4"),
    ] {
        game.apply(player, &move_action(uci)).expect("legal move");
    }
    assert_eq!(
        game.outcome(),
        Some(Outcome::win(Color::Black, EndReason::Checkmate))
    );
}

#[test]
fn standard_reports_stalemate() {
    // White to move, one move from stalemating the lone black king: Qc6-g6 leaves
    // Black (Kh8) with no legal move and not in check.
    let mut game = game_from_fen("standard", "7k/5K2/2Q5/8/8/8/8/8 w - - 0 1");
    game.apply(Color::White, &move_action("c6g6"))
        .expect("Qg6 is legal");
    assert_eq!(game.outcome(), Some(Outcome::draw(EndReason::Stalemate)));
}

#[test]
fn standard_threefold_repetition_is_claimable() {
    let mut game = new_game("standard");
    // Shuffle knights out and back twice; the start position recurs a third time.
    for (player, uci) in [
        (Color::White, "g1f3"),
        (Color::Black, "g8f6"),
        (Color::White, "f3g1"),
        (Color::Black, "f6g8"),
        (Color::White, "g1f3"),
        (Color::Black, "g8f6"),
        (Color::White, "f3g1"),
        (Color::Black, "f6g8"),
    ] {
        game.apply(player, &move_action(uci)).expect("legal move");
    }

    // The game is not over on its own, but a draw is now claimable by the side to
    // move (White), and is surfaced in the view.
    assert!(game.outcome().is_none());
    assert!(view(game.as_ref(), Color::White).can_claim_draw);
    assert!(game
        .legal_actions(Color::White)
        .contains(&claim_draw_action()));
    // The opponent cannot claim on White's turn.
    assert!(!game
        .legal_actions(Color::Black)
        .contains(&claim_draw_action()));

    game.apply(Color::White, &claim_draw_action())
        .expect("threefold draw is claimable");
    assert_eq!(game.outcome(), Some(Outcome::draw(EndReason::Repetition)));
}

#[test]
fn standard_fivefold_repetition_is_automatic() {
    let mut game = new_game("standard");
    // Four full knight-shuffle cycles bring the start position to its fifth
    // occurrence, which is an automatic draw needing no claim.
    let cycle = [
        (Color::White, "g1f3"),
        (Color::Black, "g8f6"),
        (Color::White, "f3g1"),
        (Color::Black, "f6g8"),
    ];
    for _ in 0..4 {
        for (player, uci) in cycle {
            game.apply(player, &move_action(uci)).expect("legal move");
        }
    }
    assert_eq!(game.outcome(), Some(Outcome::draw(EndReason::Repetition)));
}

#[test]
fn standard_fifty_move_rule_is_claimable() {
    // A position whose halfmove clock has already reached 100 plies: the
    // fifty-move draw is claimable immediately, before any further move.
    let mut game = game_from_fen("standard", "4k3/8/8/8/8/8/R7/4K3 w - - 100 80");
    assert!(view(game.as_ref(), Color::White).can_claim_draw);

    game.apply(Color::White, &claim_draw_action())
        .expect("fifty-move draw is claimable");
    assert_eq!(
        game.outcome(),
        Some(Outcome::draw(EndReason::FiftyMoveRule))
    );
}

#[test]
fn chess960_is_registered_and_playable() {
    let mut game = new_game("chess960");
    assert_eq!(game.variant_id(), "chess960");
    // The default chess960 start position is the classical setup; at least one
    // legal move exists and the view exposes the claim flag (false at the start).
    let before = view(game.as_ref(), Color::White);
    assert!(!before.legal_moves_uci.is_empty());
    assert!(!before.can_claim_draw);
    let first = before.legal_moves_uci[0].clone();
    game.apply(Color::White, &move_action(&first))
        .expect("a listed move is legal");
}
