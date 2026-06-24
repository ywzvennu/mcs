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
// The sqlx integration suite is backend-parameterised: the same test bodies run
// against SQLite (default) or a real Postgres service in CI, selected at runtime
// through `MCS_TEST_DATABASE_URL`. It is compiled whenever either backend
// feature is active.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod harness;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod sqlx_backend;

use mcs_core::{Action, Color, EndReason, Outcome, VariantOptions};
use mcs_domain::{
    ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Seek, TimeControl, User, UserId,
};
use memory::{
    InMemoryRepos, MemoryActionLogRepo, MemoryGameRepo, MemoryRatingRepo, MemoryRevokedTokenRepo,
    MemorySeekRepo, MemorySessionRepo, MemoryUserRepo,
};
use time::OffsetDateTime;

use mcs_domain::Rating;

use crate::{
    ActionLogRepo, GameRepo, RatingRepo, RecordedAction, Repositories, RevokedTokenRepo, SeekRepo,
    SessionRepo, StorageError, UserRepo,
};

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

/// Builds a [`RecordedAction`] at `ply` whose payload is a distinct JSON object,
/// so listed actions can be checked to preserve their exact `Action` content.
fn sample_action(ply: u32, player: Color) -> RecordedAction {
    RecordedAction {
        ply,
        player,
        action: Action::new(serde_json::json!({ "move": format!("e{ply}") })),
        clock_white_ms: Some(180_000 - u64::from(ply)),
        clock_black_ms: Some(170_000 - u64::from(ply)),
        created_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(i64::from(ply)),
    }
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

#[tokio::test]
async fn game_repo_list_unfinished_excludes_finished() {
    let repo = MemoryGameRepo::default();
    let white = UserId::new();
    let black = UserId::new();

    // A created game, an active game, and a finished game.
    let created = sample_game(white, black);
    repo.create(&created).await.unwrap();

    let mut active = sample_game(white, black);
    active.lifecycle = GameLifecycle::Active;
    repo.create(&active).await.unwrap();

    let mut finished = sample_game(white, black);
    finished.finish(
        Outcome::win(mcs_core::Color::White, EndReason::Checkmate),
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10),
    );
    repo.create(&finished).await.unwrap();

    let unfinished = repo.list_unfinished().await.unwrap();
    let ids: Vec<GameId> = unfinished.iter().map(|g| g.id).collect();
    assert_eq!(unfinished.len(), 2);
    assert!(ids.contains(&created.id));
    assert!(ids.contains(&active.id));
    assert!(!ids.contains(&finished.id));
}

// ---------------------------------------------------------------------------
// ActionLogRepo tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn action_log_append_then_list_in_ply_order() {
    let repo = MemoryActionLogRepo::default();
    let game = GameId::new();

    // Append out of order; `list` must still return ascending by ply.
    repo.append(game, &sample_action(2, Color::White))
        .await
        .unwrap();
    repo.append(game, &sample_action(0, Color::White))
        .await
        .unwrap();
    repo.append(game, &sample_action(1, Color::Black))
        .await
        .unwrap();

    let listed = repo.list(game).await.unwrap();
    let plies: Vec<u32> = listed.iter().map(|a| a.ply).collect();
    assert_eq!(plies, vec![0, 1, 2]);
    assert_eq!(listed[0], sample_action(0, Color::White));
}

#[tokio::test]
async fn action_log_duplicate_ply_is_conflict() {
    let repo = MemoryActionLogRepo::default();
    let game = GameId::new();
    repo.append(game, &sample_action(0, Color::White))
        .await
        .unwrap();
    let err = repo
        .append(game, &sample_action(0, Color::Black))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)));
}

#[tokio::test]
async fn action_log_last_ply_tracks_max() {
    let repo = MemoryActionLogRepo::default();
    let game = GameId::new();
    assert_eq!(repo.last_ply(game).await.unwrap(), None);

    repo.append(game, &sample_action(0, Color::White))
        .await
        .unwrap();
    repo.append(game, &sample_action(1, Color::Black))
        .await
        .unwrap();
    assert_eq!(repo.last_ply(game).await.unwrap(), Some(1));
}

#[tokio::test]
async fn action_log_empty_game_is_empty() {
    let repo = MemoryActionLogRepo::default();
    let game = GameId::new();
    assert!(repo.list(game).await.unwrap().is_empty());
    assert_eq!(repo.last_ply(game).await.unwrap(), None);
}

