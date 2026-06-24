//! Optional Redis-backed cluster round-trip — **never run in CI**.
//!
//! This exercises the real [`RedisNodeRegistry`](mcs_cluster::RedisNodeRegistry)
//! against a live Redis: register two nodes, confirm both appear in membership,
//! `leave` one, and confirm it disappears. It is `#[ignore]`d *and* gated on the
//! `MCS_TEST_REDIS_URL` environment variable, so the default test run (which has
//! no Redis) skips it entirely. Run it deliberately with, e.g.:
//!
//! ```text
//! MCS_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!   cargo test -p mcs-server --test cluster_redis -- --ignored
//! ```

use std::sync::Arc;

use mcs_cluster::{NodeInfo, NodeRegistry, RedisNodeRegistry};

/// A unique key prefix per run so concurrent invocations (or stale keys) cannot
/// pollute the assertions.
fn unique_prefix() -> String {
    format!("mcs:test:cluster:{}:node:", uuid::Uuid::new_v4())
}

#[tokio::test]
#[ignore = "requires a live Redis; set MCS_TEST_REDIS_URL to run"]
async fn redis_membership_register_and_leave_round_trip() {
    let Ok(url) = std::env::var("MCS_TEST_REDIS_URL") else {
        eprintln!("MCS_TEST_REDIS_URL unset; skipping");
        return;
    };
    let prefix = unique_prefix();

    let node_a = NodeInfo::new("node-a", "http://10.0.0.1:8080");
    let node_b = NodeInfo::new("node-b", "http://10.0.0.2:8080");

    let reg_a: Arc<dyn NodeRegistry> = Arc::new(
        RedisNodeRegistry::connect(&url, node_a.clone(), 30)
            .await
            .expect("connect a")
            .with_prefix(prefix.clone()),
    );
    let reg_b: Arc<dyn NodeRegistry> = Arc::new(
        RedisNodeRegistry::connect(&url, node_b.clone(), 30)
            .await
            .expect("connect b")
            .with_prefix(prefix.clone()),
    );

    reg_a.register().await.expect("register a");
    reg_b.register().await.expect("register b");

    // Both nodes are live; ownership of a sample game resolves to one of them.
    let nodes = reg_a.live_nodes().await.expect("live nodes");
    assert_eq!(nodes.len(), 2, "both nodes live; got {nodes:?}");
    let owner = mcs_cluster::owner("game-123", &nodes).expect("an owner");
    assert!(owner.id == node_a.id || owner.id == node_b.id);

    // node-b leaves; only node-a remains, and every game now owns to node-a.
    reg_b.leave().await.expect("leave b");
    let nodes = reg_a.live_nodes().await.expect("live nodes after leave");
    assert_eq!(nodes, vec![node_a.clone()], "only node-a remains");
    assert!(mcs_cluster::is_owner("game-123", &node_a.id, &nodes));

    // Clean up.
    reg_a.leave().await.expect("leave a");
}
