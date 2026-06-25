//! A [`sqlx`]-backed implementation of the repository traits.
//!
//! [`SqlxStorage`] holds a connection pool and implements [`Repositories`] plus
//! all four repository traits. One backend is selected at compile time through
//! the crate's `sqlite` / `postgres` features.
//!
//! ## No compile-time query macros
//!
//! CI runs with `SQLX_OFFLINE=true` and provides neither a live database nor a
//! `.sqlx/` metadata cache. The compile-time-checked `sqlx::query!` /
//! `sqlx::query_as!` macros would therefore fail to build. This module uses the
//! **runtime** query API exclusively — `sqlx::query`, `.bind`, the
//! `.fetch_*` / `.execute` methods, and manual [`sqlx::Row`] mapping — so the
//! crate compiles with no database in reach.
//!
//! ## Encoding conventions
//!
//! | Domain shape                     | Column type | Encoding                       |
//! |----------------------------------|-------------|--------------------------------|
//! | Ids ([`UserId`], …)              | `TEXT`      | canonical UUID string          |
//! | [`EvmAddress`]                   | `TEXT`      | lowercase `0x`-prefixed string |
//! | Enums ([`GameLifecycle`], …)     | `TEXT`      | lowercase discriminant         |
//! | [`TimeControl`][mcs_domain::TimeControl], [`Outcome`][mcs_core::Outcome] | `TEXT` | serde JSON |
//! | Timestamps                       | `TEXT`      | RFC 3339 in UTC                |
//!
//! RFC 3339 timestamps sort lexicographically in chronological order, so the
//! "newest first" listings are plain `ORDER BY created_at DESC` queries that
//! behave identically on SQLite and Postgres.

use async_trait::async_trait;
use mcs_core::{Action, Color};
use mcs_domain::{
    Challenge, ChallengeId, ChallengeStatus, ColorPreference, EvmAddress, Game, GameId,
    GameLifecycle, Rating, RatingHistoryEntry, Seek, SeekId, TimeClass, User, UserId,
};
use mcs_payments::{PaymentRecord, PaymentStore, PaymentStoreError};
use sqlx::Row;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{
    action_log::{ActionLogRepo, RecordedAction},
    error::{StorageError, StorageResult},
    ChallengeRepo, ClaimOutcome, GameRepo, RatingHistoryRepo, RatingRepo, Repositories,
    RevokedTokenRepo, SeekRepo, SessionRepo, UserRepo,
};

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

/// The sqlx database backend chosen at compile time.
#[cfg(feature = "sqlite")]
type Backend = sqlx::Sqlite;

/// The sqlx database backend chosen at compile time.
#[cfg(all(feature = "postgres", not(feature = "sqlite")))]
type Backend = sqlx::Postgres;

/// The connection pool type for the active [`Backend`].
type DbPool = sqlx::Pool<Backend>;

/// The row type produced by the active [`Backend`].
///
/// Because exactly one backend feature is active at a time, row mapping is
/// written against this concrete type rather than a generic `R: Row` bound —
/// the latter would require spelling out a thicket of `Decode`/`ColumnIndex`
/// bounds that the concrete type satisfies for free.
type DbRow = <Backend as sqlx::Database>::Row;

/// Connection-pool tuning applied when building a [`SqlxStorage`] pool.
///
/// These knobs map onto [`sqlx::pool::PoolOptions`] and let an operator size the
/// pool for a production database (most relevant for Postgres, where many server
/// nodes share one instance). Build one from your server config and hand it to
/// [`SqlxStorage::connect_with`].
///
/// # Defaults
///
/// [`PoolConfig::default`] is conservative and backend-agnostic: 10 max
/// connections, a 30-second acquire timeout, a 10-minute idle timeout, and no
/// hard connection lifetime or statement timeout. These suit a single-node
/// SQLite file as well as a small shared Postgres.
///
/// # In-memory SQLite
///
/// In-memory SQLite is always pinned to a single connection regardless of
/// [`max_connections`](Self::max_connections): every SQLite connection gets its
/// own private in-memory database, so a multi-connection pool would scatter
/// writes across disjoint databases. [`SqlxStorage::connect_with`] enforces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// Maximum number of connections the pool may open. Default: `10`.
    ///
    /// Pinned to `1` internally for in-memory SQLite (see the type docs).
    pub max_connections: u32,
    /// How long [`acquire`](sqlx::Pool::acquire) waits for a free connection
    /// before returning a timeout error. Default: 30 s.
    pub acquire_timeout: std::time::Duration,
    /// Close a connection that has been idle in the pool for at least this long.
    /// `None` keeps idle connections indefinitely. Default: `Some(10 min)`.
    pub idle_timeout: Option<std::time::Duration>,
    /// Close (and replace) any connection older than this, regardless of use.
    /// Useful behind a load balancer that recycles backend TCP connections.
    /// `None` (the default) imposes no maximum lifetime.
    pub max_lifetime: Option<std::time::Duration>,
    /// Per-statement execution timeout, applied **only on Postgres** by issuing
    /// `SET statement_timeout` on each new connection. `None` (the default)
    /// leaves the server's own `statement_timeout` in force. Ignored on SQLite.
    pub statement_timeout: Option<std::time::Duration>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: std::time::Duration::from_secs(30),
            idle_timeout: Some(std::time::Duration::from_secs(10 * 60)),
            max_lifetime: None,
            statement_timeout: None,
        }
    }
}

/// Embedded migrator pointing at `crates/mcs-storage/migrations`.
///
/// `sqlx::migrate!` reads the SQL files at compile time (a pure file read — it
/// needs no database), producing a [`sqlx::migrate::Migrator`] that applies them
/// at runtime against whichever backend the pool speaks.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

/// Formats a timestamp as an RFC 3339 string for storage.
fn encode_time(ts: OffsetDateTime) -> Result<String, StorageError> {
    ts.format(&Rfc3339)
        .map_err(|e| StorageError::Serialization(format!("formatting timestamp: {e}")))
}

/// Parses an RFC 3339 timestamp string read from the database.
fn decode_time(s: &str) -> Result<OffsetDateTime, StorageError> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map_err(|e| StorageError::Serialization(format!("parsing timestamp {s:?}: {e}")))
}

