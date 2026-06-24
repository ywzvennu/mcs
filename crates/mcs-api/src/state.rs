//! Shared application state injected into every handler.
//!
//! [`AppState`] is the single dependency container threaded through the axum
//! router via [`axum::extract::State`]. It is cheap to clone — every field is
//! either an [`Arc`] or a small, cloneable config value — so axum can hand a
//! fresh copy to each request without contention.

use std::collections::HashMap;
use std::sync::Arc;

use mcs_auth::SessionConfig;
use mcs_cluster::{LocalRegistry, NodeInfo, NodeRegistry};
use mcs_core::{VariantOptions, VariantRegistry};
use mcs_domain::{Game, GameId, GameLifecycle, TimeControl, UserId};
use mcs_game::{recover_game, GameActor, GameCompletionHook, GameHandle, Matchmaker};
use mcs_payments::{PaymentRequirements, PaymentVerifier};
use mcs_storage::error::StorageError;
use mcs_storage::{ActionLogRepo, GameRepo, RatingRepo, Repositories, SeekRepo, UserRepo};
use time::{Duration, OffsetDateTime};
use tokio::sync::Mutex;

use crate::error::ApiError;
use crate::hub::GameHub;
use crate::presence::{InProcessPresence, PresenceTracker};
use crate::rating::RatingUpdateHook;
use crate::table::TableHub;

/// A serializer for the recover-and-insert critical section, keyed by [`GameId`].
///
/// `get_or_recover` may be called concurrently for the same absent game (two
/// clients connecting to a cold node at once). Rebuilding an actor is expensive
/// and — more importantly — must happen **exactly once**: a second actor would
/// double-record the action log and split the live game across two broadcast
/// channels. This map hands out one [`Mutex`] per game id so that, for a given
/// game, only one task at a time runs the load-recover-insert sequence; the
/// others wait, then observe the freshly inserted handle on their re-check of
/// the hub.
///
/// The outer mutex guards only the brief lookup/creation of a per-id lock and is
/// never held across recovery; the per-id lock is what is held across the
/// `.await`-heavy recovery. Both are [`tokio::sync::Mutex`]es, never a
/// [`std`] lock, so no guard is ever held across an `.await` in a way that could
/// block the runtime.
type RecoveryLocks = Arc<Mutex<HashMap<GameId, Arc<Mutex<()>>>>>;

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
    /// The per-game **table side-channel** registry (#84): the session-level
    /// mirror of [`game_hub`](Self::game_hub).
    ///
    /// Where the game hub holds each game's *board* actor, this holds each
    /// game's [`TableChannel`](crate::table::TableChannel) — the
    /// [`broadcast`](tokio::sync::broadcast) of non-board
    /// [`TableEvent`](crate::table::TableEvent)s (today, rematch offers and their
    /// answers) plus the pending rematch offer. The WebSocket endpoint
    /// subscribes a connection to this alongside the actor's `GameEvent` stream,
    /// and the rematch action handlers publish onto it. Cheap to clone (it shares
    /// an [`Arc`] internally), so it keeps `AppState` cheap to clone.
    table_hub: TableHub,
    variants: Arc<VariantRegistry>,
    matchmaker: Arc<Matchmaker>,
    game_repo: Arc<dyn GameRepo>,
    /// The append-only action log handed to every spawned
    /// [`GameActor`](mcs_game::GameActor): the actor records each applied move
    /// here and refreshes the live snapshot through [`game_repo`](Self::game_repo)
    /// as play proceeds. Held as `Arc<dyn ActionLogRepo>`, derived from the same
    /// backing store as every other handle.
    action_log: Arc<dyn ActionLogRepo>,
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
    /// The player online-presence tracker (#79).
    ///
    /// Records the last-seen instant for each user so the API can answer
    /// `GET /users/{id}/status` and annotate profiles with an `online` flag.
    /// Defaults to an [`InProcessPresence`] (per-node, in-memory); a
    /// Redis-backed implementation is the future cross-node upgrade path.
    /// Configure a non-default tracker via [`with_presence`](AppState::with_presence).
    presence: Arc<dyn PresenceTracker>,
    /// How long a user is considered online after their last authenticated
    /// request. Defaults to [`DEFAULT_ONLINE_TTL`]; override with
    /// [`with_presence`](AppState::with_presence).
    online_ttl: Duration,
    /// The cluster-routing setup (#68): the membership registry plus this node's
    /// identity, used by the WebSocket path to decide whether *this* node owns a
    /// game or should redirect the client to the owning node.
    ///
    /// Never `None`: state is created single-node, with a
    /// [`LocalRegistry`](mcs_cluster::LocalRegistry) over a synthetic local node
    /// whose [`live_nodes`](mcs_cluster::NodeRegistry::live_nodes) reports only
    /// itself — so [`owner`](mcs_cluster::owner) always resolves to this node and
    /// routing is a no-op, exactly the pre-cluster behaviour. A multi-node
    /// deployment swaps in a real registry via [`with_cluster`](AppState::with_cluster).
    cluster: Cluster,
    /// Per-game serialization for [`get_or_recover`](AppState::get_or_recover),
    /// so two concurrent connects to the same absent game rebuild its actor only
    /// once. See [`RecoveryLocks`]. Shared across clones via the inner [`Arc`].
    recovery_locks: RecoveryLocks,
    /// Maximum WebSocket message size, in bytes (#99).
    ///
    /// Set on each [`WebSocketUpgrade`](axum::extract::ws::WebSocketUpgrade)
    /// before the socket is upgraded so the axum runtime rejects oversized
    /// frames before they reach the application logic.
    ///
    /// Default: 1 MiB — matches the `[http].max_ws_message_bytes` default.
    ws_max_message_bytes: usize,
}

