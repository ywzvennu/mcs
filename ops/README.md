# MCS — Operations Guide

This document covers building and running the MCS server with Docker, the full
environment-variable reference, and guidance on single-node vs. cluster
deployments.

---

## Quick start

### Build the image

```sh
docker build -t mcs-server:latest .
```

The Dockerfile is a multi-stage build:

1. **builder** — `rust:1-slim-bookworm`; compiles `mcs-server --release`.
   Dependency crates are cached in a separate layer so incremental rebuilds
   caused by application-source changes skip the slow dep-compile step.
2. **runtime** — `debian:bookworm-slim`; contains only the compiled binary,
   CA certificates, and the `libssl3` runtime library. Runs as a non-root user
   (`uid=1001`, `gid=1001`).

### Single-node run

```sh
docker run --rm \
  -p 8080:8080 \
  -v mcs-data:/data \
  -e MCS_SESSION__SECRET="$(openssl rand -hex 32)" \
  mcs-server:latest
```

The server is reachable at `http://localhost:8080`.

### Cluster stack (two nodes + Redis)

```sh
# Generate a stable shared secret and paste it into docker-compose.yml first:
openssl rand -hex 32

# Then:
docker compose up --build
```

| Service  | Host port | Role                        |
|----------|-----------|-----------------------------|
| `redis`  | (internal)| Cluster membership backend  |
| `node-a` | 8081      | mcs-server cluster node A   |
| `node-b` | 8082      | mcs-server cluster node B   |

Both nodes register with Redis via `MCS_CLUSTER__REDIS_URL` and are reachable
at their respective `MCS_CLUSTER__ADDRESS` values (`http://node-a:8080` and
`http://node-b:8080`) on the internal `mcs-net` bridge.

---

## Environment-variable reference

All variables are prefixed `MCS_`. Nested config keys use `__` as a separator
(e.g. `MCS_SESSION__SECRET` maps to `session.secret` in `config.toml`).

### Top-level

| Variable            | Default (image)                     | Description                                      |
|---------------------|-------------------------------------|--------------------------------------------------|
| `MCS_BIND`          | `0.0.0.0:8080`                      | TCP address the HTTP server listens on.          |
| `MCS_DATABASE_URL`  | `sqlite:///data/mcs.db?mode=rwc`    | Storage connection string (SQLite or Postgres).  |
| `MCS_CONFIG`        | `config.toml`                       | Path to an optional TOML config file.            |

### Database pool (`MCS_DATABASE__*`)