/// Parses an [`EvmAddress`] read from the database.
fn decode_address(s: &str) -> Result<EvmAddress, StorageError> {
    s.parse()
        .map_err(|e| StorageError::Serialization(format!("parsing address {s:?}: {e}")))
}

/// Parses a UUID-backed id read from the database.
fn decode_id<T: std::str::FromStr>(s: &str) -> Result<T, StorageError>
where
    T::Err: std::fmt::Display,
{
    s.parse()
        .map_err(|e| StorageError::Serialization(format!("parsing id {s:?}: {e}")))
}

/// Serialises a serde value (e.g. [`TimeControl`]) to its JSON column form.
fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value)
        .map_err(|e| StorageError::Serialization(format!("serialising value: {e}")))
}

/// Deserialises a serde value from its JSON column form.
fn decode_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_str(s)
        .map_err(|e| StorageError::Serialization(format!("deserialising {s:?}: {e}")))
}

/// Encodes a [`GameLifecycle`] as its lowercase column discriminant.
fn encode_lifecycle(lc: GameLifecycle) -> &'static str {
    match lc {
        GameLifecycle::Created => "created",
        GameLifecycle::Active => "active",
        GameLifecycle::Finished => "finished",
    }
}

/// Decodes a [`GameLifecycle`] from its column discriminant.
fn decode_lifecycle(s: &str) -> Result<GameLifecycle, StorageError> {
    match s {
        "created" => Ok(GameLifecycle::Created),
        "active" => Ok(GameLifecycle::Active),
        "finished" => Ok(GameLifecycle::Finished),
        other => Err(StorageError::Serialization(format!(
            "unknown game lifecycle {other:?}"
        ))),
    }
}

/// Encodes a [`ColorPreference`] as its lowercase column discriminant.
fn encode_color_pref(cp: ColorPreference) -> &'static str {
    match cp {
        ColorPreference::White => "white",
        ColorPreference::Black => "black",
        ColorPreference::Random => "random",
    }
}

/// Decodes a [`ColorPreference`] from its column discriminant.
fn decode_color_pref(s: &str) -> Result<ColorPreference, StorageError> {
    match s {
        "white" => Ok(ColorPreference::White),
        "black" => Ok(ColorPreference::Black),
        "random" => Ok(ColorPreference::Random),
        other => Err(StorageError::Serialization(format!(
            "unknown color preference {other:?}"
        ))),
    }
}

/// Decodes a [`TimeClass`] from its lowercase column discriminant (the same
/// `snake_case` spelling [`TimeClass::as_str`] produces).
fn decode_time_class(s: &str) -> Result<TimeClass, StorageError> {
    s.parse()
        .map_err(|_| StorageError::Serialization(format!("unknown time class {s:?}")))
}

/// Encodes a [`ChallengeStatus`] as its lowercase column discriminant.
fn encode_challenge_status(status: ChallengeStatus) -> &'static str {
    match status {
        ChallengeStatus::Pending => "pending",
        ChallengeStatus::Accepted => "accepted",
        ChallengeStatus::Declined => "declined",
        ChallengeStatus::Canceled => "canceled",
    }
}

/// Decodes a [`ChallengeStatus`] from its column discriminant.
fn decode_challenge_status(s: &str) -> Result<ChallengeStatus, StorageError> {
    match s {
        "pending" => Ok(ChallengeStatus::Pending),
        "accepted" => Ok(ChallengeStatus::Accepted),
        "declined" => Ok(ChallengeStatus::Declined),
        "canceled" => Ok(ChallengeStatus::Canceled),
        other => Err(StorageError::Serialization(format!(
            "unknown challenge status {other:?}"
        ))),
    }
}

/// Encodes a [`Color`] as its lowercase column discriminant.
fn encode_color(color: Color) -> &'static str {
    match color {
        Color::White => "white",
        Color::Black => "black",
    }
}

/// Decodes a [`Color`] from its column discriminant.
fn decode_color(s: &str) -> Result<Color, StorageError> {
    match s {
        "white" => Ok(Color::White),
        "black" => Ok(Color::Black),
        other => Err(StorageError::Serialization(format!(
            "unknown color {other:?}"
        ))),
    }
}

/// Encodes a boolean as the integer column form (`0`/`1`).
///
/// The `rated` flag is stored as an INTEGER (0 = casual, 1 = rated) so the DDL
/// stays portable across SQLite and Postgres without a backend-specific boolean
/// type — matching how the schema already stores small integers.
fn encode_bool(value: bool) -> i64 {
    i64::from(value)
}

/// Decodes a boolean from its integer column form. Any non-zero value is `true`.
fn decode_bool(value: i64) -> bool {
    value != 0
}

