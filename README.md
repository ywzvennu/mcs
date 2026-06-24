# MCS — Modular Chess Server

MCS is a modular, open-source chess server backend written in Rust. It is
designed to host multiple chess variants under a single, uniform API. It ships
three variants today — **standard** chess, **Chess960** (Fischer Random), and
**Reconnaissance Blind Chess (RBC)** — and more are planned. The server is
structured as a Cargo workspace of focused crates so that variant
implementations, storage backends, and transport layers can evolve
independently.

> **Status:** Early development. The workspace skeleton and core abstractions
> are in place; most crates are stubs that will be filled in incrementally.

---

## Workspace layout

```
crates/
  mcs-core              # Variant-agnostic engine abstraction (GameSession trait,
                        #   type-erased actions/views, VariantRegistry)
  mcs-variant-standard  # Standard chess + Chess960 adapter built on cozy-chess
  mcs-domain            # Shared entities and value objects (GameId, PlayerId, …)
  mcs-storage           # Repository traits + sqlx implementation
                        #   (SQLite by default, PostgreSQL pluggable)
  mcs-auth              # SIWE / EVM-wallet authentication
  mcs-game              # Live session actor, clock engine, matchmaking
  mcs-api               # axum REST + WebSocket handlers
  mcs-server            # Binary entry point
  mcs-observability     # Tracing/logging init + request-ID middleware
```

## Tech stack

| Concern         | Library / approach                                      |
|-----------------|---------------------------------------------------------|
| Async runtime   | [tokio](https://tokio.rs)                               |
| HTTP / WS       | [axum](https://github.com/tokio-rs/axum) (REST + WS)   |
| Persistence     | [sqlx](https://github.com/launchbadger/sqlx) — SQLite (default), PostgreSQL (pluggable) |
| Chess logic     | [cozy-chess](https://github.com/analog-hors/cozy-chess) (MIT) for standard chess and Chess960 |
| Authentication  | SIWE (Sign-In with Ethereum) — EVM wallet-based auth    |
| Payments        | x402 HTTP-native payments (planned)                     |
| Serialization   | serde / serde_json                                      |
| Observability   | tracing + tracing-subscriber                            |

## Prerequisites

- Rust 1.82 or later (`rustup update stable`)
- No external services are required for a local build; SQLite is embedded

## Build

```sh
cargo build --all
```

## Test

```sh
cargo test --all
```

## Run

The `mcs-server` binary does not yet expose a runnable HTTP server; that work
is tracked in the project issue backlog. Once it does, the invocation will be:

```sh
cargo run -p mcs-server
```

Configuration will be read from environment variables and an optional
`mcs.toml` file (via [figment](https://docs.rs/figment)).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch conventions, commit style,
and the required local checks. All contributions are welcome; please open or
link an issue before submitting a PR.

## License

MCS is dual-licensed under **MIT OR Apache-2.0**. You may choose either
license. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)
for the full texts.

Every dependency is permissively licensed (the chess engine is
[cozy-chess](https://github.com/analog-hors/cozy-chess), MIT), so the
**assembled server binary is also MIT OR Apache-2.0** — there is no GPL
copyleft obligation. `cargo deny check` enforces this with no license
exceptions.
