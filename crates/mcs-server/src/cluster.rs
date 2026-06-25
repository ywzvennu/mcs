//! Cluster membership lifecycle for the server (#68).
//!
//! This module is the composition root's single contact point with
//! [`mcs_cluster`]'s Redis-backed registry. It is responsible for the runtime
//! side of clustering — connecting, registering, heart-beating, and leaving —
//! while the *routing* decision lives in `mcs-api` (the WS handler). Keeping the
//! two apart means the API never links a coordination backend; it only ever
//! holds an `Arc<dyn NodeRegistry>`.
//!
//! # Single-node default
//!
//! When `[cluster].enabled` is `false` (the default), [`setup`] does nothing:
//! it returns `None` and the [`AppState`] keeps the in-process
//! [`LocalRegistry`](mcs_cluster::LocalRegistry) it was built with, so the server
//! opens no Redis connection and behaves exactly as it did before clustering.
//!
//! # Enabled
//!
//! When enabled, [`setup`]:
//!
//! 1. connects a [`RedisNodeRegistry`](mcs_cluster::RedisNodeRegistry) for this
//!    node's [`NodeInfo`](mcs_cluster::NodeInfo) with the configured TTL;
//! 2. [`register`](mcs_cluster::NodeRegistry::register)s this node;
//! 3. spawns a background task that
//!    [`heartbeat`](mcs_cluster::NodeRegistry::heartbeat)s on the configured
//!    interval until told to stop; and
//! 4. attaches the registry to the [`AppState`] via
//!    [`with_cluster`](mcs_api::AppState::with_cluster), so the WS layer routes
//!    each game to its rendezvous owner.
//!
//! It returns a [`ClusterRuntime`] handle. Call [`ClusterRuntime::shutdown`] on
//! graceful shutdown to stop the heartbeat and
//! [`leave`](mcs_cluster::NodeRegistry::leave) the registry, so survivors notice
//! this node's departure immediately rather than waiting for its TTL to lapse.

use std::sync::Arc;
use std::time::Duration;

use mcs_api::AppState;
use mcs_cluster::{EventBus, NodeRegistry, RedisEventBus, RedisNodeRegistry};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::config::Config;

/// A live cluster membership: the registry plus its heartbeat task.
///
/// Held by the binary for the lifetime of the process. Dropping it without
/// calling [`shutdown`](ClusterRuntime::shutdown) still stops heart-beating (the
/// task aborts) and the node is evicted once its TTL lapses; calling `shutdown`
/// is the clean path that leaves the registry immediately.
pub struct ClusterRuntime {
    registry: Arc<dyn NodeRegistry>,
    /// Signals the heartbeat task to stop.
    stop: Arc<Notify>,
    /// The heartbeat task handle, taken on shutdown to await its exit.
    heartbeat: JoinHandle<()>,
}

impl std::fmt::Debug for ClusterRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Arc<dyn NodeRegistry>` is not `Debug`; summarize the useful state.
        f.debug_struct("ClusterRuntime")
            .field("registry", &"<dyn NodeRegistry>")
            .field("heartbeat_finished", &self.heartbeat.is_finished())
            .finish_non_exhaustive()
    }
}

impl ClusterRuntime {
    /// Stops the heartbeat task and removes this node from the registry.
    ///
    /// Idempotent in effect: after this returns the node is no longer renewing
    /// its TTL and has issued a best-effort `leave`. A failing `leave` is logged
    /// and ignored — the TTL is the backstop, so survivors still evict this node.
    pub async fn shutdown(self) {
        // Ask the heartbeat task to stop, then wait for it to exit so no further
        // renew can race the `leave` below.
        self.stop.notify_one();
        if let Err(error) = self.heartbeat.await {
            tracing::warn!(%error, "cluster heartbeat task did not exit cleanly");
        }
        if let Err(error) = self.registry.leave().await {
            tracing::warn!(%error, "failed to leave the cluster registry on shutdown");
        } else {
            tracing::info!("left the cluster registry");
        }
    }
}

/// Wires cluster membership into `state` per `cfg`, returning the modified state
/// and a runtime handle when clustering is enabled.
///
/// When `[cluster].enabled` is `false` this is a no-op: it returns the state
/// unchanged and `None`, so the server stays single-node and opens no Redis
/// connection.
///
/// When enabled it connects and registers a
/// [`RedisNodeRegistry`](mcs_cluster::RedisNodeRegistry), spawns the heartbeat
/// task, and attaches the registry to the state. The caller must hold the
/// returned [`ClusterRuntime`] and call [`ClusterRuntime::shutdown`] on graceful
/// shutdown.
///
/// # Errors
///
/// Returns an error if the Redis connection cannot be established or the initial
/// node registration fails — a misconfigured cluster should fail fast at startup
/// rather than silently run without membership.
pub async fn setup(
    cfg: &Config,
    state: AppState,
) -> anyhow::Result<(AppState, Option<ClusterRuntime>)> {
    if !cfg.cluster.enabled {
        // Single-node: leave the state's in-process LocalRegistry untouched.
        return Ok((state, None));
    }

    let node = cfg.cluster.node_info();
    let ttl = cfg.cluster.heartbeat_ttl_secs;
    let interval = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(1));

    tracing::info!(
        node = %node.id,
        address = %node.address,
        ttl_secs = ttl,
        interval_secs = cfg.cluster.heartbeat_interval_secs,
        "cluster mode enabled; connecting Redis membership registry",
    );

    let registry = RedisNodeRegistry::connect(&cfg.cluster.redis_url, node.clone(), ttl).await?;
    let registry: Arc<dyn NodeRegistry> = Arc::new(registry);
    registry.register().await?;

    // Spawn the heartbeat loop. It renews the TTL on `interval` and stops when
    // `stop` is signalled (graceful shutdown).
    let stop = Arc::new(Notify::new());
    let heartbeat = tokio::spawn(heartbeat_loop(
        Arc::clone(&registry),
        Arc::clone(&stop),
        interval,
    ));

    // Build the cross-node spectator-broadcast bus (#109) over the same Redis
    // and inject it into the state, so an actor's spectator frames reach a
    // watcher on any node and the WS spectator path can subscribe to them. With
    // cluster mode off this is never reached; the state keeps its in-process
    // `LocalEventBus` and no bus connection is opened.
    let bus = RedisEventBus::connect(&cfg.cluster.redis_url).await?;
    let bus: Arc<dyn EventBus> = Arc::new(bus);

    let state = state
        .with_cluster(Arc::clone(&registry), node)
        .with_event_bus(bus);
    Ok((
        state,
        Some(ClusterRuntime {
            registry,
            stop,
            heartbeat,
        }),
    ))
}

/// Renews this node's TTL on `interval` until `stop` fires.
///
/// A failed heartbeat is logged at `WARN` and retried on the next tick rather
/// than aborting the loop: the [`ConnectionManager`](redis::aio::ConnectionManager)
/// reconnects transparently, so a brief Redis blip should not drop this node from
/// membership any sooner than its TTL forces.
async fn heartbeat_loop(registry: Arc<dyn NodeRegistry>, stop: Arc<Notify>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // Skip the immediate first tick: `register` already wrote a fresh TTL.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(error) = registry.heartbeat().await {
                    tracing::warn!(%error, "cluster heartbeat failed; will retry next tick");
                }
            }
            () = stop.notified() => {
                tracing::debug!("cluster heartbeat task stopping");
                break;
            }
        }
    }
}
