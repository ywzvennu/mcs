-- Initial MCS storage schema.
--
-- Portability note: this DDL must run unchanged on both SQLite and PostgreSQL,
-- so it sticks to the lowest common denominator:
--   * TEXT for identifiers (UUIDs as their canonical string form), Ethereum
--     addresses, enum discriminants, variant ids, and JSON-encoded value
--     objects (time controls, outcomes).
--   * TEXT for timestamps, stored as RFC 3339 / ISO 8601 strings in UTC. RFC
--     3339 sorts lexicographically in chronological order, so plain
--     `ORDER BY ... DESC` gives "newest first" on both engines without needing
--     backend-specific date functions.
--   * No AUTOINCREMENT, SERIAL, ENUM types, or `now()` defaults — every value
--     is supplied by the application layer.

CREATE TABLE IF NOT EXISTS users (
    id         TEXT PRIMARY KEY,
    address    TEXT NOT NULL UNIQUE,
    username   TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS games (
    id           TEXT PRIMARY KEY,
    variant_id   TEXT NOT NULL,
    white        TEXT NOT NULL,
    black        TEXT NOT NULL,
    lifecycle    TEXT NOT NULL,
    -- JSON-encoded `mcs_core::Outcome`, NULL until the game is finished.
    outcome      TEXT,
    -- JSON-encoded `mcs_domain::TimeControl`.
    time_control TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- Listing a player's games filters on either colour; index both columns.
CREATE INDEX IF NOT EXISTS idx_games_white ON games (white);
CREATE INDEX IF NOT EXISTS idx_games_black ON games (black);
-- `list_recent` and `list_for_user` both order by creation time, newest first.
CREATE INDEX IF NOT EXISTS idx_games_created_at ON games (created_at);

CREATE TABLE IF NOT EXISTS seeks (
    id               TEXT PRIMARY KEY,
    creator          TEXT NOT NULL,
    variant_id       TEXT NOT NULL,
    -- JSON-encoded `mcs_domain::TimeControl`.
    time_control     TEXT NOT NULL,
    color_preference TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

-- SIWE auth nonces. A nonce is single-use: `consume_nonce` deletes the row it
-- validates, so the table only ever holds nonces awaiting their first (and
-- only) successful consumption. The composite primary key matches the lookup
-- key used by the session repository.
CREATE TABLE IF NOT EXISTS auth_nonces (
    address    TEXT NOT NULL,
    nonce      TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (address, nonce)
);
