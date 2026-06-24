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
//! # Keys and defaults
//!
//! | Key                | Env var               | Default                       |
//! |--------------------|-----------------------|-------------------------------|
//! | `bind`             | `MCS_BIND`            | `127.0.0.1:8080`              |
//! | `database_url`     | `MCS_DATABASE_URL`   | `sqlite://mcs.db?mode=rwc`    |
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

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use mcs_observability::LogFormat;
use serde::{Deserialize, Serialize};
use time::Duration;

/// The environment-variable prefix for configuration overrides.
const ENV_PREFIX: &str = "MCS_";

/// The environment variable naming an alternate `config.toml` path.
const CONFIG_PATH_ENV: &str = "MCS_CONFIG";

/// The default config file consulted when `MCS_CONFIG` is unset.
const DEFAULT_CONFIG_FILE: &str = "config.toml";

/// Fully-resolved server configuration.
///
/// Build one with [`Config::load`], which layers defaults, an optional
/// `config.toml`, and `MCS_`-prefixed environment variables. Every field has a
/// default, so the type also implements [`Default`] for tests and tooling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The socket address the HTTP server binds to.
    pub bind: SocketAddr,
    /// The storage connection string handed to
    /// [`SqlxStorage::connect`](mcs_storage::SqlxStorage::connect).
    ///
    /// The default `sqlite://mcs.db?mode=rwc` keeps state in a single file in
    /// the working directory, creating it on first run. Use `sqlite::memory:`
    /// for an ephemeral, test-only database.
    pub database_url: String,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            database_url: "sqlite://mcs.db?mode=rwc".to_owned(),
            log: LogConfig::default(),
            session: SessionSettings::default(),
            siwe: SiweSettings::default(),
            payments: PaymentSettings::default(),
            cluster: ClusterSettings::default(),
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

impl Config {
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
}
