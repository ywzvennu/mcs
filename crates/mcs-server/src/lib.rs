//! # mcs-server
//!
//! The runnable Modular Chess Server binary and its supporting wiring.
//!
//! This crate is the **composition root**: the single place where the otherwise
//! decoupled crates are assembled into a working server. It
//!
//! - loads layered [`Config`] from defaults, an optional `config.toml`, and
//!   `MCS_`-prefixed environment variables;
//! - connects [`SqlxStorage`](mcs_storage::SqlxStorage) (which builds the pool
//!   and runs migrations);
//! - builds a [`VariantRegistry`](mcs_core::VariantRegistry) and **registers
//!   all supported variants** (standard, Chess960, and RBC) here, keeping
//!   `mcs-api` variant-agnostic;
//! - constructs the [`AppState`](mcs_api::AppState) and the top-level router via
//!   [`mcs_api::router`], adds a `GET /health` endpoint, and wraps everything in
//!   the request-id and HTTP-trace Tower layers from `mcs-observability`.
//!
//! The wiring lives in library functions ([`build_app`], [`build_state`]) so it
//! is exercised by integration tests without binding a socket; [`main`] stays a
//! thin shell that loads config, initialises observability, and serves.
//!
//! [`main`]: ../mcs_server/fn.main.html
#![doc(html_root_url = "https://docs.rs/mcs-server")]

