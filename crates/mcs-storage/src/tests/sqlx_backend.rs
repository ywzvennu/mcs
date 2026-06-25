//! Backend-parameterised integration tests for [`SqlxStorage`].
//!
//! The same test bodies run against two backends, selected at runtime by the
//! `MCS_TEST_DATABASE_URL` environment variable (see [`super::harness`]):
//!
//! * **SQLite** (default) — each test gets a private in-memory database, torn
//!   down with the pool.
//! * **Postgres** (CI service) — each test gets a private, uniquely-named schema
//!   on the shared server, dropped on teardown.
//!
//! Either way every test is hermetic: it connects through
//! [`connect_test_storage`], which runs the embedded migrations against a fresh,
//! isolated backend, then exercises one repository's contract. The assertions
//! are identical across backends, so a green run proves the SQL is portable.

use mcs_core::{Action, Color, EndReason, Outcome, VariantOptions};
use mcs_domain::{
    Challenge, ChallengeStatus, ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Rating,
    RatingHistoryEntry, Seek, TimeControl, User, UserId,
};
use time::OffsetDateTime;

use super::harness::{connect_test_storage, TestStorage};
// The repository methods are invoked through `&dyn Trait` handles returned by
// the `Repositories` accessors, so the individual repo traits need not be in
// scope here.
use crate::{RecordedAction, Repositories, StorageError};
use mcs_payments::{PaymentRecord, PaymentStoreError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Connects to a fresh, isolated backend (SQLite or Postgres) with migrations
/// applied. See [`super::harness`] for how isolation is achieved per backend.
async fn storage() -> TestStorage {
    connect_test_storage().await
}

fn address(seed: &str) -> EvmAddress {
    // Build a deterministic, valid 40-hex address from a short seed.
    let mut hex = String::new();
    for b in seed.bytes() {
        hex.push_str(&format!("{b:02x}"));
    }
    while hex.len() < 40 {
        hex.push('0');
    }
    hex.truncate(40);
    format!("0x{hex}").parse().expect("valid address")
}

fn sample_user() -> User {
    User::new(
        address("alice"),
        Some("alice".to_owned()),
        OffsetDateTime::UNIX_EPOCH,
    )
}

fn sample_game(white: UserId, black: UserId) -> Game {
    Game::new(
        "standard".to_owned(),
        VariantOptions::default(),
        white,
        black,
        TimeControl::Unlimited,
        true,
        OffsetDateTime::UNIX_EPOCH,
    )
}

fn sample_seek(creator: UserId) -> Seek {
    Seek::new(
        creator,
        "standard".to_owned(),
        TimeControl::Unlimited,
        ColorPreference::Random,
        true,
        OffsetDateTime::UNIX_EPOCH,
    )
}

fn sample_challenge(challenger: UserId, challenged: UserId) -> Challenge {
    Challenge::new(
        challenger,
        challenged,
        "standard".to_owned(),
        TimeControl::Unlimited,
        true,
        ColorPreference::White,
        OffsetDateTime::UNIX_EPOCH,
    )
}

/// Builds a [`RecordedAction`] at `ply` with a distinct JSON payload and clocks,
/// so a listed action can be checked to round-trip exactly.
fn sample_action(ply: u32, player: Color) -> RecordedAction {
    RecordedAction {
        ply,
        player,
        action: Action::new(serde_json::json!({ "move": format!("e{ply}"), "ply": ply })),
        clock_white_ms: Some(180_000 - u64::from(ply)),
        clock_black_ms: Some(170_000 - u64::from(ply)),
        created_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(i64::from(ply)),
    }
}

// ---------------------------------------------------------------------------
// UserRepo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn user_create_get_round_trip() {
    let storage = storage().await;
    let user = sample_user();

    storage.users().create(&user).await.unwrap();
    let fetched = storage.users().get(user.id).await.unwrap();
    assert_eq!(fetched, user);
}

#[tokio::test]
async fn user_get_missing_is_not_found() {
    let storage = storage().await;
    let err = storage.users().get(UserId::new()).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

#[tokio::test]
async fn user_duplicate_address_is_conflict() {
    let storage = storage().await;
    let user = sample_user();
    storage.users().create(&user).await.unwrap();

    // A different id but the same address must trip the unique index.
    let clash = User::new(user.address.clone(), None, OffsetDateTime::UNIX_EPOCH);
    let err = storage.users().create(&clash).await.unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)));
}

#[tokio::test]
async fn user_find_by_address() {
    let storage = storage().await;
    let user = sample_user();

    assert!(storage
        .users()
        .find_by_address(&user.address)
        .await
        .unwrap()
        .is_none());

    storage.users().create(&user).await.unwrap();
    let found = storage
        .users()
        .find_by_address(&user.address)
        .await
        .unwrap();
    assert_eq!(found, Some(user));
}

#[tokio::test]
async fn user_upsert_creates_then_returns_existing() {
    let storage = storage().await;
    let addr = address("upsert");

    let created = storage.users().upsert_by_address(&addr).await.unwrap();
    assert_eq!(created.address, addr);

    let again = storage.users().upsert_by_address(&addr).await.unwrap();
    assert_eq!(created.id, again.id);
}

