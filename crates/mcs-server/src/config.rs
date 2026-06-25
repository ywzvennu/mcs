//! Layered server configuration.
//!
//! The server reads its settings from three layers, applied in increasing order
//! of precedence:
//!
//! 1. **Built-in defaults** ([`Config::default`]) — every key has a sensible
//!    development value, so the server boots with no configuration at all.
//! 2. **A `config.toml` file** in the working directory, if present. The path
//!    can be overridden with the `MCS_CONFIG` environment variable.
//! 3. **Environment variables** prefixed with `MCS_` — e.g. `MCS_BIND`,
//!    `MCS_DATABASE_URL`, `MCS_SESSION_SECRET`. Nested keys use a double
//!    underscore (`MCS_SIWE__DOMAIN`).
//!
//! See the committed `config.toml` for an annotated example of every key.
//!
//! # Run mode
//!
//! The `env` key (environment variable `MCS_ENV`) controls the server's run
//! mode and gates which validation rules are enforced at startup:
//!
//! | `env` value     | `MCS_ENV` value  | Behaviour |
//! |-----------------|------------------|-----------|
//! | `development`   | `development`    | Permissive defaults; ephemeral session secret allowed (warning logged). **Default.** |
//! | `production`    | `production`     | Strict validation: missing/weak secrets and misconfigured payments are fatal errors. |
//!
//! Set `env = "production"` in `config.toml` or `MCS_ENV=production` before
//! deploying to a live environment. The server will refuse to start if any
//! required production setting is absent or obviously wrong, printing a clear,
//! actionable error message naming the offending key.
//!
//! # Keys and defaults
//!
//! | Key                | Env var               | Default                       |
//! |--------------------|-----------------------|-------------------------------|
//! | `env`              | `MCS_ENV`             | `development`                 |
//! | `bind`             | `MCS_BIND`            | `127.0.0.1:8080`              |
//! | `database_url`     | `MCS_DATABASE_URL`   | `sqlite://mcs.db?mode=rwc`    |
//! | `database.max_connections` | `MCS_DATABASE__MAX_CONNECTIONS` | `10`           |
//! | `database.acquire_timeout_secs` | `MCS_DATABASE__ACQUIRE_TIMEOUT_SECS` | `30`  |
//! | `database.idle_timeout_secs` | `MCS_DATABASE__IDLE_TIMEOUT_SECS` | `600`       |
//! | `database.max_lifetime_secs` | `MCS_DATABASE__MAX_LIFETIME_SECS` | `0` (none)  |
//! | `database.statement_timeout_secs` | `MCS_DATABASE__STATEMENT_TIMEOUT_SECS` | `0` (PG only) |
//! | `log.format`       | `MCS_LOG__FORMAT`    | `pretty`                      |
//! | `log.level`        | `MCS_LOG__LEVEL`     | `info`                        |
//! | `session.secret`   | `MCS_SESSION__SECRET`| *(none — ephemeral)*          |
//! | `session.ttl_secs` | `MCS_SESSION__TTL_SECS` | `86400` (24h)              |
//! | `session.issuer`   | `MCS_SESSION__ISSUER`| `mcs`                         |
//! | `siwe.domain`      | `MCS_SIWE__DOMAIN`   | `localhost:8080`              |
//! | `siwe.uri`         | `MCS_SIWE__URI`      | `http://localhost:8080`       |
//! | `siwe.chain_id`    | `MCS_SIWE__CHAIN_ID` | `1`                           |
//! | `siwe.statement`   | `MCS_SIWE__STATEMENT`| `Sign in to MCS.`             |
//! | `siwe.nonce_ttl_secs` | `MCS_SIWE__NONCE_TTL_SECS` | `600` (10m)        |
//! | `payments.enabled` | `MCS_PAYMENTS__ENABLED` | `false`                    |
//! | `payments.scheme`  | `MCS_PAYMENTS__SCHEME`  | `exact`                    |
//! | `payments.network` | `MCS_PAYMENTS__NETWORK` | `base-sepolia`             |
//! | `payments.asset`   | `MCS_PAYMENTS__ASSET`   | `0x036C…aB32` (USDC)       |
//! | `payments.pay_to`  | `MCS_PAYMENTS__PAY_TO`  | *(zero address)*           |
//! | `payments.max_amount_required` | `MCS_PAYMENTS__MAX_AMOUNT_REQUIRED` | `10000` |
//! | `payments.description` | `MCS_PAYMENTS__DESCRIPTION` | `Create an MCS game.`  |
//! | `payments.max_timeout_seconds` | `MCS_PAYMENTS__MAX_TIMEOUT_SECONDS` | `300` |
//! | `payments.verifier` | `MCS_PAYMENTS__VERIFIER` | `mock`                    |
//! | `payments.facilitator_url` | `MCS_PAYMENTS__FACILITATOR_URL` | *(none)*       |
//! | `payments.facilitator_api_key` | `MCS_PAYMENTS__FACILITATOR_API_KEY` | *(none)* |
//! | `cors.allowed_origins` | `MCS_CORS__ALLOWED_ORIGINS` | `[]` (no cross-origin) |
//! | `cors.allow_credentials` | `MCS_CORS__ALLOW_CREDENTIALS` | `false`          |
//! | `cors.max_age_secs` | `MCS_CORS__MAX_AGE_SECS` | `3600` (1h)               |
//! | `cors.allow_any_origin` | `MCS_CORS__ALLOW_ANY_ORIGIN` | `false`            |
//! | `http.request_timeout_secs` | `MCS_HTTP__REQUEST_TIMEOUT_SECS` | `30`       |
//! | `http.max_body_bytes` | `MCS_HTTP__MAX_BODY_BYTES` | `65536` (64 KiB)       |
//! | `http.max_ws_message_bytes` | `MCS_HTTP__MAX_WS_MESSAGE_BYTES` | `1048576` (1 MiB) |
//! | `http.hsts` | `MCS_HTTP__HSTS` | `false`                                        |
//! | `http.hsts_max_age_secs` | `MCS_HTTP__HSTS_MAX_AGE_SECS` | `31536000` (1y) |
//! | `retention.enabled` | `MCS_RETENTION__ENABLED` | `true`                          |
//! | `retention.interval_secs` | `MCS_RETENTION__INTERVAL_SECS` | `3600` (1h)   |
//! | `retention.seek_max_age_secs` | `MCS_RETENTION__SEEK_MAX_AGE_SECS` | `86400` (24h) |
//! | `retention.challenge_max_age_secs` | `MCS_RETENTION__CHALLENGE_MAX_AGE_SECS` | `86400` (24h) |
//! | `limits.nonce_per_minute` | `MCS_LIMITS__NONCE_PER_MINUTE` | `10`           |
//! | `limits.verify_per_minute` | `MCS_LIMITS__VERIFY_PER_MINUTE` | `20`         |
//! | `limits.create_per_minute` | `MCS_LIMITS__CREATE_PER_MINUTE` | `30`         |
//! | `limits.max_ws_connections` | `MCS_LIMITS__MAX_WS_CONNECTIONS` | `10000`    |
//! | `limits.max_ws_connections_per_user` | `MCS_LIMITS__MAX_WS_CONNECTIONS_PER_USER` | `20` |
//! | `limits.max_games_per_user` | `MCS_LIMITS__MAX_GAMES_PER_USER` | `50`       |
//! | `limits.trusted_proxy_header` | `MCS_LIMITS__TRUSTED_PROXY_HEADER` | *(none)* |
//!
//! # Abuse-protection limits (#100)
//!
//! The `[limits]` section adds **per-node** abuse protection: per-IP token-bucket
//! rate limiting on the abuse-prone routes (`/auth/nonce`, `/auth/verify`,
//! `POST /seeks`, `POST /challenges`) returning **429 Too Many Requests** with a
//! `Retry-After` header when exceeded; a global and per-user cap on concurrent
//! live-game WebSocket connections; and a per-user cap on simultaneous live
//! games. Every limit lives in this process's memory, so behind a load balancer
//! the effective cluster-wide limit is up to `N x limit` for `N` nodes —
//! cluster-wide limiting would need a shared store (e.g. Redis) and is left as
//! future work. A `0` for any rate or cap disables it.
//!
//! # HTTP hardening (#99)
//!
//! A suite of security headers is injected into **every** HTTP response by a
//! `SetResponseHeader` layer applied inside the router:
//!
//! | Header | Value |
//! |--------|-------|
//! | `X-Content-Type-Options` | `nosniff` |
//! | `X-Frame-Options` | `DENY` |
//! | `Content-Security-Policy` | `default-src 'none'; frame-ancestors 'none'` |
//! | `Referrer-Policy` | `no-referrer` |
//! | `Strict-Transport-Security` | *only when `[http].hsts = true`* |
//!
//! **HSTS note**: `Strict-Transport-Security` must only be sent over TLS.
//! Enable it by setting `[http].hsts = true` **after** placing a TLS
//! terminator (e.g. nginx, a cloud load balancer, or Caddy) in front of this
//! server. Never enable it while the server terminates plain HTTP connections —
//! the browser will then refuse to connect over plain HTTP for
//! `hsts_max_age_secs` seconds, which is very hard to undo.
//!
//! A **request timeout** (`[http].request_timeout_secs`, default 30 s) aborts
//! handlers that take too long and returns 408/504 to the client, protecting
//! the server from slow-loris style attacks and runaway handlers.
//!
//! A **body-size limit** (`[http].max_body_bytes`, default 64 KiB) rejects
//! oversized JSON request bodies with **413 Payload Too Large** before the
//! handler even runs, protecting memory against upload attacks.
//!
//! A **WebSocket message-size limit** (`[http].max_ws_message_bytes`, default
//! 1 MiB) is set on each upgraded WebSocket connection to prevent a rogue
//! client from sending arbitrarily large frames.
//!
//! # CORS (#98)
//!
//! Cross-Origin Resource Sharing headers are **off by default**: with an empty
//! `allowed_origins` list the server sends no `Access-Control-Allow-Origin`
//! header and all cross-origin requests are blocked at the browser level, which
//! is the safe default for a server that has no configured browser client.
//!
//! To allow browser clients to reach the API, list each origin explicitly in
//! `[cors].allowed_origins`. For example, the `mcf` browser client must have its
//! origin (e.g. `https://mcf.example.com`) listed here:
//!
//! ```toml
//! [cors]
//! allowed_origins = ["https://mcf.example.com"]
//! allow_credentials = true   # set to true when sending Authorization headers
//! max_age_secs = 3600        # preflight cache lifetime
//! ```
//!
//! `allow_any_origin = true` is a **development-only escape hatch** that sets
//! `Access-Control-Allow-Origin: *`. It **cannot be combined with
//! `allow_credentials = true`** — the CORS spec forbids it and the browser will
//! reject such responses. Never enable `allow_any_origin` in production.
//!
//! # Payments (x402, #45)
//!
//! Game creation (`POST /seeks`) can be gated behind an x402 payment. The gate
//! is **off by default** ([`PaymentSettings::enabled`] = `false`): the server
//! boots free. When enabled, the composition root builds a
//! [`PaymentRequirements`](mcs_payments::PaymentRequirements) plus a verifier
//! and calls [`AppState::with_payment`](mcs_api::AppState::with_payment); the
//! API then wraps only the creation route in the payment layer. This is the
//! hook where, per the roadmap, RBC (and other) game creation would be charged.
//!
//! The default [`verifier`](PaymentSettings::verifier) is the development
//! [`MockVerifier`](mcs_payments::MockVerifier), which performs **no on-chain
//! checks** and must never be used in production. A real deployment sets
//! `verifier = "facilitator"` and points
//! [`facilitator_url`](PaymentSettings::facilitator_url) at a standards-compliant
//! x402 facilitator (see [`mcs_payments::FacilitatorVerifier`]); the verifier
//! delegates `/verify` + `/settle` to it.
//!
//! # Operational endpoints (#88)
//!
//! Three unauthenticated, un-gated routes serve orchestration and monitoring,
//! mounted by [`router`](crate::router) outside the API surface:
//!
//! | Method & path | Purpose | Body |
//! |---------------|---------|------|
//! | `GET /health` | **liveness** — is the process up? | `{"status":"ok"}` (always 200) |
//! | `GET /ready`  | **readiness** — are dependencies reachable? | `{"status":"ready"}` (200) or `{"status":"unavailable","failed":"…"}` (503) |
//! | `GET /metrics`| **Prometheus** scrape | `text/plain; version=0.0.4` exposition |
//!
//! Liveness touches nothing; readiness verifies the database (a `LIMIT 1` read)
//! and, when cluster mode is enabled, the Redis-backed membership store, naming
//! the first unhealthy dependency (`database` or `cluster`) in its 503 body. The
//! `/metrics` endpoint renders the recorder installed at start-up; the exported
//! series are `mcs_http_requests_total` and `mcs_http_request_duration_seconds`
//! (labelled by method, route template, and status), `mcs_games_live`,
//! `mcs_games_created_total`, `mcs_rating_updates_total`, and
//! `mcs_ws_connections_active`. See [`mcs_api::metrics`] for the full catalogue.