#[tokio::test]
async fn action_log_is_scoped_per_game() {
    let repo = MemoryActionLogRepo::default();
    let a = GameId::new();
    let b = GameId::new();
    repo.append(a, &sample_action(0, Color::White))
        .await
        .unwrap();
    // The same ply in a different game is not a conflict.
    repo.append(b, &sample_action(0, Color::White))
        .await
        .unwrap();
    assert_eq!(repo.list(a).await.unwrap().len(), 1);
    assert_eq!(repo.list(b).await.unwrap().len(), 1);
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
// RevokedTokenRepo — logout denylist
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoked_token_revoke_then_is_revoked() {
    let repo = MemoryRevokedTokenRepo::default();
    let expires = OffsetDateTime::now_utc() + time::Duration::hours(1);

    assert!(!repo.is_revoked("jti-a").await.unwrap());
    repo.revoke("jti-a", expires).await.unwrap();
    assert!(repo.is_revoked("jti-a").await.unwrap());
    // A different token is unaffected.
    assert!(!repo.is_revoked("jti-b").await.unwrap());
}

#[tokio::test]
async fn revoked_token_revoke_is_idempotent() {
    let repo = MemoryRevokedTokenRepo::default();
    let expires = OffsetDateTime::now_utc() + time::Duration::hours(1);
    repo.revoke("jti", expires).await.unwrap();
    // Revoking again must not error.
    repo.revoke("jti", expires).await.unwrap();
    assert!(repo.is_revoked("jti").await.unwrap());
}

#[tokio::test]
async fn revoked_token_purge_expired_drops_only_elapsed() {
    let repo = MemoryRevokedTokenRepo::default();
    let now = OffsetDateTime::now_utc();
    // One already-expired, one still-valid entry.
    repo.revoke("old", now - time::Duration::hours(1))
        .await
        .unwrap();
    repo.revoke("fresh", now + time::Duration::hours(1))
        .await
        .unwrap();

    let removed = repo.purge_expired(now).await.unwrap();
    assert_eq!(removed, 1, "only the expired entry is purged");
    assert!(!repo.is_revoked("old").await.unwrap());
    assert!(repo.is_revoked("fresh").await.unwrap());
}

// ---------------------------------------------------------------------------
// RatingRepo tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rating_repo_get_missing_returns_none() {
    let repo = MemoryRatingRepo::default();
    let result = repo.get(UserId::new(), "standard").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn rating_repo_upsert_then_get() {
    let repo = MemoryRatingRepo::default();
    let user = UserId::new();
    let rating = Rating {
        value: 1700.0,
        deviation: 200.0,
        volatility: 0.05,
    };

    repo.upsert(user, "standard", &rating).await.unwrap();
    let fetched = repo.get(user, "standard").await.unwrap();
    let fetched = fetched.expect("rating must exist after upsert");
    assert_eq!(fetched.value, rating.value);
    assert_eq!(fetched.deviation, rating.deviation);
    assert_eq!(fetched.volatility, rating.volatility);
}

#[tokio::test]
async fn rating_repo_upsert_overwrites() {
    let repo = MemoryRatingRepo::default();
    let user = UserId::new();

    let first = Rating {
        value: 1500.0,
        deviation: 350.0,
        volatility: 0.06,
    };
    repo.upsert(user, "standard", &first).await.unwrap();

    let second = Rating {
        value: 1620.0,
        deviation: 180.0,
        volatility: 0.05,
    };
    repo.upsert(user, "standard", &second).await.unwrap();

    let fetched = repo
        .get(user, "standard")
        .await
        .unwrap()
        .expect("rating must exist");
    assert_eq!(fetched.value, second.value);
    assert_eq!(fetched.deviation, second.deviation);
}

#[tokio::test]
async fn rating_repo_leaderboard_order_and_limit() {
    let repo = MemoryRatingRepo::default();
    let users: Vec<UserId> = (0..5).map(|_| UserId::new()).collect();
    // Insert in arbitrary order with distinct values.
    let values = [1200.0_f64, 1800.0, 1500.0, 2000.0, 1100.0];
    for (uid, v) in users.iter().zip(values.iter()) {
        let r = Rating {
            value: *v,
            deviation: 200.0,
            volatility: 0.06,
        };
        repo.upsert(*uid, "standard", &r).await.unwrap();
    }

    let board = repo.leaderboard("standard", 3).await.unwrap();
    assert_eq!(board.len(), 3);
    // Must be descending by value.
    assert!(board.windows(2).all(|w| w[0].1.value >= w[1].1.value));
    assert_eq!(board[0].1.value, 2000.0);
}

#[tokio::test]
async fn rating_repo_leaderboard_variant_isolation() {
    let repo = MemoryRatingRepo::default();
    let user = UserId::new();
    repo.upsert(
        user,
        "standard",
        &Rating {
            value: 1500.0,
            deviation: 200.0,
            volatility: 0.06,
        },
    )
    .await
    .unwrap();

    // A different variant must not appear in the leaderboard.
    let board = repo.leaderboard("chess960", 10).await.unwrap();
    assert!(board.is_empty());
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

    let game = GameId::new();
    repos
        .actions()
        .append(game, &sample_action(0, Color::White))
        .await
        .unwrap();
    assert_eq!(repos.actions().last_ply(game).await.unwrap(), Some(0));

    let addr = sample_address();
    let expires = OffsetDateTime::now_utc() + time::Duration::minutes(5);
    repos
        .sessions()
        .store_nonce(&addr, "tok", expires)
        .await
        .unwrap();
    assert!(repos.sessions().consume_nonce(&addr, "tok").await.unwrap());
}
