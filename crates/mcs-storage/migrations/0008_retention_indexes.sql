-- Indexes to support the periodic retention / GC sweeps introduced in #107.
--
-- The purge queries filter on timestamp columns that previously had no index.
-- Without an index each sweep is a full-table scan; these indexes make the
-- DELETE predicates efficient on both SQLite and PostgreSQL.

-- `purge_expired_nonces` deletes on `expires_at <= now`.
CREATE INDEX IF NOT EXISTS idx_auth_nonces_expires_at ON auth_nonces (expires_at);

-- `purge_stale` deletes seeks older than a cutoff by `created_at`.
CREATE INDEX IF NOT EXISTS idx_seeks_created_at ON seeks (created_at);

-- `purge_resolved` deletes challenges by `(status, created_at)`.
-- A composite index covers the status filter and the timestamp range together.
CREATE INDEX IF NOT EXISTS idx_challenges_status_created_at ON challenges (status, created_at);
