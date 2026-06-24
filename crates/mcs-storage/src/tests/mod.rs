//! In-memory implementations of all repository traits and integration tests.
//!
//! These implementations exist solely to:
//!
//! 1. Prove the traits are **object-safe** (they are used as `&dyn Trait`).
//! 2. Verify the traits are **ergonomically usable** with minimal ceremony.
//! 3. Document the **nonce single-use contract** through an executable test.
//!
//! They are intentionally simple: they use `std::sync::Mutex<HashMap<…>>`
//! rather than `tokio::sync::Mutex`, which is fine for tests because
//! `std::sync::Mutex` is `Send + Sync` and the critical section is tiny.

mod memory;

use mcs_core::{EndReason, Outcome};
use mcs_domain::{
    ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Seek, TimeControl, User, UserId,
};
use memory::{InMemoryRepos, MemoryGameRepo, MemorySeekRepo, MemorySessionRepo, MemoryUserRepo};
use time::OffsetDateTime;

use crate::{GameRepo, Repositories, SeekRepo, SessionRepo, StorageError, UserRepo};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sample_address() -> EvmAddress {
    "0xabcdef1234567890abcdef1234567890abcdef12"
        .parse()
        .unwrap()
}

fn sample_user() -> User {
    User::new(
        sample_address(),
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
// UserRepo tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn user_repo_create_and_get() {
    let repo = MemoryUserRepo::default();
    let user = sample_user();
    repo.create(&user).await.unwrap();
    let fetched = repo.get(user.id).await.unwrap();
    assert_eq!(fetched, user);
}

#[tokio::test]
async fn user_repo_get_missing_returns_not_found() {
    let repo = MemoryUserRepo::default();
    let err = repo.get(UserId::new()).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

#[tokio::test]
async fn user_repo_create_conflict() {
    let repo = MemoryUserRepo::default();
    let user = sample_user();
    repo.create(&user).await.unwrap();
    let err = repo.create(&user).await.unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)));
}

#[tokio::test]
async fn user_repo_find_by_address() {
    let repo = MemoryUserRepo::default();
    let user = sample_user();
    // not yet in the store
    let result = repo.find_by_address(&user.address).await.unwrap();
    assert!(result.is_none());
    // insert and look up
    repo.create(&user).await.unwrap();
    let result = repo.find_by_address(&user.address).await.unwrap();
    assert_eq!(result, Some(user));
}

#[tokio::test]
async fn user_repo_upsert_creates_then_returns_existing() {
    let repo = MemoryUserRepo::default();
    let addr: EvmAddress = "0x1111111111111111111111111111111111111111"
        .parse()
        .unwrap();

    // first call: creates a new user
    let created = repo.upsert_by_address(&addr).await.unwrap();
    assert_eq!(created.address, addr);

    // second call: returns the same user
    let fetched = repo.upsert_by_address(&addr).await.unwrap();
    assert_eq!(created.id, fetched.id);
}

// ---------------------------------------------------------------------------
// GameRepo tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn game_repo_create_and_get() {
    let repo = MemoryGameRepo::default();
    let white = UserId::new();
    let black = UserId::new();
    let game = sample_game(white, black);
    repo.create(&game).await.unwrap();
    let fetched = repo.get(game.id).await.unwrap();
    assert_eq!(fetched, game);
}

#[tokio::test]
async fn game_repo_get_missing_returns_not_found() {
    let repo = MemoryGameRepo::default();
    let err = repo.get(GameId::new()).await.unwrap_err();
    assert!(matches!(err, StorageError::NotFound));
}

#[tokio::test]
async fn game_repo_update_lifecycle() {
    let repo = MemoryGameRepo::default();
    let white = UserId::new();
    let black = UserId::new();
    let mut game = sample_game(white, black);
    repo.create(&game).await.unwrap();

    game.lifecycle = GameLifecycle::Active;
    repo.update(&game).await.unwrap();

    let fetched = repo.get(game.id).await.unwrap();
    assert_eq!(fetched.lifecycle, GameLifecycle::Active);
}

#[tokio::test]
async fn game_repo_update_finish() {
    let repo = MemoryGameRepo::default();
    let white = UserId::new();
    let black = UserId::new();
    let mut game = sample_game(white, black);
    repo.create(&game).await.unwrap();

    let later = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(3600);
    game.finish(
        Outcome::win(mcs_core::Color::White, EndReason::Checkmate),
        later,
    );
    repo.update(&game).await.unwrap();

    let fetched = repo.get(game.id).await.unwrap();
    assert_eq!(fetched.lifecycle, GameLifecycle::Finished);
    assert!(fetched.outcome.is_some());
}

#[tokio::test]
async fn game_repo_list_recent_respects_limit() {
    let repo = MemoryGameRepo::default();
    let uid = UserId::new();
    let uid2 = UserId::new();
    for _ in 0..5 {
        repo.create(&sample_game(uid, uid2)).await.unwrap();
    }
    let list = repo.list_recent(3).await.unwrap();
    assert_eq!(list.len(), 3);
}