use std::net::SocketAddr;
use std::time::Duration as StdDuration;

use axum::http::HeaderValue;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use mcs_observability::LogFormat;
use serde::{Deserialize, Serialize};
use time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// The environment-variable prefix for configuration overrides.
const ENV_PREFIX: &str = "MCS_";

/// The environment variable naming an alternate `config.toml` path.
const CONFIG_PATH_ENV: &str = "MCS_CONFIG";

/// The default config file consulted when `MCS_CONFIG` is unset.
const DEFAULT_CONFIG_FILE: &str = "config.toml";

/// Minimum acceptable byte-length for a production session secret.
///
/// 32 bytes (256 bits) matches the HS256 key size and provides adequate entropy
/// for the HMAC-SHA256 signing algorithm used by the session layer.
const MIN_SECRET_LEN: usize = 32;

/// Session secrets that literally contain "change-me", "secret", "example",
/// "dev", or "test" are rejected in production regardless of length.
const WEAK_SECRET_SUBSTRINGS: &[&str] = &["change-me", "secret", "example", "dev", "test"];

/// The run mode of the server.
///
/// Controls which validation rules [`Config::validate`] enforces at startup.
/// Set via the `env` key in `config.toml` or the `MCS_ENV` environment variable.
///
/// # Production hardening
///
/// In [`Production`](RunMode::Production) mode the server performs strict
/// pre-flight validation and **refuses to start** if any required setting is
/// absent, obviously weak, or dangerously misconfigured. This includes:
///
/// - `session.secret` must be set, at least 32 bytes long, and must not contain
///   well-known placeholder strings such as `"change-me"` or `"secret"`.
/// - When `payments.enabled` is `true` and `payments.verifier` is `"facilitator"`,
///   `payments.facilitator_url` must be set and non-empty.
/// - Basic well-formedness checks (positive TTLs, parseable bind address) apply
///   in both modes.
///
/// In [`Development`](RunMode::Development) mode none of the production-only
/// checks are enforced and the server will log a warning rather than abort when
/// using an ephemeral session secret.
///
/// # Secret rotation
///
/// To rotate `session.secret` without downtime:
/// 1. Add the new secret to all nodes as `session.secret` (rolling deploy or
///    coordinated config push).
/// 2. After all nodes have restarted, any tokens signed with the old secret will
///    expire naturally at `session.ttl_secs`; no immediate invalidation occurs
///    because the secret is rotated atomically across nodes.
/// 3. For immediate invalidation, restart all nodes simultaneously with the new
///    secret — this will force all existing sessions to re-authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// Development mode (the default): convenient defaults, ephemeral session
    /// secrets allowed, less strict validation. Never use in production.
    #[default]
    Development,
    /// Production mode: strict pre-flight validation. The server refuses to
    /// start if required secrets are absent/weak or payments are misconfigured.
    Production,
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunMode::Development => f.write_str("development"),
            RunMode::Production => f.write_str("production"),
        }
    }
}

/// A configuration validation error, naming the offending key and describing
/// the problem in actionable terms.
///
/// Returned by [`Config::validate`] when the configuration is inconsistent or
/// missing a required value. The [`Display`](std::fmt::Display) output is
/// designed to be printed directly to the operator.
#[derive(Debug)]
pub struct ConfigError {
    /// The configuration key that is missing or invalid (e.g.
    /// `session.secret`, `payments.facilitator_url`).
    pub key: &'static str,
    /// Human-readable description of the problem and how to fix it.
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "configuration error: {} — {}", self.key, self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Fully-resolved server configuration.
///
/// Build one with [`Config::load`], which layers defaults, an optional
/// `config.toml`, and `MCS_`-prefixed environment variables. Every field has a
/// default, so the type also implements [`Default`] for tests and tooling.
///
/// After loading, call [`Config::validate`] to enforce run-mode-appropriate
/// invariants before the server binds its socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The run mode of the server.
    ///
    /// Set to `"production"` via `env = "production"` in `config.toml` or
    /// `MCS_ENV=production` in the environment. Defaults to `"development"`.
    ///
    /// In production mode [`Config::validate`] enforces strict pre-flight
    /// checks (required secrets, valid payment configuration, etc.) and the
    /// server refuses to start if any check fails. In development mode the
    /// same method only validates basic well-formedness (positive TTLs,
    /// parseable bind address) and allows convenient insecure defaults.
    pub env: RunMode,
    /// The socket address the HTTP server binds to.
    pub bind: SocketAddr,
    /// The storage connection string handed to
    /// [`SqlxStorage::connect`](mcs_storage::SqlxStorage::connect).
    ///
    /// The default `sqlite://mcs.db?mode=rwc` keeps state in a single file in
    /// the working directory, creating it on first run. Use `sqlite::memory:`
    /// for an ephemeral, test-only database.
    pub database_url: String,
    /// Connection-pool tuning for the storage backend (#105).
    pub database: DatabaseSettings,
    /// Logging configuration.
    pub log: LogConfig,
    /// Session-token (JWT) configuration.
    pub session: SessionSettings,
    /// Sign-In with Ethereum challenge configuration.
    pub siwe: SiweSettings,
    /// x402 payment gating for game creation (off by default).
    pub payments: PaymentSettings,
    /// Cluster / horizontal-scaling configuration (off by default, #68).
    pub cluster: ClusterSettings,
    /// Cross-Origin Resource Sharing configuration for browser clients (#98).
    pub cors: CorsSettings,
    /// HTTP hardening limits and security-header settings (#99).
    pub http: HttpSettings,
    /// Abuse-protection limits: per-IP rate limiting and resource caps (#100).
    pub limits: LimitsSettings,
    /// Periodic retention / GC for ephemeral data (#107).
    pub retention: RetentionSettings,
}

/// Database connection-pool tuning (#105).
///
/// These knobs size the storage pool for a production deployment. They matter
/// most for Postgres, where many server nodes share a single instance, but the
/// defaults are conservative enough for a single-node SQLite file too.
///
/// Durations are expressed in whole seconds so they map cleanly onto plain
/// environment variables and TOML integers. A `0` (or omitted) optional timeout
/// disables that bound.
///
/// # `config.toml` sample
///
/// ```toml
/// [database]
/// # Maximum pool connections. For Postgres, keep `nodes * max_connections`
/// # comfortably under the server's `max_connections` setting.
/// max_connections = 10
///
/// # How long `acquire` waits for a free connection before erroring.
/// acquire_timeout_secs = 30
///
/// # Close a connection idle in the pool for this long. 0 = keep indefinitely.
/// idle_timeout_secs = 600
///
/// # Recycle any connection older than this. 0 = no maximum lifetime.
/// # Useful behind a load balancer that drops idle backend TCP connections.
/// max_lifetime_secs = 1800
///
/// # Postgres only: per-statement timeout (SET statement_timeout). 0 = unset.
/// # Ignored on SQLite.
/// statement_timeout_secs = 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseSettings {
    /// Maximum number of connections the pool may open. Default: `10`.
    ///
    /// In-memory SQLite is always pinned to a single connection regardless of
    /// this value (see [`mcs_storage::PoolConfig`]).
    pub max_connections: u32,
    /// How long `acquire` waits for a free connection before returning a timeout
    /// error, in seconds. Default: `30`.
    pub acquire_timeout_secs: u64,
    /// Close a connection idle in the pool for at least this many seconds. `0`
    /// keeps idle connections indefinitely. Default: `600` (10 min).
    pub idle_timeout_secs: u64,
    /// Recycle any connection older than this many seconds, regardless of use.
    /// `0` (the default) imposes no maximum lifetime.
    pub max_lifetime_secs: u64,
    /// Postgres-only per-statement timeout, in seconds (issued as
    /// `SET statement_timeout`). `0` (the default) leaves it unset. Ignored on
    /// SQLite.
    pub statement_timeout_secs: u64,
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        // Mirror `mcs_storage::PoolConfig::default` so the two never drift.
        Self {
            max_connections: 10,
            acquire_timeout_secs: 30,
            idle_timeout_secs: 600,
            max_lifetime_secs: 0,
            statement_timeout_secs: 0,
        }
    }
}

