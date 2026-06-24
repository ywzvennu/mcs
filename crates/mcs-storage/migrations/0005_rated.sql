-- Add the rated/casual distinction to `games` and `seeks`.
--
-- A game (or seek) is either *rated* — it feeds the post-game Glicko-2 update —
-- or *casual*, which is exempt from rating changes. Both players agree on this
-- at matchmaking, so the flag is fixed for the life of the game.
--
-- Portability note: as with the earlier migrations, this DDL must run unchanged
-- on both SQLite and PostgreSQL, so it sticks to the lowest common denominator —
-- `ALTER TABLE ... ADD COLUMN` with a BIGINT boolean (0 = casual, 1 = rated),
-- matching how the codebase already stores other small integers (bound and read
-- as 8-byte `i64`, which maps onto Postgres `BIGINT`). Each `ADD COLUMN` is its
-- own statement (both engines forbid adding several columns in one
-- `ALTER TABLE`).
--
-- Existing rows predate the distinction, so they default to `1` (rated),
-- preserving their original rating behaviour.

ALTER TABLE games ADD COLUMN rated BIGINT NOT NULL DEFAULT 1;

ALTER TABLE seeks ADD COLUMN rated BIGINT NOT NULL DEFAULT 1;