// ---------------------------------------------------------------------------
// GameRepo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn game_create_get_round_trip() {
    let storage = storage().await;
    let game = sample_game(UserId::new(), UserId::new());

    storage.games().create(&game).await.unwrap();
    let fetched = storage.games().get(game.id).await.unwrap();
    assert_eq!(fetched, game);
}

#[tokio::test]
async fn game_get_missing_is_not_found() {
    let storage = storage().await;
    let err = storage
        .games()
        .get(mcs_domain::GameId::new())
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

#[tokio::test]
async fn game_update_lifecycle_and_outcome() {
    let storage = storage().await;
    let mut game = sample_game(UserId::new(), UserId::new());
    storage.games().create(&game).await.unwrap();

    game.lifecycle = GameLifecycle::Active;
    storage.games().update(&game).await.unwrap();
    assert_eq!(
        storage.games().get(game.id).await.unwrap().lifecycle,
        GameLifecycle::Active
    );

    let later = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(3600);
    game.finish(Outcome::win(Color::White, EndReason::Checkmate), later);
    storage.games().update(&game).await.unwrap();

    let fetched = storage.games().get(game.id).await.unwrap();
    assert_eq!(fetched.lifecycle, GameLifecycle::Finished);
    assert_eq!(
        fetched.outcome,
        Some(Outcome::win(Color::White, EndReason::Checkmate))
    );
    assert_eq!(fetched.updated_at, later);
}

#[tokio::test]
async fn game_update_missing_is_not_found() {
    let storage = storage().await;
    let game = sample_game(UserId::new(), UserId::new());
    let err = storage.games().update(&game).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

#[tokio::test]
async fn game_list_recent_is_newest_first_and_limited() {
    let storage = storage().await;
    let white = UserId::new();
    let black = UserId::new();

    // Stagger creation timestamps so ordering is observable.
    for i in 0..5 {
        let mut game = sample_game(white, black);
        game.created_at = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(i);
        storage.games().create(&game).await.unwrap();
    }

    let recent = storage.games().list_recent(3).await.unwrap();
    assert_eq!(recent.len(), 3);
    // Strictly descending by created_at.
    assert!(recent
        .windows(2)
        .all(|w| w[0].created_at >= w[1].created_at));
    assert_eq!(
        recent[0].created_at,
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(4)
    );
}

#[tokio::test]
async fn game_list_for_user_matches_both_colours() {
    let storage = storage().await;
    let alice = UserId::new();
    let bob = UserId::new();
    let carol = UserId::new();

    storage
        .games()
        .create(&sample_game(alice, bob))
        .await
        .unwrap();
    storage
        .games()
        .create(&sample_game(carol, bob))
        .await
        .unwrap();
    storage
        .games()
        .create(&sample_game(alice, carol))
        .await
        .unwrap();

    assert_eq!(
        storage
            .games()
            .list_for_user(alice, 10)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        storage.games().list_for_user(bob, 10).await.unwrap().len(),
        2
    );
    assert_eq!(
        storage
            .games()
            .list_for_user(carol, 10)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn game_variant_options_round_trip() {
    let storage = storage().await;
    let mut game = sample_game(UserId::new(), UserId::new());
    game.variant_options = VariantOptions::new(serde_json::json!({
        "starting_fen": "8/8/8/8/8/8/8/8 w - - 0 1",
        "increment_ms": 2000,
    }));

    storage.games().create(&game).await.unwrap();
    let fetched = storage.games().get(game.id).await.unwrap();
    assert_eq!(fetched.variant_options, game.variant_options);
    assert_eq!(fetched, game);
}

#[tokio::test]
async fn game_rated_flag_round_trips_for_both_values() {
    let storage = storage().await;

    // The default-constructed game is rated; persist and read it back.
    let rated = sample_game(UserId::new(), UserId::new());
    assert!(rated.rated, "sample game defaults to rated");
    storage.games().create(&rated).await.unwrap();
    assert!(storage.games().get(rated.id).await.unwrap().rated);

    // A casual game round-trips as casual.
    let mut casual = sample_game(UserId::new(), UserId::new());
    casual.rated = false;
    storage.games().create(&casual).await.unwrap();
    let fetched = storage.games().get(casual.id).await.unwrap();
    assert!(!fetched.rated);
    assert_eq!(fetched, casual);
}

#[tokio::test]
async fn game_rated_flag_survives_update() {
    let storage = storage().await;
    let mut game = sample_game(UserId::new(), UserId::new());
    game.rated = false;
    storage.games().create(&game).await.unwrap();

    // An unrelated update must not disturb the persisted `rated` flag.
    game.lifecycle = GameLifecycle::Active;
    storage.games().update(&game).await.unwrap();
    assert!(!storage.games().get(game.id).await.unwrap().rated);
}

#[tokio::test]
async fn seek_rated_flag_round_trips_for_both_values() {
    let storage = storage().await;

    let rated = sample_seek(UserId::new());
    assert!(rated.rated, "sample seek defaults to rated");
    storage.seeks().create(&rated).await.unwrap();
    assert_eq!(storage.seeks().get(rated.id).await.unwrap(), Some(rated));

    let mut casual = sample_seek(UserId::new());
    casual.rated = false;
    storage.seeks().create(&casual).await.unwrap();
    let fetched = storage.seeks().get(casual.id).await.unwrap();
    assert_eq!(fetched.as_ref().map(|s| s.rated), Some(false));
    assert_eq!(fetched, Some(casual));
}

#[tokio::test]
async fn game_snapshot_round_trip() {
    let storage = storage().await;
    let mut game = sample_game(UserId::new(), UserId::new());
    game.lifecycle = GameLifecycle::Active;
    game.update_snapshot(
        15,
        Some(178_500),
        Some(201_250),
        Some(Color::Black),
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(120),
    );

    storage.games().create(&game).await.unwrap();
    let fetched = storage.games().get(game.id).await.unwrap();
    assert_eq!(fetched.ply, 15);
    assert_eq!(fetched.clock_white_ms, Some(178_500));
    assert_eq!(fetched.clock_black_ms, Some(201_250));
    assert_eq!(fetched.side_to_move, Some(Color::Black));
    assert_eq!(fetched, game);
}

#[tokio::test]
async fn game_snapshot_survives_update() {
    let storage = storage().await;
    let mut game = sample_game(UserId::new(), UserId::new());
    game.lifecycle = GameLifecycle::Active;
    storage.games().create(&game).await.unwrap();

    // Advance the live snapshot and persist via `update`.
    game.update_snapshot(
        3,
        None,
        None,
        Some(Color::White),
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(5),
    );
    storage.games().update(&game).await.unwrap();

    let fetched = storage.games().get(game.id).await.unwrap();
    assert_eq!(fetched.ply, 3);
    assert!(fetched.clock_white_ms.is_none());
    assert_eq!(fetched.side_to_move, Some(Color::White));
}

#[tokio::test]
async fn game_list_unfinished_excludes_finished_includes_others() {
    let storage = storage().await;
    let white = UserId::new();
    let black = UserId::new();

    // Created (oldest), then active, then finished (newest).
    let mut created = sample_game(white, black);
    created.created_at = OffsetDateTime::UNIX_EPOCH;
    storage.games().create(&created).await.unwrap();

    let mut active = sample_game(white, black);
    active.lifecycle = GameLifecycle::Active;
    active.created_at = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1);
    storage.games().create(&active).await.unwrap();

    let mut finished = sample_game(white, black);
    finished.created_at = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(2);
    finished.finish(
        Outcome::win(Color::White, EndReason::Checkmate),
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(3),
    );
    storage.games().create(&finished).await.unwrap();

    let unfinished = storage.games().list_unfinished().await.unwrap();
    assert_eq!(unfinished.len(), 2);
    // Oldest first.
    assert_eq!(unfinished[0].id, created.id);
    assert_eq!(unfinished[1].id, active.id);
    assert!(unfinished
        .iter()
        .all(|g| g.lifecycle != GameLifecycle::Finished));
}

// ---------------------------------------------------------------------------
// ActionLogRepo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn action_log_append_list_preserves_order_payload_and_clocks() {
    let storage = storage().await;
    let game = GameId::new();

    // Append out of ply order to prove `list` reorders by ply ascending.
    let a0 = sample_action(0, Color::White);
    let a1 = sample_action(1, Color::Black);
    let a2 = sample_action(2, Color::White);
    storage.actions().append(game, &a2).await.unwrap();
    storage.actions().append(game, &a0).await.unwrap();
    storage.actions().append(game, &a1).await.unwrap();

    let listed = storage.actions().list(game).await.unwrap();
    // Ply-ascending order, with the exact actions and clocks preserved.
    assert_eq!(listed, vec![a0.clone(), a1, a2]);
    // The exact `Action` JSON survives the TEXT column round-trip.
    assert_eq!(
        listed[0].action.as_value(),
        &serde_json::json!({ "move": "e0", "ply": 0 })
    );
    assert_eq!(listed[0].clock_white_ms, Some(180_000));
    assert_eq!(listed[0].clock_black_ms, Some(170_000));
}

#[tokio::test]
async fn action_log_last_ply_tracks_max_and_empty_is_none() {
    let storage = storage().await;
    let game = GameId::new();

    assert_eq!(storage.actions().last_ply(game).await.unwrap(), None);

    storage
        .actions()
        .append(game, &sample_action(0, Color::White))
        .await
        .unwrap();
    storage
        .actions()
        .append(game, &sample_action(1, Color::Black))
        .await
        .unwrap();
    assert_eq!(storage.actions().last_ply(game).await.unwrap(), Some(1));
}

#[tokio::test]
async fn action_log_duplicate_ply_is_conflict() {
    let storage = storage().await;
    let game = GameId::new();

    storage
        .actions()
        .append(game, &sample_action(0, Color::White))
        .await
        .unwrap();
    let err = storage
        .actions()
        .append(game, &sample_action(0, Color::Black))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)));
}