/// This node's membership view: how the WebSocket router decides game ownership.
///
/// Holds the shared [`NodeRegistry`] (the live-membership source) together with
/// this process's own [`NodeInfo`]. The router asks the registry for the live
/// set, computes the rendezvous [`owner`](mcs_cluster::owner) of a game id, and
/// compares it to [`this_node`](Cluster::this_node) to choose between *serve*
/// and *redirect*.
///
/// The single-node default wraps a [`LocalRegistry`], whose `live_nodes` returns
/// only this node — so every game resolves to this node and the router never
/// redirects.
#[derive(Clone)]
pub struct Cluster {
    /// The membership registry. Shared (`Arc`) so the state stays cheap to clone.
    registry: Arc<dyn NodeRegistry>,
    /// This process's identity and externally reachable address.
    this_node: NodeInfo,
}

impl Cluster {
    /// Returns the shared membership registry.
    #[must_use]
    pub fn registry(&self) -> &Arc<dyn NodeRegistry> {
        &self.registry
    }

    /// Returns this node's identity and address.
    #[must_use]
    pub fn this_node(&self) -> &NodeInfo {
        &self.this_node
    }
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn NodeRegistry>` is not `Debug`; summarize the identity instead.
        f.debug_struct("Cluster")
            .field("registry", &"<dyn NodeRegistry>")
            .field("this_node", &self.this_node)
            .finish()
    }
}

/// The synthetic node id used by the single-node default registry.
///
/// A deployment that never enables clustering still computes ownership through
/// the same code path; this fixed id labels the lone local node so
/// [`owner`](mcs_cluster::owner) has something to resolve to. The address is
/// irrelevant single-node (the router never redirects to it).
const LOCAL_NODE_ID: &str = "local";

/// Default TTL for the online-presence window.
///
/// A user is considered online if they have made an authenticated request —
/// REST or WebSocket — within this window. 30 seconds is the default; a
/// production deployment can override it via
/// [`AppState::with_presence`].
pub const DEFAULT_ONLINE_TTL: Duration = Duration::seconds(30);

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
        S: Repositories + GameRepo + SeekRepo + UserRepo + RatingRepo + ActionLogRepo + 'static,
    {
        // Coerce the one concrete `Arc<S>` into each trait object the layers
        // need. Every coercion shares the same allocation, so all handles read
        // and write one backing store.
        let repositories: Arc<dyn Repositories> = storage.clone();
        let seek_repo: Arc<dyn SeekRepo> = storage.clone();
        let rating_repo: Arc<dyn RatingRepo> = storage.clone();
        let action_log: Arc<dyn ActionLogRepo> = storage.clone();
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
            table_hub: TableHub::new(),
            variants,
            matchmaker: Arc::new(Matchmaker::new(seek_repo)),
            game_repo,
            action_log,
            completion_hook,
            // Payments are off by default: the router behaves exactly as before
            // until a caller opts in via `with_payment`.
            payment_gate: None,
            // Presence is on by default with an in-process tracker. A multi-node
            // deployment can swap in a cross-node implementation via `with_presence`.
            presence: Arc::new(InProcessPresence::new()),
            online_ttl: DEFAULT_ONLINE_TTL,
            // Single-node by default: a `LocalRegistry` over a synthetic local
            // node, so the WS router computes ownership through the same path a
            // cluster would but always resolves to *this* node — no redirect, no
            // Redis, byte-for-byte the pre-cluster behaviour. A multi-node
            // deployment overrides this via `with_cluster`.
            cluster: Cluster {
                this_node: NodeInfo::new(LOCAL_NODE_ID, ""),
                registry: Arc::new(LocalRegistry::new(NodeInfo::new(LOCAL_NODE_ID, ""))),
            },
            recovery_locks: Arc::new(Mutex::new(HashMap::new())),
            // 1 MiB default; matches `[http].max_ws_message_bytes` default.
            ws_max_message_bytes: 1024 * 1024,
        }
    }

    /// Enables cluster-aware WebSocket routing, returning the modified state
    /// (builder style).
    ///
    /// This does **not** change the [`AppState::new`] signature: state is created
    /// single-node (a [`LocalRegistry`] over a synthetic local node, so every
    /// game owns to this node and nothing is ever redirected), and a multi-node
    /// deployment chains `with_cluster(..)` to swap in real membership.
    ///
    /// Once set, the WS handler ([`crate::ws`]) consults `registry.live_nodes()`
    /// and the rendezvous [`owner`](mcs_cluster::owner) of the game id before
    /// upgrading: if `this_node` owns the game it is served locally (recovering
    /// the actor from the durable log on demand — the failover path); otherwise
    /// the client is told to reconnect to the owning node rather than upgraded.
    ///
    /// * `registry` — the shared membership source (e.g. a
    ///   [`RedisNodeRegistry`](mcs_cluster::RedisNodeRegistry), constructed and
    ///   registered by the server). The API holds it only as
    ///   `Arc<dyn NodeRegistry>`, so it links no backend itself.
    /// * `this_node` — this process's identity and externally reachable address,
    ///   the same `NodeInfo` registered with `registry`.
    #[must_use]
    pub fn with_cluster(mut self, registry: Arc<dyn NodeRegistry>, this_node: NodeInfo) -> Self {
        self.cluster = Cluster {
            registry,
            this_node,
        };
        self
    }

    /// Returns the cluster-routing setup (registry + this node's identity).
    ///
    /// The WebSocket handler uses this to decide between serving a game locally
    /// and redirecting to its owner. The single-node default always resolves
    /// ownership to this node, so the returned value is never `None`.
    #[must_use]
    pub fn cluster(&self) -> &Cluster {
        &self.cluster
    }

    /// Overrides the presence tracker and online TTL, returning the modified
    /// state (builder style).
    ///
    /// The default — installed automatically by [`AppState::new`] — is an
    /// [`InProcessPresence`] with a 30-second TTL.  Swap it here if you need
    /// either a shorter TTL (tests) or a cross-node implementation (production
    /// with Redis).
    ///
    /// * `tracker` — the new [`PresenceTracker`] implementation.
    /// * `ttl` — how long a user is considered online after their last seen
    ///   request.
    #[must_use]
    pub fn with_presence(mut self, tracker: Arc<dyn PresenceTracker>, ttl: Duration) -> Self {
        self.presence = tracker;
        self.online_ttl = ttl;
        self
    }

    /// Returns the shared presence tracker.
    ///
    /// Call [`PresenceTracker::mark_seen`] here after each authenticated
    /// request to keep the map fresh.
    #[must_use]
    pub fn presence(&self) -> &Arc<dyn PresenceTracker> {
        &self.presence
    }

    /// Returns the configured online-presence TTL.
    ///
    /// A user is considered online when their [`last_seen`](PresenceTracker::last_seen)
    /// instant is within this window of now. Defaults to
    /// [`DEFAULT_ONLINE_TTL`].
    #[must_use]
    pub fn online_ttl(&self) -> Duration {
        self.online_ttl
    }

    /// Overrides the WebSocket message-size limit, returning the modified state
    /// (builder style).
    ///
    /// The WS handler calls [`ws_max_message_bytes`](Self::ws_max_message_bytes)
    /// and applies the result to each [`WebSocketUpgrade`](axum::extract::ws::WebSocketUpgrade)
    /// before upgrading. The default (1 MiB) matches the `[http].max_ws_message_bytes`
    /// config default; `mcs-server`'s `build_state` sets this from config.
    #[must_use]
    pub fn with_ws_max_message_bytes(mut self, max_bytes: usize) -> Self {
        self.ws_max_message_bytes = max_bytes;
        self
    }

    /// Returns the maximum allowed WebSocket message size, in bytes.
    ///
    /// Applied to each upgraded WebSocket connection by the handler in
    /// [`crate::ws`] via `.max_message_size(state.ws_max_message_bytes())`.
    #[must_use]
    pub fn ws_max_message_bytes(&self) -> usize {
        self.ws_max_message_bytes
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

    /// Returns the table-side-channel hub (#84): the registry of per-game
    /// session-level [`TableChannel`](crate::table::TableChannel)s.
    ///
    /// The WebSocket endpoint ([`crate::ws`]) resolves a game's channel from it —
    /// creating it on first use — to subscribe a connection to the table's
    /// [`TableEvent`](crate::table::TableEvent) stream (rematch offers/answers)
    /// alongside the board `GameEvent` stream, and the rematch action handlers
    /// publish onto it. Cheap to clone; shares an [`Arc`] with this state.
    #[must_use]
    pub fn table_hub(&self) -> &TableHub {
        &self.table_hub
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

    /// Returns the action-log handle handed to each spawned game actor.
    ///
    /// When a seek pairs, the endpoint passes a clone of this to
    /// [`GameActor::spawn`](mcs_game::GameActor::spawn); the actor appends each
    /// applied move to it (and refreshes the live snapshot through
    /// [`game_repo`](Self::game_repo)) as play proceeds, building the durable
    /// move history a recovering server replays.
    #[must_use]
    pub fn action_log(&self) -> &Arc<dyn ActionLogRepo> {
        &self.action_log
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

    /// Creates, persists, spawns, and registers a live game for two named
    /// players, returning the persisted [`Game`] record.
    ///
    /// This is the shared game-creation path used by both matchmaking (a paired
    /// seek) and direct challenges (an accepted challenge): both ultimately need
    /// the same five steps, so they are written once here rather than duplicated.
    ///
    /// The steps are:
    ///
    /// 1. Instantiate a fresh [`GameSession`](mcs_core::GameSession) for
    ///    `variant_id` from the [`variants`](Self::variants) registry, built with
    ///    `options`. An unknown variant surfaces as a **400 Bad Request** via the
    ///    [`GameError`](mcs_core::GameError) mapping.
    /// 2. Build a [`Game`] record for `white` / `black` and persist it. Play
    ///    starts immediately, so the record is created already
    ///    [`Active`](GameLifecycle::Active) rather than
    ///    [`Created`](GameLifecycle::Created).
    /// 3. Spawn a [`GameActor`] over the same backing store
    ///    ([`game_repo`](Self::game_repo), [`action_log`](Self::action_log),
    ///    [`completion_hook`](Self::completion_hook), `time_control`).
    /// 4. Insert the actor's handle into the [`game_hub`](Self::game_hub) so the
    ///    WebSocket endpoint can find the live game by id.
    /// 5. Return the persisted [`Game`].
    ///
    /// `options` carries the per-game variant options. Callers that have none
    /// (matchmaking and direct challenges both currently fall into this category)
    /// pass [`VariantOptions::default`]; it is stored on the record so the game
    /// can be re-created on recovery via
    /// `VariantRegistry::new_game(variant_id, &variant_options)`.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::BadRequest`] for an unknown variant and propagates any
    /// [`StorageError`] from persisting the record (mapped through the standard
    /// [`From`] conversion).
    pub async fn create_and_spawn_game(
        &self,
        white: UserId,
        black: UserId,
        variant_id: &str,
        time_control: TimeControl,
        rated: bool,
        options: VariantOptions,
    ) -> Result<Game, ApiError> {
        // 1. Instantiate a fresh session for the agreed variant.
        let session = self.variants.new_game(variant_id, &options)?;

        // 2. Build and persist the durable record, already `Active`.
        let mut game = Game::new(
            variant_id.to_owned(),
            options,
            white,
            black,
            time_control.clone(),
            rated,
            OffsetDateTime::now_utc(),
        );
        game.lifecycle = GameLifecycle::Active;
        self.game_repo.create(&game).await?;

        // 3. Spawn the actor over the same backing store and 4. register its
        //    handle so the WebSocket endpoint can find the live game by id.
        let handle = GameActor::spawn(
            game.id,
            session,
            Arc::clone(&self.game_repo),
            Arc::clone(&self.action_log),
            Arc::clone(&self.completion_hook),
            time_control,
        );
        self.game_hub.insert(game.id, handle);

        // Count the creation (#88). This is the single shared creation path, so
        // matchmaking pairings, accepted challenges, and live rematches are all
        // tallied here exactly once.
        crate::metrics::record_game_created();

        // 5. Hand back the persisted record.
        Ok(game)
    }

    /// Resolves the live [`GameHandle`] for `game_id`, rebuilding its actor from
    /// the durable log on demand if it is not already running here.
    ///
    /// This is what lets *any* node — including a freshly restarted process —
    /// serve an in-progress game on first access, rather than relying on an eager
    /// in-memory handle (#65). It complements the M3 startup recovery (#58):
    /// instead of replaying every unfinished game up front, a game is revived
    /// lazily the first time a client reaches for it.
    ///
    /// The resolution is:
    ///
    /// 1. **Fast path** — if the [`game_hub`](Self::game_hub) already holds a
    ///    live handle, return it immediately (no storage access, no lock).
    /// 2. Otherwise load the [`Game`](mcs_domain::Game) through
    ///    [`GameRepo::get`]. A game that does not exist yields `Ok(None)`; a game
    ///    whose [`lifecycle`](mcs_domain::Game::lifecycle) is
    ///    [`Finished`](GameLifecycle::Finished) also yields `Ok(None)`, because a
    ///    finished game has no live actor to drive.
    /// 3. Otherwise rebuild a resumed actor with
    ///    [`mcs_game::recover_game`] — replaying the action log and resuming the
    ///    clocks — [`insert`](GameHub::insert) the handle into the hub, and
    ///    return it.
    ///
    /// # Concurrency
    ///
    /// Two concurrent calls for the same absent game must rebuild it **once**.
    /// Step 3 runs under a per-game lock (see [`RecoveryLocks`]): the first caller
    /// recovers and inserts; every other caller, after taking the same lock,
    /// re-checks the hub and finds the handle the first inserted — so exactly one
    /// actor is ever spawned. The lock is a [`tokio::sync::Mutex`], so it is held
    /// safely across the `.await`-heavy recovery without blocking the runtime.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Internal`] if storage cannot be read (other than a
    /// plain not-found, which is reported as `Ok(None)`) or if recovery fails —
    /// for example a variant that is no longer registered, or a durable log that
    /// diverges from the rebuilt session. Recovery failures are logged with the
    /// game id and collapsed to a generic internal error for the caller.
    pub async fn get_or_recover(&self, game_id: GameId) -> Result<Option<GameHandle>, ApiError> {
        // 1. Fast path: already live. The common case for a warm node.
        if let Some(handle) = self.game_hub.get(game_id) {
            return Ok(Some(handle));
        }

        // Take the per-game recovery lock so that, for this id, only one task
        // runs the load-recover-insert sequence below. The outer map lock is
        // released immediately; only the per-id lock is held across recovery.
        let per_game = {
            let mut locks = self.recovery_locks.lock().await;
            Arc::clone(locks.entry(game_id).or_default())
        };
        let _guard = per_game.lock().await;

        // 2. Re-check the hub now that we hold the lock: a racing caller may have
        //    recovered and inserted the handle while we were waiting.
        if let Some(handle) = self.game_hub.get(game_id) {
            return Ok(Some(handle));
        }

        // 3. Load the durable record. A missing game is `Ok(None)`, not an error;
        //    any other storage failure is a genuine internal error.
        let game = match self.game_repo.get(game_id).await {
            Ok(game) => game,
            Err(StorageError::NotFound) => return Ok(None),
            Err(error) => {
                tracing::error!(%game_id, %error, "failed to load game for recovery");
                return Err(ApiError::Internal(format!(
                    "failed to load game {game_id}: {error}"
                )));
            }
        };

        // A finished game has no live actor; nothing to revive.
        if game.lifecycle == GameLifecycle::Finished {
            return Ok(None);
        }

        // Rebuild a resumed actor from the log and register it. Handing the
        // actor the same `Arc<dyn _>` handles a freshly created game receives
        // keeps both paths writing to one backing store.
        let handle = recover_game(
            &game,
            &self.variants,
            Arc::clone(&self.action_log),
            Arc::clone(&self.game_repo),
            Arc::clone(&self.completion_hook),
        )
        .await
        .map_err(|error| {
            tracing::error!(%game_id, %error, "failed to recover game");
            ApiError::Internal(format!("failed to recover game {game_id}: {error}"))
        })?;

        // Insert under the lock so the re-check above is the only window any
        // racing caller can observe — they will find this handle, not spawn a
        // second actor. A pre-existing handle would mean a double-spawn, which
        // the lock prevents; defensively drop whatever `insert` returns.
        let _previous = self.game_hub.insert(game_id, handle.clone());

        Ok(Some(handle))
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
            .field("table_hub", &self.table_hub)
            .field("variants", &self.variants)
            .field("matchmaker", &self.matchmaker)
            .field("game_repo", &"<dyn GameRepo>")
            .field("action_log", &"<dyn ActionLogRepo>")
            .field("completion_hook", &"<dyn GameCompletionHook>")
            .field("payment_gate", &self.payment_gate)
            .field("cluster", &self.cluster)
            .field("presence", &"<dyn PresenceTracker>")
            .field("online_ttl", &self.online_ttl)
            .field("ws_max_message_bytes", &self.ws_max_message_bytes)
            .finish_non_exhaustive()
    }
}
