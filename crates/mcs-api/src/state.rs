//! Shared application state injected into every handler.
//!
//! [`AppState`] is the single dependency container threaded through the axum
//! router via [`axum::extract::State`]. It is cheap to clone — every field is
//! either an [`Arc`] or a small, cloneable config value — so axum can hand a
//! fresh copy to each request without contention.

use std::sync::Arc;

use mcs_auth::SessionConfig;
use mcs_core::VariantRegistry;
use mcs_game::{GameCompletionHook, Matchmaker};
use mcs_payments::{PaymentRequirements, PaymentVerifier};
use mcs_storage::{GameRepo, RatingRepo, Repositories, SeekRepo, UserRepo};
use time::Duration;

use crate::hub::GameHub;
use crate::rating::RatingUpdateHook;

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

/// An optional x402 payment gate for game creation (#45).
///
/// When present in [`AppState`], the router applies a
/// [`RequirePaymentLayer`](mcs_payments::RequirePaymentLayer) to the
/// `POST /seeks` creation route only (see [`crate::rest::seek_router`]), so a
/// caller must settle a payment before a seek is queued or paired into a game.
/// When absent (the default), `POST /seeks` is free and the router behaves
/// exactly as it did before payments were introduced.
///
/// This is the project's hook for charging per game: per the roadmap, RBC game
/// creation is the resource that would be priced here, but the gate is variant-
/// agnostic — it simply wraps the one creation route.
#[derive(Clone)]
pub struct PaymentGate {
    /// The payment terms advertised in every `402 Payment Required` body. The
    /// layer accepts a list so a deployment can offer multiple schemes/networks;
    /// the config-driven path builds a single entry.
    requirements: Vec<PaymentRequirements>,
    /// The shared verifier. Development uses
    /// [`MockVerifier`](mcs_payments::MockVerifier); production supplies a real
    /// facilitator-backed [`PaymentVerifier`].
    verifier: Arc<dyn PaymentVerifier>,
}

impl PaymentGate {
    /// Builds a gate from the advertised `requirements` and a shared `verifier`.
    #[must_use]
    pub fn new(requirements: PaymentRequirements, verifier: Arc<dyn PaymentVerifier>) -> Self {
        Self {
            requirements: vec![requirements],
            verifier,
        }
    }

    /// Returns the advertised payment terms (sent in `402` bodies).
    #[must_use]
    pub fn requirements(&self) -> &[PaymentRequirements] {
        &self.requirements
    }

    /// Returns the shared payment verifier.
    #[must_use]
    pub fn verifier(&self) -> &Arc<dyn PaymentVerifier> {
        &self.verifier
    }
}

impl std::fmt::Debug for PaymentGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentGate")
            .field("requirements", &self.requirements)
            .finish_non_exhaustive()
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
///
/// # Game-creation dependencies (#14)
///
/// The REST seek/game endpoints need three further pieces of shared state that
/// the [`Repositories`] aggregate cannot hand out directly (it only lends
/// `&dyn` references, while both the matchmaker and the actor need owned
/// `Arc<dyn _>` trait objects):
///
/// - [`variants`](AppState::variants) — the [`VariantRegistry`] used to
///   instantiate a fresh [`GameSession`](mcs_core::GameSession) for a paired
///   seek. It is populated **by the caller** at start-up (the server registers
///   `mcs-variant-standard`; tests register it themselves), which keeps this
///   crate free of a runtime dependency on any concrete variant.
/// - [`matchmaker`](AppState::matchmaker) — the [`Matchmaker`] that pools open
///   seeks and pairs compatible ones, built from an `Arc<dyn SeekRepo>`.
/// - [`game_repo`](AppState::game_repo) — the `Arc<dyn GameRepo>` handed to
///   each spawned [`GameActor`](mcs_game::GameActor) so it can persist results.
#[derive(Clone)]
pub struct AppState {
    storage: Arc<dyn Repositories>,
    session_config: SessionConfig,
    siwe_config: SiweConfig,
    game_hub: GameHub,
    variants: Arc<VariantRegistry>,
    matchmaker: Arc<Matchmaker>,
    game_repo: Arc<dyn GameRepo>,
    /// The completion hook handed to every spawned [`GameActor`](mcs_game::GameActor):
    /// on game end it applies the Glicko-2 rating update for both players. Held
    /// as `Arc<dyn GameCompletionHook>` so the actor stays decoupled from the
    /// concrete [`RatingUpdateHook`].
    completion_hook: Arc<dyn GameCompletionHook>,
    /// The optional x402 payment gate for game creation (#45).
    ///
    /// `None` by default — `POST /seeks` is free and the router is unchanged.
    /// When `Some`, [`crate::router`] wraps only the `POST /seeks` route in a
    /// [`RequirePaymentLayer`](mcs_payments::RequirePaymentLayer). Configure it
    /// with [`with_payment`](AppState::with_payment).
    payment_gate: Option<PaymentGate>,
}

