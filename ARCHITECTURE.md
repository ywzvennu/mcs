# MCS Architecture

This document describes the high-level design of the Modular Chess Server for
contributors. It is a design reference, not an API specification.

---

## Goals

- Support multiple chess variants under a single, uniform server API without
  forcing each variant to know anything about HTTP or storage.
- Handle both **perfect-information** variants (standard chess and Chess960:
  one move per turn, full board visible to both players) and
  **imperfect-information** variants (Reconnaissance Blind Chess: sense-then-move
  turn structure, each player sees only a partial, private view of the board).
- Keep crates small and single-purpose so they can evolve in parallel with
  minimal merge conflicts.

---

## Crate graph

Dependencies flow strictly downward. Lower crates have no knowledge of HTTP,
WebSockets, payments, or the variants above them.

```
mcs-server
    |
    +-- mcs-api ──────────────────────────────── mcs-observability
    |       |                                            |
    |       +-- mcs-game ──────────────────────┐        |
    |       |       |                          |        |
    |       +-- mcs-auth    mcs-storage        |        |
    |       |       |           |              |        |
    |       +-- mcs-payments    |              |        |
    |       |                   |              |        |
    |       +-- mcs-cluster     |              |        |
    |       |                   |              |        |
    |       +-- mcs-rating ─────+──────────────+────────┘
    |               |           |
    |               └─────┬─────┘
    |                     |
    |                  mcs-domain
    |                     |
    |                  mcs-core
    |
    +── mcs-variant-standard ──► mcs-core
    +── mcs-variant-rbc ───────► mcs-core
```

### One-line responsibilities

| Crate | Responsibility |
|---|---|
| `mcs-core` | `GameSession` trait, type-erased `Action`/`PlayerView`/`Event`, `VariantRegistry` |
| `mcs-variant-standard` | Standard FIDE chess and Chess960, backed by `cozy-chess` (MIT) |
| `mcs-variant-rbc` | Reconnaissance Blind Chess via `rbc-rs`; enforces per-player view redaction |
| `mcs-domain` | Pure domain entities: `User`/`EvmAddress`, `Game` (with `variant_options`, live snapshot, `rated` flag), `Seek`, `Challenge`, `Rating`, `Clock`, `TimeControl` |
| `mcs-storage` | Repository traits + sqlx impl; SQLite (default) or Postgres; `ActionLogRepo`, `RatingRepo`, `ChallengeRepo`, `SeekRepo::claim`; in-memory SQLite pinned to one connection for tests |
| `mcs-auth` | SIWE/EIP-4361 challenge/verify + HS256 JWT sessions; pure, IO-free |
| `mcs-rating` | Pure Glicko-2 engine (`update`, `update_single`) |
| `mcs-game` | Live game actor (`GameActor`/`GameHandle`), clock engine, matchmaker, `GameCompletionHook`, `recover_game`/`spawn_resumed` |
| `mcs-payments` | x402 protocol types, `RequirePaymentLayer` axum middleware, `MockVerifier`, feature-gated `FacilitatorVerifier` |
| `mcs-cluster` | `NodeRegistry` + `HrwDirectory` (rendezvous hash), Redis-optional |
| `mcs-api` | axum REST + WebSocket router, `AppState`, `AuthUser`, `GameHub` + `TableHub`, `RatingUpdateHook`, `PresenceTracker`, recover-on-demand |
| `mcs-server` | Binary: config, tracing init, metrics, startup recovery, cluster lifecycle |
| `mcs-observability` | `init_tracing`, `http_trace_layer`, `request_id_layers` |

---

## The variant abstraction

The central design challenge is that chess variants differ not just in rules but
in **turn structure** and **information visibility**:

| Property          | Standard / Chess960          | Reconnaissance Blind Chess      |
|-------------------|------------------------------|---------------------------------|
| Actions per turn  | One (move)                   | Two (sense, then move)          |
| Board visibility  | Full (both players)          | Partial, per-player private     |
| Terminal detection| Checkmate / stalemate / etc. | King capture                    |

### How `mcs-core` bridges both families