#[tokio::test]
async fn action_log_empty_game_returns_empty_and_none() {
    let storage = storage().await;
    let game = GameId::new();
    assert!(storage.actions().list(game).await.unwrap().is_empty());
    assert_eq!(storage.actions().last_ply(game).await.unwrap(), None);
}

#[tokio::test]
async fn action_log_untimed_clocks_round_trip_as_none() {
    let storage = storage().await;
    let game = GameId::new();
    let action = RecordedAction {
        ply: 0,
        player: Color::White,
        action: Action::new(serde_json::json!({ "resign": true })),
        clock_white_ms: None,
        clock_black_ms: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
    };
    storage.actions().append(game, &action).await.unwrap();

    let listed = storage.actions().list(game).await.unwrap();
    assert_eq!(listed, vec![action]);
}

#[tokio::test]
async fn action_log_listed_action_round_trips_through_apply_shape() {
    // Proves a stored action can be fed back into a `GameSession::apply` shape:
    // its JSON value survives the store/list round-trip unchanged, so the same
    // bytes the variant produced are the bytes it would receive on replay.
    let storage = storage().await;
    let game = GameId::new();

    let original = serde_json::json!({
        "kind": "move",
        "from": "e2",
        "to": "e4",
        "promotion": serde_json::Value::Null,
    });
    let action = RecordedAction {
        ply: 0,
        player: Color::White,
        action: Action::new(original.clone()),
        clock_white_ms: Some(120_000),
        clock_black_ms: Some(120_000),
        created_at: OffsetDateTime::UNIX_EPOCH,
    };
    storage.actions().append(game, &action).await.unwrap();

    let listed = storage.actions().list(game).await.unwrap();
    let replayed: &Action = &listed[0].action;
    // The value handed to `apply` on replay is byte-for-byte the original.
    assert_eq!(replayed.as_value(), &original);
}

