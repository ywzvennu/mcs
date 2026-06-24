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
    ColorPreference, EvmAddress, Game, GameId, GameLifecycle, Rating, Seek, SeekId, User, UserId,
};
use sqlx::Row;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{
    action_log::{ActionLogRepo, RecordedAction},
    error::{StorageError, StorageResult},
    GameRepo, RatingRepo, Repositories, SeekRepo, SessionRepo, UserRepo,
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
        let pool = DbPool::connect(database_url).await?;
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

    fn sessions(&self) -> &dyn SessionRepo {
        self
    }

    fn ratings(&self) -> &dyn RatingRepo {
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

    async fn list_open(&self) -> StorageResult<Vec<Seek>> {
        let rows = sqlx::query(
            "SELECT id, creator, variant_id, time_control, color_preference, rated, created_at \
             FROM seeks",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(seek_from_row).collect()
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
    async fn get(&self, user: UserId, variant_id: &str) -> StorageResult<Option<Rating>> {
        let row = sqlx::query(
            "SELECT value, deviation, volatility FROM ratings \
             WHERE user_id = $1 AND variant_id = $2",
        )
        .bind(user.to_string())
        .bind(variant_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| rating_from_row(&r)).transpose()
    }

    async fn upsert(&self, user: UserId, variant_id: &str, rating: &Rating) -> StorageResult<()> {
        // INSERT OR REPLACE / ON CONFLICT … DO UPDATE are both supported by
        // SQLite (3.24+) and PostgreSQL with identical syntax.
        sqlx::query(
            "INSERT INTO ratings (user_id, variant_id, value, deviation, volatility) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (user_id, variant_id) \
             DO UPDATE SET value = excluded.value, \
                           deviation = excluded.deviation, \
                           volatility = excluded.volatility",
        )
        .bind(user.to_string())
        .bind(variant_id)
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
        limit: u32,
    ) -> StorageResult<Vec<(UserId, Rating)>> {
        let rows = sqlx::query(
            "SELECT user_id, value, deviation, volatility FROM ratings \
             WHERE variant_id = $1 \
             ORDER BY value DESC \
             LIMIT $2",
        )
        .bind(variant_id)
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
}
