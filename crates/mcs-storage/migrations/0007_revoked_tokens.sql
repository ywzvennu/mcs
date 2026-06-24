-- Session-token revocation denylist (#101).
--
-- Session JWTs are stateless: once issued, a token is otherwise valid until its
-- `exp`. To support logout / revocation, each token carries a unique `jti`
-- (JWT ID). Revoking a token records its `jti` here; the auth extractor checks
-- this table on every authenticated request and rejects a token whose `jti` is
-- present. A different (non-revoked) token is unaffected.
--
-- Self-trimming: a revoked entry only needs to outlive the token it denies. The
-- token is rejected on `exp` regardless, so `expires_at` is the point past which
-- the entry can be purged. `purge_expired` deletes entries whose `expires_at`
-- has passed, keeping the denylist bounded.
--
-- Portability note: as with the earlier migrations, this DDL runs unchanged on
-- both SQLite and PostgreSQL — TEXT for the `jti` key and an RFC 3339 TEXT
-- timestamp for `expires_at` (which sorts lexicographically in chronological
-- order, so the purge predicate is a plain string comparison).

CREATE TABLE IF NOT EXISTS revoked_tokens (
    jti        TEXT PRIMARY KEY,
    expires_at TEXT NOT NULL
);

-- `purge_expired` filters on `expires_at`, so index it for the sweep.
CREATE INDEX IF NOT EXISTS idx_revoked_tokens_expires_at ON revoked_tokens (expires_at);
