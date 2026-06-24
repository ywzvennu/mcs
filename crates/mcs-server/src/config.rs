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

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            database_url: "sqlite://mcs.db?mode=rwc".to_owned(),
            log: LogConfig::default(),
            session: SessionSettings::default(),
            siwe: SiweSettings::default(),
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
}
