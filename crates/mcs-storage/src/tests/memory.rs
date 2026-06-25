//! In-memory repository implementations used exclusively in tests.
//!
//! These structs satisfy all repository traits using `Mutex<HashMap<…>>` so
//! no real database is needed. They are the reference implementations used to
//! verify trait object safety and ergonomics.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use mcs_domain::{
    Challenge, ChallengeId, ChallengeStatus, EvmAddress, Game, GameId, GameLifecycle, Rating,
    RatingHistoryEntry, Seek, SeekId, User, UserId,
};
use time::OffsetDateTime;

use mcs_payments::{PaymentRecord, PaymentStore, PaymentStoreError};

use crate::{
    action_log::{ActionLogRepo, RecordedAction},
    challenge::ChallengeRepo,
    error::{StorageError, StorageResult},
    game::GameRepo,
    rating::RatingRepo,
    rating_history::RatingHistoryRepo,
    repositories::Repositories,
    revoked_token::RevokedTokenRepo,
    seek::SeekRepo,
    session::SessionRepo,
    user::UserRepo,
    ClaimOutcome,
};

// ---------------------------------------------------------------------------
// MemoryUserRepo
// ---------------------------------------------------------------------------

/// In-memory [`UserRepo`] backed by a `HashMap`.
#[derive(Debug, Default)]
pub(super) struct MemoryUserRepo {
    by_id: Mutex<HashMap<UserId, User>>,
}

#[async_trait]
impl UserRepo for MemoryUserRepo {
    async fn create(&self, user: &User) -> StorageResult<()> {
        let mut map = self.by_id.lock().expect("mutex poisoned");
        if map.contains_key(&user.id) {
            return Err(StorageError::Conflict(format!(
                "user id {} already exists",
                user.id
            )));
        }
        if map.values().any(|u| u.address == user.address) {
            return Err(StorageError::Conflict(format!(
                "user address {} already exists",
                user.address
            )));
        }
        map.insert(user.id, user.clone());
        Ok(())
    }

    async fn get(&self, id: UserId) -> StorageResult<User> {
        let map = self.by_id.lock().expect("mutex poisoned");
        map.get(&id).cloned().ok_or(StorageError::NotFound)
    }

    async fn find_by_address(&self, addr: &EvmAddress) -> StorageResult<Option<User>> {
        let map = self.by_id.lock().expect("mutex poisoned");
        Ok(map.values().find(|u| &u.address == addr).cloned())
    }

    async fn upsert_by_address(&self, addr: &EvmAddress) -> StorageResult<User> {
        let mut map = self.by_id.lock().expect("mutex poisoned");
        if let Some(existing) = map.values().find(|u| &u.address == addr).cloned() {
            return Ok(existing);
        }
        let user = User::new(addr.clone(), None, OffsetDateTime::now_utc());
        map.insert(user.id, user.clone());
        Ok(user)
    }