`mcs-core` defines the object-safe `GameSession` trait. Each variant keeps its
own strong internal types but exposes them across the trait boundary as
type-erased, serde-serializable payloads — `Action`, `PlayerView`, and `Event`.
The session actor, the WebSocket handler, and the storage layer all work through
these boundary types and remain variant-agnostic.

For **perfect-information** variants the `view_for(color)` and
`spectator_view()` methods return identical board state; the actor broadcasts one
message per update and every subscriber receives the same view.

For **imperfect-information** variants (RBC), `view_for(color)` returns only
what that player is entitled to see — their own pieces, plus the result of their
own latest sense. `spectator_view()` is redacted until the game ends. Because the
actor calls `view_for` per subscriber before broadcasting, the same code path
handles both families without any variant-specific branching above `mcs-core`.

### Standard chess and Chess960 (`mcs-variant-standard`)

Both standard FIDE chess and Chess960 (Fischer Random) are implemented by a
single `StandardGame` session wrapping a [`cozy-chess`](https://github.com/analog-hors/cozy-chess)
`Board` — a permissively licensed (MIT) move generator. cozy-chess handles move
legality, application, FEN, check detection, and terminal status; the adapter
maps its `GameStatus` onto the `mcs-core` `Outcome`/`EndReason` enums and adds
the non-board mechanics (resignation, draw offers) the engine does not track. It
also detects insufficient-material dead positions explicitly, since cozy-chess
itself keeps those `Ongoing`.

The two variants share one wire protocol — UCI moves plus resign/draw
meta-actions — and differ only in how castling is spelled:

- **`standard`** uses **classic UCI** castling (`e1g1`, `e1c1`), translated
  to/from cozy-chess's internal king-captures-rook form at the wire boundary so
  existing clients are unaffected.
- **`chess960`** uses **UCI_960** (king-to-rook) castling, e.g. `e1h1`, because
  the rook's starting file is not fixed. Chess960 accepts `{ "position": 0..=959 }`
  (Scharnagl number) or `{ "fen": "..." }` options.

`mcs_variant_standard::register` registers both factories.

### RBC (`mcs-variant-rbc`)

All rules, move resolution, sensing, and result adjudication are delegated to
the `rbc-rs` git crate. `mcs-variant-rbc` adapts `rbc-rs` to the `mcs-core`
boundary types and enforces the sense-then-move phase ordering through the
`GameSession` trait. A `view_for` call returns only a player's own pieces and
their most recent 3×3 sense result; the opponent's positions are never revealed
while the game is live.

---

## Transport split

MCS uses two complementary transports:

### REST / HTTP

Used for operations that are naturally request/response:

| Route | Purpose |
|---|---|
| `GET /auth/nonce` | Issue a single-use SIWE challenge |
| `POST /auth/verify` | Verify wallet signature, mint session JWT |
| `GET /variants` | List every registered variant |
| `POST /seeks` | Post a seek; queue or pair into a game (x402-gated when enabled) |
| `DELETE /seeks/{id}` | Cancel a seek |
| `POST /challenges` | Challenge a specific opponent |
| `GET /challenges` | List pending challenges |
| `POST /challenges/{id}/accept` | Accept a challenge; create the game |
| `POST /challenges/{id}/decline` | Decline a challenge |
| `DELETE /challenges/{id}` | Cancel one's own challenge |
| `GET /games/{id}` | Fetch a single game |
| `GET /games` | List recent games |
| `POST /games/{id}/rematch` | Durable rematch challenge (for offline players) |
| `GET /games/{id}/moves` | Full action log ordered by ply |
| `GET /games/{id}/pgn` | PGN export for board-style variants |
| `GET /leaderboard` | Top-rated players for a variant |
| `GET /users/{id}` | Public profile |
| `GET /profile` | Authenticated caller's profile |
| `GET /health` | Liveness probe (`{"status":"ok"}`) |
| `GET /ready` | Readiness probe |
| `GET /metrics` | Prometheus metrics |

x402 is HTTP-native: when payments are enabled the server returns `402 Payment
Required` with a body describing the payment requirements; the client settles
on-chain and retries with an `X-Payment` header. This flow maps cleanly onto
ordinary HTTP request/response and belongs in the REST layer. Only the
seek-creation route is wrapped in the payment layer — the rest of the API is
always free.

### WebSocket

Used for the real-time game loop where low latency and server-push matter.

A client opens one WebSocket per game at `GET /ws/game/{id}?token=<jwt>`. The
JWT is validated before the upgrade, so an unauthenticated request is rejected
with 401 and never reaches the streaming task. The verified user ID is matched
against the game to resolve the connection's role: White, Black, or Spectator.

**Protocol flow:**

1. On connect the server sends one `Snapshot` frame describing the current
   position from that connection's perspective (player view, status, color,
   clocks, ply). A reconnecting client can resync from this single frame.