// ---------------------------------------------------------------------------
// SeekRepo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn seek_create_get_remove() {
    let storage = storage().await;
    let seek = sample_seek(UserId::new());

    storage.seeks().create(&seek).await.unwrap();
    assert_eq!(
        storage.seeks().get(seek.id).await.unwrap(),
        Some(seek.clone())
    );

    storage.seeks().remove(seek.id).await.unwrap();
    assert!(storage.seeks().get(seek.id).await.unwrap().is_none());
}

#[tokio::test]
async fn seek_remove_is_idempotent() {
    let storage = storage().await;
    let seek = sample_seek(UserId::new());
    storage.seeks().create(&seek).await.unwrap();
    storage.seeks().remove(seek.id).await.unwrap();
    // Removing again must not error.
    storage.seeks().remove(seek.id).await.unwrap();
}

#[tokio::test]
async fn seek_list_open() {
    let storage = storage().await;
    let a = sample_seek(UserId::new());
    let b = sample_seek(UserId::new());
    let c = sample_seek(UserId::new());
    storage.seeks().create(&a).await.unwrap();
    storage.seeks().create(&b).await.unwrap();
    storage.seeks().create(&c).await.unwrap();
    storage.seeks().remove(b.id).await.unwrap();

    let open = storage.seeks().list_open().await.unwrap();
    assert_eq!(open.len(), 2);
    assert!(open.iter().any(|s| s.id == a.id));
    assert!(open.iter().any(|s| s.id == c.id));
}

// ---------------------------------------------------------------------------
// ChallengeRepo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn challenge_create_get_round_trip() {
    let storage = storage().await;
    let challenge = sample_challenge(UserId::new(), UserId::new());

    storage.challenges().create(&challenge).await.unwrap();
    let fetched = storage.challenges().get(challenge.id).await.unwrap();
    assert_eq!(fetched, challenge);
}