/// Encodes an optional millisecond clock as the signed integer column form.
///
/// The values originate as `u64` but fit comfortably in `i64` for any
/// realistic clock; conversion is lossless across the supported range.
fn encode_clock(ms: Option<u64>) -> Option<i64> {
    ms.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

/// The full column list used by every `games` read, kept in one place so the
/// row-mapping in [`game_from_row`] stays in lock-step with the queries.
const GAME_SELECT: &str = "SELECT id, variant_id, variant_options, white, black, lifecycle, \
     outcome, time_control, rated, ply, clock_white_ms, clock_black_ms, side_to_move, \
     created_at, updated_at FROM games";

/// Reconstructs a [`User`] from a database row.
fn user_from_row(row: &DbRow) -> Result<User, StorageError> {
    Ok(User {
        id: decode_id::<UserId>(&row.try_get::<String, _>("id")?)?,
        address: decode_address(&row.try_get::<String, _>("address")?)?,
        username: row.try_get::<Option<String>, _>("username")?,
        created_at: decode_time(&row.try_get::<String, _>("created_at")?)?,
    })
}

/// Reconstructs a [`Game`] from a database row.
fn game_from_row(row: &DbRow) -> Result<Game, StorageError> {
    let outcome = match row.try_get::<Option<String>, _>("outcome")? {
        Some(json) => Some(decode_json(&json)?),
        None => None,
    };
    let side_to_move = match row.try_get::<Option<String>, _>("side_to_move")? {
        Some(s) => Some(decode_color(&s)?),
        None => None,
    };
    Ok(Game {
        id: decode_id::<GameId>(&row.try_get::<String, _>("id")?)?,
        variant_id: row.try_get::<String, _>("variant_id")?,
        variant_options: decode_json(&row.try_get::<String, _>("variant_options")?)?,
        white: decode_id::<UserId>(&row.try_get::<String, _>("white")?)?,
        black: decode_id::<UserId>(&row.try_get::<String, _>("black")?)?,
        lifecycle: decode_lifecycle(&row.try_get::<String, _>("lifecycle")?)?,
        outcome,
        time_control: decode_json(&row.try_get::<String, _>("time_control")?)?,
        rated: decode_bool(row.try_get::<i64, _>("rated")?),
        ply: decode_u32(row.try_get::<i64, _>("ply")?, "ply")?,
        clock_white_ms: decode_clock(row.try_get::<Option<i64>, _>("clock_white_ms")?)?,
        clock_black_ms: decode_clock(row.try_get::<Option<i64>, _>("clock_black_ms")?)?,
        side_to_move,
        created_at: decode_time(&row.try_get::<String, _>("created_at")?)?,
        updated_at: decode_time(&row.try_get::<String, _>("updated_at")?)?,
    })
}

/// Converts a signed integer column into a `u32`, rejecting negatives.
fn decode_u32(value: i64, field: &str) -> Result<u32, StorageError> {
    u32::try_from(value)
        .map_err(|_| StorageError::Serialization(format!("{field} out of range: {value}")))
}

/// Converts an optional signed clock column (milliseconds) into a `u64`.
fn decode_clock(value: Option<i64>) -> Result<Option<u64>, StorageError> {
    value
        .map(|ms| {
            u64::try_from(ms)
                .map_err(|_| StorageError::Serialization(format!("clock out of range: {ms}")))
        })
        .transpose()
}

/// The full column list used by every `challenges` read, kept beside
/// [`challenge_from_row`] so the queries and the row mapping stay aligned.
const CHALLENGE_SELECT: &str = "SELECT id, challenger, challenged, variant_id, time_control, \
     rated, color_preference, status, game_id, created_at FROM challenges";

/// Reconstructs a [`Challenge`] from a database row.
fn challenge_from_row(row: &DbRow) -> Result<Challenge, StorageError> {
    let game_id = match row.try_get::<Option<String>, _>("game_id")? {
        Some(s) => Some(decode_id::<GameId>(&s)?),
        None => None,
    };
    Ok(Challenge {
        id: decode_id::<ChallengeId>(&row.try_get::<String, _>("id")?)?,
        challenger: decode_id::<UserId>(&row.try_get::<String, _>("challenger")?)?,
        challenged: decode_id::<UserId>(&row.try_get::<String, _>("challenged")?)?,
        variant_id: row.try_get::<String, _>("variant_id")?,
        time_control: decode_json(&row.try_get::<String, _>("time_control")?)?,
        rated: decode_bool(row.try_get::<i64, _>("rated")?),
        color_preference: decode_color_pref(&row.try_get::<String, _>("color_preference")?)?,
        status: decode_challenge_status(&row.try_get::<String, _>("status")?)?,
        game_id,
        created_at: decode_time(&row.try_get::<String, _>("created_at")?)?,
    })
}

/// Reconstructs a [`Seek`] from a database row.
fn seek_from_row(row: &DbRow) -> Result<Seek, StorageError> {
    Ok(Seek {
        id: decode_id::<SeekId>(&row.try_get::<String, _>("id")?)?,
        creator: decode_id::<UserId>(&row.try_get::<String, _>("creator")?)?,
        variant_id: row.try_get::<String, _>("variant_id")?,
        time_control: decode_json(&row.try_get::<String, _>("time_control")?)?,
        color_preference: decode_color_pref(&row.try_get::<String, _>("color_preference")?)?,
        rated: decode_bool(row.try_get::<i64, _>("rated")?),
        created_at: decode_time(&row.try_get::<String, _>("created_at")?)?,
    })
}

// ---------------------------------------------------------------------------
// SqlxStorage
// ---------------------------------------------------------------------------

/// A [`Repositories`] implementation backed by a [`sqlx`] connection pool.
///
/// Construct it with [`SqlxStorage::connect`], which builds the pool and applies
/// the embedded migrations. The struct is cheap to clone-share behind an `Arc`
/// because the pool itself is reference-counted internally.
///
/// # Example
///
/// ```rust,ignore
/// use mcs_storage::SqlxStorage;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let storage = SqlxStorage::connect("sqlite::memory:").await?;
/// let users = storage.users();
/// # let _ = users;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SqlxStorage {
    pool: DbPool,
}

impl SqlxStorage {
    /// Connects to `database_url`, building a pool and running migrations.
    ///
    /// The URL form depends on the active backend feature, e.g.
    /// `"sqlite::memory:"` or `"sqlite://mcs.db"` for SQLite and
    /// `"postgres://user:pass@host/db"` for Postgres.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] if the pool cannot be established or a
    ///   migration fails to apply.
    pub async fn connect(database_url: &str) -> StorageResult<Self> {
        Self::connect_with(database_url, PoolConfig::default()).await
    }