See [Connection-pool tuning](#connection-pool-tuning-database) for the full
table. Keys: `MCS_DATABASE__MAX_CONNECTIONS` (10),
`MCS_DATABASE__ACQUIRE_TIMEOUT_SECS` (30), `MCS_DATABASE__IDLE_TIMEOUT_SECS`
(600), `MCS_DATABASE__MAX_LIFETIME_SECS` (0 = none), and
`MCS_DATABASE__STATEMENT_TIMEOUT_SECS` (0 = unset, Postgres only).

### Logging (`MCS_LOG__*`)

| Variable           | Default | Description                                                  |
|--------------------|---------|--------------------------------------------------------------|
| `MCS_LOG__FORMAT`  | `json`  | `json` (structured) or `pretty` (human-readable).           |
| `MCS_LOG__LEVEL`   | `info`  | `tracing` filter directive, e.g. `info,mcs_api=debug`.      |

### Session tokens (`MCS_SESSION__*`)

| Variable                | Default          | Description                                                   |
|-------------------------|------------------|---------------------------------------------------------------|
| `MCS_SESSION__SECRET`   | *(none)*         | **Required in production.** HMAC-SHA256 signing key (>= 32 bytes). Omitting it causes an ephemeral key to be generated on each restart, invalidating all existing sessions. |
| `MCS_SESSION__TTL_SECS` | `86400` (24 h)   | Session-token lifetime in seconds.                            |
| `MCS_SESSION__ISSUER`   | `mcs`            | JWT `iss` claim.                                              |

### Sign-In with Ethereum (`MCS_SIWE__*`)

| Variable                   | Default                   | Description                              |
|----------------------------|---------------------------|------------------------------------------|
| `MCS_SIWE__DOMAIN`         | `localhost:8080`          | RFC 3986 authority for SIWE challenges.  |
| `MCS_SIWE__URI`            | `http://localhost:8080`   | RFC 3986 URI for SIWE challenges.        |
| `MCS_SIWE__CHAIN_ID`       | `1` (Ethereum mainnet)    | EIP-155 chain ID.                        |
| `MCS_SIWE__STATEMENT`      | `Sign in to MCS.`         | Statement shown in the user's wallet.    |
| `MCS_SIWE__NONCE_TTL_SECS` | `600` (10 min)            | Nonce validity window.                   |

### Payments / x402 (`MCS_PAYMENTS__*`)

| Variable                          | Default                        | Description                                           |
|-----------------------------------|--------------------------------|-------------------------------------------------------|
| `MCS_PAYMENTS__ENABLED`           | `false`                        | Gate `POST /seeks` behind an x402 payment.           |
| `MCS_PAYMENTS__SCHEME`            | `exact`                        | x402 scheme.                                          |
| `MCS_PAYMENTS__NETWORK`           | `base-sepolia`                 | Target network.                                       |
| `MCS_PAYMENTS__ASSET`             | `0x036C…` (USDC/Base Sepolia)  | Payment-token contract address.                       |
| `MCS_PAYMENTS__PAY_TO`            | `0x0000…` (zero address)       | On-chain recipient. **Set before enabling.**          |
| `MCS_PAYMENTS__MAX_AMOUNT_REQUIRED` | `10000`                      | Max token amount (asset-smallest-unit).               |
| `MCS_PAYMENTS__DESCRIPTION`       | `Create an MCS game.`          | Human-readable payment description.                   |
| `MCS_PAYMENTS__MAX_TIMEOUT_SECONDS` | `300`                        | Authorization expiry window.                          |
| `MCS_PAYMENTS__VERIFIER`          | `mock`                         | Verifier implementation. `mock` is dev-only; never use in production. |

### Cluster (`MCS_CLUSTER__*`)

| Variable                            | Default                    | Description                                                                 |
|-------------------------------------|----------------------------|-----------------------------------------------------------------------------|
| `MCS_CLUSTER__ENABLED`              | `false`                    | Join a Redis-backed cluster. `false` runs single-node with no Redis.        |
| `MCS_CLUSTER__NODE_ID`              | *(generated UUID)*         | Stable identifier for this node. Pin it for production pods/hosts.          |
| `MCS_CLUSTER__ADDRESS`              | `http://127.0.0.1:8080`    | Externally reachable base URL for this node (used by peers for redirects).  |
| `MCS_CLUSTER__REDIS_URL`            | `redis://127.0.0.1:6379`   | Redis connection URL for membership.                                         |
| `MCS_CLUSTER__HEARTBEAT_TTL_SECS`   | `15`                       | Membership TTL; a node missing this many seconds of heartbeats is evicted.  |
| `MCS_CLUSTER__HEARTBEAT_INTERVAL_SECS` | `5`                     | How often (seconds) this node renews its TTL. Must be well below TTL.       |

---

## Single-node vs. cluster

### Single node (default)

`MCS_CLUSTER__ENABLED` defaults to `false`. No Redis connection is opened; all
game state is managed in-process. This mode is ideal for development, staging,
and low-traffic deployments.

```sh
docker run --rm \
  -p 8080:8080 \
  -v mcs-data:/data \
  -e MCS_SESSION__SECRET="$(openssl rand -hex 32)" \
  mcs-server:latest
```

### Cluster (horizontal scaling)

Set `MCS_CLUSTER__ENABLED=true` and supply the Redis URL and per-node
`MCS_CLUSTER__NODE_ID` / `MCS_CLUSTER__ADDRESS`. Each node registers itself in
Redis with a heartbeat TTL; peers discover the full membership list via Redis
and route WebSocket game traffic to the owning node by rendezvous hash.

**All nodes MUST share the same `MCS_SESSION__SECRET`** so that a JWT minted on
node-a is accepted on node-b.

The `docker-compose.yml` in the project root starts a two-node cluster for
local testing. Adapt it to Kubernetes by replacing `node-a` / `node-b` with
a `Deployment` scaled to the desired replica count, ensuring each pod has a
unique `MCS_CLUSTER__NODE_ID` (e.g. the pod name via the Downward API).

#### Reverse proxy / game-id-aware load balancing

A plain L7 proxy (nginx, Caddy, Traefik) can sit in front of the cluster. For
stateless endpoints (auth, seek listing, variants) any node can serve the
request, so round-robin suffices. For WebSocket game connections, the client
should be directed to the node that owns the game. The server exposes the
owning node's `MCS_CLUSTER__ADDRESS` in the response so the client SDK (or a
smart upstream selector) can redirect there. A sticky-session rule (IP hash or
cookie) is a simpler but less precise alternative.

---

## Postgres

The default storage backend is SQLite, which is convenient for development and
single-node deployments but is not designed for multiple concurrent writers. A
shared **Postgres** instance is the recommended backend for production and for
cluster deployments: it gives all nodes a consistent view of seeks, users, and
game records without per-node data partitioning.

### Building the Postgres image

The storage backend is a compile-time choice. The default image bundles the
SQLite driver; pass `DB_BACKEND=postgres` to build a Postgres-only binary
(`cargo build --release -p mcs-server --no-default-features --features postgres`):

