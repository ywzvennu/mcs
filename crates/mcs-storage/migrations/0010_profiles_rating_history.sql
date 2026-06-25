-- Username editing + per-user rating history (#126).
--
-- Two additions, both portable across SQLite and PostgreSQL:
--
--   1. A case-insensitive uniqueness guarantee on `users.username`, so two
--      usernames that differ only by case (e.g. "Alice" / "alice") cannot both
--      exist. The expression index `LOWER(username)` is supported identically by
--      SQLite (3.9+) and PostgreSQL. NULL usernames are exempt (an account may
--      have no display name, and multiple NULLs never collide under a unique
--      index on either engine), so the existing rows are unaffected.
--
--   2. An append-only `rating_history` table: one snapshot of a player's rating
--      in a variant, recorded each time a rated game is scored. A single rated
--      game appends two rows (one per player). Timestamps are RFC 3339 TEXT in
--      UTC, so "most recent first" is a plain `ORDER BY created_at DESC` that
--      behaves the same on both engines. `value` and `deviation` use
--      DOUBLE PRECISION for the same f64-portability reason as the `ratings`
--      table (Postgres REAL is only 4 bytes and would not decode as f64).

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_username_lower
    ON users (LOWER(username));

CREATE TABLE IF NOT EXISTS rating_history (
    user_id    TEXT NOT NULL,
    variant_id TEXT NOT NULL,
    value      DOUBLE PRECISION NOT NULL,
    deviation  DOUBLE PRECISION NOT NULL,
    game_id    TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- `list(user, variant, limit)` filters on (user_id, variant_id) and orders by
-- created_at descending; this composite index serves both the filter and the
-- ordering.
CREATE INDEX IF NOT EXISTS idx_rating_history_user_variant_time
    ON rating_history (user_id, variant_id, created_at DESC);
