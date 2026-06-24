//! Node registry trait and the in-process [`LocalRegistry`].
//!
//! A [`NodeRegistry`] is the cluster's source of membership truth: it records
//! that *this* node is alive (with a time-to-live), renews that liveness on a
//! heartbeat, and reports the full set of currently-live nodes. The rendezvous
//! [`directory`](crate::directory) then turns that membership into per-game
//! ownership.

use async_trait::async_trait;

use crate::error::ClusterError;
use crate::types::NodeInfo;

/// Records node liveness and reports cluster membership.
///
/// The TTL model keeps coordination cheap and self-healing: a node `register`s
/// with a TTL, periodically `heartbeat`s to renew it, and disappears
/// automatically once it stops (its TTL lapses) — even if it crashes without
/// calling [`leave`](NodeRegistry::leave). [`live_nodes`](NodeRegistry::live_nodes)
/// only ever returns nodes whose TTL has not expired.
#[async_trait]
pub trait NodeRegistry: Send + Sync {
    /// Records this node as live with a fresh TTL.
    async fn register(&self) -> Result<(), ClusterError>;

    /// Renews this node's TTL. Call this on an interval shorter than the TTL.
    async fn heartbeat(&self) -> Result<(), ClusterError>;

    /// Removes this node from the registry (graceful shutdown).
    async fn leave(&self) -> Result<(), ClusterError>;

    /// Returns every node whose TTL is currently unexpired.
    async fn live_nodes(&self) -> Result<Vec<NodeInfo>, ClusterError>;
}

/// In-process registry for single-node deployments.
///
/// `LocalRegistry` needs no external backend: its [`live_nodes`] always returns
/// exactly the one node it was built with, so that node owns every game. This is
/// the default the server runs with when no Redis (or other backend) is
/// configured, and it is what lets the whole crate build and test with no Redis
/// present. The TTL/heartbeat operations are no-ops that always succeed.
///
/// [`live_nodes`]: NodeRegistry::live_nodes
///
/// ```
/// use mcs_cluster::{LocalRegistry, NodeInfo, NodeRegistry, is_owner};
///
/// # tokio_test_block(async {
/// let me = NodeInfo::new("solo", "http://127.0.0.1:8080");
/// let registry = LocalRegistry::new(me.clone());
/// registry.register().await.unwrap();
///
/// let nodes = registry.live_nodes().await.unwrap();
/// assert_eq!(nodes, vec![me.clone()]);
/// // A single node owns everything.
/// assert!(is_owner("any-game-id", &me.id, &nodes));
/// # });
/// # fn tokio_test_block<F: std::future::Future>(f: F) -> F::Output {
/// #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct LocalRegistry {
    node: NodeInfo,
}

impl LocalRegistry {
    /// Builds a registry that reports only `node` as live.
    #[must_use]
    pub fn new(node: NodeInfo) -> Self {
        Self { node }
    }

    /// Returns the node this registry represents.
    #[must_use]
    pub fn node(&self) -> &NodeInfo {
        &self.node
    }
}

#[async_trait]
impl NodeRegistry for LocalRegistry {
    async fn register(&self) -> Result<(), ClusterError> {
        Ok(())
    }

    async fn heartbeat(&self) -> Result<(), ClusterError> {
        Ok(())
    }

    async fn leave(&self) -> Result<(), ClusterError> {
        Ok(())
    }

    async fn live_nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        Ok(vec![self.node.clone()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::is_owner;
    use crate::types::NodeId;

    fn me() -> NodeInfo {
        NodeInfo::new("solo", "http://127.0.0.1:8080")
    }

    #[tokio::test]
    async fn live_nodes_returns_the_single_node() {
        let reg = LocalRegistry::new(me());
        reg.register().await.unwrap();
        reg.heartbeat().await.unwrap();
        assert_eq!(reg.live_nodes().await.unwrap(), vec![me()]);
        assert_eq!(reg.node(), &me());
    }

    #[tokio::test]
    async fn single_node_owns_any_game() {
        let reg = LocalRegistry::new(me());
        let nodes = reg.live_nodes().await.unwrap();
        for g in 0..200 {
            let game = format!("game-{g}");
            assert!(is_owner(&game, &NodeId::from("solo"), &nodes));
        }
    }

    #[tokio::test]
    async fn leave_then_live_nodes_still_succeeds() {
        let reg = LocalRegistry::new(me());
        reg.leave().await.unwrap();
        // The local registry is stateless; leaving is a no-op.
        assert_eq!(reg.live_nodes().await.unwrap(), vec![me()]);
    }
}
