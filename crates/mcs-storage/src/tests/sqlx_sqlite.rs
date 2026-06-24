//! Integration tests for [`SqlxStorage`] against an in-memory SQLite pool.
//!
//! Each test connects to a fresh `"sqlite::memory:"` database, which runs the
//! embedded migrations on connect, then exercises one repository's contract.
//! In-memory SQLite needs no filesystem and is torn down when the pool drops,
//! so the tests are hermetic and fast.

use mcs_core::{Color, EndReason, Outcome};
use mcs_domain::{
    ColorPreference, EvmAddress, Game, GameLifecycle, Seek, TimeControl, User, UserId,
};
use time::OffsetDateTime;

// The repository methods are invoked through `&dyn Trait` handles returned by
// the `Repositories` accessors, so the individual repo traits need not be in
// scope here.
use crate::{Repositories, SqlxStorage, StorageError};

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