2. Every applied action produces an `Update` frame carrying the per-player
   `PlayerView` and the broadcast `GameEvent`.
3. The client submits play with a `Submit` frame; a rejected action returns an
   `Error` frame without closing the socket.
4. A client may request `since_ply` on reconnect to indicate the last position it
   holds, enabling targeted resync rather than a full replay.

**Draw offers** travel as ordinary board actions (`offer_draw`, `accept_draw`,
`decline_draw`) submitted through the same `Submit` frame. The variant emits the
corresponding events and both players receive them as normal `Update` frames.
Accepting ends the game.

**Rematch (live path):** Once a game finishes, the two players can negotiate a
rematch directly over their open sockets without polling the REST endpoint.
Rematch events are not board actions, so they travel on a separate per-game
**table side-channel** (`TableHub`/`TableChannel`) that every WebSocket
connection subscribes to alongside the actor's board-event stream.
`RematchOffer`, `RematchAccept`, and `RematchDecline` frames are exchanged; on
accept the server creates a new game with colours swapped and both clients pivot
to the new game's socket. The offline `POST /games/{id}/rematch` REST endpoint
remains the path for a player who is not currently connected.

---

## Ratings

Ratings use **Glicko-2** (`mcs-rating` crate). The algorithm follows the
Glickman (2012) reference exactly, converting to and from the internal μ/φ/σ
scale, computing the rating variance and performance delta, updating volatility
with the Illinois root-finding algorithm, and converting back.

The wiring is a `GameCompletionHook` (`RatingUpdateHook` in `mcs-api`): the game
actor invokes the hook after persisting a terminal result. The hook reads each
player's current `Rating` for the game's `variant_id` (defaulting to the global
default for unrated players), computes the Glicko-2 update against the
opponent's *pre-game* rating, and writes both new ratings via `RatingRepo`.

**Casual games** (`Game::rated == false`) skip the hook entirely: no rating rows
are read or written.

---

## Payments (x402)

Game creation can be gated behind an x402 payment. The gate is **off by default**
(`payments.enabled = false`); the server boots free.

When enabled, `mcs-server` builds a `PaymentRequirements` struct (scheme,
network, USDC asset address, max amount, pay-to address) and a verifier, then
calls `AppState::with_payment`. The API wraps only the seek-creation route in
`RequirePaymentLayer`.

**Verifiers:**

- `MockVerifier` — development only; performs no on-chain checks. Never use in
  production.
- `FacilitatorVerifier` (feature-gated behind `facilitator`) — delegates
  `/verify` and `/settle` calls to a standards-compliant x402 facilitator
  service. Enabled by setting `payments.verifier = "facilitator"` and providing
  `payments.facilitator_url`.

---

## Durability and recovery

Every action applied to a live game is durably recorded before the broadcast
reaches any subscriber:

1. `GameActor::submit_action` calls `GameSession::apply` to produce the new
   state.
2. The action is appended to the `ActionLogRepo` (append-only, keyed by game ID
   and ply index).
3. The game's live snapshot (ply, clocks, side to move) is refreshed in
   `GameRepo`.
4. The `GameEvent` is broadcast to subscribers.

### Recover-on-demand

When a WebSocket client connects to a game that has no running actor on this
node, `AppState::get_or_recover` rebuilds the actor from the durable log:

