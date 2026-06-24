-- Per-user, per-variant Glicko-2 rating records.
--
-- The primary key is the composite (user_id, variant_id) pair; there is
-- exactly one rating row per player per game variant.  All three Glicko-2
-- parameters are stored as REAL so they survive a round-trip through both
-- SQLite (which uses IEEE 754 doubles) and PostgreSQL (DOUBLE PRECISION).
--
-- No foreign-key constraint on user_id is added: variants are application-
-- level strings, and the users table may not always be in the same schema
-- slice on Postgres.  Referential integrity is enforced by the application.

CREATE TABLE IF NOT EXISTS ratings (
    user_id    TEXT    NOT NULL,
    variant_id TEXT    NOT NULL,
    value      REAL    NOT NULL,
    deviation  REAL    NOT NULL,
    volatility REAL    NOT NULL,
    PRIMARY KEY (user_id, variant_id)
);

-- leaderboard() queries ORDER BY value DESC for a single variant.
CREATE INDEX IF NOT EXISTS idx_ratings_variant_value
    ON ratings (variant_id, value DESC);
