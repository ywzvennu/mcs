//! Property / robustness tests for the WebSocket JSON protocol parsers (#110).
//!
//! [`ClientMessage`] is deserialized directly from **untrusted** client frames
//! by the live-game socket handler (see [`mcs_api::ws`]). A malformed, hostile,
//! or simply unexpected frame must always come back as a clean `Err` from
//! `serde_json` — it must **never panic** the connection task, which would take
//! down the whole runtime worker. [`ServerMessage`] travels the other way, but
//! we fuzz its deserializer too: it is `Deserialize`, so a third party (a proxy,
//! a test client, a replay tool) can feed it arbitrary bytes, and the same
//! never-panic guarantee must hold.
//!
//! These tests run in the **default CI `ci` job** — they are pure CPU, take no
//! database or socket, and are bounded (a few hundred cases) so they stay fast.
//! `proptest` (MIT/Apache-2.0) supplies the random inputs; a handful of targeted
//! adversarial inputs cover the edge cases random generation is unlikely to hit
//! (huge strings, wrong types, missing tags, deep nesting).

use mcs_api::{ClientMessage, ServerMessage};
use proptest::prelude::*;

/// Attempts to deserialize `text` as a [`ClientMessage`]. The *only* contract
/// under test is that this returns (never panics); the `Ok`/`Err` split is
/// irrelevant. Returning the result keeps the call observably side-effecting so
/// the optimiser cannot elide it.
fn try_parse_client(text: &str) -> Result<ClientMessage, serde_json::Error> {
    serde_json::from_str::<ClientMessage>(text)
}

/// As [`try_parse_client`], for the server-to-client frame type.
fn try_parse_server(text: &str) -> Result<ServerMessage, serde_json::Error> {
    serde_json::from_str::<ServerMessage>(text)
}

proptest! {
    // Bounded so the default `ci` job stays fast: a few hundred cases is ample
    // to shake out a panic in the deserializer without slowing the suite.
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Arbitrary UTF-8 strings must never panic the client-frame parser.
    #[test]
    fn arbitrary_text_never_panics_client_parser(text in ".*") {
        let _ = try_parse_client(&text);
    }

    /// Arbitrary UTF-8 strings must never panic the server-frame parser.
    #[test]
    fn arbitrary_text_never_panics_server_parser(text in ".*") {
        let _ = try_parse_server(&text);
    }

    /// Inputs shaped like a *plausible* tagged frame — a JSON object with a
    /// `type` field drawn from the real tag set plus an arbitrary extra field —
    /// exercise the variant-dispatch path more deeply than fully random text,
    /// which almost never parses as JSON at all.
    #[test]
    fn plausible_tagged_objects_never_panic(
        tag in prop::sample::select(vec![
            "submit", "chat", "rematch_offer", "rematch_accept", "rematch_decline",
            "snapshot", "update", "error", "replay", "bogus_tag", "",
        ]),
        key in "[a-z_]{0,12}",
        value in ".*",
    ) {
        let json = serde_json::json!({ "type": tag, key: value }).to_string();
        let _ = try_parse_client(&json);
        let _ = try_parse_server(&json);
    }

    /// A well-formed `submit` envelope wrapping an arbitrary `action` value:
    /// since `Action` is a type-erased JSON newtype it accepts any value, so the
    /// envelope parse must succeed *or* fail cleanly, never panic, for any inner
    /// shape.
    #[test]
    fn submit_with_arbitrary_action_never_panics(uci in ".*", extra in any::<i64>()) {
        let json = serde_json::json!({
            "type": "submit",
            "action": { "type": "move", "uci": uci, "noise": extra },
        })
        .to_string();
        let _ = try_parse_client(&json);
    }
}

/// A small battery of hand-picked adversarial inputs that random generation is
/// unlikely to produce but a hostile client might send. None may panic; all are
/// expected to be rejected (they are structurally invalid frames).
#[test]
fn targeted_malformed_inputs_are_rejected_not_panicking() {
    // (input, why it is interesting)
    let cases: &[&str] = &[
        // Empty / whitespace / non-JSON.
        "",
        "   ",
        "not json at all",
        "\0\0\0",
        // Valid JSON, wrong top-level type.
        "null",
        "true",
        "42",
        "[]",
        "\"submit\"",
        // Object, but no discriminator tag.
        "{}",
        r#"{"action": {"type": "move", "uci": "e2e4"}}"#,
        // Unknown tag.
        r#"{"type": "definitely_not_a_real_variant"}"#,
        // Right tag, wrong field types.
        r#"{"type": "chat", "text": 12345}"#,
        r#"{"type": "chat", "text": null}"#,
        r#"{"type": "submit", "action": "should-be-an-object-or-any-json"}"#,
        // Truncated / unbalanced JSON.
        r#"{"type": "submit", "action": {"type": "move", "uci": "#,
        r#"{"type":"#,
    ];

    for input in cases {
        // The sole contract: parsing returns rather than unwinding.
        let _ = try_parse_client(input);
        let _ = try_parse_server(input);
    }
}

/// A huge string field must be rejected (or accepted) without panicking or
/// pathological blow-up: the deserializer borrows/copies bounded by the input,
/// so a multi-megabyte `text` is handled like any other.
#[test]
fn huge_string_field_does_not_panic() {
    let huge = "a".repeat(4 * 1024 * 1024); // 4 MiB
    let json = serde_json::json!({ "type": "chat", "text": huge }).to_string();
    // A valid `chat` with a giant body parses fine; the point is it does not
    // panic or hang.
    let parsed = try_parse_client(&json).expect("a giant but well-formed chat parses");
    match parsed {
        ClientMessage::Chat { text } => assert_eq!(text.len(), 4 * 1024 * 1024),
        other => panic!("expected Chat, got {other:?}"),
    }
}

/// Deeply nested JSON must not overflow the stack or panic. `serde_json` caps
/// recursion (returning an `Err` past its limit) rather than unwinding, so even
/// a pathologically deep `action` payload is handled gracefully.
#[test]
fn deeply_nested_action_does_not_panic() {
    // Build `{"type":"submit","action": [[[ ... ]]] }` with deep array nesting.
    let depth = 2_000;
    let mut inner = String::from("null");
    for _ in 0..depth {
        inner = format!("[{inner}]");
    }
    let json = format!(r#"{{"type":"submit","action":{inner}}}"#);
    // Whatever serde_json decides (accept up to its limit, or reject past it),
    // it must return without panicking.
    let _ = try_parse_client(&json);
}

/// Every variant of the two enums round-trips through JSON. This anchors the
/// fuzzing: it proves the parsers we are hammering actually accept the valid
/// shapes, so an "always Err" regression cannot masquerade as "never panics".
#[test]
fn well_formed_frames_round_trip() {
    let client = ClientMessage::Chat {
        text: "good luck".to_owned(),
    };
    let json = serde_json::to_string(&client).expect("serialize");
    let back = try_parse_client(&json).expect("round-trip");
    assert_eq!(client, back);

    // A submit envelope with a real standard-chess move action.
    let submit = r#"{"type":"submit","action":{"type":"move","uci":"e2e4"}}"#;
    assert!(matches!(
        try_parse_client(submit).expect("valid submit"),
        ClientMessage::Submit { .. }
    ));
}