1. Instantiate a fresh `GameSession` for the game's variant via the
   `VariantRegistry`.
2. Replay every `RecordedAction` in ply order through `GameSession::apply`,
   driving the session to the exact position the log describes.
3. Spawn a `resumed` actor seeded with the game's persisted ply and each side's
   remaining clock, so play continues exactly where it left off.

A per-game mutex prevents double-recovery when two clients connect to the same
cold game concurrently.

### Startup recovery

At startup `mcs-server` calls `GameRepo::list_unfinished` and recovers each
live game into the `GameHub` before accepting traffic. A game that fails to
recover (corrupt log, or a variant whose rules changed) is logged and skipped;
recovery of one game never aborts the others.

### Deterministic replay

Because the action log stores the exact serialized `Action` values that were
accepted by `GameSession::apply`, replay is deterministic for all variants —
including RBC, where the sense result is part of the recorded action stream.
There is no probabilistic or time-dependent state: replaying the full log from a
fresh session always reconstructs the exact in-progress position.

---

## Clustering and failover

MCS supports optional horizontal scaling. The design goal is zero chatter
between nodes: every routing decision is a pure function of the live node set.

### Node membership

`mcs-cluster` provides a `NodeRegistry` trait. The default is `LocalRegistry`
(single-node, no external service). The `redis` feature enables
`RedisNodeRegistry`, which stores each node's `NodeInfo` (ID, HTTP base URL)
under a TTL key. `mcs-server` registers the local node on startup, spawns a
background heartbeat task to renew the TTL, and calls `ClusterRuntime::shutdown`
on graceful exit to leave the registry immediately rather than waiting for the
TTL to lapse.

### HRW ownership

`HrwDirectory` implements rendezvous hashing (Highest Random Weight): given the
current live node set it maps each `GameId` to a deterministic owner. Because
every node computes the same hash function over the same node set, there is no
leader, no gossip, and no lock. The mapping agrees across the cluster for free.

### Routing and failover

When a WebSocket request arrives for a game whose HRW owner is a *different*
node, the current node returns `421 Misdirected Request` with the owner's base
URL so the client can reconnect to the correct node. If the owner has crashed,
its TTL lapses and the live node set shrinks; the next connection picks the new
HRW owner, which rebuilds the actor via recover-on-demand from the durable log.

**Current limitation:** cross-node spectator broadcast is not yet implemented.
A game's connections all route to its HRW owner; a spectator on a non-owner node
is redirected rather than receiving a proxied event stream.

---

## Authentication

Login uses **Sign-In with Ethereum** (EIP-4361, SIWE):

1. The client requests a challenge from `GET /auth/nonce`. The server generates
   an unpredictable nonce, persists it via `SessionRepo`, and returns a
   structured SIWE message string.
2. The wallet displays the message and the user signs it, returning a 65-byte
   EIP-191 personal-signature.
3. The client posts the message and signature to `POST /auth/verify`. The server
   recovers the signing address from the signature, checks it matches the address
   embedded in the message, and enforces single-use by consuming the nonce.
4. On success the server maps the address to a `UserId` (creating the user on
   first login) and mints an HS256 JWT session token.

Subsequent REST requests supply `Authorization: Bearer <jwt>`; WebSocket
connections supply `?token=<jwt>` as a query parameter (browsers cannot set
arbitrary headers on WebSocket upgrades). Both paths validate the JWT locally
with `mcs-auth::verify_session`.

`mcs-auth` is IO-free and synchronous. All stateful concerns (nonce storage,
user lookup) belong to the integration layer (`mcs-storage`, `mcs-api`).

### Presence

Presence is tracked per-node and in-process (`InProcessPresence`). A user is
"online" from the perspective of the node that handled their last authenticated
request. In a multi-node deployment a cross-node `PresenceTracker` backed by
Redis is the natural upgrade path; the trait is designed so that swapping
implementations requires no call-site changes.

---

## Observability and operations

### Tracing and request IDs