impl DatabaseSettings {
    /// Builds the storage layer's [`PoolConfig`](mcs_storage::PoolConfig) from
    /// these settings, translating each whole-second value into a
    /// [`Duration`](std::time::Duration) and mapping the `0`-means-disabled
    /// optional timeouts onto `None`.
    #[must_use]
    pub fn to_pool_config(&self) -> mcs_storage::PoolConfig {
        let secs = |s: u64| std::time::Duration::from_secs(s);
        let opt = |s: u64| (s != 0).then(|| secs(s));
        mcs_storage::PoolConfig {
            max_connections: self.max_connections,
            acquire_timeout: secs(self.acquire_timeout_secs),
            idle_timeout: opt(self.idle_timeout_secs),
            max_lifetime: opt(self.max_lifetime_secs),
            statement_timeout: opt(self.statement_timeout_secs),
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Output format: `pretty` (human-readable) or `json` (structured).
    pub format: LogFormatChoice,
    /// The fallback tracing filter directive used when `RUST_LOG` is unset,
    /// e.g. `info` or `info,mcs_api=debug`.
    pub level: String,
}

/// Serializable mirror of [`LogFormat`].
///
/// [`LogFormat`] is defined in `mcs-observability` without `serde` support, so
/// this local enum provides the (de)serialization and converts into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormatChoice {
    /// Human-readable, multi-line output (the default).
    #[default]
    Pretty,
    /// Newline-delimited JSON for log aggregation.
    Json,
}

impl From<LogFormatChoice> for LogFormat {
    fn from(choice: LogFormatChoice) -> Self {
        match choice {
            LogFormatChoice::Pretty => LogFormat::Pretty,
            LogFormatChoice::Json => LogFormat::Json,
        }
    }
}

/// Session-token configuration as read from the environment/file.
///
/// Durations are expressed in whole seconds so they map cleanly onto plain
/// environment variables and TOML integers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSettings {
    /// The HMAC-SHA256 signing secret, as a UTF-8 string.
    ///
    /// **Required in production.** When absent, the server generates a random
    /// ephemeral secret at startup and logs a prominent warning: sessions minted
    /// against an ephemeral secret do not survive a restart.
    pub secret: Option<String>,
    /// How long an issued session token stays valid, in seconds.
    pub ttl_secs: u64,
    /// The `iss` (issuer) claim written into and required on session tokens.
    pub issuer: String,
}

/// Sign-In with Ethereum challenge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SiweSettings {
    /// The RFC 3986 authority requesting the sign-in (the SIWE `domain`).
    pub domain: String,
    /// The RFC 3986 URI of the resource being signed into (the SIWE `uri`).
    pub uri: String,
    /// The EIP-155 chain ID the session is bound to (`1` = Ethereum mainnet).
    pub chain_id: u64,
    /// The human-readable statement shown in the user's wallet.
    pub statement: String,
    /// How long a freshly issued nonce remains valid, in seconds.
    pub nonce_ttl_secs: u64,
}

/// x402 payment-gate configuration for game creation (#45).
///
/// All fields map onto a single
/// [`PaymentRequirements`](mcs_payments::PaymentRequirements) entry plus a
/// verifier selector. The gate is **disabled by default** — `enabled = false`
/// leaves `POST /seeks` free and is the only field that matters until an
/// operator opts in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PaymentSettings {
    /// Whether to gate game creation behind an x402 payment. `false` by default,
    /// so the server boots free and behaves exactly as before.
    pub enabled: bool,
    /// The x402 scheme advertised to clients. The canonical value is `"exact"`.
    pub scheme: String,
    /// The target network (e.g. `"base"`, `"base-sepolia"`).
    pub network: String,
    /// The accepted payment-token contract address (e.g. USDC).
    pub asset: String,
    /// The on-chain address that must receive the payment.
    pub pay_to: String,
    /// The maximum token amount accepted, in the asset's smallest unit (e.g.
    /// `"10000"` is 0.01 USDC at 6 decimals).
    pub max_amount_required: String,
    /// Human-readable description shown to the payer before paying.
    pub description: String,
    /// Maximum seconds a signed authorization may stay pending before expiring.
    pub max_timeout_seconds: u64,
    /// Which [`PaymentVerifier`](mcs_payments::PaymentVerifier) implementation to
    /// build. Defaults to the development [`Mock`](VerifierChoice::Mock).
    pub verifier: VerifierChoice,
    /// Base URL of the x402 facilitator, e.g.
    /// `https://facilitator.example.com`. **Required** when
    /// [`verifier`](Self::verifier) is [`VerifierChoice::Facilitator`];
    /// [`build_verifier`](Self::build_verifier) errors if it is missing. Ignored
    /// for the mock verifier.
    pub facilitator_url: Option<String>,
    /// Optional bearer token sent to the facilitator as
    /// `Authorization: Bearer {token}`. Only consulted when
    /// [`verifier`](Self::verifier) is [`VerifierChoice::Facilitator`].
    pub facilitator_api_key: Option<String>,
}

/// Selects which payment verifier the server constructs when payments are
/// [`enabled`](PaymentSettings::enabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifierChoice {
    /// The development [`MockVerifier`](mcs_payments::MockVerifier): accepts any
    /// well-formed payload whose scheme/network/asset match. **Never use in
    /// production** — it performs no cryptographic or on-chain checks. This is
    /// the default so a misconfiguration cannot silently enable real charging.
    #[default]
    Mock,
    /// The production [`FacilitatorVerifier`](mcs_payments::FacilitatorVerifier):
    /// delegates `/verify` + `/settle` to the x402 facilitator at
    /// [`facilitator_url`](PaymentSettings::facilitator_url). The URL is required;
    /// [`build_verifier`](PaymentSettings::build_verifier) errors without it.
    Facilitator,
}

/// Cluster / horizontal-scaling configuration (#68).
///
/// **Disabled by default** ([`enabled`](ClusterSettings::enabled) = `false`):
/// the server runs as a single node with an in-process
/// [`LocalRegistry`](mcs_cluster::LocalRegistry), opens no Redis connection, and
/// behaves byte-for-byte as it did before clustering. Only when enabled does the
/// composition root connect a
/// [`RedisNodeRegistry`](mcs_cluster::RedisNodeRegistry), register this node,
/// spawn a heartbeat task, and hand the registry to
/// [`AppState::with_cluster`](mcs_api::AppState::with_cluster) — so the WS layer
/// routes each game to its rendezvous owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterSettings {
    /// Whether to join a cluster. `false` by default, so the server boots
    /// single-node and touches no cluster backend.
    pub enabled: bool,
    /// This node's stable id, used both to register membership and to compute
    /// rendezvous ownership. Must be unique and stable per process. When empty
    /// (the default), a fresh UUID is generated at startup — fine for ephemeral
    /// nodes, but pin it for stable identities (e.g. a pod or hostname).
    pub node_id: String,
    /// This node's externally reachable base URL, e.g. `http://127.0.0.1:8080`.
    /// Peers and clients use it to reach this node; it is the address a redirect
    /// points other nodes' clients at, so it must be reachable from them (not a
    /// loopback in a real multi-host deployment).
    pub address: String,
    /// The Redis connection URL backing membership, e.g.
    /// `redis://127.0.0.1:6379`. Only consulted when [`enabled`](Self::enabled).
    pub redis_url: String,
    /// The liveness TTL, in seconds: a node missing this many seconds of
    /// heartbeats is evicted from membership by Redis. Defaults to `15`.
    pub heartbeat_ttl_secs: u64,
    /// How often, in seconds, to renew this node's TTL. Must be comfortably
    /// shorter than [`heartbeat_ttl_secs`](Self::heartbeat_ttl_secs). Defaults
    /// to `5`.
    pub heartbeat_interval_secs: u64,
}

/// HTTP hardening limits and security-header settings (#99).
///
/// All values have safe, conservative defaults: the server ships with security
/// headers enabled, a 30-second request timeout, and a 64 KiB body limit.
/// Operators only need to touch this section to raise limits or to enable HSTS
/// (which **requires** a TLS terminator in front of the server — see the
/// module-level notes).
///
/// # `config.toml` sample
///
/// ```toml
/// [http]
/// # How long a handler may run before the server returns 408/504.
/// request_timeout_secs = 30
///
/// # Maximum JSON / form body accepted before the handler is called.
/// # Bodies exceeding this size are rejected with 413 Payload Too Large.
/// max_body_bytes = 65536
///
/// # Maximum WebSocket message size (single frame or reassembled message).
/// # Frames exceeding this size are rejected and the socket is closed.
/// max_ws_message_bytes = 1048576
///
/// # Enable Strict-Transport-Security ONLY behind a TLS terminator.
/// # Never enable while the server itself handles plain HTTP connections.
/// # hsts = false
/// # hsts_max_age_secs = 31536000  # 1 year
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpSettings {
    /// How long a handler may run before the server considers the request
    /// timed out and returns a timeout error, in seconds.
    ///
    /// Default: `30`.
    pub request_timeout_secs: u64,

    /// Maximum accepted request body size, in bytes. Bodies larger than this
    /// are rejected with **413 Payload Too Large** before the handler runs.
    ///
    /// The default (65 536 = 64 KiB) is generous for JSON API payloads and
    /// tight enough to protect against trivial memory-exhaustion attacks.
    pub max_body_bytes: usize,

    /// Maximum WebSocket message size, in bytes. A single message that exceeds
    /// this limit (whether a single frame or a fragmented multi-frame message)
    /// is rejected and the socket is closed by the server.
    ///
    /// Default: `1 048 576` (1 MiB).
    pub max_ws_message_bytes: usize,

    /// Whether to include `Strict-Transport-Security` in every response.
    ///
    /// **Only enable behind a TLS terminator.** When the server itself handles
    /// plain HTTP connections, setting this to `true` causes browsers to refuse
    /// to connect over plain HTTP for [`hsts_max_age_secs`](Self::hsts_max_age_secs)
    /// seconds — which is very hard to undo.
    ///
    /// Default: `false`.
    pub hsts: bool,

    /// The `max-age` directive of the `Strict-Transport-Security` header, in
    /// seconds. Only consulted when [`hsts`](Self::hsts) is `true`.
    ///
    /// Default: `31 536 000` (1 year — the recommended production value).
    pub hsts_max_age_secs: u64,
}

