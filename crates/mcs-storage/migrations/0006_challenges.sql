-- Direct challenges: an invitation from one specific player to another.
--
-- Unlike a seek (which floats in an open pool and is paired by the matchmaker),
-- a challenge names its opponent up front and never enters matchmaking;
-- accepting one creates a game directly.
--
-- Portability note: as with the earlier migrations, this DDL runs unchanged on
-- both SQLite and PostgreSQL, so it sticks to the lowest common denominator —
-- TEXT for ids, enum discriminants, variant ids, and JSON-encoded value objects
-- (the time control); a BIGINT boolean (0 = casual, 1 = rated) for `rated`,
-- matching how the codebase stores other small integers (bound and read as
-- 8-byte `i64`, which maps onto Postgres `BIGINT`); and RFC 3339 TEXT for
-- timestamps. The `game_id` is NULL until the challenge is accepted.

CREATE TABLE IF NOT EXISTS challenges (
    id               TEXT PRIMARY KEY,
    challenger       TEXT NOT NULL,
    challenged       TEXT NOT NULL,
    variant_id       TEXT NOT NULL,
    -- JSON-encoded `mcs_domain::TimeControl`.
    time_control     TEXT NOT NULL,
    rated            BIGINT NOT NULL DEFAULT 1,
    color_preference TEXT NOT NULL,
    -- One of: pending, accepted, declined, canceled.
    status           TEXT NOT NULL,
    -- The created game, NULL until the challenge is accepted.
    game_id          TEXT,
    created_at       TEXT NOT NULL
);

-- The incoming/outgoing listings filter on a participant plus the pending
-- status, so index both participant columns.
CREATE INDEX IF NOT EXISTS idx_challenges_challenger ON challenges (challenger);
CREATE INDEX IF NOT EXISTS idx_challenges_challenged ON challenges (challenged);