impl AppState {
    /// Builds the application state from a single storage handle plus
    /// configuration.
    ///
    /// `storage` is taken as a concrete `Arc<S>` whose type implements every
    /// repository trait the API needs ([`Repositories`] for the existing
    /// handlers, plus [`SeekRepo`] for the matchmaker and [`GameRepo`] for actor
    /// spawning). The trait-object handles are derived internally by cloning the
    /// same `Arc` and coercing it independently, so all of them share one
    /// backing store — exactly the property the live-game path relies on, where
    /// the API reads through `Arc<dyn Repositories>` and an actor persists
    /// through `Arc<dyn GameRepo>` over the very same database.
    ///
    /// * `storage` — the backing store, implementing all repository traits.
    /// * `variants` — the registry of game variants, pre-populated by the
    ///   caller (the server registers `mcs-variant-standard`; tests do the
    ///   same). Held behind an [`Arc`] so the clone stays cheap.
    /// * `session_config` — JWT signing/verification parameters, shared by the
    ///   `/auth/verify` handler (issuance) and the [`AuthUser`](crate::AuthUser)
    ///   extractor (verification).
    /// * `siwe_config` — the SIWE challenge parameters for `/auth/nonce`.
    ///
    /// The [`game_hub`](AppState::game_hub) starts empty; games are inserted as
    /// they are created.
    #[must_use]
    pub fn new<S>(
        storage: Arc<S>,
        variants: Arc<VariantRegistry>,
        session_config: SessionConfig,
        siwe_config: SiweConfig,
    ) -> Self
    where
        S: Repositories + GameRepo + SeekRepo + UserRepo + RatingRepo + 'static,
    {
        // Coerce the one concrete `Arc<S>` into each trait object the layers
        // need. Every coercion shares the same allocation, so all handles read
        // and write one backing store.
        let repositories: Arc<dyn Repositories> = storage.clone();
        let seek_repo: Arc<dyn SeekRepo> = storage.clone();
        let rating_repo: Arc<dyn RatingRepo> = storage.clone();
        let game_repo: Arc<dyn GameRepo> = storage;

        // The rating updater is the game-completion hook: when an actor ends a
        // game, it recomputes both players' Glicko-2 ratings over this same
        // store. Holding it as the abstract trait keeps `mcs-game` decoupled.
        let completion_hook: Arc<dyn GameCompletionHook> =
            Arc::new(RatingUpdateHook::new(rating_repo));

        Self {
            storage: repositories,
            session_config,
            siwe_config,
            game_hub: GameHub::new(),
            variants,
            matchmaker: Arc::new(Matchmaker::new(seek_repo)),
            game_repo,
            completion_hook,
            // Payments are off by default: the router behaves exactly as before
            // until a caller opts in via `with_payment`.
            payment_gate: None,
        }
    }

    /// Enables the x402 payment gate on game creation, returning the modified
    /// state (builder style).
    ///
    /// This does **not** change the `AppState::new` signature: state is created
    /// payment-free, and a caller that wants gating chains `with_payment(..)`.
    /// Once set, [`crate::router`] wraps the `POST /seeks` route — and only that
    /// route — in a [`RequirePaymentLayer`](mcs_payments::RequirePaymentLayer):
    /// an unpaid request gets `402 Payment Required` (with the `requirements` in
    /// the body), and a request carrying a valid `X-PAYMENT` header proceeds to
    /// the handler. All other routes remain free.
    ///
    /// Ordering: the payment layer wraps the route, so it runs **before** the
    /// handler's [`AuthUser`](crate::AuthUser) extractor. An unpaid request is
    /// therefore answered with `402` whether or not it is authenticated; a paid
    /// request then still needs a valid session (`401` otherwise). This keeps
    /// the payment challenge cheap to serve and independent of auth state.
    ///
    /// * `requirements` — the payment terms advertised in `402` bodies.
    /// * `verifier` — the shared verifier; use
    ///   [`MockVerifier`](mcs_payments::MockVerifier) in development and a real
    ///   facilitator-backed [`PaymentVerifier`] in production.
    #[must_use]
    pub fn with_payment(
        mut self,
        requirements: PaymentRequirements,
        verifier: Arc<dyn PaymentVerifier>,
    ) -> Self {
        self.payment_gate = Some(PaymentGate::new(requirements, verifier));
        self
    }

    /// Returns the configured payment gate, if any.
    ///
    /// [`crate::router`] consults this to decide whether to wrap the creation
    /// route in the x402 layer. `None` means game creation is free.
    #[must_use]
    pub fn payment_gate(&self) -> Option<&PaymentGate> {
        self.payment_gate.as_ref()
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

    /// Returns the registry of game variants used to instantiate sessions.
    ///
    /// The seek/game-creation endpoints (#14) call
    /// [`new_game`](VariantRegistry::new_game) on it to build a fresh
    /// [`GameSession`](mcs_core::GameSession) for a paired seek.
    #[must_use]
    pub fn variants(&self) -> &Arc<VariantRegistry> {
        &self.variants
    }

    /// Returns the seek-pool matchmaker.
    ///
    /// `POST /seeks` submits to it and `DELETE /seeks/{id}` cancels through it.
    #[must_use]
    pub fn matchmaker(&self) -> &Arc<Matchmaker> {
        &self.matchmaker
    }

    /// Returns the game repository handle used to spawn game actors.
    ///
    /// When a seek pairs, the endpoint persists the new [`Game`](mcs_domain::Game)
    /// and hands a clone of this handle to
    /// [`GameActor::spawn`](mcs_game::GameActor::spawn) so the actor can record
    /// the result when play concludes.
    #[must_use]
    pub fn game_repo(&self) -> &Arc<dyn GameRepo> {
        &self.game_repo
    }

    /// Returns the game-completion hook handed to each spawned game actor.
    ///
    /// When a seek pairs, the endpoint passes a clone of this to
    /// [`GameActor::spawn`](mcs_game::GameActor::spawn); the actor invokes it once
    /// the game finishes, applying the post-game Glicko-2 rating update for both
    /// players over the same backing store the API reads.
    #[must_use]
    pub fn completion_hook(&self) -> &Arc<dyn GameCompletionHook> {
        &self.completion_hook
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
            .field("variants", &self.variants)
            .field("matchmaker", &self.matchmaker)
            .field("game_repo", &"<dyn GameRepo>")
            .field("completion_hook", &"<dyn GameCompletionHook>")
            .field("payment_gate", &self.payment_gate)
            .finish()
    }
}