/// Periodic retention / GC configuration for ephemeral data (#107).
///
/// Controls the background task that periodically purges expired and stale
/// ephemeral rows from storage:
///
/// - **auth nonces** — always purged when the retention task runs; `nonce` ages
///   are controlled by [`SiweSettings::nonce_ttl_secs`] upstream, not here.
/// - **revoked tokens** — always purged; the denylist is self-trimming.
/// - **stale seeks** — open seeks older than [`seek_max_age_secs`] are removed.
/// - **resolved challenges** — declined/canceled challenges older than
///   [`challenge_max_age_secs`] are removed. Accepted challenges (attached to a
///   game) are never deleted by the GC task.
///
/// Set [`enabled`](RetentionSettings::enabled) to `false` to disable the
/// background task entirely (useful in tests and operator opt-outs).
/// Set an individual `*_max_age_secs` to `0` to skip that particular sweep.
///
/// # `config.toml` sample
///
/// ```toml
/// [retention]
/// enabled = true
/// interval_secs = 3600           # run the sweep every hour
/// seek_max_age_secs = 86400      # remove open seeks older than 24 h
/// challenge_max_age_secs = 86400 # remove resolved challenges older than 24 h
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionSettings {
    /// Whether the periodic retention task is running. `true` by default.
    pub enabled: bool,
    /// How often the retention task wakes up and runs a sweep, in seconds.
    ///
    /// Default: `3600` (1 hour). Must be greater than zero.
    pub interval_secs: u64,
    /// Maximum age of an open seek before it is considered stale and purged, in
    /// seconds. `0` disables the seek sweep. Default: `86400` (24 h).
    pub seek_max_age_secs: u64,
    /// Maximum age of a resolved (Declined or Canceled) challenge before it is
    /// purged, in seconds. `0` disables the challenge sweep. Default: `86400`
    /// (24 h).
    pub challenge_max_age_secs: u64,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 3600,
            seek_max_age_secs: 86_400,
            challenge_max_age_secs: 86_400,
        }
    }
}

/// Abuse-protection limits: per-IP rate limiting and resource caps (#100).
///
/// All limits are enforced **per node** (per server process). In a multi-node
/// deployment a client's requests may be spread across nodes, so the effective
/// cluster-wide limit is up to `N x limit` for `N` nodes; cluster-wide limiting
/// would need a shared store (e.g. Redis) and is deliberately left as future
/// work, mirroring presence and cluster membership.
///
/// The per-IP rate limits are token buckets: a route's `*_per_minute` value is
/// both the sustained refill rate and the burst ceiling. A value of `0` disables
/// that particular limit. Likewise a `0` connection/game cap is treated as
/// "unlimited".
///
/// # `config.toml` sample
///
/// ```toml
/// [limits]
/// # Per-IP request rates (requests per minute). 0 disables a limit.
/// nonce_per_minute = 10           # GET  /auth/nonce
/// verify_per_minute = 20          # POST /auth/verify
/// create_per_minute = 30          # POST /seeks, POST /challenges
///
/// # Concurrent live-game WebSocket connections (this node).
/// max_ws_connections = 10000      # global cap; 0 = unlimited
/// max_ws_connections_per_user = 20
///
/// # Simultaneous live games a single user may play (this node).
/// max_games_per_user = 50         # 0 = unlimited
///
/// # Trust a reverse-proxy header for the real client IP. Leave unset when the
/// # server is exposed directly; a client can spoof this header otherwise.
/// # trusted_proxy_header = "x-forwarded-for"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsSettings {
    /// Per-IP request rate for `GET /auth/nonce`, in requests per minute. `0`
    /// disables the limit. Default: `10`.
    pub nonce_per_minute: u32,
    /// Per-IP request rate for `POST /auth/verify`, in requests per minute. `0`
    /// disables the limit. Default: `20`.
    pub verify_per_minute: u32,
    /// Per-IP request rate for the game-creation routes (`POST /seeks`,
    /// `POST /challenges`), in requests per minute. `0` disables the limit.
    /// Default: `30`.
    pub create_per_minute: u32,
    /// Global cap on concurrent live-game WebSocket connections on this node.
    /// `0` disables the cap. Default: `10000`.
    pub max_ws_connections: u32,
    /// Per-user cap on concurrent live-game WebSocket connections on this node.
    /// `0` disables the cap. Default: `20`.
    pub max_ws_connections_per_user: u32,
    /// Per-user cap on simultaneous live games on this node. `0` disables the
    /// cap. Default: `50`.
    pub max_games_per_user: u32,
    /// Optional trusted reverse-proxy header to read the real client IP from
    /// (e.g. `"x-forwarded-for"`). Unset (the default) uses the socket peer
    /// address. **Only** set this behind a trusted proxy — a client can spoof
    /// the header otherwise, evading the per-IP rate limits.
    pub trusted_proxy_header: Option<String>,
}

impl Default for LimitsSettings {
    fn default() -> Self {
        // Mirror `mcs_api::LimitsConfig::default` so the two never drift.
        Self {
            nonce_per_minute: 10,
            verify_per_minute: 20,
            create_per_minute: 30,
            max_ws_connections: 10_000,
            max_ws_connections_per_user: 20,
            max_games_per_user: 50,
            trusted_proxy_header: None,
        }
    }
}

impl LimitsSettings {
    /// Builds the API layer's [`LimitsConfig`](mcs_api::LimitsConfig) from these
    /// settings, translating each per-minute rate into a token-bucket tier.
    #[must_use]
    pub fn to_limits_config(&self) -> mcs_api::LimitsConfig {
        mcs_api::LimitsConfig {
            nonce: mcs_api::RateLimitTier::per_minute(self.nonce_per_minute),
            verify: mcs_api::RateLimitTier::per_minute(self.verify_per_minute),
            create: mcs_api::RateLimitTier::per_minute(self.create_per_minute),
            max_ws_connections: self.max_ws_connections,
            max_ws_connections_per_user: self.max_ws_connections_per_user,
            max_games_per_user: self.max_games_per_user,
            trusted_proxy_header: self.trusted_proxy_header.clone(),
        }
    }
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            request_timeout_secs: 30,
            // 64 KiB: generous for JSON API payloads, tight against memory attacks.
            max_body_bytes: 64 * 1024,
            // 1 MiB: enough for a complete board state JSON with commentary.
            max_ws_message_bytes: 1024 * 1024,
            // HSTS is off by default: it must only be enabled behind TLS.
            hsts: false,
            // 1 year: the standard production HSTS max-age.
            hsts_max_age_secs: 365 * 24 * 60 * 60,
        }
    }
}

/// Cross-Origin Resource Sharing (CORS) configuration for browser clients (#98).
///
/// Controls which origins can make cross-origin HTTP requests to the API. By
/// default **no cross-origin requests are allowed**: [`allowed_origins`] is
/// empty and the server sends no `Access-Control-Allow-Origin` header.
///
/// # `config.toml` sample
///
/// ```toml
/// [cors]
/// # List every browser origin that should be allowed to call this server.
/// # The mcf browser client's origin must be listed here.
/// allowed_origins = ["https://mcf.example.com", "https://staging.mcf.example.com"]
///
/// # Set to true when the browser sends credentials (e.g. Authorization headers).
/// # Cannot be combined with allow_any_origin = true.
/// allow_credentials = true
///
/// # How long (in seconds) the browser may cache a preflight response.
/// max_age_secs = 3600
///
/// # DEV ONLY: accept requests from every origin.
/// # Never use in production; incompatible with allow_credentials = true.
/// # allow_any_origin = false
/// ```
///
/// [`allowed_origins`]: CorsSettings::allowed_origins
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsSettings {
    /// Exact origins permitted to make cross-origin requests, e.g.
    /// `["https://mcf.example.com"]`. Each entry must be a valid HTTP origin
    /// string (`scheme://host[:port]` — no path, no trailing slash).
    ///
    /// **Default: empty** — no cross-origin requests are permitted, which is
    /// the safe default for a server without a configured browser client.
    pub allowed_origins: Vec<String>,

    /// Whether to allow browsers to send credentials (cookies and
    /// `Authorization` headers) with cross-origin requests.
    ///
    /// **Cannot be combined with [`allow_any_origin`](Self::allow_any_origin).**
    /// The CORS specification forbids `Access-Control-Allow-Origin: *` together
    /// with `Access-Control-Allow-Credentials: true`; browsers will reject such
    /// responses.
    ///
    /// Default: `false`.
    pub allow_credentials: bool,

    /// How long (in seconds) a browser may cache a preflight
    /// (`OPTIONS`) response. Default: `3600` (1 hour).
    pub max_age_secs: u64,

    /// **Development-only escape hatch.** When `true`, the server responds
    /// with `Access-Control-Allow-Origin: *`, permitting requests from any
    /// origin without listing them individually.
    ///
    /// **Never enable in production.** Incompatible with
    /// [`allow_credentials = true`](Self::allow_credentials) — the CORS spec
    /// forbids combining a wildcard origin with credentials, and browsers will
    /// block such responses. When both are set to `true` the server logs a
    /// warning and ignores `allow_credentials`.
    ///
    /// Default: `false`.
    pub allow_any_origin: bool,
}

impl Default for ClusterSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            // Empty ⇒ generate a UUID at startup (see `build_node_id`).
            node_id: String::new(),
            address: "http://127.0.0.1:8080".to_owned(),
            redis_url: "redis://127.0.0.1:6379".to_owned(),
            heartbeat_ttl_secs: 15,
            heartbeat_interval_secs: 5,
        }
    }
}

