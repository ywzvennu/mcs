//! Redis-backed [`NodeRegistry`] (enabled by the `redis` feature).
//!
//! Membership is stored as one key per node, `{prefix}{id}`, whose value is the
//! node's address and whose Redis-managed TTL *is* its liveness. Registering and
//! heartbeating both `SET` the key with `EX ttl`, so a node that stops
//! heartbeating is evicted by Redis automatically once the TTL lapses — no
//! reaper process required. [`live_nodes`](RedisNodeRegistry::live_nodes) scans
//! for surviving keys with `SCAN MATCH {prefix}*`.
//!
//! `SCAN` (rather than `KEYS`) is used so membership lookups never block the
//! Redis server, which matters once a cluster is large enough to need this in
//! the first place.

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

use crate::error::ClusterError;
use crate::registry::NodeRegistry;
use crate::types::{NodeId, NodeInfo};

/// Default key prefix for node entries. Overridable via
/// [`RedisNodeRegistry::with_prefix`].
const DEFAULT_PREFIX: &str = "mcs:cluster:node:";

/// A [`NodeRegistry`] that coordinates membership through Redis.
///
/// Construct one per process with [`connect`](RedisNodeRegistry::connect),
/// passing the Redis URL, this process's [`NodeInfo`], and a TTL. Spawn a task
/// that calls [`heartbeat`](NodeRegistry::heartbeat) on an interval comfortably
/// shorter than `ttl_secs` (a third of the TTL is a common choice).
#[derive(Clone)]
pub struct RedisNodeRegistry {
    conn: ConnectionManager,
    node: NodeInfo,
    ttl_secs: u64,
    prefix: String,
}

impl std::fmt::Debug for RedisNodeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ConnectionManager` is not `Debug`; summarize the useful state instead.
        f.debug_struct("RedisNodeRegistry")
            .field("node", &self.node)
            .field("ttl_secs", &self.ttl_secs)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl RedisNodeRegistry {
    /// Connects to Redis at `url` and prepares to register `node` with a
    /// `ttl_secs`-second liveness window.
    ///
    /// The underlying [`ConnectionManager`] reconnects transparently, so the
    /// registry tolerates brief Redis outages. This does not register the node;
    /// call [`register`](NodeRegistry::register) once connected.
    ///
    /// # Errors
    ///
    /// Returns [`ClusterError::Backend`] if the URL is invalid or the initial
    /// connection cannot be established.
    pub async fn connect(url: &str, node: NodeInfo, ttl_secs: u64) -> Result<Self, ClusterError> {
        let client = redis::Client::open(url)?;
        let conn = client.get_connection_manager().await?;
        Ok(Self {
            conn,
            node,
            ttl_secs,
            prefix: DEFAULT_PREFIX.to_owned(),
        })
    }

    /// Overrides the key prefix used for node entries (default
    /// `mcs:cluster:node:`). Useful to isolate multiple logical clusters that
    /// share one Redis.
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The Redis key for this node.
    fn self_key(&self) -> String {
        format!("{}{}", self.prefix, self.node.id)
    }

    /// Recovers a [`NodeId`] from a full Redis key by stripping the prefix.
    fn id_from_key(&self, key: &str) -> NodeId {
        NodeId::from(key.strip_prefix(&self.prefix).unwrap_or(key))
    }

    /// Writes (or rewrites) this node's key with a fresh TTL. Shared by
    /// `register` and `heartbeat`, which are the same Redis operation.
    async fn set_with_ttl(&self) -> Result<(), ClusterError> {
        let mut conn = self.conn.clone();
        conn.set_ex::<_, _, ()>(self.self_key(), &self.node.address, self.ttl_secs)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl NodeRegistry for RedisNodeRegistry {
    async fn register(&self) -> Result<(), ClusterError> {
        tracing::debug!(node = %self.node.id, ttl = self.ttl_secs, "registering node");
        self.set_with_ttl().await
    }

    async fn heartbeat(&self) -> Result<(), ClusterError> {
        self.set_with_ttl().await
    }

    async fn leave(&self) -> Result<(), ClusterError> {
        tracing::debug!(node = %self.node.id, "node leaving");
        let mut conn = self.conn.clone();
        conn.del::<_, ()>(self.self_key()).await?;
        Ok(())
    }

    async fn live_nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        let mut conn = self.conn.clone();
        let pattern = format!("{}*", self.prefix);

        // Collect surviving keys via a non-blocking cursor scan.
        let mut keys: Vec<String> = Vec::new();
        let mut iter = conn.scan_match::<_, String>(&pattern).await?;
        while let Some(key) = iter.next_item().await {
            keys.push(key);
        }
        drop(iter);

        // Resolve each key's address. A key can expire between the scan and the
        // GET; such a node is simply no longer live, so we skip nil replies.
        let mut nodes = Vec::with_capacity(keys.len());
        for key in keys {
            let address: Option<String> = conn.get(&key).await?;
            if let Some(address) = address {
                nodes.push(NodeInfo {
                    id: self.id_from_key(&key),
                    address,
                });
            }
        }
        Ok(nodes)
    }
}