`mcs-observability` initialises a `tracing-subscriber` registry at startup
(JSON or pretty format, controlled by `log.format`; level from `RUST_LOG` or
`log.level`). The `http_trace_layer` records HTTP method, path, status, and
latency for every request. The `request_id_layers` pair attaches an
`x-request-id` to every request (reading an existing header or generating a
UUID v4) and propagates it to the response, so correlated logs can be traced
end-to-end.

### Prometheus metrics

The server exposes `GET /metrics` in the Prometheus text format. Metrics cover
standard tokio and HTTP layer statistics.

### Readiness and liveness

`GET /health` returns `{"status":"ok"}` as long as the HTTP server is accepting
connections (liveness). `GET /ready` checks that the storage backend is
reachable (readiness).

### Configuration

`mcs-server` reads configuration from three layers (lowest to highest priority):

1. Built-in defaults — the server boots with no external config at all.
2. `config.toml` in the working directory (path overridable with `MCS_CONFIG`).
3. `MCS_`-prefixed environment variables (nested keys use `__`, e.g.
   `MCS_SIWE__DOMAIN`).

Significant keys: `MCS_BIND`, `MCS_DATABASE_URL`, `MCS_SESSION__SECRET`,
`MCS_PAYMENTS__ENABLED`, `MCS_PAYMENTS__VERIFIER`.

### Docker / Compose

The repository ships a `Dockerfile` and a `docker-compose.yml` for local
development and production deployment. The compose file starts the server with
optional Redis (for clustering) and mounts a volume for the SQLite database.

### CI matrix

The GitHub Actions CI pipeline runs five jobs on every push and pull request:

| Job | What it checks |
|---|---|
| `ci` | `cargo fmt --check`, `clippy --all-features -D warnings`, `cargo test --all-features`, `cargo build --all-features` |
| `redis` | `mcs-cluster` and `mcs-server` Redis integration tests against a real Redis 7 service container |
| `postgres-compile` | `mcs-storage --features postgres` compiles and passes clippy |
| `msrv` | Workspace builds on Rust 1.82 (the minimum supported version) |
| `deny` | `cargo deny check` for security advisories, license compliance, and duplicate crates |

---

## Licensing

The MCS crates are dual-licensed under **MIT OR Apache-2.0**, and so is the
**assembled `mcs-server` binary**: every dependency is permissively licensed, so
there is no GPL copyleft obligation.

The chess engine behind `mcs-variant-standard` is
[`cozy-chess`](https://crates.io/crates/cozy-chess) (MIT). The previous engine,
`shakmaty`, was GPL-3.0-or-later and required a copyleft exception in
`deny.toml`; replacing it removed both the dependency and the exception, leaving
the distributed binary free of copyleft. `cargo deny check` enforces the
permissive allow-list with **no license exceptions** — any GPL dependency
(direct or transitive) now fails CI.

The crate boundary between `mcs-core` and the variant crates remains the natural
license boundary, so a future variant could still introduce a copyleft engine
behind a clearly scoped exception if ever needed.

---

## Future work

- **On-chain x402 settlement specifics.** `FacilitatorVerifier` delegates
  verification and settlement to an external x402 facilitator service. The
  specifics of on-chain finality guarantees and multi-chain support are not yet
  defined.
- **Cross-node event bus.** Spectator connections are currently redirected to the
  game's owner node. A shared event bus (e.g. Redis pub/sub) would allow any node
  to serve spectators by forwarding the owner's broadcast, eliminating client
  redirects for read-only consumers.
- **Anti-cheat and moderation.** No engine-assistance detection, move-time
  analysis, or moderator tooling exists yet.
- **Tournaments.** No bracket, round-robin, or Swiss tournament management.
- **Analysis and engine integration.** No position analysis, opening-book
  lookup, or engine-powered hints.
- **Account management.** No username, avatar, email, or account deletion
  workflows beyond EVM-address-based identity.
- **Rate limiting.** No per-IP or per-user rate limiting on any endpoint.
- **Cross-node presence.** `PresenceTracker` is per-node today. A Redis-backed
  implementation would make online status consistent across a cluster.
- **PGN / history for RBC.** The `GET /games/{id}/pgn` endpoint is defined for
  board-style variants; a history export format for imperfect-information games
  is not yet specified.