impl Default for CorsSettings {
    fn default() -> Self {
        Self {
            // Empty by default: no cross-origin requests are allowed until an
            // operator explicitly lists origins. This is the safe default.
            allowed_origins: Vec::new(),
            allow_credentials: false,
            max_age_secs: 3600,
            allow_any_origin: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            env: RunMode::default(),
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            database_url: "sqlite://mcs.db?mode=rwc".to_owned(),
            database: DatabaseSettings::default(),
            log: LogConfig::default(),
            session: SessionSettings::default(),
            siwe: SiweSettings::default(),
            payments: PaymentSettings::default(),
            cluster: ClusterSettings::default(),
            cors: CorsSettings::default(),
            http: HttpSettings::default(),
            limits: LimitsSettings::default(),
            retention: RetentionSettings::default(),
        }
    }
}

impl Default for PaymentSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            scheme: "exact".to_owned(),
            network: "base-sepolia".to_owned(),
            // USDC on Base Sepolia (6 decimals) — a sensible testnet default.
            asset: "0x036CbD53842c5426634e7929541eC2318f3dCF7e".to_owned(),
            // The zero address: a deliberately invalid recipient so an operator
            // who enables payments without setting `pay_to` notices immediately.
            pay_to: "0x0000000000000000000000000000000000000000".to_owned(),
            max_amount_required: "10000".to_owned(),
            description: "Create an MCS game.".to_owned(),
            max_timeout_seconds: 300,
            verifier: VerifierChoice::default(),
            facilitator_url: None,
            facilitator_api_key: None,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormatChoice::Pretty,
            level: "info".to_owned(),
        }
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            secret: None,
            ttl_secs: 24 * 60 * 60,
            issuer: "mcs".to_owned(),
        }
    }
}

impl Default for SiweSettings {
    fn default() -> Self {
        Self {
            domain: "localhost:8080".to_owned(),
            uri: "http://localhost:8080".to_owned(),
            chain_id: 1,
            statement: "Sign in to MCS.".to_owned(),
            nonce_ttl_secs: 10 * 60,
        }
    }
}

impl CorsSettings {
    /// Builds a [`CorsLayer`] from this configuration.
    ///
    /// Allowed methods: `GET`, `POST`, `DELETE`, `OPTIONS`.
    /// Allowed request headers: `authorization`, `content-type`.
    /// No sensitive response headers are exposed.
    /// Preflight TTL: [`max_age_secs`](Self::max_age_secs).
    ///
    /// Origin handling:
    ///
    /// - When [`allow_any_origin`](Self::allow_any_origin) is `true`, the layer
    ///   echoes `Access-Control-Allow-Origin: *`. **Development only** — never
    ///   use in production. Incompatible with credentials (the CORS spec forbids
    ///   the combination; browsers will reject such responses).
    /// - Otherwise, the layer checks each inbound `Origin` against
    ///   [`allowed_origins`](Self::allowed_origins). Only listed origins receive
    ///   an `Access-Control-Allow-Origin` header in the response.
    /// - When [`allowed_origins`](Self::allowed_origins) is empty **and**
    ///   [`allow_any_origin`](Self::allow_any_origin) is `false`, no
    ///   `Access-Control-Allow-Origin` header is ever sent.
    ///
    /// Origins that cannot be parsed as valid [`HeaderValue`]s are silently
    /// skipped (and a warning is logged) so a single bad entry does not prevent
    /// the server from starting.
    pub fn build_cors_layer(&self) -> CorsLayer {
        use axum::http::{header, Method};
        use tower_http::cors::AllowHeaders;

        if self.allow_any_origin && self.allow_credentials {
            tracing::warn!(
                "cors.allow_any_origin = true is incompatible with \
                 cors.allow_credentials = true (the CORS spec forbids \
                 Access-Control-Allow-Origin: * with credentials); \
                 ignoring allow_credentials"
            );
        }

        let allow_origin: AllowOrigin = if self.allow_any_origin {
            AllowOrigin::any()
        } else if self.allowed_origins.is_empty() {
            // No configured origins → no cross-origin requests allowed.
            AllowOrigin::list(std::iter::empty::<HeaderValue>())
        } else {
            let values: Vec<HeaderValue> = self
                .allowed_origins
                .iter()
                .filter_map(|origin| {
                    origin
                        .parse::<HeaderValue>()
                        .map_err(|err| {
                            tracing::warn!(
                                %origin,
                                %err,
                                "cors.allowed_origins: skipping invalid origin"
                            );
                        })
                        .ok()
                })
                .collect();
            AllowOrigin::list(values)
        };

        let mut layer = CorsLayer::new()
            .allow_origin(allow_origin)
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers(AllowHeaders::list([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
            ]))
            .max_age(StdDuration::from_secs(self.max_age_secs));

        // Only set credentials header when not in wildcard mode (the CORS spec
        // forbids the combination, and tower-http will panic if both are set).
        if self.allow_credentials && !self.allow_any_origin {
            layer = layer.allow_credentials(true);
        }

        layer
    }
}

impl HttpSettings {
    /// Returns the request timeout as a [`StdDuration`].
    #[must_use]
    pub fn request_timeout(&self) -> StdDuration {
        StdDuration::from_secs(self.request_timeout_secs)
    }

    /// Builds the `Strict-Transport-Security` header value string when
    /// [`hsts`](Self::hsts) is enabled, or `None` when it is disabled.
    ///
    /// The value uses `max-age=<secs>; includeSubDomains` — the recommended
    /// production form. `preload` is omitted because registering a domain for
    /// browser preload lists is an operator decision made outside the server.
    ///
    /// # Why `None` when disabled?
    ///
    /// Sending an HSTS header over a plain-HTTP connection is dangerous: the
    /// browser will then refuse to connect over plain HTTP for `max-age` seconds,
    /// which is very hard to undo. The default is therefore to omit the header
    /// entirely, and only emit it when the operator has explicitly opted in.
    #[must_use]
    pub fn hsts_header_value(&self) -> Option<String> {
        if self.hsts {
            Some(format!(
                "max-age={}; includeSubDomains",
                self.hsts_max_age_secs
            ))
        } else {
            None
        }
    }
}

impl Config {
    /// Validates the configuration for the current [`RunMode`].
    ///
    /// Call this after [`Config::load`] and before the server binds its socket.
    /// A failed validation means the operator must fix their configuration;
    /// the returned [`ConfigError`] names the offending key and explains the
    /// problem in actionable terms.
    ///
    /// # Validation rules — both modes
    ///
    /// These checks are enforced in every run mode, including development:
    ///
    /// - `session.ttl_secs` must be greater than zero.
    /// - `siwe.nonce_ttl_secs` must be greater than zero.
    /// - `http.request_timeout_secs` must be greater than zero.
    ///
    /// # Validation rules — production only (`env = "production"`)
    ///
    /// The following checks are only enforced when
    /// [`env`](Config::env) is [`RunMode::Production`]:
    ///
    /// - `session.secret` must be set, at least 32 bytes long, and must not
    ///   contain obvious placeholder strings (`"change-me"`, `"secret"`,
    ///   `"example"`, `"dev"`, `"test"`).
    /// - When `payments.enabled = true` and `payments.verifier = "facilitator"`,
    ///   `payments.facilitator_url` must be set and non-empty.
    /// - When `payments.enabled = true`, `payments.verifier` must not be `"mock"`:
    ///   the mock verifier performs no on-chain checks and must never be used in
    ///   production.
    /// - `cors.allow_any_origin` must not be `true`: wildcard CORS is a
    ///   development-only escape hatch.
    ///
    /// # Errors
    ///
    /// Returns `Err(ConfigError)` on the first validation failure encountered,
    /// naming the offending key and describing the problem.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mcs_server::Config;
    ///
    /// let cfg = Config::load().unwrap();
    /// if let Err(e) = cfg.validate() {
    ///     eprintln!("{e}");
    ///     std::process::exit(1);
    /// }
    /// ```
    pub fn validate(&self) -> Result<(), ConfigError> {
        // ── Both-mode checks (basic well-formedness) ─────────────────────────

        if self.session.ttl_secs == 0 {
            return Err(ConfigError {
                key: "session.ttl_secs",
                message: "must be greater than zero (set MCS_SESSION__TTL_SECS or \
                          session.ttl_secs in config.toml)"
                    .to_owned(),
            });
        }

        if self.siwe.nonce_ttl_secs == 0 {
            return Err(ConfigError {
                key: "siwe.nonce_ttl_secs",
                message: "must be greater than zero (set MCS_SIWE__NONCE_TTL_SECS or \
                          siwe.nonce_ttl_secs in config.toml)"
                    .to_owned(),
            });
        }

        if self.http.request_timeout_secs == 0 {
            return Err(ConfigError {
                key: "http.request_timeout_secs",
                message: "must be greater than zero (set MCS_HTTP__REQUEST_TIMEOUT_SECS or \
                          http.request_timeout_secs in config.toml)"
                    .to_owned(),
            });
        }

        // ── Production-only checks ────────────────────────────────────────────
        if self.env == RunMode::Production {
            self.validate_production()?;
        }

        Ok(())
    }

    /// Enforces production-only validation rules.
    ///
    /// Called by [`validate`](Self::validate) when `env = "production"`. Never
    /// call directly; use [`validate`](Self::validate) instead so development
    /// mode skips these checks.
    fn validate_production(&self) -> Result<(), ConfigError> {
        // session.secret: required, sufficiently long, not a placeholder.
        match self.session.secret.as_deref() {
            None | Some("") => {
                return Err(ConfigError {
                    key: "session.secret",
                    message: format!(
                        "must be set in production — sessions signed with an ephemeral \
                         secret are invalidated on every restart. \
                         Generate a secret with `openssl rand -hex 32` and set it via \
                         MCS_SESSION__SECRET or session.secret in config.toml. \
                         Minimum length: {MIN_SECRET_LEN} bytes."
                    ),
                });
            }
            Some(s) if s.len() < MIN_SECRET_LEN => {
                return Err(ConfigError {
                    key: "session.secret",
                    message: format!(
                        "is too short ({} bytes); production requires at least {MIN_SECRET_LEN} \
                         bytes. Generate a strong secret with `openssl rand -hex 32`.",
                        s.len()
                    ),
                });
            }
            Some(s) => {
                let lower = s.to_ascii_lowercase();
                for placeholder in WEAK_SECRET_SUBSTRINGS {
                    if lower.contains(placeholder) {
                        return Err(ConfigError {
                            key: "session.secret",
                            message: format!(
                                "looks like a placeholder (contains {placeholder:?}). \
                                 Use a high-entropy random value for production. \
                                 Generate one with `openssl rand -hex 32` and set it via \
                                 MCS_SESSION__SECRET or session.secret in config.toml."
                            ),
                        });
                    }
                }
            }
        }

        // payments: mock verifier must never be used in production.
        if self.payments.enabled && self.payments.verifier == VerifierChoice::Mock {
            return Err(ConfigError {
                key: "payments.verifier",
                message: "is \"mock\" but payments.enabled = true in production. \
                          The mock verifier performs no on-chain checks and must never be \
                          used in production. Set payments.verifier = \"facilitator\" and \
                          provide a valid payments.facilitator_url."
                    .to_owned(),
            });
        }

        // payments: facilitator verifier needs a URL.
        if self.payments.enabled && self.payments.verifier == VerifierChoice::Facilitator {
            let url_missing = self
                .payments
                .facilitator_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .is_none();
            if url_missing {
                return Err(ConfigError {
                    key: "payments.facilitator_url",
                    message: "is required when payments.enabled = true and \
                              payments.verifier = \"facilitator\". \
                              Set MCS_PAYMENTS__FACILITATOR_URL or \
                              payments.facilitator_url in config.toml."
                        .to_owned(),
                });
            }
        }

        // cors: allow_any_origin is a dev-only escape hatch.
        if self.cors.allow_any_origin {
            return Err(ConfigError {
                key: "cors.allow_any_origin",
                message: "must not be true in production. This wildcard CORS setting \
                          allows any origin to make cross-origin requests and must only \
                          be used during development. List specific origins in \
                          cors.allowed_origins instead."
                    .to_owned(),
            });
        }

        Ok(())
    }