pub mod cluster;
pub mod config;
pub mod metrics;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::{routing::get, Json, Router};
use mcs_api::{AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::VariantRegistry;
use mcs_storage::SqlxStorage;
use serde::Serialize;
use tower::ServiceBuilder;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;

pub use cluster::ClusterRuntime;
pub use config::Config;

/// The body returned by the `GET /health` liveness endpoint.
#[derive(Debug, Clone, Serialize)]
struct Health {
    /// Always `"ok"` when the process is serving requests.
    status: &'static str,
}

/// Liveness probe: returns `200 OK` with `{"status":"ok"}`.
///
/// This endpoint touches no shared state, so it succeeds as long as the HTTP
/// server is accepting connections — exactly what an orchestrator's liveness
/// check needs.
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

/// Builds the [`AppState`] from configuration and a connected storage handle.
///
/// Registers all supported game variants into a fresh
/// [`VariantRegistry`](mcs_core::VariantRegistry):
///
/// - **standard** — ordinary FIDE chess (`mcs-variant-standard`);
/// - **chess960** — Fischer Random Chess (`mcs-variant-standard`);
/// - **rbc** — Reconnaissance Blind Chess (`mcs-variant-rbc`).
///
/// After registration the count of registered variants is logged at `INFO`
/// level so operators can confirm all variants loaded.
///
/// The `secret` used to key session tokens is provided by the caller (rather
/// than read from `cfg` directly) so that [`main`](crate) can decide between a
/// configured secret and a generated ephemeral one — and log accordingly —
/// before handing the resolved bytes here.
///
/// # Errors
///
/// Returns an error only when payments are enabled with an invalid verifier
/// configuration — e.g. `verifier = "facilitator"` with no `facilitator_url`
/// (see [`PaymentSettings::build_verifier`](config::PaymentSettings::build_verifier)).
///
/// [`main`]: ../mcs_server/fn.main.html
pub fn build_state(
    cfg: &Config,
    storage: Arc<SqlxStorage>,
    session_secret: Vec<u8>,
) -> anyhow::Result<AppState> {
    let mut variants = VariantRegistry::new();
    // `mcs_variant_standard::register` adds both `standard` and `chess960`.
    mcs_variant_standard::register(&mut variants);
    mcs_variant_rbc::register(&mut variants);
    tracing::info!(count = variants.ids().len(), "variant registry built");

    let session_config = SessionConfig::new(
        session_secret,
        cfg.session_ttl(),
        cfg.session.issuer.clone(),
    );

    let siwe_config = SiweConfig::new(
        cfg.siwe.domain.clone(),
        cfg.siwe.uri.clone(),
        cfg.siwe.chain_id,
        cfg.siwe.statement.clone(),
        cfg.nonce_ttl(),
    );

    let state = AppState::new(storage, Arc::new(variants), session_config, siwe_config)
        // Thread the WS message-size limit from config into the state so the
        // WebSocket handler can apply it to each upgrade (#99).
        .with_ws_max_message_bytes(cfg.http.max_ws_message_bytes);

    // Optionally gate game creation behind an x402 payment (#45). Off by default:
    // when `[payments].enabled` is false the state is returned untouched and
    // `POST /seeks` stays free. When enabled, build the requirements + verifier
    // and attach the gate; the API then wraps only the creation route. This is
    // the hook where, per the roadmap, RBC game creation would be charged.
    if cfg.payments.enabled {
        let requirements = cfg.payments.requirements("/seeks");
        let verifier = cfg.payments.build_verifier()?;
        tracing::info!(
            scheme = %cfg.payments.scheme,
            network = %cfg.payments.network,
            pay_to = %cfg.payments.pay_to,
            verifier = ?cfg.payments.verifier,
            "x402 payment gate enabled on game creation"
        );
        Ok(state.with_payment(requirements, verifier))
    } else {
        Ok(state)
    }
}

/// Assembles the complete application [`Router`] from an [`AppState`].
///
/// The result is the `mcs-api` top-level router (auth, REST, and WebSocket
/// routes) merged with the operational endpoints, wrapped in the observability
/// middleware stack: a request-id layer (read-or-generate `x-request-id`), its
/// propagation to the response, and the HTTP trace layer.
///
/// This takes a fully-built [`AppState`] rather than a [`Config`] so tests can
/// inject an in-memory backend; see [`build_app`] for the config-driven path.
///
/// # Operational endpoints (#88)
///
/// Three probe/observability routes are mounted here, outside the API surface so
/// they are unauthenticated and free of the payment gate:
///
/// - `GET /health` — **liveness**: always `200 {"status":"ok"}` while the
///   process is up; touches no dependency (see [`health`]).
/// - `GET /ready` — **readiness**: `200 {"status":"ready"}` only when the
///   database (and, in a cluster, Redis) are reachable, else `503` naming the
///   failed dependency (see [`mcs_api::ready_router`]).
/// - `GET /metrics` — **Prometheus** exposition text for the metrics the API
///   records (HTTP request count/latency, live games, games created, rating
///   updates, active WebSocket connections; see [`mcs_api::metrics`]).
///
/// The Prometheus recorder is installed (once per process) by
/// [`metrics::handle`] the first time this function runs.
///
/// # CORS layer (#98)
///
/// When a [`CorsSettings`](config::CorsSettings) is supplied via `cors_layer`,
/// it is applied as the outermost layer on the router so browsers receive the
/// correct preflight and simple-request CORS headers. Pass
/// [`None`] to disable CORS (the safe default for servers without a configured
/// browser client). The config-driven path in [`build_app`] always supplies the
/// layer built from `[cors]` config; integration tests can pass a custom layer.
pub fn router(state: AppState) -> Router {
    router_with_cors(state, None, None)
}

/// Like [`router`], but with an explicit CORS layer and HTTP hardening options.
///
/// Both optional parameters are `None`-safe: omitting them is equivalent to
/// calling [`router`] with default hardening settings (security headers on,
/// no HSTS, 30 s timeout, 64 KiB body limit from the state).
///
/// Used by [`build_app`] (which supplies the full config-driven layers) and by
/// integration tests that want to inject a custom CORS or HTTP-limit layer
/// without going through `build_app`.
///
/// # Security-header layer (#99)
///
/// When `http_config` is supplied, the following headers are added to every
/// response:
///
/// - `X-Content-Type-Options: nosniff`
/// - `X-Frame-Options: DENY`
/// - `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`
/// - `Referrer-Policy: no-referrer`
/// - `Strict-Transport-Security: max-age=…; includeSubDomains` — **only** when
///   [`HttpSettings::hsts`](config::HttpSettings::hsts) is `true`.
///
/// A request [`TimeoutLayer`] aborts handlers that exceed
/// [`request_timeout_secs`](config::HttpSettings::request_timeout_secs).
///
/// A [`DefaultBodyLimit`] layer rejects oversized request bodies with **413**.
pub fn router_with_cors(
    state: AppState,
    cors_layer: Option<tower_http::cors::CorsLayer>,
    http_config: Option<&config::HttpSettings>,
) -> Router {
    let (set_request_id, propagate_request_id) = mcs_observability::request_id_layers();

    // Install (once) the Prometheus recorder and mount the scrape endpoint. The
    // API router's middleware records into this recorder; `/metrics` renders it.
    let metrics_handle = metrics::handle();

    // Build the body-size limit and timeout from config or fall back to defaults.
    let default_http = config::HttpSettings::default();
    let http = http_config.unwrap_or(&default_http);

    // Security-header layers: each injects one header into every response using
    // `SetResponseHeaderLayer::overriding` so a handler that accidentally sets
    // the same header is overridden by the hardening layer, not the other way.
    let sec_headers = ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        ));

    let middleware = ServiceBuilder::new()
        .layer(set_request_id)
        .layer(propagate_request_id)
        .layer(mcs_observability::http_trace_layer())
        // Request timeout: handlers that take longer than the configured limit
        // are aborted and the client receives 408 Request Timeout.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            http.request_timeout(),
        ));

    let base = mcs_api::router(state)
        .route("/health", get(health))
        .merge(metrics::metrics_router(metrics_handle))
        // Body-size limit: any request body larger than this is rejected with
        // 413 Payload Too Large before the handler is even called.
        .layer(DefaultBodyLimit::max(http.max_body_bytes))
        .layer(middleware)
        // Security-header layer sits between the middleware and the outermost
        // CORS layer so that all responses (including CORS preflights) carry
        // the security headers.
        .layer(sec_headers);

    // HSTS: add Strict-Transport-Security when enabled. Applied here (inside
    // CORS) so the header travels on all responses, including preflight ones.
    let base = if let Some(hsts_value) = http.hsts_header_value() {
        let hsts_hv =
            HeaderValue::from_str(&hsts_value).expect("hsts header value is always valid ASCII");
        base.layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("strict-transport-security"),
            hsts_hv,
        ))
    } else {
        base
    };

    // The CORS layer is applied after all other layers so it can inspect and
    // annotate responses before they leave the server.
    if let Some(cors) = cors_layer {
        base.layer(cors)
    } else {
        base
    }
}