    /// Connects to `database_url` with explicit pool tuning, building the pool
    /// from `pool` and running migrations.
    ///
    /// The [`PoolConfig`] knobs (max connections, acquire/idle/lifetime
    /// timeouts, and a Postgres-only `statement_timeout`) are applied to the
    /// [`sqlx::pool::PoolOptions`] before connecting. In-memory SQLite is always
    /// pinned to a single connection regardless of
    /// [`max_connections`](PoolConfig::max_connections) — every in-memory SQLite
    /// connection has its own private database, so a multi-connection pool would
    /// scatter writes across disjoint databases and reads would
    /// non-deterministically miss them.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] if the pool cannot be established or a
    ///   migration fails to apply.
    pub async fn connect_with(database_url: &str, pool: PoolConfig) -> StorageResult<Self> {
        // In-memory SQLite must use exactly one connection (see method docs);
        // every other backend/URL honours the configured maximum.
        let is_memory_sqlite = database_url.contains(":memory:");
        let max_connections = if is_memory_sqlite {
            1
        } else {
            pool.max_connections.max(1)
        };

        let options = sqlx::pool::PoolOptions::<Backend>::new()
            .max_connections(max_connections)
            .acquire_timeout(pool.acquire_timeout)
            .idle_timeout(pool.idle_timeout)
            .max_lifetime(pool.max_lifetime);

        // `statement_timeout` is a Postgres server setting; apply it per
        // connection via `SET`. On SQLite the option is silently ignored: there
        // is no equivalent server-side statement timeout, and `is_memory_sqlite`
        // would never be Postgres anyway.
        #[cfg(all(feature = "postgres", not(feature = "sqlite")))]
        let options = match pool.statement_timeout {
            Some(timeout) => {
                let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
                options.after_connect(move |conn, _meta| {
                    Box::pin(async move {
                        use sqlx::Executor;
                        conn.execute(format!("SET statement_timeout = {millis}").as_str())
                            .await?;
                        Ok(())
                    })
                })
            }
            None => options,
        };

        let pool = options.connect(database_url).await?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|e| StorageError::Backend(format!("running migrations: {e}")))?;
        Ok(Self { pool })
    }

    /// Builds a [`SqlxStorage`] from an already-configured pool, running
    /// migrations against it.
    ///
    /// Useful when the caller needs to tune pool options (max connections,
    /// timeouts) before handing the pool over.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Backend`] if a migration fails to apply.
    pub async fn from_pool(pool: DbPool) -> StorageResult<Self> {
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|e| StorageError::Backend(format!("running migrations: {e}")))?;
        Ok(Self { pool })
    }

    /// Returns a reference to the underlying connection pool.
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl Repositories for SqlxStorage {
    fn users(&self) -> &dyn UserRepo {
        self
    }

    fn games(&self) -> &dyn GameRepo {
        self
    }

    fn actions(&self) -> &dyn ActionLogRepo {
        self
    }

    fn seeks(&self) -> &dyn SeekRepo {
        self
    }

    fn challenges(&self) -> &dyn ChallengeRepo {
        self
    }

    fn sessions(&self) -> &dyn SessionRepo {
        self
    }

    fn revoked_tokens(&self) -> &dyn RevokedTokenRepo {
        self
    }

    fn ratings(&self) -> &dyn RatingRepo {
        self
    }

    fn rating_history(&self) -> &dyn RatingHistoryRepo {
        self
    }

    fn payments(&self) -> &dyn PaymentStore {
        self
    }
}

// ---------------------------------------------------------------------------
// UserRepo
// ---------------------------------------------------------------------------