    /// Loads configuration by layering defaults, an optional `config.toml`, and
    /// `MCS_`-prefixed environment variables (highest precedence).
    ///
    /// The TOML file is optional: if no file exists at the resolved path the
    /// layer contributes nothing. The path is `config.toml` in the working
    /// directory unless overridden by the `MCS_CONFIG` environment variable.
    ///
    /// # Errors
    ///
    /// Returns an error if a present `config.toml` is malformed or if any value
    /// (file or environment) fails to deserialize into [`Config`].
    pub fn load() -> anyhow::Result<Self> {
        let config_path =
            std::env::var(CONFIG_PATH_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_FILE.to_owned());

        let config = Figment::from(Serialized::defaults(Config::default()))
            // `Toml::file` is a no-op when the file is absent, so a missing
            // `config.toml` is not an error — defaults and env still apply.
            .merge(Toml::file(config_path))
            // `MCS_BIND`, `MCS_DATABASE_URL`, `MCS_SESSION__SECRET`, ...
            .merge(Env::prefixed(ENV_PREFIX).split("__"))
            .extract()?;

        Ok(config)
    }

    /// The configured log output format, as the observability crate's type.
    #[must_use]
    pub fn log_format(&self) -> LogFormat {
        self.log.format.into()
    }

    /// The session-token time-to-live as a [`time::Duration`].
    #[must_use]
    pub fn session_ttl(&self) -> Duration {
        Duration::seconds(i64::try_from(self.session.ttl_secs).unwrap_or(i64::MAX))
    }

    /// The SIWE nonce time-to-live as a [`time::Duration`].
    #[must_use]
    pub fn nonce_ttl(&self) -> Duration {
        Duration::seconds(i64::try_from(self.siwe.nonce_ttl_secs).unwrap_or(i64::MAX))
    }
}

impl PaymentSettings {
    /// Builds the x402 [`PaymentRequirements`] advertised in `402` bodies from
    /// these settings.
    ///
    /// `resource` is the path the gate protects (`/seeks`); it is echoed back to
    /// the client so it knows which request the payment unlocks.
    #[must_use]
    pub fn requirements(&self, resource: &str) -> mcs_payments::PaymentRequirements {
        mcs_payments::PaymentRequirements {
            scheme: self.scheme.clone(),
            network: self.network.clone(),
            max_amount_required: self.max_amount_required.clone(),
            resource: resource.to_owned(),
            description: self.description.clone(),
            mime_type: "application/json".to_owned(),
            pay_to: self.pay_to.clone(),
            max_timeout_seconds: self.max_timeout_seconds,
            asset: self.asset.clone(),
            extra: None,
        }
    }

    /// Constructs the shared [`PaymentVerifier`](mcs_payments::PaymentVerifier)
    /// selected by [`verifier`](PaymentSettings::verifier).
    ///
    /// - [`VerifierChoice::Mock`] returns a
    ///   [`MockVerifier`](mcs_payments::MockVerifier) — development/test only,
    ///   performing no on-chain checks.
    /// - [`VerifierChoice::Facilitator`] returns a
    ///   [`FacilitatorVerifier`](mcs_payments::FacilitatorVerifier) targeting
    ///   [`facilitator_url`](Self::facilitator_url), authenticated with
    ///   [`facilitator_api_key`](Self::facilitator_api_key) when present.
    ///
    /// # Errors
    ///
    /// Returns an error if [`verifier`](Self::verifier) is
    /// [`VerifierChoice::Facilitator`] but
    /// [`facilitator_url`](Self::facilitator_url) is unset or blank — a clear
    /// signal to the operator rather than a silently mock-backed gate.
    pub fn build_verifier(
        &self,
    ) -> anyhow::Result<std::sync::Arc<dyn mcs_payments::PaymentVerifier>> {
        match self.verifier {
            VerifierChoice::Mock => Ok(std::sync::Arc::new(mcs_payments::MockVerifier)),
            VerifierChoice::Facilitator => {
                let url = self
                    .facilitator_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "payments.verifier is \"facilitator\" but \
                             payments.facilitator_url is not set"
                        )
                    })?;
                let verifier = match self.facilitator_api_key.as_deref() {
                    Some(key) if !key.is_empty() => {
                        mcs_payments::FacilitatorVerifier::with_api_key(url, key)
                    }
                    _ => mcs_payments::FacilitatorVerifier::new(url),
                };
                Ok(std::sync::Arc::new(verifier))
            }
        }
    }
}

impl ClusterSettings {
    /// Resolves this node's [`NodeInfo`](mcs_cluster::NodeInfo) from the settings.
    ///
    /// If [`node_id`](Self::node_id) is empty a fresh UUID is generated so the
    /// node still has a unique, stable-for-this-process identity; otherwise the
    /// configured id is used verbatim. The [`address`](Self::address) is carried
    /// through unchanged.
    #[must_use]
    pub fn node_info(&self) -> mcs_cluster::NodeInfo {
        let id = if self.node_id.trim().is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            self.node_id.clone()
        };
        mcs_cluster::NodeInfo::new(id, self.address.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cfg = Config::default();
        assert_eq!(cfg.bind.port(), 8080);
        assert!(cfg.database_url.starts_with("sqlite://"));
        assert_eq!(cfg.session.issuer, "mcs");
        assert!(cfg.session.secret.is_none());
        assert_eq!(cfg.siwe.chain_id, 1);
    }

    // ── RunMode / env key tests (#102) ───────────────────────────────────────

    #[test]
    fn env_defaults_to_development() {
        let cfg = Config::default();
        assert_eq!(
            cfg.env,
            RunMode::Development,
            "env must default to development"
        );
    }

