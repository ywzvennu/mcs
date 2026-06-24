# MCS Architecture

This document describes the high-level design of the Modular Chess Server for
contributors. It is a design reference, not an API specification.

---

## Goals

- Support multiple chess variants under a single, uniform server API without
  forcing each variant to know anything about HTTP or storage.
- Handle both **perfect-information** variants (standard chess: one move per
  turn, full board visible to both players) and **imperfect-information**
  variants (Reconnaissance Blind Chess: sense-then-move turn structure, each
  player sees only a partial, private view of the board).
- Keep crates small and single-purpose so they can evolve in parallel with
  minimal merge conflicts.

---

## Crate graph

Dependencies flow strictly downward. Lower crates have no knowledge of HTTP,
WebSockets, or the variants above them.

```
mcs-server
    |
mcs-api ─────────────────────────────────── mcs-observability
    |                                               |
mcs-game ──────────────────────────────────────────┘
    |
mcs-auth   mcs-storage
    |           |
    └─────┬─────┘
          |
       mcs-domain
          |
       mcs-core  ◄── mcs-variant-standard
```

---

## Crate responsibilities

### `mcs-core`

The variant-agnostic engine abstraction. Defines:

- An object-safe `GameSession` trait that every variant adapter implements.
  The trait surface covers: applying an action, querying the view for a given
  player, and inspecting terminal state.
- Type-erased `serde_json::Value`-based action and view types so that the
  session actor can forward messages without knowing the concrete variant
  types.
- A `VariantRegistry` that maps a variant identifier string to a factory
  function that creates a boxed `GameSession`.

This crate has no async dependencies; it is pure logic.

### `mcs-variant-standard`

The standard chess adapter. Wraps [shakmaty](https://github.com/niklasf/shakmaty)
and implements the `GameSession` trait from `mcs-core`. Handles move
generation, legality checking, and serialisation of positions and moves to and
from the type-erased JSON layer. Registers itself in the `VariantRegistry` at
startup.

### `mcs-domain`

Shared entities and value objects used across crates:

- `GameId`, `PlayerId`, `SeekId` (UUID newtype wrappers)
- `Color` (White / Black)
- Domain-level enums such as `GameResult`, `GameStatus`, `TimeControl`

No async, no HTTP, no storage. All types derive `serde::{Serialize, Deserialize}`.

### `mcs-storage`

Repository trait definitions and their sqlx implementation:

- `GameRepository`, `UserRepository`, `SeekRepository` traits
- SQLite is the default and is always available (embedded, no external service)
- PostgreSQL is pluggable via a feature flag and the same trait implementations
- Database migrations live under `crates/mcs-storage/migrations/`

### `mcs-auth`

SIWE (Sign-In with Ethereum) authentication:

- Verifies an EVM wallet signature over a standard SIWE message
- Issues a short-lived JWT session token on success
- Exposes a `CurrentUser` extractor for use in axum handlers

### `mcs-game`

The live game runtime:

- A per-game **session actor** (tokio task) that owns the `Box<dyn GameSession>`
  and serialises all mutations through a message channel
- A **clock engine** that tracks increment/delay time controls and emits
  timeout events
- **Matchmaking** logic for creating and matching seeks

### `mcs-api`

axum router wiring:

- REST handlers for request/response actions: authentication, game creation,
  seek management, game export (PGN/JSON), and any payment-gated endpoints
- WebSocket handlers for the real-time game loop (moves, clock ticks, presence,
  chat broadcasts)
- Mounts `mcs-observability` middleware (request-ID, structured trace spans)

### `mcs-server`

The binary entry point. Reads configuration (environment variables and an
optional `mcs.toml`) via [figment](https://docs.rs/figment), wires together the
dependency graph, runs database migrations, and starts the axum server.

### `mcs-observability`

- `init_tracing()` helper that configures `tracing-subscriber` with JSON output
  and `RUST_LOG`-driven filtering
- A Tower middleware layer that injects a `X-Request-Id` header and attaches it
  to every trace span for the lifetime of a request

---

## The variant abstraction

The central design tension is that chess variants differ not just in rules but
in **turn structure** and **information visibility**:

| Property          | Standard chess          | Reconnaissance Blind Chess     |
|-------------------|-------------------------|--------------------------------|
| Actions per turn  | One (move)              | Two (sense, then move)         |
| Board visibility  | Full (both players)     | Partial, per-player private    |
| Terminal detection| Checkmate / stalemate   | King capture                   |

`mcs-core` resolves this by making the `GameSession` trait action- and
view-agnostic at the Rust type level. Actions and views are serialised to
`serde_json::Value` at the boundary; each variant is responsible for
deserialising them into its own concrete types. This lets the session actor,
the WebSocket handler, and the storage layer treat all variants uniformly while
each variant's internal logic remains fully typed.

---

## Transport split

MCS uses two complementary transports:

### REST / HTTP

Used for operations that are naturally request/response:

- Authentication (SIWE challenge + verify)
- Create game, post seek, cancel seek
- Query game state, export PGN
- Any **x402 payment-gated** actions

x402 is HTTP-native: a server returns `402 Payment Required` with a
`X-Payment-Required` header describing the payment, the client settles on-chain
and retries with a `X-Payment-Payload` header. This flow maps directly onto
normal HTTP request/response and therefore belongs in the REST layer.

### WebSocket

Used for the real-time game loop where low latency and server-push matter:

- Submitting moves and receiving the updated game state
- Clock tick events
- Presence (connected/disconnected)
- Chat messages

A client establishes one WebSocket connection per active game. The server-side
session actor broadcasts to all connected clients for that game whenever state
changes.

---

## Future work

- **Ratings** — Glicko-2 rating system, updated at game completion
- **`mcs-payments`** — x402 integration over EVM / USDC; payment verification
  middleware and wallet-balance checks before game creation
- **`mcs-variant-rbc`** — Reconnaissance Blind Chess, integrating the
  [`rbc-rs`](https://github.com/ywzvennu/rbc-rs) crate as the rule engine
- **More shakmaty variants** — Chess960, Antichess, Three-check, and others
  that shakmaty already supports can each get a thin `mcs-core` adapter
- **Spectator mode** — read-only WebSocket connections for observers
- **Tournament support** — bracket and round-robin management