#[tokio::test]
async fn challenge_get_missing_is_not_found() {
    let storage = storage().await;
    let err = storage
        .challenges()
        .get(mcs_domain::ChallengeId::new())
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

#[tokio::test]
async fn challenge_list_incoming_and_outgoing_filter_by_party_and_pending() {
    let storage = storage().await;
    let alice = UserId::new();
    let bob = UserId::new();
    let carol = UserId::new();

    // Alice → Bob, and Carol → Bob: both incoming for Bob, outgoing for their
    // respective challengers.
    let a_to_b = sample_challenge(alice, bob);
    let c_to_b = sample_challenge(carol, bob);
    // Alice → Carol: outgoing for Alice, incoming for Carol.
    let a_to_c = sample_challenge(alice, carol);
    for c in [&a_to_b, &c_to_b, &a_to_c] {
        storage.challenges().create(c).await.unwrap();
    }

    let bob_in = storage.challenges().list_incoming(bob).await.unwrap();
    assert_eq!(bob_in.len(), 2);
    assert!(bob_in.iter().all(|c| c.challenged == bob));

    let alice_out = storage.challenges().list_outgoing(alice).await.unwrap();
    assert_eq!(alice_out.len(), 2);
    assert!(alice_out.iter().all(|c| c.challenger == alice));

    // Alice has no incoming; Carol has exactly one outgoing.
    assert!(storage
        .challenges()
        .list_incoming(alice)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        storage
            .challenges()
            .list_outgoing(carol)
            .await
            .unwrap()
            .len(),
        1
    );

    // A non-pending challenge drops out of both listings.
    let mut declined = a_to_b.clone();
    declined.decline();
    storage.challenges().update(&declined).await.unwrap();
    assert_eq!(
        storage.challenges().list_incoming(bob).await.unwrap().len(),
        1
    );
    assert_eq!(
        storage
            .challenges()
            .list_outgoing(alice)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn challenge_update_accept_records_status_and_game() {
    let storage = storage().await;
    let mut challenge = sample_challenge(UserId::new(), UserId::new());
    storage.challenges().create(&challenge).await.unwrap();

    let game = GameId::new();
    assert!(challenge.accept(game));
    storage.challenges().update(&challenge).await.unwrap();

    let fetched = storage.challenges().get(challenge.id).await.unwrap();
    assert_eq!(fetched.status, ChallengeStatus::Accepted);
    assert_eq!(fetched.game_id, Some(game));
    assert_eq!(fetched, challenge);
}

#[tokio::test]
async fn challenge_update_missing_is_not_found() {
    let storage = storage().await;
    let challenge = sample_challenge(UserId::new(), UserId::new());
    let err = storage.challenges().update(&challenge).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

#[tokio::test]
async fn challenge_rated_and_color_round_trip() {
    let storage = storage().await;
    let mut casual = sample_challenge(UserId::new(), UserId::new());
    casual.rated = false;
    casual.color_preference = ColorPreference::Random;
    storage.challenges().create(&casual).await.unwrap();
    let fetched = storage.challenges().get(casual.id).await.unwrap();
    assert!(!fetched.rated);
    assert_eq!(fetched.color_preference, ColorPreference::Random);
    assert_eq!(fetched, casual);
}

// ---------------------------------------------------------------------------
// SessionRepo — nonce replay-prevention contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nonce_happy_path_then_replay_rejected() {
    let storage = storage().await;
    let addr = address("nonce");
    let expires = OffsetDateTime::now_utc() + time::Duration::minutes(10);

    storage
        .sessions()
        .store_nonce(&addr, "tok", expires)
        .await
        .unwrap();

    assert!(storage
        .sessions()
        .consume_nonce(&addr, "tok")
        .await
        .unwrap());
    // Replay: the nonce was deleted by the first consume.
    assert!(!storage
        .sessions()
        .consume_nonce(&addr, "tok")
        .await
        .unwrap());
}

#[tokio::test]
async fn nonce_expired_is_rejected() {
    let storage = storage().await;
    let addr = address("expired");

    storage
        .sessions()
        .store_nonce(&addr, "old", OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    assert!(!storage
        .sessions()
        .consume_nonce(&addr, "old")
        .await
        .unwrap());
}

#[tokio::test]
async fn nonce_unknown_is_rejected() {
    let storage = storage().await;
    let addr = address("ghost");
    assert!(!storage
        .sessions()
        .consume_nonce(&addr, "never_stored")
        .await
        .unwrap());
}

#[tokio::test]
async fn nonce_store_supersedes_previous_expiry() {
    let storage = storage().await;
    let addr = address("res_tore");

    // Store expired, then re-store the same (address, nonce) with a future
    // expiry: the second store must win.
    storage
        .sessions()
        .store_nonce(&addr, "tok", OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    storage
        .sessions()
        .store_nonce(
            &addr,
            "tok",
            OffsetDateTime::now_utc() + time::Duration::minutes(5),
        )
        .await
        .unwrap();

    assert!(storage
        .sessions()
        .consume_nonce(&addr, "tok")
        .await
        .unwrap());
}

// ---------------------------------------------------------------------------
// RevokedTokenRepo — logout denylist integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoked_token_revoke_then_is_revoked() {
    let storage = storage().await;
    let expires = OffsetDateTime::now_utc() + time::Duration::hours(1);

    assert!(!storage.revoked_tokens().is_revoked("jti-a").await.unwrap());
    storage
        .revoked_tokens()
        .revoke("jti-a", expires)
        .await
        .unwrap();
    assert!(storage.revoked_tokens().is_revoked("jti-a").await.unwrap());
    // A different, non-revoked token stays valid.
    assert!(!storage.revoked_tokens().is_revoked("jti-b").await.unwrap());
}

#[tokio::test]
async fn revoked_token_revoke_is_idempotent() {
    let storage = storage().await;
    let expires = OffsetDateTime::now_utc() + time::Duration::hours(1);
    storage
        .revoked_tokens()
        .revoke("jti", expires)
        .await
        .unwrap();
    // Re-revoking the same jti must not raise a conflict.
    storage
        .revoked_tokens()
        .revoke("jti", expires)
        .await
        .unwrap();
    assert!(storage.revoked_tokens().is_revoked("jti").await.unwrap());
}

#[tokio::test]
async fn revoked_token_purge_expired_drops_only_elapsed() {
    let storage = storage().await;
    let now = OffsetDateTime::now_utc();
    storage
        .revoked_tokens()
        .revoke("old", now - time::Duration::hours(1))
        .await
        .unwrap();
    storage
        .revoked_tokens()
        .revoke("fresh", now + time::Duration::hours(1))
        .await
        .unwrap();

    let removed = storage.revoked_tokens().purge_expired(now).await.unwrap();
    assert_eq!(removed, 1);
    assert!(!storage.revoked_tokens().is_revoked("old").await.unwrap());
    assert!(storage.revoked_tokens().is_revoked("fresh").await.unwrap());
}

// ---------------------------------------------------------------------------
// RatingRepo — SQLite integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rating_get_missing_returns_none() {
    let storage = storage().await;
    let result = storage
        .ratings()
        .get(UserId::new(), "standard")
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn rating_upsert_then_get_round_trip() {
    let storage = storage().await;
    let user = UserId::new();
    let rating = Rating {
        value: 1750.0,
        deviation: 220.0,
        volatility: 0.055,
    };

    storage
        .ratings()
        .upsert(user, "standard", &rating)
        .await
        .unwrap();
    let fetched = storage
        .ratings()
        .get(user, "standard")
        .await
        .unwrap()
        .expect("rating must exist after upsert");

    assert_eq!(fetched.value, rating.value);
    assert_eq!(fetched.deviation, rating.deviation);
    assert_eq!(fetched.volatility, rating.volatility);
}

#[tokio::test]
async fn rating_upsert_overwrites_existing() {
    let storage = storage().await;
    let user = UserId::new();

    storage
        .ratings()
        .upsert(
            user,
            "standard",
            &Rating {
                value: 1500.0,
                deviation: 350.0,
                volatility: 0.06,
            },
        )
        .await
        .unwrap();

    let updated = Rating {
        value: 1650.0,
        deviation: 150.0,
        volatility: 0.05,
    };
    storage
        .ratings()
        .upsert(user, "standard", &updated)
        .await
        .unwrap();

    let fetched = storage
        .ratings()
        .get(user, "standard")
        .await
        .unwrap()
        .expect("rating must exist");
    assert_eq!(fetched.value, updated.value);
    assert_eq!(fetched.deviation, updated.deviation);
    assert_eq!(fetched.volatility, updated.volatility);
}

#[tokio::test]
async fn rating_leaderboard_ordering_and_limit() {
    let storage = storage().await;
    let values = [1200.0_f64, 1800.0, 1500.0, 2000.0, 1100.0];
    for v in &values {
        let user = UserId::new();
        storage
            .ratings()
            .upsert(
                user,
                "standard",
                &Rating {
                    value: *v,
                    deviation: 200.0,
                    volatility: 0.06,
                },
            )
            .await
            .unwrap();
    }

    let board = storage.ratings().leaderboard("standard", 3).await.unwrap();
    assert_eq!(board.len(), 3);
    // Must be non-ascending by value.
    assert!(board.windows(2).all(|w| w[0].1.value >= w[1].1.value));
    assert_eq!(board[0].1.value, 2000.0);
}

#[tokio::test]
async fn rating_leaderboard_empty_variant_returns_empty() {
    let storage = storage().await;
    let board = storage
        .ratings()
        .leaderboard("nonexistent", 10)
        .await
        .unwrap();
    assert!(board.is_empty());
}

#[tokio::test]
async fn rating_list_for_user_returns_all_variants() {
    let storage = storage().await;
    let user = UserId::new();
    let other = UserId::new();

    storage
        .ratings()
        .upsert(user, "standard", &Rating::default())
        .await
        .unwrap();
    storage
        .ratings()
        .upsert(
            user,
            "chess960",
            &Rating {
                value: 1620.0,
                deviation: 110.0,
                volatility: 0.05,
            },
        )
        .await
        .unwrap();
    // A different user's rating must not appear.
    storage
        .ratings()
        .upsert(other, "standard", &Rating::default())
        .await
        .unwrap();

    let ratings = storage.ratings().list_for_user(user).await.unwrap();
    assert_eq!(ratings.len(), 2);
    // The sqlx impl orders by variant_id ascending.
    assert_eq!(ratings[0].0, "chess960");
    assert_eq!(ratings[1].0, "standard");

    assert!(storage
        .ratings()
        .list_for_user(UserId::new())
        .await
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// UserRepo::set_username — SQLite/Postgres integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_username_sets_changes_and_clears_via_reassign() {
    let storage = storage().await;
    let user = User::new(address("uname"), None, OffsetDateTime::UNIX_EPOCH);
    storage.users().create(&user).await.unwrap();

    storage
        .users()
        .set_username(user.id, "alice")
        .await
        .unwrap();
    assert_eq!(
        storage
            .users()
            .get(user.id)
            .await
            .unwrap()
            .username
            .as_deref(),
        Some("alice")
    );

    // Change to a new name.
    storage.users().set_username(user.id, "bob").await.unwrap();
    assert_eq!(
        storage
            .users()
            .get(user.id)
            .await
            .unwrap()
            .username
            .as_deref(),
        Some("bob")
    );

    // Re-assign the same name in a different casing: a no-op success, since it
    // only collides with the user's own row.
    storage.users().set_username(user.id, "BOB").await.unwrap();
    assert_eq!(
        storage
            .users()
            .get(user.id)
            .await
            .unwrap()
            .username
            .as_deref(),
        Some("BOB")
    );
}

#[tokio::test]
async fn set_username_case_insensitive_uniqueness_conflict() {
    let storage = storage().await;
    let alice = User::new(address("alice_u"), None, OffsetDateTime::UNIX_EPOCH);
    let bob = User::new(address("bob_u"), None, OffsetDateTime::UNIX_EPOCH);
    storage.users().create(&alice).await.unwrap();
    storage.users().create(&bob).await.unwrap();

    storage
        .users()
        .set_username(alice.id, "Carol")
        .await
        .unwrap();
    // Bob requests the same name in a different casing.
    let err = storage
        .users()
        .set_username(bob.id, "carol")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)));
}

#[tokio::test]
async fn set_username_unknown_user_is_not_found() {
    let storage = storage().await;
    let err = storage
        .users()
        .set_username(UserId::new(), "ghost")
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

// ---------------------------------------------------------------------------
// RatingHistoryRepo — SQLite/Postgres integration tests
// ---------------------------------------------------------------------------

fn history_entry(user: UserId, variant: &str, value: f64, secs: i64) -> RatingHistoryEntry {
    RatingHistoryEntry {
        user_id: user,
        variant_id: variant.to_owned(),
        value,
        deviation: 100.0,
        game_id: GameId::new(),
        created_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(secs),
    }
}

#[tokio::test]
async fn rating_history_record_and_list_most_recent_first() {
    let storage = storage().await;
    let user = UserId::new();

    storage
        .rating_history()
        .record(&history_entry(user, "standard", 1500.0, 0))
        .await
        .unwrap();
    storage
        .rating_history()
        .record(&history_entry(user, "standard", 1520.0, 10))
        .await
        .unwrap();
    storage
        .rating_history()
        .record(&history_entry(user, "standard", 1490.0, 20))
        .await
        .unwrap();

    let listed = storage
        .rating_history()
        .list(user, "standard", 10)
        .await
        .unwrap();
    assert_eq!(listed.len(), 3);
    // Most-recent-first.
    assert_eq!(listed[0].value, 1490.0);
    assert_eq!(listed[1].value, 1520.0);
    assert_eq!(listed[2].value, 1500.0);

    // The limit truncates after ordering.
    let limited = storage
        .rating_history()
        .list(user, "standard", 2)
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].value, 1490.0);
}

#[tokio::test]
async fn rating_history_round_trips_entry_fields() {
    let storage = storage().await;
    let entry = history_entry(UserId::new(), "standard", 1612.5, 42);
    storage.rating_history().record(&entry).await.unwrap();

    let listed = storage
        .rating_history()
        .list(entry.user_id, "standard", 10)
        .await
        .unwrap();
    assert_eq!(listed, vec![entry]);
}

#[tokio::test]
async fn rating_history_scoped_per_user_and_variant() {
    let storage = storage().await;
    let user = UserId::new();
    let other = UserId::new();

    storage
        .rating_history()
        .record(&history_entry(user, "standard", 1500.0, 0))
        .await
        .unwrap();
    storage
        .rating_history()
        .record(&history_entry(user, "chess960", 1600.0, 0))
        .await
        .unwrap();
    storage
        .rating_history()
        .record(&history_entry(other, "standard", 1400.0, 0))
        .await
        .unwrap();

    assert_eq!(
        storage
            .rating_history()
            .list(user, "standard", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(storage
        .rating_history()
        .list(user, "atomic", 10)
        .await
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// Retention / GC purge methods
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nonce_purge_expired_removes_only_elapsed() {
    let storage = storage().await;
    let addr = address("purge_nonce");
    let now = OffsetDateTime::now_utc();

    storage
        .sessions()
        .store_nonce(&addr, "expired", now - time::Duration::minutes(5))
        .await
        .unwrap();
    storage
        .sessions()
        .store_nonce(&addr, "live", now + time::Duration::minutes(5))
        .await
        .unwrap();

    let removed = storage.sessions().purge_expired_nonces(now).await.unwrap();
    assert_eq!(removed, 1, "only the expired nonce is purged");
    // Live nonce is still consumable.
    assert!(storage
        .sessions()
        .consume_nonce(&addr, "live")
        .await
        .unwrap());
    // Expired nonce is gone.
    assert!(!storage
        .sessions()
        .consume_nonce(&addr, "expired")
        .await
        .unwrap());
}

#[tokio::test]
async fn seek_purge_stale_removes_only_old_seeks() {
    let storage = storage().await;
    let now = OffsetDateTime::now_utc();

    // Old seek: created 2 hours ago.
    let mut old = sample_seek(UserId::new());
    old.created_at = now - time::Duration::hours(2);
    storage.seeks().create(&old).await.unwrap();

    // Fresh seek: created just now.
    let mut fresh = sample_seek(UserId::new());
    fresh.created_at = now;
    storage.seeks().create(&fresh).await.unwrap();

    let cutoff = now - time::Duration::hours(1);
    let removed = storage.seeks().purge_stale(cutoff).await.unwrap();
    assert_eq!(removed, 1, "only the stale seek is removed");

    let open = storage.seeks().list_open().await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, fresh.id);
}

#[tokio::test]
async fn challenge_purge_resolved_removes_old_declined_and_canceled_only() {
    let storage = storage().await;
    let now = OffsetDateTime::now_utc();
    let old_ts = now - time::Duration::hours(2);
    let cutoff = now - time::Duration::hours(1);

    // Old declined — purged.
    let mut old_declined = sample_challenge(UserId::new(), UserId::new());
    old_declined.created_at = old_ts;
    storage.challenges().create(&old_declined).await.unwrap();
    old_declined.decline();
    storage.challenges().update(&old_declined).await.unwrap();

    // Old canceled — purged.
    let mut old_canceled = sample_challenge(UserId::new(), UserId::new());
    old_canceled.created_at = old_ts;
    storage.challenges().create(&old_canceled).await.unwrap();
    old_canceled.cancel();
    storage.challenges().update(&old_canceled).await.unwrap();

    // Fresh declined — kept (within retention window).
    let mut fresh_declined = sample_challenge(UserId::new(), UserId::new());
    // created_at defaults to UNIX_EPOCH in sample_challenge, so override
    fresh_declined.created_at = now;
    storage.challenges().create(&fresh_declined).await.unwrap();
    fresh_declined.decline();
    storage.challenges().update(&fresh_declined).await.unwrap();

    // Old pending — kept (not in a resolved terminal state).
    let mut old_pending = sample_challenge(UserId::new(), UserId::new());
    old_pending.created_at = old_ts;
    storage.challenges().create(&old_pending).await.unwrap();

    let removed = storage.challenges().purge_resolved(cutoff).await.unwrap();
    assert_eq!(removed, 2, "old declined and canceled are purged");

    // Fresh declined and old pending still present.
    assert!(storage.challenges().get(fresh_declined.id).await.is_ok());
    assert!(storage.challenges().get(old_pending.id).await.is_ok());
    // Old declined and canceled are gone.
    assert!(matches!(
        storage.challenges().get(old_declined.id).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        storage.challenges().get(old_canceled.id).await,
        Err(StorageError::NotFound)
    ));
}

// ---------------------------------------------------------------------------
// PaymentStore — x402 settled-payment idempotency (#108)
// ---------------------------------------------------------------------------

fn sample_payment(key: &str) -> PaymentRecord {
    PaymentRecord {
        idempotency_key: key.to_owned(),
        payer: "0xPayer".to_owned(),
        amount: "10000".to_owned(),
        asset: "0xUSDC".to_owned(),
        network: "base-sepolia".to_owned(),
        transaction: Some("0xhash".to_owned()),
        resource: "/seeks".to_owned(),
        created_at: OffsetDateTime::UNIX_EPOCH,
    }
}

#[tokio::test]
async fn payment_record_then_find_round_trip() {
    let storage = storage().await;
    let key = "exact:base-sepolia:0xabc";

    assert!(storage.payments().find(key).await.unwrap().is_none());

    let record = sample_payment(key);
    storage.payments().record(&record).await.unwrap();

    let found = storage
        .payments()
        .find(key)
        .await
        .unwrap()
        .expect("record must exist after recording");
    assert_eq!(found, record);
}

#[tokio::test]
async fn payment_duplicate_key_is_conflict() {
    let storage = storage().await;
    let record = sample_payment("exact:base-sepolia:dup");
    storage.payments().record(&record).await.unwrap();

    // The PK on `idempotency_key` makes a second insert the "already recorded"
    // conflict the middleware falls back on.
    let err = storage.payments().record(&record).await.unwrap_err();
    assert!(matches!(err, PaymentStoreError::Conflict));
}

#[tokio::test]
async fn payment_without_transaction_round_trips_as_none() {
    let storage = storage().await;
    let mut record = sample_payment("exact:base-sepolia:notx");
    record.transaction = None;
    storage.payments().record(&record).await.unwrap();

    let found = storage
        .payments()
        .find(&record.idempotency_key)
        .await
        .unwrap()
        .expect("record must exist");
    assert!(found.transaction.is_none());
    assert_eq!(found, record);
}

// ---------------------------------------------------------------------------
// Repositories aggregate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repositories_aggregate_is_object_safe() {
    let storage = storage().await;
    // Borrow through the `Deref` to `SqlxStorage` to prove the aggregate is
    // usable behind a `&dyn Repositories` vtable.
    let repos: &dyn Repositories = &*storage;

    let user = sample_user();
    repos.users().create(&user).await.unwrap();
    assert_eq!(repos.users().get(user.id).await.unwrap(), user);

    let seek = sample_seek(user.id);
    repos.seeks().create(&seek).await.unwrap();
    assert_eq!(repos.seeks().list_open().await.unwrap().len(), 1);
}
