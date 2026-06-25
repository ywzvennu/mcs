//! The `mcs-server` binary entry point.
//!
//! Kept deliberately thin: it loads [`Config`], initialises observability,
//! resolves the session secret (configured or ephemeral), and hands off to the
//! library wiring in [`mcs_server`] to build and serve the app with graceful
//! shutdown. See the crate-level docs in `lib.rs` for the composition details.

use anyhow::Context as _;
use mcs_observability::ObservabilityConfig;
use mcs_server::{build_app, Config};
use rand::RngCore as _;
use tokio::net::TcpListener;

/// Length, in bytes, of a generated ephemeral session secret. 32 bytes (256
/// bits) matches the HS256 key size and the entropy floor `mcs-auth` documents.
const EPHEMERAL_SECRET_LEN: usize = 32;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load().context("loading configuration")?;

    // Install the global tracing subscriber before anything else logs, so the
    // warnings below (and all later output) honour the configured format/level.
    mcs_observability::init(&ObservabilityConfig {
        format: cfg.log_format(),
        default_directive: cfg.log.level.clone(),
    })
    .context("installing the tracing subscriber")?;

    tracing::info!(mode = %cfg.env, "run mode");

    // Validate configuration before binding the socket so a misconfigured
    // production server exits with a clear, actionable error message rather
    // than silently using insecure defaults (e.g. an ephemeral session secret).
    if let Err(e) = cfg.validate() {
        // Use `tracing::error!` so the message lands in the structured log even
        // if the operator is reading journald or a log aggregator, then bail.
        tracing::error!("{e}");
        anyhow::bail!("{e}");
    }

    let session_secret = resolve_session_secret(&cfg);

    // `cluster` is `Some` only when cluster mode is enabled; single-node it is
    // `None` and the server runs exactly as before. `retention_token` stops the
    // GC background task on graceful shutdown.
    let (app, cluster, retention_token) = build_app(&cfg, session_secret)
        .await
        .context("building the application")?;

    let listener = TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding to {}", cfg.bind))?;
    let local_addr = listener
        .local_addr()
        .context("reading the bound local address")?;

    tracing::info!(address = %local_addr, "mcs-server listening");

    // Serve with per-connection `ConnectInfo<SocketAddr>` so the per-IP rate
    // limiter (#100) can read each request's socket peer address. When a trusted
    // reverse proxy is configured (`[limits].trusted_proxy_header`), the real
    // client IP is taken from that header instead.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("serving HTTP")?;

    // Graceful shutdown drained. Stop the retention task first (it holds no
    // connections open, so this is instant) then leave the cluster registry
    // promptly so survivors notice this node's departure without waiting for
    // its TTL to lapse.
    retention_token.cancel();
    if let Some(cluster) = cluster {
        cluster.shutdown().await;
    }

    tracing::info!("mcs-server shut down cleanly");
    Ok(())
}

/// Resolves the session-signing secret from configuration.
///
/// If `session.secret` is set, its bytes are used directly. Otherwise a random
/// 256-bit secret is generated for this process only and a prominent warning is
/// logged: sessions signed with an ephemeral secret are invalidated on every
/// restart. This fallback is allowed only in development mode — production mode
/// rejects a missing/weak secret earlier via [`Config::validate`].
fn resolve_session_secret(cfg: &Config) -> Vec<u8> {
    match &cfg.session.secret {
        Some(secret) => secret.clone().into_bytes(),
        None => {
            let mut secret = vec![0u8; EPHEMERAL_SECRET_LEN];
            rand::thread_rng().fill_bytes(&mut secret);
            tracing::warn!(
                "no session.secret configured (MCS_SESSION__SECRET / config.toml \
                 [session] secret = \"…\"); \
                 generated an ephemeral secret — ALL SESSIONS WILL BE INVALIDATED ON RESTART. \
                 Set a stable secret for production (MCS_ENV=production enforces this)."
            );
            secret
        }
    }
}

/// Resolves when the process receives a shutdown signal.
///
/// Completes on `Ctrl-C` (SIGINT) on every platform, and additionally on
/// SIGTERM on Unix so that container orchestrators get a clean, graceful
/// shutdown rather than a hard kill. Returning from this future causes
/// [`axum::serve`] to stop accepting new connections and drain in-flight ones.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => tracing::error!(%error, "failed to install SIGTERM handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received; draining connections");
}