#[tokio::test]
async fn game_repo_list_for_user() {
    let repo = MemoryGameRepo::default();
    let alice = UserId::new();
    let bob = UserId::new();
    let carol = UserId::new();

    repo.create(&sample_game(alice, bob)).await.unwrap();
    repo.create(&sample_game(carol, bob)).await.unwrap();
    repo.create(&sample_game(alice, carol)).await.unwrap();

    let alice_games = repo.list_for_user(alice, 10).await.unwrap();
    assert_eq!(alice_games.len(), 2);

    let bob_games = repo.list_for_user(bob, 10).await.unwrap();
    assert_eq!(bob_games.len(), 2);

    let carol_games = repo.list_for_user(carol, 10).await.unwrap();
    assert_eq!(carol_games.len(), 2);
}

// ---------------------------------------------------------------------------
// SeekRepo tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn seek_repo_create_get_remove() {
    let repo = MemorySeekRepo::default();
    let uid = UserId::new();
    let seek = sample_seek(uid);

    repo.create(&seek).await.unwrap();
    let fetched = repo.get(seek.id).await.unwrap();
    assert_eq!(fetched, Some(seek.clone()));

    repo.remove(seek.id).await.unwrap();
    let after = repo.get(seek.id).await.unwrap();
    assert!(after.is_none());
}

#[tokio::test]
async fn seek_repo_remove_idempotent() {
    let repo = MemorySeekRepo::default();
    let seek = sample_seek(UserId::new());
    repo.create(&seek).await.unwrap();
    repo.remove(seek.id).await.unwrap();
    // second remove must not error
    repo.remove(seek.id).await.unwrap();
}

#[tokio::test]
async fn seek_repo_list_open() {
    let repo = MemorySeekRepo::default();
    let a = sample_seek(UserId::new());
    let b = sample_seek(UserId::new());
    let c = sample_seek(UserId::new());
    repo.create(&a).await.unwrap();
    repo.create(&b).await.unwrap();
    repo.create(&c).await.unwrap();
    repo.remove(b.id).await.unwrap();

    let open = repo.list_open().await.unwrap();
    assert_eq!(open.len(), 2);
    assert!(open.iter().any(|s| s.id == a.id));
    assert!(open.iter().any(|s| s.id == c.id));
}

// ---------------------------------------------------------------------------
// SessionRepo — nonce lifecycle & replay-prevention contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_repo_nonce_happy_path() {
    let repo = MemorySessionRepo::default();
    let addr = sample_address();
    let nonce = "abc123";
    let expires = OffsetDateTime::now_utc() + time::Duration::minutes(10);

    repo.store_nonce(&addr, nonce, expires).await.unwrap();

    // first consume returns true (nonce was valid)
    let ok = repo.consume_nonce(&addr, nonce).await.unwrap();
    assert!(ok, "first consume must return true");

    // second consume returns false (nonce was already consumed — replay rejected)
    let replay = repo.consume_nonce(&addr, nonce).await.unwrap();
    assert!(
        !replay,
        "second consume must return false to prevent replay"
    );
}

#[tokio::test]
async fn session_repo_expired_nonce_is_rejected() {
    let repo = MemorySessionRepo::default();
    let addr = sample_address();
    let nonce = "expired_nonce";
    // expires at Unix epoch — already in the past
    let expires = OffsetDateTime::UNIX_EPOCH;

    repo.store_nonce(&addr, nonce, expires).await.unwrap();

    let ok = repo.consume_nonce(&addr, nonce).await.unwrap();
    assert!(!ok, "expired nonce must be rejected");
}

#[tokio::test]
async fn session_repo_unknown_nonce_returns_false() {
    let repo = MemorySessionRepo::default();
    let addr = sample_address();

    let ok = repo.consume_nonce(&addr, "does_not_exist").await.unwrap();
    assert!(!ok);
}

#[tokio::test]
async fn session_repo_nonce_per_address_is_independent() {
    let repo = MemorySessionRepo::default();
    let addr1: EvmAddress = "0x1111111111111111111111111111111111111111"
        .parse()
        .unwrap();
    let addr2: EvmAddress = "0x2222222222222222222222222222222222222222"
        .parse()
        .unwrap();
    let nonce = "shared_nonce_text";
    let expires = OffsetDateTime::now_utc() + time::Duration::minutes(5);

    repo.store_nonce(&addr1, nonce, expires).await.unwrap();
    repo.store_nonce(&addr2, nonce, expires).await.unwrap();

    // consuming for addr1 must not affect addr2
    assert!(repo.consume_nonce(&addr1, nonce).await.unwrap());
    assert!(repo.consume_nonce(&addr2, nonce).await.unwrap());
}

// ---------------------------------------------------------------------------
// Repositories aggregate — object-safety proof
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repositories_aggregate_is_object_safe() {
    let repos: &dyn Repositories = &InMemoryRepos::default();
    // Drive round-trips through the aggregate handle to prove the vtable works.
    let user = sample_user();
    repos.users().create(&user).await.unwrap();
    let fetched = repos.users().get(user.id).await.unwrap();
    assert_eq!(fetched, user);

    let seek = sample_seek(user.id);
    repos.seeks().create(&seek).await.unwrap();
    let open = repos.seeks().list_open().await.unwrap();
    assert_eq!(open.len(), 1);

    let addr = sample_address();
    let expires = OffsetDateTime::now_utc() + time::Duration::minutes(5);
    repos
        .sessions()
        .store_nonce(&addr, "tok", expires)
        .await
        .unwrap();
    assert!(repos.sessions().consume_nonce(&addr, "tok").await.unwrap());
}
