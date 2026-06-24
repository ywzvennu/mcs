-- Add the variant options and the live in-progress snapshot to `games`.
--
-- These columns let a recovering server rebuild and present an in-progress
-- game: `variant_options` (with `variant_id`) re-creates the session, while
-- the snapshot columns record the latest observed move count, clocks, and side
-- to move.
--
-- Portability note: as with `0001_init.sql`, this DDL must run unchanged on
-- both SQLite and PostgreSQL, so it sticks to the lowest common denominator —
-- `ALTER TABLE ... ADD COLUMN` with TEXT/INTEGER types and simple literal
-- defaults. Each `ADD COLUMN` is its own statement (both engines forbid adding
-- several columns in one `ALTER TABLE`).

-- JSON-encoded `mcs_core::VariantOptions`. Existing rows predate per-game
-- options, so they default to the JSON null literal, which deserialises to
-- `VariantOptions::default()`.
ALTER TABLE games ADD COLUMN variant_options TEXT NOT NULL DEFAULT 'null';

-- Live snapshot: half-moves played so far.
ALTER TABLE games ADD COLUMN ply INTEGER NOT NULL DEFAULT 0;

-- Live snapshot: remaining clocks in milliseconds; NULL for untimed games or
-- before the first snapshot is recorded.
ALTER TABLE games ADD COLUMN clock_white_ms INTEGER;
ALTER TABLE games ADD COLUMN clock_black_ms INTEGER;

-- Live snapshot: whose turn it is, as the lowercase `mcs_core::Color`
-- discriminant ("white"/"black"); NULL when not applicable.
ALTER TABLE games ADD COLUMN side_to_move TEXT;
