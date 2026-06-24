//! In-memory repository implementations used exclusively in tests.
//!
//! These structs satisfy all repository traits using `Mutex<HashMap<…>>` so
//! no real database is needed. They are the reference implementations used to
//! verify trait object safety and ergonomics.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use mcs_domain::{
    Challenge, ChallengeId, ChallengeStatus, EvmAddress, Game, GameId, GameLifecycle, Rating, Seek,
    SeekId, User, UserId,
};
use time::OffsetDateTime;

use crate::{
    action_log::{ActionLogRepo, RecordedAction},
    challenge::ChallengeRepo,
    error::{StorageError, StorageResult},
    game::GameRepo,
    rating::RatingRepo,
    repositories::Repositories,
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
    ratings: MemoryRatingRepo,
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

    fn ratings(&self) -> &dyn RatingRepo {
        &self.ratings
    }
}