    /// The `env` key parses correctly from TOML.
    #[allow(clippy::result_large_err)]
    #[test]
    fn env_key_parses_production_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", r#"env = "production""#)?;
            let cfg = Config::load().expect("load config with env = production");
            assert_eq!(cfg.env, RunMode::Production);
            Ok(())
        });
    }

    /// The `env` key parses `development` explicitly from TOML.
    #[allow(clippy::result_large_err)]
    #[test]
    fn env_key_parses_development_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", r#"env = "development""#)?;
            let cfg = Config::load().expect("load config with env = development");
            assert_eq!(cfg.env, RunMode::Development);
            Ok(())
        });
    }

    /// `MCS_ENV` overrides the TOML value.
    #[allow(clippy::result_large_err)]
    #[test]
    fn env_key_set_via_env_var() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("MCS_ENV", "production");
            let cfg = Config::load().expect("load config with MCS_ENV");
            assert_eq!(cfg.env, RunMode::Production);
            Ok(())
        });
    }

    // ── Config::validate — both-mode (well-formedness) tests ─────────────────

    #[test]
    fn validate_passes_with_default_config() {
        let cfg = Config::default();
        // Default config is development mode: should always pass.
        assert!(
            cfg.validate().is_ok(),
            "default config must pass validation in development mode"
        );
    }

    #[test]
    fn validate_fails_on_zero_session_ttl() {
        let cfg = Config {
            session: SessionSettings {
                ttl_secs: 0,
                ..SessionSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("zero session ttl must fail validation");
        assert_eq!(err.key, "session.ttl_secs");
    }

    #[test]
    fn validate_fails_on_zero_nonce_ttl() {
        let cfg = Config {
            siwe: SiweSettings {
                nonce_ttl_secs: 0,
                ..SiweSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("zero nonce ttl must fail validation");
        assert_eq!(err.key, "siwe.nonce_ttl_secs");
    }

    #[test]
    fn validate_fails_on_zero_request_timeout() {
        let cfg = Config {
            http: HttpSettings {
                request_timeout_secs: 0,
                ..HttpSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("zero request timeout must fail validation");
        assert_eq!(err.key, "http.request_timeout_secs");
    }

    // ── Config::validate — production mode tests ──────────────────────────────

    /// Convenience: a strong production secret string used across several tests.
    fn strong_secret() -> String {
        "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_owned()
    }

    /// A fully valid production config passes.
    #[test]
    fn validate_production_valid_config_passes() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some(strong_secret()),
                ..SessionSettings::default()
            },
            // Payments disabled by default: no extra requirements.
            ..Config::default()
        };
        assert!(
            cfg.validate().is_ok(),
            "valid production config must pass validation"
        );
    }

    /// Production mode with missing session.secret → error naming the key.
    #[test]
    fn validate_production_missing_secret_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: None,
                ..SessionSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("missing secret must fail in production");
        assert_eq!(
            err.key, "session.secret",
            "error must name session.secret; got key {:?}, message: {}",
            err.key, err.message
        );
    }

    /// Production mode with an empty session.secret → error naming the key.
    #[test]
    fn validate_production_empty_secret_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some(String::new()),
                ..SessionSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("empty secret must fail in production");
        assert_eq!(err.key, "session.secret");
    }

    /// Production mode with a secret shorter than MIN_SECRET_LEN → error.
    #[test]
    fn validate_production_short_secret_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                // 16 bytes — below the 32-byte minimum.
                secret: Some("a1b2c3d4e5f6a1b2".to_owned()),
                ..SessionSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("short secret must fail in production");
        assert_eq!(err.key, "session.secret");
        assert!(
            err.message.contains("too short"),
            "error message should mention length; got: {}",
            err.message
        );
    }

    /// A secret that contains a placeholder substring is rejected in production.
    #[test]
    fn validate_production_placeholder_secret_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                // >= 32 bytes but contains "change-me".
                secret: Some("change-me-to-a-long-random-string-here".to_owned()),
                ..SessionSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("placeholder secret must fail in production");
        assert_eq!(err.key, "session.secret");
        assert!(
            err.message.contains("placeholder"),
            "error must mention placeholder; got: {}",
            err.message
        );
    }

    /// A secret that contains "secret" (case-insensitive) is rejected.
    #[test]
    fn validate_production_weak_secret_substring_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some("my-super-secret-key-that-is-very-long-indeed".to_owned()),
                ..SessionSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("secret containing 'secret' must fail in production");
        assert_eq!(err.key, "session.secret");
    }

    /// Production mode with payments enabled + mock verifier → error.
    #[test]
    fn validate_production_payments_enabled_mock_verifier_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some(strong_secret()),
                ..SessionSettings::default()
            },
            payments: PaymentSettings {
                enabled: true,
                verifier: VerifierChoice::Mock,
                ..PaymentSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("mock verifier + payments in production must fail");
        assert_eq!(
            err.key, "payments.verifier",
            "error must name payments.verifier; got: {:?}",
            err.key
        );
    }

    /// Production mode with payments enabled, facilitator verifier, but no URL → error.
    #[test]
    fn validate_production_payments_facilitator_without_url_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some(strong_secret()),
                ..SessionSettings::default()
            },
            payments: PaymentSettings {
                enabled: true,
                verifier: VerifierChoice::Facilitator,
                facilitator_url: None,
                ..PaymentSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("facilitator verifier without url must fail in production");
        assert_eq!(
            err.key, "payments.facilitator_url",
            "error must name payments.facilitator_url; got: {:?}",
            err.key
        );
    }

    /// Production mode with payments enabled, facilitator verifier, blank URL → error.
    #[test]
    fn validate_production_payments_facilitator_blank_url_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some(strong_secret()),
                ..SessionSettings::default()
            },
            payments: PaymentSettings {
                enabled: true,
                verifier: VerifierChoice::Facilitator,
                facilitator_url: Some("   ".to_owned()),
                ..PaymentSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("blank facilitator url must fail in production");
        assert_eq!(err.key, "payments.facilitator_url");
    }

    /// Production mode with payments enabled, facilitator verifier, valid URL → ok.
    #[test]
    fn validate_production_payments_facilitator_with_url_passes() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some(strong_secret()),
                ..SessionSettings::default()
            },
            payments: PaymentSettings {
                enabled: true,
                verifier: VerifierChoice::Facilitator,
                facilitator_url: Some("https://facilitator.example.com".to_owned()),
                ..PaymentSettings::default()
            },
            ..Config::default()
        };
        assert!(
            cfg.validate().is_ok(),
            "valid facilitator payment config must pass production validation"
        );
    }

    /// Production mode with allow_any_origin = true → error.
    #[test]
    fn validate_production_allow_any_origin_fails() {
        let cfg = Config {
            env: RunMode::Production,
            session: SessionSettings {
                secret: Some(strong_secret()),
                ..SessionSettings::default()
            },
            cors: CorsSettings {
                allow_any_origin: true,
                ..CorsSettings::default()
            },
            ..Config::default()
        };
        let err = cfg
            .validate()
            .expect_err("allow_any_origin in production must fail");
        assert_eq!(
            err.key, "cors.allow_any_origin",
            "error must name cors.allow_any_origin; got: {:?}",
            err.key
        );
    }

    /// Development mode ignores production-only rules: missing secret is ok.
    #[test]
    fn validate_development_missing_secret_passes() {
        // Default config is already development mode with no secret.
        let cfg = Config::default();
        assert!(
            cfg.validate().is_ok(),
            "missing secret must be allowed in development mode"
        );
    }

    /// Development mode: payments enabled + mock verifier is allowed.
    #[test]
    fn validate_development_payments_mock_verifier_passes() {
        let cfg = Config {
            payments: PaymentSettings {
                enabled: true,
                verifier: VerifierChoice::Mock,
                ..PaymentSettings::default()
            },
            ..Config::default()
        };
        assert!(
            cfg.validate().is_ok(),
            "mock verifier with payments enabled must be allowed in development"
        );
    }

    /// Development mode: allow_any_origin is allowed.
    #[test]
    fn validate_development_allow_any_origin_passes() {
        let cfg = Config {
            cors: CorsSettings {
                allow_any_origin: true,
                ..CorsSettings::default()
            },
            ..Config::default()
        };
        assert!(
            cfg.validate().is_ok(),
            "allow_any_origin must be allowed in development mode"
        );
    }

    /// ConfigError Display output names the key and message.
    #[test]
    fn config_error_display_includes_key_and_message() {
        let err = ConfigError {
            key: "session.secret",
            message: "must be set in production".to_owned(),
        };
        let display = err.to_string();
        assert!(
            display.contains("session.secret"),
            "display must contain key"
        );
        assert!(
            display.contains("must be set in production"),
            "display must contain message"
        );
    }

    // ── HttpSettings tests (#99) ─────────────────────────────────────────────

    #[test]
    fn http_settings_defaults_are_sensible() {
        let cfg = Config::default();
        // 30-second default timeout.
        assert_eq!(cfg.http.request_timeout_secs, 30);
        // 64 KiB body limit.
        assert_eq!(cfg.http.max_body_bytes, 64 * 1024);
        // 1 MiB WS message limit.
        assert_eq!(cfg.http.max_ws_message_bytes, 1024 * 1024);
        // HSTS off by default (unsafe to send over plain HTTP).
        assert!(!cfg.http.hsts, "hsts must be off by default");
        // 1-year HSTS max-age when enabled.
        assert_eq!(cfg.http.hsts_max_age_secs, 365 * 24 * 60 * 60);
    }

    #[test]
    fn http_settings_hsts_header_value_when_disabled() {
        let settings = HttpSettings::default();
        assert!(
            settings.hsts_header_value().is_none(),
            "hsts is off by default; no header value should be produced"
        );
    }

    #[test]
    fn http_settings_hsts_header_value_when_enabled() {
        let settings = HttpSettings {
            hsts: true,
            hsts_max_age_secs: 31_536_000,
            ..HttpSettings::default()
        };
        let value = settings
            .hsts_header_value()
            .expect("hsts enabled must produce a value");
        assert!(
            value.contains("max-age=31536000"),
            "hsts header must contain max-age; got {value:?}"
        );
        assert!(
            value.contains("includeSubDomains"),
            "hsts header should include includeSubDomains; got {value:?}"
        );
    }

    #[test]
    fn http_settings_request_timeout_converts() {
        let settings = HttpSettings {
            request_timeout_secs: 60,
            ..HttpSettings::default()
        };
        assert_eq!(
            settings.request_timeout(),
            StdDuration::from_secs(60),
            "request_timeout() must return the configured seconds as a Duration"
        );
    }

    /// The `[http]` section parses from TOML, overriding only the keys given.
    #[allow(clippy::result_large_err)]
    #[test]
    fn http_section_parses_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                    [http]
                    request_timeout_secs = 60
                    max_body_bytes = 131072
                    max_ws_message_bytes = 2097152
                    hsts = true
                    hsts_max_age_secs = 63072000
                "#,
            )?;
            let cfg = Config::load().expect("load config with [http]");
            assert_eq!(cfg.http.request_timeout_secs, 60);
            assert_eq!(cfg.http.max_body_bytes, 131_072);
            assert_eq!(cfg.http.max_ws_message_bytes, 2_097_152);
            assert!(cfg.http.hsts);
            assert_eq!(cfg.http.hsts_max_age_secs, 63_072_000);
            // An unrelated section must not be disturbed.
            assert!(!cfg.payments.enabled);
            Ok(())
        });
    }

    // ── LimitsSettings tests (#100) ──────────────────────────────────────────

    #[test]
    fn limits_defaults_are_sensible() {
        let cfg = Config::default();
        assert_eq!(cfg.limits.nonce_per_minute, 10);
        assert_eq!(cfg.limits.verify_per_minute, 20);
        assert_eq!(cfg.limits.create_per_minute, 30);
        assert_eq!(cfg.limits.max_ws_connections, 10_000);
        assert_eq!(cfg.limits.max_ws_connections_per_user, 20);
        assert_eq!(cfg.limits.max_games_per_user, 50);
        assert!(
            cfg.limits.trusted_proxy_header.is_none(),
            "no proxy header is trusted by default"
        );
    }

    #[test]
    fn limits_to_config_mirrors_settings() {
        let cfg = Config::default();
        let api = cfg.limits.to_limits_config();
        assert_eq!(api.nonce.replenish_per_minute, 10);
        assert_eq!(api.verify.replenish_per_minute, 20);
        assert_eq!(api.create.replenish_per_minute, 30);
        assert_eq!(api.max_ws_connections, 10_000);
        assert_eq!(api.max_ws_connections_per_user, 20);
        assert_eq!(api.max_games_per_user, 50);
    }

    /// The default `[limits]` config matches the API layer's own defaults, so the
    /// two definitions never drift.
    #[test]
    fn limits_defaults_match_api_defaults() {
        let api_default = mcs_api::LimitsConfig::default();
        let from_cfg = LimitsSettings::default().to_limits_config();
        assert_eq!(
            from_cfg.nonce.replenish_per_minute,
            api_default.nonce.replenish_per_minute
        );
        assert_eq!(
            from_cfg.create.replenish_per_minute,
            api_default.create.replenish_per_minute
        );
        assert_eq!(from_cfg.max_ws_connections, api_default.max_ws_connections);
        assert_eq!(from_cfg.max_games_per_user, api_default.max_games_per_user);
    }

    /// The `[limits]` section parses from TOML, overriding only the keys given.
    #[allow(clippy::result_large_err)]
    #[test]
    fn limits_section_parses_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                    [limits]
                    nonce_per_minute = 5
                    verify_per_minute = 7
                    create_per_minute = 9
                    max_ws_connections = 100
                    max_ws_connections_per_user = 3
                    max_games_per_user = 11
                    trusted_proxy_header = "x-forwarded-for"
                "#,
            )?;
            let cfg = Config::load().expect("load config with [limits]");
            assert_eq!(cfg.limits.nonce_per_minute, 5);
            assert_eq!(cfg.limits.verify_per_minute, 7);
            assert_eq!(cfg.limits.create_per_minute, 9);
            assert_eq!(cfg.limits.max_ws_connections, 100);
            assert_eq!(cfg.limits.max_ws_connections_per_user, 3);
            assert_eq!(cfg.limits.max_games_per_user, 11);
            assert_eq!(
                cfg.limits.trusted_proxy_header.as_deref(),
                Some("x-forwarded-for")
            );
            // An untouched section keeps its default.
            assert!(!cfg.payments.enabled);
            Ok(())
        });
    }

    #[test]
    fn durations_convert_from_seconds() {
        let cfg = Config::default();
        assert_eq!(cfg.session_ttl(), Duration::seconds(86_400));
        assert_eq!(cfg.nonce_ttl(), Duration::seconds(600));
    }

    #[test]
    fn log_format_choice_maps_to_observability_enum() {
        assert_eq!(LogFormat::from(LogFormatChoice::Pretty), LogFormat::Pretty);
        assert_eq!(LogFormat::from(LogFormatChoice::Json), LogFormat::Json);
    }

    #[test]
    fn payments_are_disabled_by_default() {
        let cfg = Config::default();
        assert!(!cfg.payments.enabled, "payments must be off by default");
        assert_eq!(cfg.payments.scheme, "exact");
        assert_eq!(cfg.payments.verifier, VerifierChoice::Mock);
    }

    #[test]
    fn payment_settings_build_matching_requirements() {
        let cfg = Config::default();
        let reqs = cfg.payments.requirements("/seeks");
        assert_eq!(reqs.scheme, cfg.payments.scheme);
        assert_eq!(reqs.network, cfg.payments.network);
        assert_eq!(reqs.asset, cfg.payments.asset);
        assert_eq!(reqs.resource, "/seeks");
        assert_eq!(reqs.mime_type, "application/json");
    }

    #[test]
    fn mock_verifier_builds_without_a_url() {
        let cfg = Config::default();
        assert!(
            cfg.payments.build_verifier().is_ok(),
            "the default mock verifier needs no facilitator url"
        );
    }

    #[test]
    fn facilitator_verifier_builds_with_a_url() {
        let mut cfg = Config::default();
        cfg.payments.verifier = VerifierChoice::Facilitator;
        cfg.payments.facilitator_url = Some("https://facilitator.example.com".to_owned());
        assert!(
            cfg.payments.build_verifier().is_ok(),
            "a facilitator verifier with a url must build"
        );
    }

    #[test]
    fn facilitator_verifier_builds_with_url_and_api_key() {
        let mut cfg = Config::default();
        cfg.payments.verifier = VerifierChoice::Facilitator;
        cfg.payments.facilitator_url = Some("https://facilitator.example.com".to_owned());
        cfg.payments.facilitator_api_key = Some("secret-token".to_owned());
        assert!(cfg.payments.build_verifier().is_ok());
    }

    #[test]
    fn facilitator_verifier_errors_without_a_url() {
        let mut cfg = Config::default();
        cfg.payments.verifier = VerifierChoice::Facilitator;
        cfg.payments.facilitator_url = None;
        let err = match cfg.payments.build_verifier() {
            Err(err) => err,
            Ok(_) => panic!("missing facilitator_url must be an error"),
        };
        assert!(
            err.to_string().contains("facilitator_url"),
            "the error should name the missing key, got: {err}"
        );
    }

    #[test]
    fn facilitator_verifier_errors_on_a_blank_url() {
        let mut cfg = Config::default();
        cfg.payments.verifier = VerifierChoice::Facilitator;
        cfg.payments.facilitator_url = Some("   ".to_owned());
        assert!(
            cfg.payments.build_verifier().is_err(),
            "a blank facilitator_url must be rejected like a missing one"
        );
    }

    /// The `[payments]` section parses the facilitator keys from TOML.
    #[allow(clippy::result_large_err)]
    #[test]
    fn payments_facilitator_keys_parse_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                    [payments]
                    enabled = true
                    verifier = "facilitator"
                    facilitator_url = "https://facilitator.example.com"
                    facilitator_api_key = "secret-token"
                "#,
            )?;
            let cfg = Config::load().expect("load config with facilitator [payments]");
            assert!(cfg.payments.enabled);
            assert_eq!(cfg.payments.verifier, VerifierChoice::Facilitator);
            assert_eq!(
                cfg.payments.facilitator_url.as_deref(),
                Some("https://facilitator.example.com")
            );
            assert_eq!(
                cfg.payments.facilitator_api_key.as_deref(),
                Some("secret-token")
            );
            assert!(cfg.payments.build_verifier().is_ok());
            Ok(())
        });
    }

    #[test]
    fn cluster_is_disabled_by_default() {
        let cfg = Config::default();
        assert!(!cfg.cluster.enabled, "cluster must be off by default");
        // Empty id ⇒ a UUID is minted; non-default TTLs are sensible.
        assert!(cfg.cluster.node_id.is_empty());
        assert_eq!(cfg.cluster.heartbeat_ttl_secs, 15);
        assert_eq!(cfg.cluster.heartbeat_interval_secs, 5);
        assert!(cfg.cluster.redis_url.starts_with("redis://"));
    }

    #[test]
    fn cluster_node_info_generates_an_id_when_unset() {
        let cfg = Config::default();
        let info = cfg.cluster.node_info();
        assert!(!info.id.as_str().is_empty(), "an id must be generated");
        assert_eq!(info.address, cfg.cluster.address);

        // Two resolutions of an empty id yield distinct generated ids.
        assert_ne!(cfg.cluster.node_info().id, cfg.cluster.node_info().id);
    }

    #[test]
    fn cluster_node_info_uses_a_configured_id_verbatim() {
        let mut cfg = Config::default();
        cfg.cluster.node_id = "node-a".to_owned();
        cfg.cluster.address = "http://10.0.0.7:8080".to_owned();
        let info = cfg.cluster.node_info();
        assert_eq!(info.id.as_str(), "node-a");
        assert_eq!(info.address, "http://10.0.0.7:8080");
    }

    /// The `[cluster]` section parses from TOML, overriding only the keys given
    /// and leaving the rest at their defaults.
    // `Jail::expect_with` requires a closure returning `figment::Result`, whose
    // `Err` is large; it is a test fixture, not a hot path, so allow it.
    #[allow(clippy::result_large_err)]
    #[test]
    fn cluster_section_parses_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                    [cluster]
                    enabled = true
                    node_id = "node-b"
                    address = "http://10.0.0.8:8080"
                    redis_url = "redis://redis:6379"
                    heartbeat_ttl_secs = 30
                    heartbeat_interval_secs = 10
                "#,
            )?;
            let cfg = Config::load().expect("load config with [cluster]");
            assert!(cfg.cluster.enabled);
            assert_eq!(cfg.cluster.node_id, "node-b");
            assert_eq!(cfg.cluster.address, "http://10.0.0.8:8080");
            assert_eq!(cfg.cluster.redis_url, "redis://redis:6379");
            assert_eq!(cfg.cluster.heartbeat_ttl_secs, 30);
            assert_eq!(cfg.cluster.heartbeat_interval_secs, 10);
            // An untouched section keeps its default.
            assert!(!cfg.payments.enabled);
            Ok(())
        });
    }

    // ── Database / pool config tests (#105) ──────────────────────────────────

    #[test]
    fn database_defaults_mirror_pool_config_default() {
        let cfg = Config::default();
        assert_eq!(cfg.database.max_connections, 10);
        assert_eq!(cfg.database.acquire_timeout_secs, 30);
        assert_eq!(cfg.database.idle_timeout_secs, 600);
        assert_eq!(cfg.database.max_lifetime_secs, 0);
        assert_eq!(cfg.database.statement_timeout_secs, 0);

        // The translated pool config must match the storage crate's default so
        // the two never drift.
        assert_eq!(
            cfg.database.to_pool_config(),
            mcs_storage::PoolConfig::default()
        );
    }

    #[test]
    fn database_to_pool_config_maps_zero_to_none() {
        let settings = DatabaseSettings {
            max_connections: 25,
            acquire_timeout_secs: 5,
            idle_timeout_secs: 0,
            max_lifetime_secs: 1800,
            statement_timeout_secs: 0,
        };
        let pool = settings.to_pool_config();
        assert_eq!(pool.max_connections, 25);
        assert_eq!(pool.acquire_timeout, std::time::Duration::from_secs(5));
        // 0 disables the optional timeouts.
        assert_eq!(pool.idle_timeout, None);
        assert_eq!(pool.statement_timeout, None);
        // A non-zero optional value is carried through.
        assert_eq!(
            pool.max_lifetime,
            Some(std::time::Duration::from_secs(1800))
        );
    }

    #[allow(clippy::result_large_err)]
    #[test]
    fn database_section_parses_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                    [database]
                    max_connections = 50
                    acquire_timeout_secs = 15
                    idle_timeout_secs = 120
                    max_lifetime_secs = 1800
                    statement_timeout_secs = 30
                "#,
            )?;
            let cfg = Config::load().expect("load config with [database]");
            assert_eq!(cfg.database.max_connections, 50);
            assert_eq!(cfg.database.acquire_timeout_secs, 15);
            assert_eq!(cfg.database.idle_timeout_secs, 120);
            assert_eq!(cfg.database.max_lifetime_secs, 1800);
            assert_eq!(cfg.database.statement_timeout_secs, 30);
            Ok(())
        });
    }

    #[allow(clippy::result_large_err)]
    #[test]
    fn database_max_connections_overrides_via_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("MCS_DATABASE__MAX_CONNECTIONS", "42");
            let cfg = Config::load().expect("load config with MCS_DATABASE__MAX_CONNECTIONS");
            assert_eq!(cfg.database.max_connections, 42);
            // Untouched keys keep their defaults.
            assert_eq!(cfg.database.acquire_timeout_secs, 30);
            Ok(())
        });
    }
}
