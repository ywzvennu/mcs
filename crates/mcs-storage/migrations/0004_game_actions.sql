-- Persistent, append-only action log: one row per half-move (ply) recorded for
-- a game.
--
-- This is the authoritative record of *what was played*, distinct from the live
-- snapshot columns on `games` (which only carry the latest observed position).
-- Replaying a game's actions in `ply` order reconstructs the full move history,
-- which a recovering server can feed back into a `GameSession`.
--
-- Portability note: as with the earlier migrations, this DDL must run unchanged
-- on both SQLite and PostgreSQL, so it sticks to the lowest common denominator —
-- TEXT/INTEGER columns and a composite PRIMARY KEY. The `(game_id, ply)` primary
-- key both enforces "one action per ply per game" (a duplicate append surfaces
-- as a uniqueness conflict) and provides the index that backs `game_id`-scoped
-- ordered reads, so no extra index is needed.

CREATE TABLE game_actions (
    -- The owning game's id, as a canonical UUID string (matches `games.id`).
    game_id         TEXT    NOT NULL,
    -- Zero-based half-move index within the game; unique per game.
    ply             INTEGER NOT NULL,
    -- Who played, as the lowercase `mcs_core::Color` discriminant
    -- ("white"/"black").
    player          TEXT    NOT NULL,
    -- JSON-encoded `mcs_core::Action` (a type-erased serde value).
    action          TEXT    NOT NULL,
    -- Remaining clocks in milliseconds at the moment the action was recorded;
    -- NULL for untimed games.
    clock_white_ms  INTEGER,
    clock_black_ms  INTEGER,
    -- When the action was recorded, as an RFC 3339 UTC timestamp.
    created_at      TEXT    NOT NULL,
    PRIMARY KEY (game_id, ply)
);