```sh
docker build --build-arg DB_BACKEND=postgres -t mcs-server:postgres .
```

The same selection is available to a direct `cargo` build:

```sh
cargo build -p mcs-server --no-default-features --features postgres
```

### Running against Postgres

Point `MCS_DATABASE_URL` at a Postgres DSN and run the Postgres image:

```sh
docker run --rm \
  -p 8080:8080 \
  -e MCS_DATABASE_URL="postgres://user:password@db-host:5432/mcs" \
  -e MCS_SESSION__SECRET="$(openssl rand -hex 32)" \
  mcs-server:postgres
```

**Migrations apply automatically on startup.** The server runs the embedded
migrations against the target database before serving traffic, so a fresh
database needs no manual schema step. This works the same way against a managed
Postgres (Amazon RDS, Cloud SQL, Neon, Supabase, etc.): create an empty
database, grant the connecting role `CREATE`/`USAGE` on its schema, and set
`MCS_DATABASE_URL` to the provider's DSN (append `?sslmode=require` when the
provider mandates TLS).

### Compose profile

`docker-compose.yml` ships a `postgres` profile that brings up a `postgres:16`
service (with a `pg_isready` healthcheck) plus a single `mcs-pg` server built
with `DB_BACKEND=postgres`:

```sh
docker compose --profile postgres up --build
```

| Service     | Host port | Role                                   |
|-------------|-----------|----------------------------------------|
| `postgres`  | (internal)| Shared Postgres 16 database            |
| `mcs-pg`    | 8083      | mcs-server (Postgres backend)          |

The `mcs-pg` service waits for the database to pass its healthcheck, then runs
migrations and starts serving on host port 8083. The default (no-profile) stack
(`redis`, `node-a`, `node-b`) is unaffected and still starts with a plain
`docker compose up`. Validate the profile without building with
`docker compose --profile postgres config`.

To grow the profile into a multi-node Postgres cluster, add more server services
pointing at the same `MCS_DATABASE_URL` and a shared Redis (`MCS_CLUSTER__*`, see
[Cluster](#cluster-horizontal-scaling)) — Postgres removes the per-node SQLite
data-ownership constraint.

### Connection-pool tuning (`[database]`)

The `[database]` config section (env prefix `MCS_DATABASE__`) sizes the storage
connection pool. It matters most for Postgres, where several nodes share one
instance, but applies to every backend. Optional timeouts are in whole seconds;
`0` disables that bound.

| Variable                                | Default      | Description                                                                 |
|-----------------------------------------|--------------|-----------------------------------------------------------------------------|
| `MCS_DATABASE__MAX_CONNECTIONS`         | `10`         | Max pool connections. Keep `nodes x this` under the server's `max_connections`. In-memory SQLite is always pinned to 1. |
| `MCS_DATABASE__ACQUIRE_TIMEOUT_SECS`    | `30`         | How long `acquire` waits for a free connection before erroring.             |
| `MCS_DATABASE__IDLE_TIMEOUT_SECS`       | `600`        | Close a connection idle in the pool this long. `0` = keep indefinitely.     |
| `MCS_DATABASE__MAX_LIFETIME_SECS`       | `0` (none)   | Recycle any connection older than this. `0` = no maximum lifetime. Useful behind a load balancer that drops idle backend TCP connections. |
| `MCS_DATABASE__STATEMENT_TIMEOUT_SECS`  | `0` (unset)  | **Postgres only**: per-statement timeout via `SET statement_timeout`. `0` leaves it unset. Ignored on SQLite. |

### Backups and point-in-time recovery (PITR)

Postgres is the system of record for all durable game state, so back it up:

- **Logical dumps** — schedule `pg_dump`/`pg_dumpall` (or `pg_basebackup` for a
  physical base) and ship the artifacts off-host. The compose profile stores the
  database in the `postgres-data` named volume; a `docker compose exec postgres
  pg_dump -U mcs mcs` captures a consistent snapshot.
- **PITR** — for production, enable WAL archiving (`archive_mode = on`,
  `archive_command`/`archive_library`) so you can restore to any point in time,
  or rely on the equivalent automated-backup + PITR feature of your managed
  Postgres. Test restores periodically; an untested backup is not a backup.

---

## Volume layout

| Path          | Purpose                            |
|---------------|------------------------------------|
| `/data`       | SQLite database file (`mcs.db`).   |

Mount a named Docker volume (or a host directory) at `/data` to persist game
state across container restarts. With the Postgres backend the server keeps no
local state, so the `/data` volume is unused; durability comes from Postgres
instead (the compose `postgres` profile persists it in the `postgres-data`
named volume).

---

## Health check

The server exposes a `/health` endpoint that returns `200 OK` when it is ready
to accept requests. The `docker-compose.yml` healthchecks poll this endpoint
every 10 seconds.
