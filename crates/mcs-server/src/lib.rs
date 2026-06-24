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
//!   all supported variants** (standard, RBC, and the shakmaty family) here,
//!   keeping `mcs-api` variant-agnostic;
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

pub mod config;

use std::sync::Arc;

use axum::{routing::get, Json, Router};
use mcs_api::{AppState, SiweConfig};
use mcs_auth::SessionConfig;
use mcs_core::VariantRegistry;
use mcs_storage::SqlxStorage;
use serde::Serialize;
use tower::ServiceBuilder;

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
/// - **rbc** — Reconnaissance Blind Chess (`mcs-variant-rbc`);
/// - **atomic, antichess, crazyhouse, kingofthehill, threecheck, racingkings,
///   horde, chess960** — the shakmaty family (`mcs-variant-shakmaty`).
///
/// After registration the count of registered variants is logged at `INFO`
/// level so operators can confirm all variants loaded.
///
/// The `secret` used to key session tokens is provided by the caller (rather
/// than read from `cfg` directly) so that [`main`](crate) can decide between a
/// configured secret and a generated ephemeral one — and log accordingly —
/// before handing the resolved bytes here.
///
/// [`main`]: ../mcs_server/fn.main.html
#[must_use]
pub fn build_state(cfg: &Config, storage: Arc<SqlxStorage>, session_secret: Vec<u8>) -> AppState {
    let mut variants = VariantRegistry::new();
    mcs_variant_standard::register(&mut variants);
    mcs_variant_rbc::register(&mut variants);
    mcs_variant_shakmaty::register_all(&mut variants);
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

    let state = AppState::new(storage, Arc::new(variants), session_config, siwe_config);

    // Optionally gate game creation behind an x402 payment (#45). Off by default:
    // when `[payments].enabled` is false the state is returned untouched and
    // `POST /seeks` stays free. When enabled, build the requirements + verifier
    // and attach the gate; the API then wraps only the creation route. This is
    // the hook where, per the roadmap, RBC game creation would be charged.
    if cfg.payments.enabled {
        let requirements = cfg.payments.requirements("/seeks");
        let verifier = cfg.payments.build_verifier();
        tracing::info!(
            scheme = %cfg.payments.scheme,
            network = %cfg.payments.network,
            pay_to = %cfg.payments.pay_to,
            "x402 payment gate enabled on game creation"
        );
        state.with_payment(requirements, verifier)
    } else {
        state
    }
}

/// Assembles the complete application [`Router`] from an [`AppState`].
///
/// The result is the `mcs-api` top-level router (auth, REST, and WebSocket
/// routes) merged with a `GET /health` endpoint, wrapped in the observability
/// middleware stack: a request-id layer (read-or-generate `x-request-id`), its
/// propagation to the response, and the HTTP trace layer.
///
/// This takes a fully-built [`AppState`] rather than a [`Config`] so tests can
/// inject an in-memory backend; see [`build_app`] for the config-driven path.
pub fn router(state: AppState) -> Router {
    let (set_request_id, propagate_request_id) = mcs_observability::request_id_layers();

    let middleware = ServiceBuilder::new()
        .layer(set_request_id)
        .layer(propagate_request_id)
        .layer(mcs_observability::http_trace_layer());

    mcs_api::router(state)
        .route("/health", get(health))
        .layer(middleware)
}

/// Connects storage and assembles the application [`Router`] from `cfg`.
///
/// This is the config-driven entry point used by [`main`](crate): it connects
/// [`SqlxStorage`](mcs_storage::SqlxStorage) (building the pool and running
/// migrations), then builds the state and router. The session `secret` is
/// supplied separately so the caller controls the ephemeral-secret policy.
///
/// # Errors
///
/// Returns an error if the storage backend cannot be connected or its
/// migrations fail to apply.
///
/// [`main`]: ../mcs_server/fn.main.html
pub async fn build_app(cfg: &Config, session_secret: Vec<u8>) -> anyhow::Result<Router> {
    let storage = Arc::new(SqlxStorage::connect(&cfg.database_url).await?);
    let state = build_state(cfg, storage, session_secret);
    Ok(router(state))
}
