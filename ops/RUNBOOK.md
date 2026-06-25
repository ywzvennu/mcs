# MCS — Production Runbook

This runbook covers deploying and operating the MCS production stack
(`docker-compose.prod.yml`).  For environment-variable reference and
development/testing guidance see `ops/README.md`.

---

## Table of contents

1. [First-time deploy](#1-first-time-deploy)
2. [Routine upgrade](#2-routine-upgrade)
3. [Scaling nodes up or down](#3-scaling-nodes-up-or-down)
4. [Rotating the session secret](#4-rotating-the-session-secret)
5. [Payments — mock vs facilitator](#5-payments--mock-vs-facilitator)
6. [Postgres backup and restore](#6-postgres-backup-and-restore)
7. [Health, readiness, and liveness probes](#7-health-readiness-and-liveness-probes)
8. [Metrics scraping](#8-metrics-scraping)
9. [Incident response basics](#9-incident-response-basics)

---

## 1. First-time deploy

### Prerequisites

- Docker >= 24 with the Compose v2 plugin (`docker compose version`).
- DNS for your domain pointing at this host.
- Ports 80 and 443 open on the host firewall.

### Steps

```sh
# 1. Clone the repository (or pull the release tarball).
git clone https://github.com/ywzvennu/mcs.git && cd mcs

# 2. Create the .env file from the template.
cp .env.example .env
$EDITOR .env   # fill in POSTGRES_PASSWORD, MCS_SESSION__SECRET, MCS_DOMAIN, ...

# 3. (Optional) build images locally instead of pulling from GHCR.
#    Pre-built images are published on every version tag:
#      ghcr.io/ywzvennu/mcs-server:<version>-postgres
#    To use them, set MCS_IMAGE in .env and skip this step.
docker compose -f docker-compose.prod.yml build

# 4. Bring the stack up.
docker compose -f docker-compose.prod.yml up -d

# 5. Check all containers are healthy.
docker compose -f docker-compose.prod.yml ps
```

Expected output — all services should show `healthy` or `running`:

```
NAME       IMAGE                            STATUS
caddy      caddy:2-alpine                   running
node-a     ghcr.io/ywzvennu/mcs-server:...  healthy
node-b     ghcr.io/ywzvennu/mcs-server:...  healthy
postgres   postgres:16                      healthy
redis      redis:7-alpine                   healthy
```

Caddy provisions a TLS certificate automatically on first start.  DNS must
resolve before Caddy can complete the ACME challenge.

---

## 2. Routine upgrade

```sh
# Pull the new image tags.
docker compose -f docker-compose.prod.yml pull node-a node-b

# Rolling restart: bring node-b down first so node-a keeps serving traffic,
# then swap.
docker compose -f docker-compose.prod.yml up -d --no-deps node-b
# Wait until node-b is healthy again.
docker compose -f docker-compose.prod.yml ps node-b

docker compose -f docker-compose.prod.yml up -d --no-deps node-a
docker compose -f docker-compose.prod.yml ps node-a
```

Migrations apply automatically when a node starts.  Because sqlx migrations
are append-only and idempotent, node-a running the old binary while node-b has
already migrated is safe for all schema changes that preserve backward
compatibility.

---

## 3. Scaling nodes up or down

### Add a third node

1. Copy the `node-b` block in `docker-compose.prod.yml` to a new `node-c`
   block.  Change:
   - `MCS_CLUSTER__NODE_ID: "node-c"`
   - `MCS_CLUSTER__ADDRESS: "http://node-c:8080"`
   - service name: `node-c`
2. Add `node-c:8080` to the `reverse_proxy` line in `ops/proxy/Caddyfile`.
3. Reload Caddy and start node-c:

```sh
docker compose -f docker-compose.prod.yml up -d --no-deps caddy node-c
```

### Remove a node (scale down)

1. Remove the node's upstream from the Caddyfile and reload Caddy so new
   connections are not routed there.
2. Wait for all existing WebSocket connections to drain.  Caddy's
   `fail_duration` will stop sending new requests to the node within 30 s of
   the health check failing.
3. Stop the node:

```sh
docker compose -f docker-compose.prod.yml stop node-c
docker compose -f docker-compose.prod.yml rm -f node-c
```

The node deregisters from Redis automatically on shutdown (heartbeat TTL
expires within 15 s).  Running games routed to the stopped node will receive
a connection error; clients must reconnect and will be redirected to an
active node via the standard 421 mechanism.

### Pool sizing note

Each node opens `MCS_DATABASE__MAX_CONNECTIONS` connections to Postgres (default
10).  With `N` nodes, keep `N * 10` well under Postgres's `max_connections`
(default 100).  Increase `max_connections` in Postgres or lower per-node
`MCS_DATABASE__MAX_CONNECTIONS` as you scale out.

---

## 4. Rotating the session secret

All nodes must share the same `MCS_SESSION__SECRET` at all times.  An
immediate simultaneous rotation forces all existing sessions to re-authenticate.

### Zero-downtime rotation (tokens expire naturally)

1. Generate a new secret:
   ```sh
   openssl rand -hex 32
   ```
2. Update `MCS_SESSION__SECRET` in `.env` with the new value.
3. Rolling restart (same procedure as upgrade, section 2).  Sessions minted
   with the old secret will remain valid until their TTL expires (default 24 h);
   after that, clients must sign in again.

### Immediate rotation (all sessions invalidated)

1. Generate a new secret and update `.env`.
2. Restart all nodes simultaneously:
   ```sh
   docker compose -f docker-compose.prod.yml restart node-a node-b
   ```
   All existing JWTs are immediately invalidated because the signing key has
   changed.  Users must sign in again.

---

## 5. Payments — mock vs facilitator

Payments gate `POST /seeks` behind an x402 payment.  They are **off by
default** (`MCS_PAYMENTS__ENABLED=false`).

### Development / staging (mock verifier)

The mock verifier accepts any well-formed payment payload with no on-chain
checks.  It is suitable for integration testing only.

```sh
MCS_PAYMENTS__ENABLED=true
MCS_PAYMENTS__VERIFIER=mock
```

**Never deploy with `verifier=mock` in production.**  The server enforces this
in `MCS_ENV=production` mode and will refuse to start.

### Production (facilitator verifier)

```sh
MCS_PAYMENTS__ENABLED=true
MCS_PAYMENTS__VERIFIER=facilitator
MCS_PAYMENTS__FACILITATOR_URL=https://facilitator.example.com
MCS_PAYMENTS__FACILITATOR_API_KEY=your-bearer-token   # if required
MCS_PAYMENTS__NETWORK=base
MCS_PAYMENTS__ASSET=0xYourUSDCContractAddress
MCS_PAYMENTS__PAY_TO=0xYourReceivingWalletAddress
MCS_PAYMENTS__MAX_AMOUNT_REQUIRED=10000   # 0.01 USDC at 6 decimals
```

Add these to `.env` and restart the nodes.  The server validates that
`facilitator_url` is non-empty when `verifier=facilitator` and payments are
enabled, so a misconfiguration is caught at startup rather than at payment
time.

### Disabling payments

Set `MCS_PAYMENTS__ENABLED=false` (or omit the variable) and restart nodes.
In-flight games are unaffected; only new `POST /seeks` requests are ungated.

---

## 6. Postgres backup and restore

Postgres is the system of record for all durable game state.

### Logical dump (pg_dump)

Schedule this as a cron job and ship the dump off-host (S3, GCS, etc.):

```sh
# Consistent snapshot with no table locks (uses a single transaction).
docker compose -f docker-compose.prod.yml exec -T postgres \
  pg_dump -U mcs --format=custom mcs \
  > mcs_backup_$(date +%Y%m%dT%H%M%S).pgdump
```

Restore into a fresh database:

```sh
# Create the target database first if needed.
createdb -U mcs mcs_restore

pg_restore --no-owner -d postgres://mcs:PASSWORD@host:5432/mcs_restore \
  mcs_backup_<timestamp>.pgdump
```

### Point-in-time recovery (PITR)

For production environments where RPO < 1 hour matters:

1. Enable WAL archiving in `postgresql.conf`:
   ```ini
   archive_mode = on
   archive_command = 'gzip < %p > /wal-archive/%f.gz'
   ```
   Or use a managed Postgres service (Amazon RDS, Cloud SQL, Supabase, Neon)
   whose automated-backup + PITR feature handles this transparently.

2. Periodically take a physical base backup:
   ```sh
   pg_basebackup -h localhost -U mcs -D /backups/base -Ft -Xs -P
   ```

3. To restore to a point in time, stop the server, restore the base backup,
   configure `recovery_target_time` in `recovery.conf` (Postgres 12+:
   `postgresql.conf`), and start Postgres.  Test your restore procedure
   periodically — an untested backup is not a backup.

### Backup verification

At minimum, weekly:

```sh
# Restore the dump into a temporary schema and count rows.
pg_restore --no-owner -d postgres://mcs:PASSWORD@host/mcs_verify \
  mcs_backup_latest.pgdump
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U mcs mcs_verify -c "SELECT relname, n_live_tup FROM pg_stat_user_tables;"
```

---

## 7. Health, readiness, and liveness probes

MCS exposes three unauthenticated probe endpoints on every node.

| Endpoint   | Probe type  | Returns                                    | When to use |
|------------|-------------|---------------------------------------------|-------------|
| `GET /health` | Liveness  | `200 {"status":"ok"}` always              | Docker / k8s liveness probe — restarts the container if it stops responding. |
| `GET /ready`  | Readiness  | `200 {"status":"ready"}` or `503 {"status":"unavailable","failed":"database\|cluster"}` | Docker / k8s readiness probe — removes the pod from the LB if dependencies are down. |
| `GET /metrics`| Metrics   | Prometheus text exposition                 | Prometheus scrape endpoint. |

`/health` touches nothing and always returns 200 as long as the process is
alive.

`/ready` performs a lightweight read probe on the database (`SELECT 1 LIMIT 1`)
and, when cluster mode is enabled, verifies the Redis membership store.  It
returns 503 if either check fails, naming the failing dependency in the body.

### Docker Compose healthcheck

Both nodes in `docker-compose.prod.yml` use `wget -qO- http://localhost:8080/health`
as the healthcheck command.  This ensures dependent services (Caddy) only start
after the mcs-server is up.

### Kubernetes liveness + readiness

```yaml
livenessProbe:
  httpGet:
    path: /health
    port: 8080
  initialDelaySeconds: 10
  periodSeconds: 10
  failureThreshold: 3

readinessProbe:
  httpGet:
    path: /ready
    port: 8080
  initialDelaySeconds: 5
  periodSeconds: 10
  failureThreshold: 3
```

Use a separate `startupProbe` with a longer `failureThreshold` if migrations
take more than 10 s:

```yaml
startupProbe:
  httpGet:
    path: /health
    port: 8080
  failureThreshold: 30
  periodSeconds: 5
```

---

## 8. Metrics scraping

Each node exposes Prometheus metrics at `GET /metrics` (unauthenticated).

### Exported series

| Metric | Labels | Description |
|--------|--------|-------------|
| `mcs_http_requests_total` | `method`, `route`, `status` | Total HTTP requests. |
| `mcs_http_request_duration_seconds` | `method`, `route`, `status` | Request latency histogram. |
| `mcs_games_live` | — | Current live game count on this node. |
| `mcs_games_created_total` | — | Cumulative games created on this node. |
| `mcs_rating_updates_total` | — | Cumulative rating updates. |
| `mcs_ws_connections_active` | — | Active WebSocket connections on this node. |

### Prometheus scrape config

Each node must be scraped individually (metrics are per-node, not aggregated).
In the compose stack the nodes are not exposed on host ports, so scrape from
within the `mcs-internal` network or through Caddy:

```yaml
# prometheus.yml
scrape_configs:
  - job_name: mcs
    static_configs:
      - targets:
          - node-a:8080
          - node-b:8080
    metrics_path: /metrics
```

Or add a Caddy route that proxies `/metrics` to both nodes with separate
scrape targets behind the proxy.

### Key alerts (examples)

```yaml
# Alert if any node is not ready for more than 2 minutes.
- alert: McsNodeNotReady
  expr: up{job="mcs"} == 0
  for: 2m

# Alert if p95 latency exceeds 500 ms.
- alert: McsHighLatency
  expr: histogram_quantile(0.95, rate(mcs_http_request_duration_seconds_bucket[5m])) > 0.5
  for: 5m

# Alert if error rate exceeds 1%.
- alert: McsHighErrorRate
  expr: rate(mcs_http_requests_total{status=~"5.."}[5m]) / rate(mcs_http_requests_total[5m]) > 0.01
  for: 5m
```

---

## 9. Incident response basics

### Viewing logs

```sh
# Tail all services.
docker compose -f docker-compose.prod.yml logs -f

# Tail a single service.
docker compose -f docker-compose.prod.yml logs -f node-a

# Filter by log level (structured JSON).
docker compose -f docker-compose.prod.yml logs node-a | jq 'select(.level == "ERROR")'
```

Set `MCS_LOG__LEVEL=debug` for a specific node to increase verbosity temporarily:

```sh
docker compose -f docker-compose.prod.yml \
  run --rm -e MCS_LOG__LEVEL=debug node-a
```

### A node is down

**Automatic recovery**: `restart: unless-stopped` means Docker will restart a
crashed container automatically.  Caddy's health check removes the node from
the upstream pool within one health-check interval (10 s) and re-adds it once
the node passes two consecutive checks.

**Manual intervention**:

```sh
# Check status.
docker compose -f docker-compose.prod.yml ps

# Read the last 100 lines of the crashed container's logs.
docker compose -f docker-compose.prod.yml logs --tail=100 node-a

# Force a restart.
docker compose -f docker-compose.prod.yml restart node-a
```

### Game routing and 421 redirects

When a WebSocket game connection reaches a node that does not own that game,
the server responds with **421 Misdirected Request** and includes the owning
node's address.  The MCS client SDK follows the redirect transparently.  Plain
HTTP clients that do not follow 421s will see a connection error; they should
be updated to handle 421.

If node-a is permanently removed, games owned by node-a whose WebSocket
connections were active become inaccessible until clients reconnect.  The
rendezvous hash re-assigns ownership to a remaining node after the evicted
node's heartbeat TTL (15 s) expires from Redis.  In-progress games are
preserved in Postgres; clients can reconnect and resume once the new owner
node picks them up.

### Postgres is down

1. mcs-server nodes begin returning `503` on `/ready` (with `"failed":"database"`).
2. All API endpoints that touch the database will fail with 5xx errors.
3. Resolve the Postgres outage (check `docker compose logs postgres`).
4. Once Postgres recovers and passes its healthcheck, the mcs-server nodes
   automatically reconnect (sqlx reconnects on next acquire from the pool).
5. No node restart is required unless Postgres was down long enough for the
   acquire-timeout to exhaust the pool queue.

### Redis is down

1. Cluster heartbeats stop.  After `MCS_CLUSTER__HEARTBEAT_TTL_SECS` (15 s),
   nodes evict each other from membership, so rendezvous routing may degrade
   (each node sees only itself).
2. `/ready` returns `503` with `"failed":"cluster"`.
3. New game-creation requests still succeed (routed to the receiving node).
   Existing WebSocket connections are unaffected.
4. Restore Redis; nodes re-register within one heartbeat interval (5 s).

### Emergency: restart the full stack

```sh
docker compose -f docker-compose.prod.yml down
docker compose -f docker-compose.prod.yml up -d
```

All persistent state is in the named Docker volumes (`postgres-data`,
`redis-data`, `caddy-data`).  A `down` without `-v` preserves volumes.

### Emergency: wipe and redeploy from backup

```sh
# DESTRUCTIVE: removes all volumes (game data will be lost unless restored
# from backup).
docker compose -f docker-compose.prod.yml down -v

# Restore Postgres data from dump (see section 6).
docker compose -f docker-compose.prod.yml up -d postgres
docker compose -f docker-compose.prod.yml exec -T postgres \
  pg_restore -U mcs -d mcs < mcs_backup_latest.pgdump

# Bring the full stack back up.
docker compose -f docker-compose.prod.yml up -d
```