#[async_trait]
impl UserRepo for SqlxStorage {
    async fn create(&self, user: &User) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO users (id, address, username, created_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(user.id.to_string())
        .bind(user.address.to_string())
        .bind(user.username.clone())
        .bind(encode_time(user.created_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: UserId) -> StorageResult<User> {
        let row = sqlx::query("SELECT id, address, username, created_at FROM users WHERE id = $1")
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?;
        user_from_row(&row)
    }

    async fn find_by_address(&self, addr: &EvmAddress) -> StorageResult<Option<User>> {
        let row =
            sqlx::query("SELECT id, address, username, created_at FROM users WHERE address = $1")
                .bind(addr.to_string())
                .fetch_optional(&self.pool)
                .await?;
        row.map(|r| user_from_row(&r)).transpose()
    }

    async fn upsert_by_address(&self, addr: &EvmAddress) -> StorageResult<User> {
        // Fast path: the address is already registered.
        if let Some(user) = self.find_by_address(addr).await? {
            return Ok(user);
        }

        // Slow path: insert a fresh user. A concurrent request may win the race
        // and insert first; the unique index on `address` then turns our INSERT
        // into a `Conflict`, at which point we re-read the winner's row.
        let user = User::new(addr.clone(), None, OffsetDateTime::now_utc());
        match UserRepo::create(self, &user).await {
            Ok(()) => Ok(user),
            Err(StorageError::Conflict(_)) => self
                .find_by_address(addr)
                .await?
                .ok_or_else(|| StorageError::Backend("upsert race left no row".to_owned())),
            Err(e) => Err(e),
        }
    }

    async fn set_username(&self, user: UserId, name: &str) -> StorageResult<()> {
        // Case-insensitive uniqueness is enforced by the `LOWER(username)`
        // unique index from migration 0010: an UPDATE that would collide with a
        // *different* user's name (in any casing) trips the index and surfaces as
        // `StorageError::Conflict`. Re-assigning the user the same name they
        // already hold updates their own row, so it does not collide.
        let affected = sqlx::query("UPDATE users SET username = $1 WHERE id = $2")
            .bind(name)
            .bind(user.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected();

        if affected == 0 {
            // No row matched the id: either the user does not exist, or the
            // UPDATE was a no-op because the stored name is byte-for-byte equal.
            // Distinguish the two by checking existence, so a re-assignment of an
            // unchanged name is a success rather than a spurious NotFound.
            return match UserRepo::get(self, user).await {
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            };
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GameRepo
// ---------------------------------------------------------------------------

#[async_trait]
impl GameRepo for SqlxStorage {
    async fn create(&self, game: &Game) -> StorageResult<()> {
        let outcome = game.outcome.as_ref().map(encode_json).transpose()?;
        sqlx::query(
            "INSERT INTO games \
             (id, variant_id, variant_options, white, black, lifecycle, outcome, time_control, \
              rated, ply, clock_white_ms, clock_black_ms, side_to_move, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(game.id.to_string())
        .bind(game.variant_id.clone())
        .bind(encode_json(&game.variant_options)?)
        .bind(game.white.to_string())
        .bind(game.black.to_string())
        .bind(encode_lifecycle(game.lifecycle))
        .bind(outcome)
        .bind(encode_json(&game.time_control)?)
        .bind(encode_bool(game.rated))
        .bind(i64::from(game.ply))
        .bind(encode_clock(game.clock_white_ms))
        .bind(encode_clock(game.clock_black_ms))
        .bind(game.side_to_move.map(encode_color))
        .bind(encode_time(game.created_at)?)
        .bind(encode_time(game.updated_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: GameId) -> StorageResult<Game> {
        let row = sqlx::query(&format!("{GAME_SELECT} WHERE id = $1"))
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?;
        game_from_row(&row)
    }

    async fn update(&self, game: &Game) -> StorageResult<()> {
        let outcome = game.outcome.as_ref().map(encode_json).transpose()?;
        let affected = sqlx::query(
            "UPDATE games SET variant_id = $1, variant_options = $2, white = $3, black = $4, \
             lifecycle = $5, outcome = $6, time_control = $7, rated = $8, ply = $9, \
             clock_white_ms = $10, clock_black_ms = $11, side_to_move = $12, created_at = $13, \
             updated_at = $14 \
             WHERE id = $15",
        )
        .bind(game.variant_id.clone())
        .bind(encode_json(&game.variant_options)?)
        .bind(game.white.to_string())
        .bind(game.black.to_string())
        .bind(encode_lifecycle(game.lifecycle))
        .bind(outcome)
        .bind(encode_json(&game.time_control)?)
        .bind(encode_bool(game.rated))
        .bind(i64::from(game.ply))
        .bind(encode_clock(game.clock_white_ms))
        .bind(encode_clock(game.clock_black_ms))
        .bind(game.side_to_move.map(encode_color))
        .bind(encode_time(game.created_at)?)
        .bind(encode_time(game.updated_at)?)
        .bind(game.id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();

        if affected == 0 {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }

    async fn list_recent(&self, limit: u32) -> StorageResult<Vec<Game>> {
        let rows = sqlx::query(&format!("{GAME_SELECT} ORDER BY created_at DESC LIMIT $1"))
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(game_from_row).collect()
    }

    async fn list_for_user(&self, user: UserId, limit: u32) -> StorageResult<Vec<Game>> {
        let uid = user.to_string();
        let rows = sqlx::query(&format!(
            "{GAME_SELECT} WHERE white = $1 OR black = $2 ORDER BY created_at DESC LIMIT $3"
        ))
        .bind(&uid)
        .bind(&uid)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(game_from_row).collect()
    }

    async fn list_unfinished(&self) -> StorageResult<Vec<Game>> {
        // Anything not yet `finished` is unfinished — `created` and `active`
        // games. Ordering by `created_at` (oldest first) gives recovery a
        // stable, deterministic processing order.
        let rows = sqlx::query(&format!(
            "{GAME_SELECT} WHERE lifecycle != $1 ORDER BY created_at"
        ))
        .bind(encode_lifecycle(GameLifecycle::Finished))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(game_from_row).collect()
    }
}

// ---------------------------------------------------------------------------
// ActionLogRepo
// ---------------------------------------------------------------------------

/// The column list used by every `game_actions` read, kept beside
/// [`recorded_action_from_row`] so the query and the row-mapping stay aligned.
const ACTION_SELECT: &str = "SELECT ply, player, action, clock_white_ms, clock_black_ms, \
     created_at FROM game_actions";

/// Reconstructs a [`RecordedAction`] from a `game_actions` row.
fn recorded_action_from_row(row: &DbRow) -> Result<RecordedAction, StorageError> {
    Ok(RecordedAction {
        ply: decode_u32(row.try_get::<i64, _>("ply")?, "ply")?,
        player: decode_color(&row.try_get::<String, _>("player")?)?,
        // The action is stored as its JSON string; `Action` is `#[serde(transparent)]`
        // over a `serde_json::Value`, so decoding the column reproduces it exactly.
        action: decode_json::<Action>(&row.try_get::<String, _>("action")?)?,
        clock_white_ms: decode_clock(row.try_get::<Option<i64>, _>("clock_white_ms")?)?,
        clock_black_ms: decode_clock(row.try_get::<Option<i64>, _>("clock_black_ms")?)?,
        created_at: decode_time(&row.try_get::<String, _>("created_at")?)?,
    })
}

#[async_trait]
impl ActionLogRepo for SqlxStorage {
    async fn append(&self, game_id: GameId, action: &RecordedAction) -> StorageResult<()> {
        // A duplicate `(game_id, ply)` violates the primary key; the sqlx error
        // mapping turns that uniqueness violation into `StorageError::Conflict`,
        // so a double-append is reported rather than silently swallowed.
        sqlx::query(
            "INSERT INTO game_actions \
             (game_id, ply, player, action, clock_white_ms, clock_black_ms, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(game_id.to_string())
        .bind(i64::from(action.ply))
        .bind(encode_color(action.player))
        .bind(encode_json(&action.action)?)
        .bind(encode_clock(action.clock_white_ms))
        .bind(encode_clock(action.clock_black_ms))
        .bind(encode_time(action.created_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list(&self, game_id: GameId) -> StorageResult<Vec<RecordedAction>> {
        let rows = sqlx::query(&format!("{ACTION_SELECT} WHERE game_id = $1 ORDER BY ply"))
            .bind(game_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(recorded_action_from_row).collect()
    }

    async fn last_ply(&self, game_id: GameId) -> StorageResult<Option<u32>> {
        // `MAX(ply)` yields one row whose value is NULL when the log is empty,
        // which maps cleanly onto `Option<i64>` → `Option<u32>`.
        let row = sqlx::query("SELECT MAX(ply) AS max_ply FROM game_actions WHERE game_id = $1")
            .bind(game_id.to_string())
            .fetch_one(&self.pool)
            .await?;
        row.try_get::<Option<i64>, _>("max_ply")?
            .map(|v| decode_u32(v, "ply"))
            .transpose()
    }
}

// ---------------------------------------------------------------------------
// SeekRepo
// ---------------------------------------------------------------------------

#[async_trait]
impl SeekRepo for SqlxStorage {
    async fn create(&self, seek: &Seek) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO seeks \
             (id, creator, variant_id, time_control, color_preference, rated, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(seek.id.to_string())
        .bind(seek.creator.to_string())
        .bind(seek.variant_id.clone())
        .bind(encode_json(&seek.time_control)?)
        .bind(encode_color_pref(seek.color_preference))
        .bind(encode_bool(seek.rated))
        .bind(encode_time(seek.created_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: SeekId) -> StorageResult<Option<Seek>> {
        let row = sqlx::query(
            "SELECT id, creator, variant_id, time_control, color_preference, rated, created_at \
             FROM seeks WHERE id = $1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| seek_from_row(&r)).transpose()
    }

    async fn remove(&self, id: SeekId) -> StorageResult<()> {
        // Idempotent: deleting an absent seek affects zero rows and is not an
        // error — the desired post-condition (seek absent) already holds.
        sqlx::query("DELETE FROM seeks WHERE id = $1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn claim(&self, id: SeekId) -> StorageResult<ClaimOutcome> {
        // Atomic claim: a single DELETE both removes the seek and reports how
        // many rows it touched. Because the delete is the test, two concurrent
        // accepts of the same seek can never both observe it as present — at
        // most one DELETE matches a row, so exactly one caller is `Claimed`.
        // This mirrors the single-use nonce consumption in `consume_nonce`.
        let affected = sqlx::query("DELETE FROM seeks WHERE id = $1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected();

        if affected > 0 {
            Ok(ClaimOutcome::Claimed)
        } else {
            Ok(ClaimOutcome::AlreadyClaimed)
        }
    }

    async fn list_open(&self) -> StorageResult<Vec<Seek>> {
        let rows = sqlx::query(
            "SELECT id, creator, variant_id, time_control, color_preference, rated, created_at \
             FROM seeks",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(seek_from_row).collect()
    }

    async fn purge_stale(&self, older_than: OffsetDateTime) -> StorageResult<u64> {
        // Delete open seeks that predate the cutoff. The lexicographic
        // comparison is sound because both sides are RFC 3339 UTC timestamps.
        let affected = sqlx::query("DELETE FROM seeks WHERE created_at < $1")
            .bind(encode_time(older_than)?)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected)
    }
}

// ---------------------------------------------------------------------------
// ChallengeRepo
// ---------------------------------------------------------------------------

#[async_trait]
impl ChallengeRepo for SqlxStorage {
    async fn create(&self, challenge: &Challenge) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO challenges \
             (id, challenger, challenged, variant_id, time_control, rated, color_preference, \
              status, game_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(challenge.id.to_string())
        .bind(challenge.challenger.to_string())
        .bind(challenge.challenged.to_string())
        .bind(challenge.variant_id.clone())
        .bind(encode_json(&challenge.time_control)?)
        .bind(encode_bool(challenge.rated))
        .bind(encode_color_pref(challenge.color_preference))
        .bind(encode_challenge_status(challenge.status))
        .bind(challenge.game_id.map(|id| id.to_string()))
        .bind(encode_time(challenge.created_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: ChallengeId) -> StorageResult<Challenge> {
        let row = sqlx::query(&format!("{CHALLENGE_SELECT} WHERE id = $1"))
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?;
        challenge_from_row(&row)
    }

    async fn list_incoming(&self, user: UserId) -> StorageResult<Vec<Challenge>> {
        let rows = sqlx::query(&format!(
            "{CHALLENGE_SELECT} WHERE challenged = $1 AND status = $2 ORDER BY created_at DESC"
        ))
        .bind(user.to_string())
        .bind(encode_challenge_status(ChallengeStatus::Pending))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(challenge_from_row).collect()
    }

    async fn list_outgoing(&self, user: UserId) -> StorageResult<Vec<Challenge>> {
        let rows = sqlx::query(&format!(
            "{CHALLENGE_SELECT} WHERE challenger = $1 AND status = $2 ORDER BY created_at DESC"
        ))
        .bind(user.to_string())
        .bind(encode_challenge_status(ChallengeStatus::Pending))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(challenge_from_row).collect()
    }

    async fn update(&self, challenge: &Challenge) -> StorageResult<()> {
        let affected = sqlx::query("UPDATE challenges SET status = $1, game_id = $2 WHERE id = $3")
            .bind(encode_challenge_status(challenge.status))
            .bind(challenge.game_id.map(|id| id.to_string()))
            .bind(challenge.id.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected();

        if affected == 0 {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }

    async fn purge_resolved(&self, older_than: OffsetDateTime) -> StorageResult<u64> {
        // Delete declined and canceled challenges older than the cutoff.
        // Accepted challenges are attached to a game and kept for history.
        // The lexicographic comparison is sound because both sides are RFC 3339
        // UTC timestamps.
        let affected =
            sqlx::query("DELETE FROM challenges WHERE status IN ($1, $2) AND created_at < $3")
                .bind(encode_challenge_status(ChallengeStatus::Declined))
                .bind(encode_challenge_status(ChallengeStatus::Canceled))
                .bind(encode_time(older_than)?)
                .execute(&self.pool)
                .await?
                .rows_affected();
        Ok(affected)
    }
}

// ---------------------------------------------------------------------------
// SessionRepo
// ---------------------------------------------------------------------------

#[async_trait]
impl SessionRepo for SqlxStorage {
    async fn store_nonce(
        &self,
        address: &EvmAddress,
        nonce: &str,
        expires_at: OffsetDateTime,
    ) -> StorageResult<()> {
        // Supersede any earlier entry for the same (address, nonce) pair. The
        // ON CONFLICT clause is supported identically by SQLite (3.24+) and
        // Postgres, keeping the statement portable.
        sqlx::query(
            "INSERT INTO auth_nonces (address, nonce, expires_at) VALUES ($1, $2, $3) \
             ON CONFLICT (address, nonce) DO UPDATE SET expires_at = excluded.expires_at",
        )
        .bind(address.to_string())
        .bind(nonce)
        .bind(encode_time(expires_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn consume_nonce(&self, address: &EvmAddress, nonce: &str) -> StorageResult<bool> {
        // Atomic single-use consumption: a single DELETE removes the row only
        // when it exists *and* has not expired, then reports how many rows it
        // touched. Because the delete is the test, two concurrent calls can
        // never both observe the nonce as valid — at most one DELETE matches.
        //
        // The expiry comparison is a lexicographic string comparison, which is
        // sound because both sides are RFC 3339 UTC timestamps.
        let now = encode_time(OffsetDateTime::now_utc())?;
        let affected = sqlx::query(
            "DELETE FROM auth_nonces WHERE address = $1 AND nonce = $2 AND expires_at > $3",
        )
        .bind(address.to_string())
        .bind(nonce)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(affected > 0)
    }

    async fn purge_expired_nonces(&self, now: OffsetDateTime) -> StorageResult<u64> {
        // Drop nonces whose expiry has already passed. The lexicographic
        // comparison is sound because both sides are RFC 3339 UTC timestamps.
        let affected = sqlx::query("DELETE FROM auth_nonces WHERE expires_at <= $1")
            .bind(encode_time(now)?)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected)
    }
}

// ---------------------------------------------------------------------------
// RevokedTokenRepo
// ---------------------------------------------------------------------------

#[async_trait]
impl RevokedTokenRepo for SqlxStorage {
    async fn revoke(&self, jti: &str, expires_at: OffsetDateTime) -> StorageResult<()> {
        // Idempotent: revoking the same `jti` twice keeps the (identical) entry.
        // The ON CONFLICT clause is supported identically by SQLite (3.24+) and
        // Postgres, keeping the statement portable.
        sqlx::query(
            "INSERT INTO revoked_tokens (jti, expires_at) VALUES ($1, $2) \
             ON CONFLICT (jti) DO UPDATE SET expires_at = excluded.expires_at",
        )
        .bind(jti)
        .bind(encode_time(expires_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn is_revoked(&self, jti: &str) -> StorageResult<bool> {
        // A single indexed point lookup on the primary key. We do not filter on
        // expiry here: an expired-but-still-listed token is independently
        // rejected on its `exp`, and `purge_expired` keeps such rows from piling
        // up — so presence alone answers "is this token revoked?".
        let row = sqlx::query("SELECT 1 AS present FROM revoked_tokens WHERE jti = $1")
            .bind(jti)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    async fn purge_expired(&self, now: OffsetDateTime) -> StorageResult<u64> {
        // Drop entries whose token has already expired (and is thus rejected on
        // expiry anyway). The comparison is a lexicographic string comparison,
        // which is sound because both sides are RFC 3339 UTC timestamps.
        let affected = sqlx::query("DELETE FROM revoked_tokens WHERE expires_at <= $1")
            .bind(encode_time(now)?)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected)
    }
}

// ---------------------------------------------------------------------------
// RatingRepo
// ---------------------------------------------------------------------------

/// Reconstructs a [`Rating`] from a database row.
fn rating_from_row(row: &DbRow) -> Result<Rating, StorageError> {
    Ok(Rating {
        value: row.try_get::<f64, _>("value")?,
        deviation: row.try_get::<f64, _>("deviation")?,
        volatility: row.try_get::<f64, _>("volatility")?,
    })
}

#[async_trait]
impl RatingRepo for SqlxStorage {
    async fn get(
        &self,
        user: UserId,
        variant_id: &str,
        time_class: TimeClass,
    ) -> StorageResult<Option<Rating>> {
        let row = sqlx::query(
            "SELECT value, deviation, volatility FROM ratings \
             WHERE user_id = $1 AND variant_id = $2 AND time_class = $3",
        )
        .bind(user.to_string())
        .bind(variant_id)
        .bind(time_class.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| rating_from_row(&r)).transpose()
    }

    async fn upsert(
        &self,
        user: UserId,
        variant_id: &str,
        time_class: TimeClass,
        rating: &Rating,
    ) -> StorageResult<()> {
        // INSERT OR REPLACE / ON CONFLICT … DO UPDATE are both supported by
        // SQLite (3.24+) and PostgreSQL with identical syntax.
        sqlx::query(
            "INSERT INTO ratings (user_id, variant_id, time_class, value, deviation, volatility) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (user_id, variant_id, time_class) \
             DO UPDATE SET value = excluded.value, \
                           deviation = excluded.deviation, \
                           volatility = excluded.volatility",
        )
        .bind(user.to_string())
        .bind(variant_id)
        .bind(time_class.as_str())
        .bind(rating.value)
        .bind(rating.deviation)
        .bind(rating.volatility)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn leaderboard(
        &self,
        variant_id: &str,
        time_class: TimeClass,
        limit: u32,
    ) -> StorageResult<Vec<(UserId, Rating)>> {
        let rows = sqlx::query(
            "SELECT user_id, value, deviation, volatility FROM ratings \
             WHERE variant_id = $1 AND time_class = $2 \
             ORDER BY value DESC \
             LIMIT $3",
        )
        .bind(variant_id)
        .bind(time_class.as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                let user_id = decode_id::<UserId>(&row.try_get::<String, _>("user_id")?)?;
                let rating = rating_from_row(row)?;
                Ok((user_id, rating))
            })
            .collect()
    }

    async fn list_for_user(&self, user: UserId) -> StorageResult<Vec<(String, TimeClass, Rating)>> {
        let rows = sqlx::query(
            "SELECT variant_id, time_class, value, deviation, volatility FROM ratings \
             WHERE user_id = $1 \
             ORDER BY variant_id, time_class",
        )
        .bind(user.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                let variant_id = row.try_get::<String, _>("variant_id")?;
                let time_class = decode_time_class(&row.try_get::<String, _>("time_class")?)?;
                let rating = rating_from_row(row)?;
                Ok((variant_id, time_class, rating))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// RatingHistoryRepo
// ---------------------------------------------------------------------------

/// Reconstructs a [`RatingHistoryEntry`] from a `rating_history` row.
fn rating_history_from_row(row: &DbRow) -> Result<RatingHistoryEntry, StorageError> {
    Ok(RatingHistoryEntry {
        user_id: decode_id::<UserId>(&row.try_get::<String, _>("user_id")?)?,
        variant_id: row.try_get::<String, _>("variant_id")?,
        time_class: decode_time_class(&row.try_get::<String, _>("time_class")?)?,
        value: row.try_get::<f64, _>("value")?,
        deviation: row.try_get::<f64, _>("deviation")?,
        game_id: decode_id::<GameId>(&row.try_get::<String, _>("game_id")?)?,
        created_at: decode_time(&row.try_get::<String, _>("created_at")?)?,
    })
}

#[async_trait]
impl RatingHistoryRepo for SqlxStorage {
    async fn record(&self, entry: &RatingHistoryEntry) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO rating_history \
             (user_id, variant_id, time_class, value, deviation, game_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(entry.user_id.to_string())
        .bind(&entry.variant_id)
        .bind(entry.time_class.as_str())
        .bind(entry.value)
        .bind(entry.deviation)
        .bind(entry.game_id.to_string())
        .bind(encode_time(entry.created_at)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list(
        &self,
        user: UserId,
        variant_id: &str,
        time_class: TimeClass,
        limit: u32,
    ) -> StorageResult<Vec<RatingHistoryEntry>> {
        // Most-recent-first. RFC 3339 timestamps sort lexicographically in
        // chronological order, so `ORDER BY created_at DESC` is correct on both
        // SQLite and Postgres.
        let rows = sqlx::query(
            "SELECT user_id, variant_id, time_class, value, deviation, game_id, created_at \
             FROM rating_history \
             WHERE user_id = $1 AND variant_id = $2 AND time_class = $3 \
             ORDER BY created_at DESC \
             LIMIT $4",
        )
        .bind(user.to_string())
        .bind(variant_id)
        .bind(time_class.as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(rating_history_from_row).collect()
    }
}

// ---------------------------------------------------------------------------
// PaymentStore — x402 settled-payment idempotency (#108)
// ---------------------------------------------------------------------------

/// Reconstructs a [`PaymentRecord`] from a `payments` row.
fn payment_from_row(row: &DbRow) -> Result<PaymentRecord, StorageError> {
    Ok(PaymentRecord {
        idempotency_key: row.try_get::<String, _>("idempotency_key")?,
        payer: row.try_get::<String, _>("payer")?,
        amount: row.try_get::<String, _>("amount")?,
        asset: row.try_get::<String, _>("asset")?,
        network: row.try_get::<String, _>("network")?,
        transaction: row.try_get::<Option<String>, _>("transaction_ref")?,
        resource: row.try_get::<String, _>("resource")?,
        created_at: decode_time(&row.try_get::<String, _>("created_at")?)?,
    })
}

/// Maps a [`StorageError`] arising inside [`PaymentStore`] onto the payment
/// crate's own error type: a uniqueness conflict on `idempotency_key` becomes
/// [`PaymentStoreError::Conflict`] (the "already recorded" signal), everything
/// else a [`PaymentStoreError::Backend`].
fn to_store_error(err: StorageError) -> PaymentStoreError {
    match err {
        StorageError::Conflict(_) => PaymentStoreError::Conflict,
        other => PaymentStoreError::Backend(other.to_string()),
    }
}

#[async_trait]
impl PaymentStore for SqlxStorage {
    async fn find(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<PaymentRecord>, PaymentStoreError> {
        let row = sqlx::query(
            "SELECT idempotency_key, payer, amount, asset, network, transaction_ref, resource, \
             created_at FROM payments WHERE idempotency_key = $1",
        )
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| to_store_error(e.into()))?;
        row.map(|r| payment_from_row(&r))
            .transpose()
            .map_err(to_store_error)
    }

    async fn record(&self, record: &PaymentRecord) -> Result<(), PaymentStoreError> {
        // The PRIMARY KEY on `idempotency_key` is the idempotency guarantee: a
        // duplicate INSERT violates it and surfaces as `StorageError::Conflict`,
        // which `to_store_error` maps to `PaymentStoreError::Conflict` — the
        // "already recorded" signal the middleware falls back on.
        let created_at = encode_time(record.created_at).map_err(to_store_error)?;
        sqlx::query(
            "INSERT INTO payments \
             (idempotency_key, payer, amount, asset, network, transaction_ref, resource, \
              created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&record.idempotency_key)
        .bind(&record.payer)
        .bind(&record.amount)
        .bind(&record.asset)
        .bind(&record.network)
        .bind(record.transaction.as_deref())
        .bind(&record.resource)
        .bind(created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| to_store_error(e.into()))?;
        Ok(())
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod pool_config_tests {
    use super::*;

    #[test]
    fn pool_config_default_is_conservative() {
        let cfg = PoolConfig::default();
        assert_eq!(cfg.max_connections, 10);
        assert_eq!(cfg.acquire_timeout, std::time::Duration::from_secs(30));
        assert_eq!(
            cfg.idle_timeout,
            Some(std::time::Duration::from_secs(10 * 60))
        );
        assert_eq!(cfg.max_lifetime, None);
        assert_eq!(cfg.statement_timeout, None);
    }

    /// In-memory SQLite is pinned to a single connection even when the config
    /// asks for many, so all access shares one coherent database.
    #[tokio::test]
    async fn connect_with_pins_in_memory_sqlite_to_single_connection() {
        let cfg = PoolConfig {
            max_connections: 32,
            ..PoolConfig::default()
        };
        let storage = SqlxStorage::connect_with("sqlite::memory:", cfg)
            .await
            .expect("connect in-memory sqlite with a large max_connections");
        // A multi-connection in-memory pool would give each acquire a private
        // empty database; with the single-connection pin both acquires see the
        // same migrated schema, so this size reflects the enforced cap.
        assert_eq!(storage.pool().options().get_max_connections(), 1);
    }

    /// A non-memory configuration honours the requested maximum.
    #[tokio::test]
    async fn connect_with_honours_max_connections_for_file_sqlite() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mcs_pool_{}.db", uuid::Uuid::new_v4().simple()));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let cfg = PoolConfig {
            max_connections: 4,
            ..PoolConfig::default()
        };
        let storage = SqlxStorage::connect_with(&url, cfg)
            .await
            .expect("connect file sqlite with a configured pool");
        assert_eq!(storage.pool().options().get_max_connections(), 4);
        drop(storage);
        let _ = std::fs::remove_file(&path);
    }
}
