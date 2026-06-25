-- Settled x402 payments, for idempotent paid actions (#108).
--
-- A paid action (today: game creation via `POST /seeks`) must charge the payer
-- at most ONCE even if the client retries the same `X-PAYMENT` header — after a
-- dropped connection, a proxy retry, or a double-click. The x402 verify+settle
-- step is not naturally idempotent, so the payment middleware derives a stable
-- `idempotency_key` from the payment payload (for the "exact"/EIP-3009 scheme,
-- the single-use on-chain authorization nonce; otherwise a content hash) and
-- records one row here the first time a payment settles.
--
-- The PRIMARY KEY on `idempotency_key` is the idempotency guarantee: a second
-- INSERT under the same key violates it, which the storage layer surfaces as a
-- conflict — the signal that the payment was already recorded (so the request is
-- served from this row rather than re-verified, re-settled, and re-charged).
--
-- Portability note: like the earlier migrations this DDL runs unchanged on both
-- SQLite and PostgreSQL — TEXT for the key, addresses, and amounts (amounts are
-- token base-units kept as strings to preserve arbitrary precision exactly as
-- they appear on the wire), a nullable TEXT for the optional transaction hash,
-- and an RFC 3339 TEXT timestamp for `created_at`.

CREATE TABLE IF NOT EXISTS payments (
    idempotency_key TEXT PRIMARY KEY,
    payer           TEXT NOT NULL,
    amount          TEXT NOT NULL,
    asset           TEXT NOT NULL,
    network         TEXT NOT NULL,
    transaction_ref TEXT,
    resource        TEXT NOT NULL,
    created_at      TEXT NOT NULL
);

-- Operational queries (e.g. listing a payer's recorded payments) filter on the
-- payer address; index it so those scans stay cheap.
CREATE INDEX IF NOT EXISTS idx_payments_payer ON payments (payer);
