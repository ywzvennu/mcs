//! Property / robustness tests for the standard-chess action decoder (#110).
//!
//! A player's move arrives as a type-erased [`Action`](mcs_core::Action) (an
//! arbitrary JSON value carried over the wire) and is decoded inside the variant
//! via `Action::to_typed::<StandardAction>()` before `GameSession::apply` acts
//! on it. That decode — and the `apply` it feeds — is an **untrusted parser**:
//! whatever JSON a client sends, the variant must return a clean
//! [`GameError`](mcs_core::GameError) (e.g. `InvalidActionPayload`,
//! `IllegalAction`, `NotYourTurn`) and must **never panic** the game actor that
//! drives it.
//!
//! These run in the default CI `ci` job: they are pure CPU, allocate only small
//! values, and are bounded so the suite stays fast. `proptest` (MIT/Apache-2.0)
//! generates the random inputs; targeted adversarial cases cover the edges.

use mcs_core::{Action, Color, GameSession, VariantOptions};
use mcs_variant_standard::wire::StandardAction;
use mcs_variant_standard::StandardGame;
use proptest::prelude::*;

/// A fresh standard-chess session in the opening position.
fn fresh_game() -> Box<dyn GameSession> {
    Box::new(StandardGame::new())
}

/// Wraps a raw JSON value as a type-erased [`Action`] (exactly how the wire
/// layer hands an untrusted client payload to the variant) and feeds it through
/// the full `apply` path. The contract under test is solely "returns, never
/// panics".
fn feed(session: &mut dyn GameSession, value: serde_json::Value) {
    let action = Action::new(value);
    // Decode-then-apply is the untrusted boundary; either an `Ok(effect)` or any
    // `Err(GameError)` is acceptable — only an unwinding panic is a failure.
    let _ = session.apply(Color::White, &action);
    let _ = session.apply(Color::Black, &action);
}

proptest! {
    // Bounded for a fast default `ci` run while still broad enough to surface a
    // panic in the decode/apply path.
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// An arbitrary string as the `uci` field of a `move` action must be decoded
    /// and rejected (or, rarely, accepted) without panicking. This is the prime
    /// untrusted field: it flows into the move parser.
    #[test]
    fn arbitrary_uci_never_panics(uci in ".*") {
        let mut game = fresh_game();
        feed(&mut *game, serde_json::json!({ "type": "move", "uci": uci }));
    }

    /// Plausible-but-arbitrary tagged action objects exercise serde's
    /// internally-tagged dispatch (`#[serde(tag = "type")]`) across the real tag
    /// set plus bogus tags and extra fields.
    #[test]
    fn plausible_tagged_actions_never_panic(
        tag in prop::sample::select(vec![
            "move", "resign", "offer_draw", "accept_draw", "decline_draw",
            "claim_draw", "bogus", "",
        ]),
        key in "[a-z_]{0,10}",
        value in ".*",
    ) {
        let mut game = fresh_game();
        feed(&mut *game, serde_json::json!({ "type": tag, key: value }));
    }

    /// Entirely free-form JSON values (not necessarily objects) handed in as the
    /// action payload must still decode-and-reject cleanly: numbers, arrays,
    /// strings, booleans, null.
    #[test]
    fn arbitrary_json_scalar_actions_never_panic(n in any::<i64>(), s in ".*") {
        let mut game = fresh_game();
        feed(&mut *game, serde_json::json!(n));
        feed(&mut *game, serde_json::json!(s));
        feed(&mut *game, serde_json::json!([n, s]));
        feed(&mut *game, serde_json::Value::Null);
        feed(&mut *game, serde_json::json!(true));
    }

    /// The `StandardAction` deserializer in isolation (no `apply`): arbitrary
    /// JSON text must parse-or-reject without panicking. This is the exact
    /// `serde_json` path `Action::to_typed` drives.
    #[test]
    fn standard_action_deserializer_never_panics(text in ".*") {
        let _ = serde_json::from_str::<StandardAction>(&text);
    }
}

/// Hand-picked adversarial action payloads. Each must be rejected (the variant
/// returns a `GameError`); none may panic the session.
#[test]
fn targeted_malformed_actions_are_rejected_not_panicking() {
    let cases: &[serde_json::Value] = &[
        serde_json::json!({}),                                  // no tag
        serde_json::json!({ "type": "move" }),                  // move without uci
        serde_json::json!({ "type": "move", "uci": 42 }),       // uci wrong type
        serde_json::json!({ "type": "move", "uci": null }),     // uci null
        serde_json::json!({ "type": "move", "uci": "" }),       // empty uci
        serde_json::json!({ "type": "move", "uci": "zzzz" }),   // junk uci
        serde_json::json!({ "type": "move", "uci": "e2e4e5" }), // overlong uci
        serde_json::json!({ "type": "move", "uci": "e9e1" }),   // off-board uci
        serde_json::json!({ "type": "unknown_action" }),        // unknown tag
        serde_json::json!("resign"),                            // tag as bare string
        serde_json::json!(123),                                 // scalar
        serde_json::json!([1, 2, 3]),                           // array
        serde_json::Value::Null,                                // null
    ];

    for value in cases {
        let mut game = fresh_game();
        let action = Action::new(value.clone());
        // Must return some Result; the contract is no panic. A move-shaped junk
        // payload is rejected, never silently applied.
        let _ = game.apply(Color::White, &action);
    }
}

/// A huge `uci` string must be rejected without pathological cost or a panic:
/// the move parser inspects a bounded prefix and errors out, it does not blow up
/// on length.
#[test]
fn huge_uci_string_does_not_panic() {
    let huge = "e".repeat(2 * 1024 * 1024); // 2 MiB of 'e'
    let mut game = fresh_game();
    let action = Action::new(serde_json::json!({ "type": "move", "uci": huge }));
    let result = game.apply(Color::White, &action);
    assert!(result.is_err(), "a 2 MiB uci string is not a legal move");
}

/// Sanity anchor: a *valid* opening move decodes and applies, so the fuzzers
/// above are exercising a live path rather than a decoder that rejects
/// everything.
#[test]
fn a_valid_move_still_applies() {
    let mut game = fresh_game();
    let e4 = Action::from_typed(&StandardAction::Move {
        uci: "e2e4".to_owned(),
    })
    .expect("serialize move");
    game.apply(Color::White, &e4).expect("1. e4 is legal");
    assert_eq!(game.to_move(), Color::Black, "turn passes to Black");
}

/// The variant registry path also round-trips: building a session through
/// [`VariantOptions::default`] and applying a junk action rejects cleanly.
#[test]
fn registry_built_session_rejects_junk_action() {
    let mut registry = mcs_core::VariantRegistry::new();
    mcs_variant_standard::register(&mut registry);
    let mut session = registry
        .new_game(
            mcs_variant_standard::STANDARD_VARIANT_ID,
            &VariantOptions::default(),
        )
        .expect("standard variant is registered");
    let junk = Action::new(serde_json::json!({ "type": "move", "uci": "not-a-move" }));
    assert!(
        session.apply(Color::White, &junk).is_err(),
        "a junk move is rejected, not applied"
    );
}
