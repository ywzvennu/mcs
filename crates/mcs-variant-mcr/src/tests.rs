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

/// The number of variants this adapter registers: mcr's **whole** catalog, with
/// nothing deferred. Since #155 this includes `standard` and `chess960`; since
/// #156 the phased Duck / Placement / Sittuyin and redacted Fog of War; and since
/// #163 jieqi (its per-player redaction now delegated to mcr).
const EXPECTED_REGISTERED: usize = 119;

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

/// Helper: a new game created the way the server does — options resolved through
/// [`prepare_new_game_options`](mcs_core::VariantFactory::prepare_new_game_options)
/// first (so jieqi gets its per-game reveal seed), then a session built from them.
/// Returns the resolved options alongside the session so a test can replay them
/// through the recovery path.
fn new_prepared_game(variant: &str) -> (Box<dyn GameSession>, VariantOptions) {
    let registry = registry();
    let options = registry
        .prepare_new_game_options(variant, &VariantOptions::default())
        .expect("options resolve");
    let game = registry
        .new_game(variant, &options)
        .expect("variant is registered");
    (game, options)
}

#[test]
fn registers_the_expected_catalog() {
    let registry = registry();
    let ids = registry.ids();
    assert_eq!(ids.len(), EXPECTED_REGISTERED);

    // A representative sample of the catalog is present: ordinary chess and
    // Chess960 (now mcr-owned, #155), an 8x8 fairy, a hand variant, a
    // large-board variant, a small board, the phased variants (#156), and both
    // hidden-information variants — Fog of War and jieqi — whose per-player views
    // mcr redacts (#163).
    for present in [
        "standard",
        "chess960",
        "kingofthehill",
        "crazyhouse",
        "shogi",
        "minishogi",
        "xiangqi",
        "atomic",
        "fogofwar",
        "jieqi",
        "duck",
        "placement",
        "sittuyin",
    ] {
        assert!(
            registry.get(present).is_some(),
            "{present} should be registered"
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

// ---------------------------------------------------------------------------
// Phased variants routed through the single-action seam (#156): Duck's two-part
// move is one combined UCI, and Placement / Sittuyin deploy through open drops.
// ---------------------------------------------------------------------------

#[test]
fn duck_two_part_move_applies_as_one_action() {
    let mut game = new_game("duck");
    assert_eq!(game.variant_id(), "duck");

    // Every duck move is a single combined UCI: a piece move plus a `,`-separated
    // duck placement (e.g. `b1a3,a3b3`). The move list surfaces them whole.
    let before = view(game.as_ref(), Color::White);
    let mv = before
        .legal_moves_uci
        .iter()
        .find(|u| u.contains(','))
        .expect("duck moves carry a duck-placement addendum")
        .clone();
    game.apply(Color::White, &move_action(&mv))
        .expect("a listed duck move is legal");

    // The move applied as one action and it is now Black's turn; the duck (`*`)
    // is on the board.
    assert_eq!(game.to_move(), Color::Black);
    assert!(
        view(game.as_ref(), Color::Black).fen.contains('*'),
        "the duck should be on the board after the first move"
    );
}

#[test]
fn duck_reaches_a_terminal() {
    // Duck's king is non-royal: capturing it leaves the opponent with no move,
    // which mcr's `Game` seam adjudicates as a (single-position) terminal. With
    // the white queen on e7 adjacent to the black king on e8, the combined move
    // `e7e8,<duck>` captures the king and ends the game — confirming the two-part
    // move drives the session all the way to a finished outcome.
    let mut game = game_from_fen("duck", "4k3/4Q3/8/8/8/8/8/4K3 w - - 0 1");
    let mate = view(game.as_ref(), Color::White)
        .legal_moves_uci
        .into_iter()
        .find(|u| u.starts_with("e7e8,"))
        .expect("a king-capturing move should be available");
    let effect = game
        .apply(Color::White, &move_action(&mate))
        .expect("the king capture is legal");
    assert!(
        matches!(effect.status, GameStatus::Finished(_)),
        "capturing the king ends the game"
    );
    assert!(
        game.outcome().is_some(),
        "the game records a terminal outcome"
    );
    // No further action is admitted once finished.
    assert!(game.legal_actions(Color::White).is_empty());
}

#[test]
fn placement_and_sittuyin_deploy_through_open_drops() {
    for variant in ["placement", "sittuyin"] {
        let mut game = new_game(variant);
        assert_eq!(game.variant_id(), variant);

        // The opening phase offers only drops (`role@square`); the pocket rides
        // in the FEN's `[..]` bracket, visible to both sides (open deployment).
        let before = view(game.as_ref(), Color::White);
        assert!(
            before.legal_moves_uci.iter().all(|u| u.contains('@')),
            "{variant} setup should offer only drops, got {:?}",
            before.legal_moves_uci
        );
        assert!(
            before.fen.contains('['),
            "{variant} FEN should carry the deployment pocket"
        );

        // Deploying a held piece is an ordinary single action.
        let drop = before.legal_moves_uci[0].clone();
        game.apply(Color::White, &move_action(&drop))
            .expect("a listed drop is legal");
        assert_eq!(game.to_move(), Color::Black);
    }
}

// ---------------------------------------------------------------------------
// Hidden-information redaction, delegated to mcr (#163): a player's view never
// leaks the opponent's hidden information, and a spectator's view is redacted
// while the game is in progress. The adapter computes none of this — it passes
// mcr's `view_for` / `spectator_view` output through — so these tests exercise
// mcr's redaction across both hidden-information variants: Fog of War (fog) and
// jieqi (concealed `Dark` identities + a stripped reveal seed).
// ---------------------------------------------------------------------------

/// Helper: the piece-placement field (first FEN field) of a view — the part that
/// would carry a leaked opponent piece.
fn placement_of(view: &McrView) -> &str {
    view.fen.split(' ').next().unwrap_or("")
}

#[test]
fn fogofwar_is_registered_and_playable() {
    let mut game = new_game("fogofwar");
    assert_eq!(game.variant_id(), "fogofwar");
    let before = view(game.as_ref(), Color::White);
    assert_eq!(before.side_to_move, Color::White);
    // The side to move sees its own legal moves through mcr's redacted view.
    assert_eq!(before.legal_moves_uci.len(), 20);
    game.apply(Color::White, &move_action("e2e4"))
        .expect("e2e4 is legal in fog of war");
    assert_eq!(game.to_move(), Color::Black);
}

#[test]
fn fogofwar_player_view_never_leaks_the_opponent() {
    let game = new_game("fogofwar");

    // At the start neither side attacks past the fourth rank, so a player sees
    // only their own army: no opponent piece may appear in the redacted board.
    let white = view(game.as_ref(), Color::White);
    let white_placement = placement_of(&white);
    assert!(
        !white_placement.chars().any(|c| c.is_ascii_lowercase()),
        "White's fog view leaked a black piece: {white_placement}"
    );

    // The non-moving player gets no move list (only the side to move does),
    // and symmetrically sees no white piece.
    let black = view(game.as_ref(), Color::Black);
    assert!(black.legal_moves_uci.is_empty());
    let black_placement = placement_of(&black);
    assert!(
        !black_placement.chars().any(|c| c.is_ascii_uppercase()),
        "Black's fog view leaked a white piece: {black_placement}"
    );
}

#[test]
fn fogofwar_spectator_is_redacted_while_in_progress() {
    let game = new_game("fogofwar");

    // A spectator of an in-progress fog game sees mcr's doubly redacted board —
    // every secret piece hidden from both sides — and no move list, so nothing
    // leaks. Neither king is visible from the mutually-hidden start.
    let spectator = game
        .spectator_view()
        .to_typed::<McrView>()
        .expect("spectator view decodes");
    assert_eq!(spectator.status, GameStatus::Ongoing);
    assert!(
        spectator.legal_moves_uci.is_empty(),
        "a spectator sees no move list while the game is in progress"
    );
    let placement = placement_of(&spectator);
    assert!(
        !placement.contains('k') && !placement.contains('K'),
        "neither king should be visible to a spectator: {placement}"
    );
}

// ---------------------------------------------------------------------------
// jieqi (hidden Xiangqi), re-enabled in #163: created with a per-game reveal seed
// so it is genuine hidden information, its per-player redaction delegated to mcr.
// ---------------------------------------------------------------------------

/// The canonical jieqi generals and dark tokens (mcr dialect): `k`/`K` are the
/// face-up generals, `=d`/`=D` a face-down (concealed) piece.
#[test]
fn jieqi_is_registered_and_playable_with_a_seed() {
    let (mut game, options) = new_prepared_game("jieqi");
    assert_eq!(game.variant_id(), "jieqi");

    // The resolved options carry a seeded starting FEN (mcr's optional seventh
    // field): a seven-field jieqi FEN opts the game into the stochastic reveal.
    let fen = options.as_value()["fen"]
        .as_str()
        .expect("jieqi options carry a seeded start FEN");
    assert_eq!(
        fen.split_whitespace().count(),
        7,
        "the start FEN must carry a trailing seed field: {fen}"
    );
    fen.split_whitespace()
        .nth(6)
        .unwrap()
        .parse::<u64>()
        .expect("the seventh field is a u64 seed");

    // The game is playable: the side to move has legal moves and one applies.
    let before = view(game.as_ref(), Color::White);
    assert_eq!(before.side_to_move, Color::White);
    let first = before.legal_moves_uci[0].clone();
    game.apply(Color::White, &move_action(&first))
        .expect("a listed jieqi move is legal");
    assert_eq!(game.to_move(), Color::Black);
}

#[test]
fn jieqi_view_conceals_identities_and_never_leaks_the_seed() {
    let (game, _options) = new_prepared_game("jieqi");

    for color in [Color::White, Color::Black] {
        let v = view(game.as_ref(), color);
        // mcr strips the seed from every redacted view: the FEN is back to six
        // fields, so the reveal seed can never cross the boundary to a client.
        assert_eq!(
            v.fen.split(' ').count(),
            6,
            "the reveal seed must be stripped from a player view: {}",
            v.fen
        );
        // Concealed pieces stay generic `Dark` tokens — no unflipped identity is
        // revealed to either player.
        let placement = placement_of(&v);
        assert!(
            placement.contains("=d") && placement.contains("=D"),
            "concealed pieces must render as Dark: {placement}"
        );
    }

    // The spectator view likewise carries no seed.
    let spectator = game
        .spectator_view()
        .to_typed::<McrView>()
        .expect("spectator view decodes");
    assert_eq!(
        spectator.fen.split(' ').count(),
        6,
        "the spectator view must not carry the seed: {}",
        spectator.fen
    );
}

#[test]
fn jieqi_recovery_replays_to_the_same_seeded_assignment() {
    // Recovery rebuilds a game by replaying its action log through a fresh session
    // created from the *persisted* options. The seed is resolved once (at
    // creation) and carried in those options, so a fresh session built from the
    // same options and driven through the same moves must reveal identical
    // concealed identities — otherwise a recorded move could become illegal on
    // replay. This pins that determinism.
    let registry = registry();
    let options = registry
        .prepare_new_game_options("jieqi", &VariantOptions::default())
        .expect("options resolve");

    // The original game: play several plies, each a first-listed legal move for
    // the side to move, revealing dark pieces as they go. Record the UCIs.
    let mut original = registry
        .new_game("jieqi", &options)
        .expect("jieqi is registered");
    let mut played = Vec::new();
    for _ in 0..8 {
        let mover = original.to_move();
        let moves = view(original.as_ref(), mover).legal_moves_uci;
        let uci = moves[0].clone();
        original
            .apply(mover, &move_action(&uci))
            .expect("a listed move is legal in the original");
        played.push((mover, uci));
    }
    let original_spectator = original.spectator_view();

    // The recovered game: a fresh session from the identical persisted options,
    // replaying the recorded log. Every move must still be legal (no divergence),
    // and the final board must match byte-for-byte — the same seed reproduced the
    // same reveals.
    let mut recovered = registry
        .new_game("jieqi", &options)
        .expect("jieqi is registered");
    for (player, uci) in &played {
        recovered
            .apply(*player, &move_action(uci))
            .expect("the recorded move replays legally on recovery");
    }
    assert_eq!(
        recovered.spectator_view(),
        original_spectator,
        "recovery must reproduce the same seeded jieqi assignment"
    );
}

#[test]
fn prepare_options_seeds_only_jieqi_and_honors_explicit_positions() {
    let registry = registry();

    // A perfect-information variant's options are returned unchanged.
    let untouched = registry
        .prepare_new_game_options("standard", &VariantOptions::default())
        .expect("resolve");
    assert_eq!(untouched, VariantOptions::default());

    // jieqi with an explicit FEN is honored as-is — the seed (present or absent)
    // is never re-randomized, which is what makes the recovery path deterministic.
    let explicit = VariantOptions::new(serde_json::json!({
        "fen": "=d=d=d=dk=d=d=d=d/9/1=d5=d1/=d1=d1=d1=d1=d/9/9/=D1=D1=D1=D1=D/1=D5=D1/9/=D=D=D=DK=D=D=D=D w - - 0 1 42"
    }));
    let resolved = registry
        .prepare_new_game_options("jieqi", &explicit)
        .expect("resolve");
    assert_eq!(
        resolved, explicit,
        "an explicit jieqi position is untouched"
    );
}
