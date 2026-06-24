//! Integration tests for [`SqlxStorage`] against an in-memory SQLite pool.
//!
//! Each test connects to a fresh `"sqlite::memory:"` database, which runs the
//! embedded migrations on connect, then exercises one repository's contract.
//! In-memory SQLite needs no filesystem and is torn down when the pool drops,
//! so the tests are hermetic and fast.

use mcs_core::{Action, Color, EndReason, Outcome, VariantOptions};
use mcs_domain::{
    ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Rating, Seek, TimeControl, User,
    UserId,
};
use time::OffsetDateTime;

// The repository methods are invoked through `&dyn Trait` handles returned by
// the `Repositories` accessors, so the individual repo traits need not be in
// scope here.
use crate::{RecordedAction, Repositories, SqlxStorage, StorageError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Connects to a private in-memory SQLite database with migrations applied.
async fn storage() -> SqlxStorage {
    SqlxStorage::connect("sqlite::memory:")
        .await
        .expect("connect + migrate in-memory sqlite")
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
        OffsetDateTime::UNIX_EPOCH,
    )
}

fn sample_seek(creator: UserId) -> Seek {
    Seek::new(
        creator,
        "standard".to_owned(),
        TimeControl::Unlimited,
        ColorPreference::Random,
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

// ---------------------------------------------------------------------------
// Repositories aggregate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repositories_aggregate_is_object_safe() {
    let storage = storage().await;
    let repos: &dyn Repositories = &storage;

    let user = sample_user();
    repos.users().create(&user).await.unwrap();
    assert_eq!(repos.users().get(user.id).await.unwrap(), user);

    let seek = sample_seek(user.id);
    repos.seeks().create(&seek).await.unwrap();
    assert_eq!(repos.seeks().list_open().await.unwrap().len(), 1);
}
