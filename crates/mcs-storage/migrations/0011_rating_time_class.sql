-- Key ratings per (variant, time_class) instead of per variant alone (#132).
--
-- A player's bullet strength is now tracked separately from their classical
-- strength: both the current-rating table and the append-only history log gain
-- a `time_class` column, and the `ratings` primary key widens to the composite
-- (user_id, variant_id, time_class).
--
-- Portability: the steps below use only DDL that SQLite and PostgreSQL both
-- accept. Changing a primary key in place is **not** portable (SQLite cannot
-- `ALTER TABLE ... ADD CONSTRAINT`), so `ratings` is rebuilt via the standard
-- create-copy-drop-rename dance, which both engines support identically.
--
-- Existing rows pre-date the time-class split, so they are backfilled with
-- `'classical'` — the conservative default for the legacy per-variant ratings
-- (historically these aggregated all paces; classical is the catch-all bucket).

-- 1. Rebuild `ratings` with the wider composite primary key.
CREATE TABLE ratings_new (
    user_id    TEXT    NOT NULL,
    variant_id TEXT    NOT NULL,
    time_class TEXT    NOT NULL,
    value      DOUBLE PRECISION NOT NULL,
    deviation  DOUBLE PRECISION NOT NULL,
    volatility DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (user_id, variant_id, time_class)
);

INSERT INTO ratings_new (user_id, variant_id, time_class, value, deviation, volatility)
    SELECT user_id, variant_id, 'classical', value, deviation, volatility
    FROM ratings;

DROP TABLE ratings;

ALTER TABLE ratings_new RENAME TO ratings;

-- leaderboard() queries ORDER BY value DESC for a single (variant, time_class).
CREATE INDEX IF NOT EXISTS idx_ratings_variant_class_value
    ON ratings (variant_id, time_class, value DESC);

-- 2. Add `time_class` to the history log, backfilling legacy rows to classical.
ALTER TABLE rating_history ADD COLUMN time_class TEXT NOT NULL DEFAULT 'classical';

-- `list(user, variant, time_class, limit)` filters on
-- (user_id, variant_id, time_class) and orders by created_at descending; this
-- composite index serves both the filter and the ordering.
CREATE INDEX IF NOT EXISTS idx_rating_history_user_variant_class_time
    ON rating_history (user_id, variant_id, time_class, created_at DESC);