/// Rebuilds the live actors for every game that was still in progress, inserting
/// each into the [`AppState`]'s live-game hub.
///
/// After a restart a game in progress exists only in storage. This lists every
/// unfinished [`Game`](mcs_domain::Game) via
/// [`GameRepo::list_unfinished`](mcs_storage::GameRepo::list_unfinished) and, for
/// each, calls [`mcs_game::recover_game`] to replay its durable action log into a
/// resumed [`GameActor`](mcs_game::GameActor), then registers the returned handle
/// in [`AppState::game_hub`] so clients can reconnect and play on. Each side's
/// clock resumes from its last persisted remaining time as of *now*, so the time
/// the server was down is not charged to either player.
///
/// Recovery is best-effort and isolated: a single game that fails to recover (an
/// unknown variant, an unreadable or divergent log) is logged at `WARN` and
/// skipped — it never aborts startup or blocks the other games. The number of
/// games successfully recovered is returned and logged at `INFO`.
///
/// Called once during [`build_app`], before the server begins serving.
///
/// # Errors
///
/// Returns an error only if the initial
/// [`list_unfinished`](mcs_storage::GameRepo::list_unfinished) query fails;
/// per-game recovery failures are logged and skipped rather than propagated.
pub async fn recover_games(state: &AppState) -> anyhow::Result<usize> {
    let unfinished = state.game_repo().list_unfinished().await?;
    let total = unfinished.len();

    let mut recovered = 0usize;
    for game in &unfinished {
        match mcs_game::recover_game(
            game,
            state.variants(),
            state.action_log().clone(),
            state.game_repo().clone(),
            state.completion_hook().clone(),
        )
        .await
        {
            Ok(handle) => {
                state.game_hub().insert(game.id, handle);
                recovered += 1;
            }
            Err(error) => {
                tracing::warn!(
                    game_id = %game.id,
                    %error,
                    "skipping a game that could not be recovered",
                );
            }
        }
    }

    tracing::info!(count = recovered, total, "recovered in-progress games",);

    Ok(recovered)
}

/// Connects storage and assembles the application [`Router`] from `cfg`.
///
/// This is the config-driven entry point used by [`main`](crate): it connects
/// [`SqlxStorage`](mcs_storage::SqlxStorage) (building the pool and running
/// migrations), builds the state, **recovers any games that were in progress**
/// (see [`recover_games`]) into the live-game hub, **wires cluster membership**
/// (see [`cluster::setup`]) when `[cluster].enabled`, then assembles the router.
/// The session `secret` is supplied separately so the caller controls the
/// ephemeral-secret policy.
///
/// Returns the assembled [`Router`] together with an optional [`ClusterRuntime`]:
/// `Some` when cluster mode is enabled (the caller must
/// [`shutdown`](ClusterRuntime::shutdown) it on graceful shutdown to leave the
/// registry promptly), `None` for a single-node server.
///
/// # Errors
///
/// Returns an error if the storage backend cannot be connected, its migrations
/// fail to apply, the unfinished-games query fails, or — when cluster mode is
/// enabled — the Redis connection or initial node registration fails. Individual
/// games that cannot be recovered are logged and skipped, never aborting startup.
///
/// [`main`]: ../mcs_server/fn.main.html
pub async fn build_app(
    cfg: &Config,
    session_secret: Vec<u8>,
) -> anyhow::Result<(Router, Option<ClusterRuntime>)> {
    let storage = Arc::new(SqlxStorage::connect(&cfg.database_url).await?);
    let state = build_state(cfg, storage, session_secret)?;
    recover_games(&state).await?;
    // Wire cluster membership when enabled (no-op otherwise; the state keeps its
    // single-node local registry and no Redis connection is opened).
    let (state, cluster) = cluster::setup(cfg, state).await?;
    // Build the CORS layer from config and apply it to the assembled router so
    // browser clients receive the correct CORS headers. With the default (empty)
    // allowed_origins the layer is effectively a no-op for cross-origin requests.
    let cors_layer = cfg.cors.build_cors_layer();
    Ok((
        router_with_cors(state, Some(cors_layer), Some(&cfg.http)),
        cluster,
    ))
}
