-- Per-user, per-variant Glicko-2 rating records.
--
-- The primary key is the composite (user_id, variant_id) pair; there is
-- exactly one rating row per player per game variant.  All three Glicko-2
-- parameters are stored as DOUBLE PRECISION so they survive a round-trip
-- through both SQLite (IEEE 754 doubles via REAL affinity) and PostgreSQL
-- (an 8-byte float decoded as `f64`). Postgres `REAL` is only 4 bytes and
-- would not decode as `f64`, so the wider type is required for portability.
--
-- No foreign-key constraint on user_id is added: variants are application-
-- level strings, and the users table may not always be in the same schema
-- slice on Postgres.  Referential integrity is enforced by the application.

CREATE TABLE IF NOT EXISTS ratings (
    user_id    TEXT    NOT NULL,
    variant_id TEXT    NOT NULL,
    value      DOUBLE PRECISION NOT NULL,
    deviation  DOUBLE PRECISION NOT NULL,
    volatility DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (user_id, variant_id)
);

-- leaderboard() queries ORDER BY value DESC for a single variant.
CREATE INDEX IF NOT EXISTS idx_ratings_variant_value
    ON ratings (variant_id, value DESC);