    async fn set_username(&self, user: UserId, name: &str) -> StorageResult<()> {
        let mut map = self.by_id.lock().expect("mutex poisoned");
        // Case-insensitive uniqueness: a different user already holding `name`
        // (compared without regard to case) is a conflict.
        let lowered = name.to_lowercase();
        let clash = map.iter().any(|(id, u)| {
            *id != user
                && u.username
                    .as_deref()
                    .is_some_and(|existing| existing.to_lowercase() == lowered)
        });
        if clash {
            return Err(StorageError::Conflict(format!(
                "username {name:?} already taken"
            )));
        }
        match map.get_mut(&user) {
            Some(u) => {
                // Store the name verbatim; only the comparison is case-folded.
                u.username = Some(name.to_owned());
                Ok(())
            }
            None => Err(StorageError::NotFound),
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryGameRepo
// ---------------------------------------------------------------------------

/// In-memory [`GameRepo`] backed by a `HashMap`.
#[derive(Debug, Default)]
pub(super) struct MemoryGameRepo {
    games: Mutex<HashMap<GameId, Game>>,
}

#[async_trait]
impl GameRepo for MemoryGameRepo {
    async fn create(&self, game: &Game) -> StorageResult<()> {
        let mut map = self.games.lock().expect("mutex poisoned");
        if map.contains_key(&game.id) {
            return Err(StorageError::Conflict(format!(
                "game id {} already exists",
                game.id
            )));
        }
        map.insert(game.id, game.clone());
        Ok(())
    }

    async fn get(&self, id: GameId) -> StorageResult<Game> {
        let map = self.games.lock().expect("mutex poisoned");
        map.get(&id).cloned().ok_or(StorageError::NotFound)
    }

    async fn update(&self, game: &Game) -> StorageResult<()> {
        let mut map = self.games.lock().expect("mutex poisoned");
        if !map.contains_key(&game.id) {
            return Err(StorageError::NotFound);
        }
        map.insert(game.id, game.clone());
        Ok(())
    }

    async fn list_recent(&self, limit: u32) -> StorageResult<Vec<Game>> {
        let map = self.games.lock().expect("mutex poisoned");
        let mut games: Vec<Game> = map.values().cloned().collect();
        // Newest first (by created_at); stable sort for determinism.
        games.sort_by_key(|g| std::cmp::Reverse(g.created_at));
        games.truncate(limit as usize);
        Ok(games)
    }

    async fn list_for_user(&self, user: UserId, limit: u32) -> StorageResult<Vec<Game>> {
        let map = self.games.lock().expect("mutex poisoned");
        let mut games: Vec<Game> = map
            .values()
            .filter(|g| g.white == user || g.black == user)
            .cloned()
            .collect();
        games.sort_by_key(|g| std::cmp::Reverse(g.created_at));
        games.truncate(limit as usize);
        Ok(games)
    }

    async fn list_unfinished(&self) -> StorageResult<Vec<Game>> {
        let map = self.games.lock().expect("mutex poisoned");
        let mut games: Vec<Game> = map
            .values()
            .filter(|g| g.lifecycle != GameLifecycle::Finished)
            .cloned()
            .collect();
        // Oldest first, matching the sqlx implementation.
        games.sort_by_key(|g| g.created_at);
        Ok(games)
    }
}

// ---------------------------------------------------------------------------
// MemoryActionLogRepo
// ---------------------------------------------------------------------------

/// In-memory [`ActionLogRepo`] backed by a per-game `HashMap` keyed on `ply`.
#[derive(Debug, Default)]
pub(super) struct MemoryActionLogRepo {
    actions: Mutex<HashMap<GameId, HashMap<u32, RecordedAction>>>,
}

#[async_trait]
impl ActionLogRepo for MemoryActionLogRepo {
    async fn append(&self, game_id: GameId, action: &RecordedAction) -> StorageResult<()> {
        let mut map = self.actions.lock().expect("mutex poisoned");
        let log = map.entry(game_id).or_default();
        if log.contains_key(&action.ply) {
            return Err(StorageError::Conflict(format!(
                "action for game {game_id} ply {} already exists",
                action.ply
            )));
        }
        log.insert(action.ply, action.clone());
        Ok(())
    }

    async fn list(&self, game_id: GameId) -> StorageResult<Vec<RecordedAction>> {
        let map = self.actions.lock().expect("mutex poisoned");
        let Some(log) = map.get(&game_id) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<RecordedAction> = log.values().cloned().collect();
        out.sort_by_key(|a| a.ply);
        Ok(out)
    }

    async fn last_ply(&self, game_id: GameId) -> StorageResult<Option<u32>> {
        let map = self.actions.lock().expect("mutex poisoned");
        Ok(map.get(&game_id).and_then(|log| log.keys().copied().max()))
    }
}

// ---------------------------------------------------------------------------
// MemorySeekRepo
// ---------------------------------------------------------------------------

/// In-memory [`SeekRepo`] backed by a `HashMap`.
#[derive(Debug, Default)]
pub(super) struct MemorySeekRepo {
    seeks: Mutex<HashMap<SeekId, Seek>>,
}

#[async_trait]
impl SeekRepo for MemorySeekRepo {
    async fn create(&self, seek: &Seek) -> StorageResult<()> {
        let mut map = self.seeks.lock().expect("mutex poisoned");
        if map.contains_key(&seek.id) {
            return Err(StorageError::Conflict(format!(
                "seek id {} already exists",
                seek.id
            )));
        }
        map.insert(seek.id, seek.clone());
        Ok(())
    }

    async fn get(&self, id: SeekId) -> StorageResult<Option<Seek>> {
        let map = self.seeks.lock().expect("mutex poisoned");
        Ok(map.get(&id).cloned())
    }

    async fn remove(&self, id: SeekId) -> StorageResult<()> {
        let mut map = self.seeks.lock().expect("mutex poisoned");
        map.remove(&id);
        Ok(())
    }

    async fn claim(&self, id: SeekId) -> StorageResult<ClaimOutcome> {
        // Atomic under the single map lock: the remove both deletes and reports
        // prior presence, so concurrent claimants of one seek can never both win.
        let mut map = self.seeks.lock().expect("mutex poisoned");
        if map.remove(&id).is_some() {
            Ok(ClaimOutcome::Claimed)
        } else {
            Ok(ClaimOutcome::AlreadyClaimed)
        }
    }

    async fn list_open(&self) -> StorageResult<Vec<Seek>> {
        let map = self.seeks.lock().expect("mutex poisoned");
        Ok(map.values().cloned().collect())
    }

    async fn purge_stale(&self, older_than: OffsetDateTime) -> StorageResult<u64> {
        let mut map = self.seeks.lock().expect("mutex poisoned");
        let before = map.len();
        map.retain(|_, seek| seek.created_at >= older_than);
        Ok((before - map.len()) as u64)
    }
}

// ---------------------------------------------------------------------------
// MemoryChallengeRepo
// ---------------------------------------------------------------------------

/// In-memory [`ChallengeRepo`] backed by a `HashMap`.
#[derive(Debug, Default)]
pub(super) struct MemoryChallengeRepo {
    challenges: Mutex<HashMap<ChallengeId, Challenge>>,
}

#[async_trait]
impl ChallengeRepo for MemoryChallengeRepo {
    async fn create(&self, challenge: &Challenge) -> StorageResult<()> {
        let mut map = self.challenges.lock().expect("mutex poisoned");
        if map.contains_key(&challenge.id) {
            return Err(StorageError::Conflict(format!(
                "challenge id {} already exists",
                challenge.id
            )));
        }
        map.insert(challenge.id, challenge.clone());
        Ok(())
    }

    async fn get(&self, id: ChallengeId) -> StorageResult<Challenge> {
        let map = self.challenges.lock().expect("mutex poisoned");
        map.get(&id).cloned().ok_or(StorageError::NotFound)
    }

    async fn list_incoming(&self, user: UserId) -> StorageResult<Vec<Challenge>> {
        let map = self.challenges.lock().expect("mutex poisoned");
        Ok(map
            .values()
            .filter(|c| c.challenged == user && c.status == ChallengeStatus::Pending)
            .cloned()
            .collect())
    }

    async fn list_outgoing(&self, user: UserId) -> StorageResult<Vec<Challenge>> {
        let map = self.challenges.lock().expect("mutex poisoned");
        Ok(map
            .values()
            .filter(|c| c.challenger == user && c.status == ChallengeStatus::Pending)
            .cloned()
            .collect())
    }

    async fn update(&self, challenge: &Challenge) -> StorageResult<()> {
        let mut map = self.challenges.lock().expect("mutex poisoned");
        if !map.contains_key(&challenge.id) {
            return Err(StorageError::NotFound);
        }
        map.insert(challenge.id, challenge.clone());
        Ok(())
    }

    async fn purge_resolved(&self, older_than: OffsetDateTime) -> StorageResult<u64> {
        let mut map = self.challenges.lock().expect("mutex poisoned");
        let before = map.len();
        map.retain(|_, c| {
            // Keep pending and accepted challenges; only remove declined/canceled
            // ones that are old enough.
            !(matches!(
                c.status,
                ChallengeStatus::Declined | ChallengeStatus::Canceled
            ) && c.created_at < older_than)
        });
        Ok((before - map.len()) as u64)
    }
}

// ---------------------------------------------------------------------------
// MemorySessionRepo
// ---------------------------------------------------------------------------

/// A stored nonce entry.
#[derive(Debug, Clone)]
struct NonceEntry {
    expires_at: OffsetDateTime,
}

/// In-memory [`SessionRepo`] backed by a nested `HashMap`.
///
/// Key: `(address_string, nonce_string)` → [`NonceEntry`].
#[derive(Debug, Default)]
pub(super) struct MemorySessionRepo {
    nonces: Mutex<HashMap<(String, String), NonceEntry>>,
}

#[async_trait]
impl SessionRepo for MemorySessionRepo {
    async fn store_nonce(
        &self,
        address: &EvmAddress,
        nonce: &str,
        expires_at: OffsetDateTime,
    ) -> StorageResult<()> {
        let mut map = self.nonces.lock().expect("mutex poisoned");
        map.insert(
            (address.to_string(), nonce.to_owned()),
            NonceEntry { expires_at },
        );
        Ok(())
    }

    async fn consume_nonce(&self, address: &EvmAddress, nonce: &str) -> StorageResult<bool> {
        let mut map = self.nonces.lock().expect("mutex poisoned");
        let key = (address.to_string(), nonce.to_owned());
        match map.get(&key) {
            None => Ok(false),
            Some(entry) => {
                if entry.expires_at < OffsetDateTime::now_utc() {
                    // Expired: remove the stale entry and reject.
                    map.remove(&key);
                    Ok(false)
                } else {
                    // Valid and unexpired: atomically consume.
                    map.remove(&key);
                    Ok(true)
                }
            }
        }
    }

    async fn purge_expired_nonces(&self, now: OffsetDateTime) -> StorageResult<u64> {
        let mut map = self.nonces.lock().expect("mutex poisoned");
        let before = map.len();
        map.retain(|_, entry| entry.expires_at > now);
        Ok((before - map.len()) as u64)
    }
}

// ---------------------------------------------------------------------------
// MemoryRevokedTokenRepo
// ---------------------------------------------------------------------------

/// In-memory [`RevokedTokenRepo`] backed by a `HashMap`.
///
/// Key: `jti` string → the token's `expires_at`.
#[derive(Debug, Default)]
pub(super) struct MemoryRevokedTokenRepo {
    revoked: Mutex<HashMap<String, OffsetDateTime>>,
}

#[async_trait]
impl RevokedTokenRepo for MemoryRevokedTokenRepo {
    async fn revoke(&self, jti: &str, expires_at: OffsetDateTime) -> StorageResult<()> {
        let mut map = self.revoked.lock().expect("mutex poisoned");
        // Idempotent insert/overwrite: the same token always carries the same
        // expiry, so re-revoking is a no-op in effect.
        map.insert(jti.to_owned(), expires_at);
        Ok(())
    }

    async fn is_revoked(&self, jti: &str) -> StorageResult<bool> {
        let map = self.revoked.lock().expect("mutex poisoned");
        Ok(map.contains_key(jti))
    }

    async fn purge_expired(&self, now: OffsetDateTime) -> StorageResult<u64> {
        let mut map = self.revoked.lock().expect("mutex poisoned");
        let before = map.len();
        map.retain(|_, expires_at| *expires_at > now);
        Ok((before - map.len()) as u64)
    }
}

// ---------------------------------------------------------------------------
// MemoryRatingRepo
// ---------------------------------------------------------------------------

/// In-memory [`RatingRepo`] backed by a `HashMap`.
///
/// Key: `(user_id_string, variant_id_string)` → [`Rating`].
#[derive(Debug, Default)]
pub(super) struct MemoryRatingRepo {
    ratings: Mutex<HashMap<(String, String), Rating>>,
}

#[async_trait]
impl RatingRepo for MemoryRatingRepo {
    async fn get(&self, user: UserId, variant_id: &str) -> StorageResult<Option<Rating>> {
        let map = self.ratings.lock().expect("mutex poisoned");
        Ok(map.get(&(user.to_string(), variant_id.to_owned())).cloned())
    }

    async fn upsert(&self, user: UserId, variant_id: &str, rating: &Rating) -> StorageResult<()> {
        let mut map = self.ratings.lock().expect("mutex poisoned");
        map.insert((user.to_string(), variant_id.to_owned()), rating.clone());
        Ok(())
    }

    async fn leaderboard(
        &self,
        variant_id: &str,
        limit: u32,
    ) -> StorageResult<Vec<(UserId, Rating)>> {
        let map = self.ratings.lock().expect("mutex poisoned");
        let mut entries: Vec<(UserId, Rating)> = map
            .iter()
            .filter(|((_, vid), _)| vid == variant_id)
            .map(|((uid, _), r)| {
                let user_id: UserId = uid.parse().expect("stored UserId must be valid UUID");
                (user_id, r.clone())
            })
            .collect();
        // Highest value first; break ties deterministically by user_id string.
        entries.sort_by(|(a_id, a_r), (b_id, b_r)| {
            b_r.value
                .partial_cmp(&a_r.value)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a_id.to_string().cmp(&b_id.to_string()))
        });
        entries.truncate(limit as usize);
        Ok(entries)
    }

    async fn list_for_user(&self, user: UserId) -> StorageResult<Vec<(String, Rating)>> {
        let map = self.ratings.lock().expect("mutex poisoned");
        let uid = user.to_string();
        let mut entries: Vec<(String, Rating)> = map
            .iter()
            .filter(|((u, _), _)| *u == uid)
            .map(|((_, vid), r)| (vid.clone(), r.clone()))
            .collect();
        // Stable, deterministic order by variant id (matches the sqlx impl).
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// MemoryRatingHistoryRepo
// ---------------------------------------------------------------------------

/// In-memory [`RatingHistoryRepo`] backed by an append-only `Vec`.
#[derive(Debug, Default)]
pub(super) struct MemoryRatingHistoryRepo {
    entries: Mutex<Vec<RatingHistoryEntry>>,
}

#[async_trait]
impl RatingHistoryRepo for MemoryRatingHistoryRepo {
    async fn record(&self, entry: &RatingHistoryEntry) -> StorageResult<()> {
        let mut log = self.entries.lock().expect("mutex poisoned");
        log.push(entry.clone());
        Ok(())
    }

    async fn list(
        &self,
        user: UserId,
        variant_id: &str,
        limit: u32,
    ) -> StorageResult<Vec<RatingHistoryEntry>> {
        let log = self.entries.lock().expect("mutex poisoned");
        let mut out: Vec<RatingHistoryEntry> = log
            .iter()
            .filter(|e| e.user_id == user && e.variant_id == variant_id)
            .cloned()
            .collect();
        // Most-recent-first.
        out.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        out.truncate(limit as usize);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// MemoryPaymentStore — x402 settled-payment idempotency (#108)
// ---------------------------------------------------------------------------

/// In-memory [`PaymentStore`] backed by a `HashMap` keyed on `idempotency_key`.
#[derive(Debug, Default)]
pub(super) struct MemoryPaymentStore {
    records: Mutex<HashMap<String, PaymentRecord>>,
}

#[async_trait]
impl PaymentStore for MemoryPaymentStore {
    async fn find(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<PaymentRecord>, PaymentStoreError> {
        let map = self.records.lock().expect("mutex poisoned");
        Ok(map.get(idempotency_key).cloned())
    }

    async fn record(&self, record: &PaymentRecord) -> Result<(), PaymentStoreError> {
        let mut map = self.records.lock().expect("mutex poisoned");
        // Unique on `idempotency_key`: a second record is the "already recorded"
        // conflict the middleware falls back on.
        if map.contains_key(&record.idempotency_key) {
            return Err(PaymentStoreError::Conflict);
        }
        map.insert(record.idempotency_key.clone(), record.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// InMemoryRepos — aggregate
// ---------------------------------------------------------------------------

/// A fully in-memory [`Repositories`] implementation for use in tests.
#[derive(Debug, Default)]
pub(super) struct InMemoryRepos {
    users: MemoryUserRepo,
    games: MemoryGameRepo,
    actions: MemoryActionLogRepo,
    seeks: MemorySeekRepo,
    challenges: MemoryChallengeRepo,
    sessions: MemorySessionRepo,
    revoked_tokens: MemoryRevokedTokenRepo,
    ratings: MemoryRatingRepo,
    rating_history: MemoryRatingHistoryRepo,
    payments: MemoryPaymentStore,
}

impl Repositories for InMemoryRepos {
    fn users(&self) -> &dyn UserRepo {
        &self.users
    }

    fn games(&self) -> &dyn GameRepo {
        &self.games
    }

    fn actions(&self) -> &dyn ActionLogRepo {
        &self.actions
    }

    fn seeks(&self) -> &dyn SeekRepo {
        &self.seeks
    }

    fn challenges(&self) -> &dyn ChallengeRepo {
        &self.challenges
    }

    fn sessions(&self) -> &dyn SessionRepo {
        &self.sessions
    }

    fn revoked_tokens(&self) -> &dyn RevokedTokenRepo {
        &self.revoked_tokens
    }

    fn ratings(&self) -> &dyn RatingRepo {
        &self.ratings
    }

    fn rating_history(&self) -> &dyn RatingHistoryRepo {
        &self.rating_history
    }

    fn payments(&self) -> &dyn PaymentStore {
        &self.payments
    }
}
