//! Shared application state injected into every handler.
//!
//! [`AppState`] is the single dependency container threaded through the axum
//! router via [`axum::extract::State`]. It is cheap to clone — every field is
//! either an [`Arc`] or a small, cloneable config value — so axum can hand a
//! fresh copy to each request without contention.

use std::sync::Arc;

use mcs_auth::SessionConfig;
use mcs_storage::Repositories;
use time::Duration;

use crate::hub::GameHub;

/// Configuration for the Sign-In with Ethereum (EIP-4361) challenge that the
/// server hands to wallets at `GET /auth/nonce`.
///
/// These values are fixed for a deployment and bind every issued challenge to
/// this server's identity (`domain` / `uri`) and target chain (`chain_id`),
/// while `nonce_ttl` bounds how long a freshly minted nonce stays valid.
#[derive(Debug, Clone)]
pub struct SiweConfig {
    /// The RFC 3986 authority requesting the sign-in, e.g. `"chess.example"`
    /// or `"localhost:8080"`. Embedded verbatim in the SIWE message `domain`.
    pub domain: String,
    /// The RFC 3986 URI of the resource being signed into, e.g.
    /// `"https://chess.example"`. Embedded as the SIWE message `uri`.
    pub uri: String,
    /// The EIP-155 chain ID the session is bound to (`1` = Ethereum mainnet).
    pub chain_id: u64,
    /// The human-readable statement shown to the user in their wallet, e.g.
    /// `"Sign in to MCS."`. Must not contain a newline.
    pub statement: String,
    /// How long a freshly issued nonce remains valid. After this window the
    /// stored nonce is rejected by `consume_nonce`, forcing the client to
    /// request a new challenge. Typically 5–15 minutes.
    pub nonce_ttl: Duration,
}

impl SiweConfig {
    /// Creates a new SIWE challenge configuration.
    #[must_use]
    pub fn new(
        domain: String,
        uri: String,
        chain_id: u64,
        statement: String,
        nonce_ttl: Duration,
    ) -> Self {
        Self {
            domain,
            uri,
            chain_id,
            statement,
            nonce_ttl,
        }
    }
}

/// The shared, cloneable state for the MCS HTTP API.
///
/// A single instance is built at start-up and passed to [`crate::router`];
/// axum clones it per request. All heavy or shared state lives behind an
/// [`Arc`], so a clone is just a handful of atomic reference-count bumps.
///
/// # Storage handle
///
/// Storage is held as `Arc<dyn Repositories>` rather than a concrete type so
/// the API layer stays backend-agnostic: production wires in `SqlxStorage`
/// while tests inject an in-memory or SQLite implementation. The trait is
/// object-safe by design (see [`mcs_storage::Repositories`]).
///
/// # Live-game hub (#14 / #15)
///
/// The [`game_hub`](AppState::game_hub) is the shared, in-memory registry of
/// running games (the [`mcs_game`] actors and their broadcast channels). The
/// REST creation endpoints (#14) spawn a game's actor and insert its handle
/// here; the WebSocket endpoint (#15, see [`crate::ws`]) looks the handle up by
/// id so a connecting client can stream and submit moves. The hub is itself
/// cloneable (it shares an [`Arc`] internally), so adding it keeps `AppState`
/// cheap to clone and the [`AuthUser`](crate::AuthUser) extractor unaffected.
#[derive(Clone)]
pub struct AppState {
    storage: Arc<dyn Repositories>,
    session_config: SessionConfig,
    siwe_config: SiweConfig,
    game_hub: GameHub,
}

impl AppState {
    /// Builds the application state from its dependencies.
    ///
    /// * `storage` — the repository aggregate, already wrapped in an [`Arc`].
    /// * `session_config` — JWT signing/verification parameters, shared by the
    ///   `/auth/verify` handler (issuance) and the [`AuthUser`](crate::AuthUser)
    ///   extractor (verification).
    /// * `siwe_config` — the SIWE challenge parameters for `/auth/nonce`.
    ///
    /// The [`game_hub`](AppState::game_hub) starts empty; games are inserted as
    /// they are created.
    #[must_use]
    pub fn new(
        storage: Arc<dyn Repositories>,
        session_config: SessionConfig,
        siwe_config: SiweConfig,
    ) -> Self {
        Self {
            storage,
            session_config,
            siwe_config,
            game_hub: GameHub::new(),
        }
    }

    /// Returns the shared storage handle.
    #[must_use]
    pub fn storage(&self) -> &Arc<dyn Repositories> {
        &self.storage
    }

    /// Returns the JWT session configuration.
    #[must_use]
    pub fn session_config(&self) -> &SessionConfig {
        &self.session_config
    }

    /// Returns the SIWE challenge configuration.
    #[must_use]
    pub fn siwe_config(&self) -> &SiweConfig {
        &self.siwe_config
    }

    /// Returns the live-game hub: the registry of running game actors.
    ///
    /// The REST creation endpoints (#14) insert newly created games here; the
    /// WebSocket endpoint (#15) resolves a game's [`GameHandle`](mcs_game::GameHandle)
    /// from it. The returned reference borrows a cheaply cloneable handle to the
    /// shared registry.
    #[must_use]
    pub fn game_hub(&self) -> &GameHub {
        &self.game_hub
    }
}

// `SessionConfig` deliberately has no `Debug` derive that exposes the secret
// (it redacts it), and `Arc<dyn Repositories>` is not `Debug`. Provide a manual,
// secret-free `Debug` so the workspace `missing_debug_implementations` lint is
// satisfied without ever printing storage internals or the signing key.
impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("storage", &"<dyn Repositories>")
            .field("session_config", &self.session_config)
            .field("siwe_config", &self.siwe_config)
            .field("game_hub", &self.game_hub)
            .finish()
    }
}
