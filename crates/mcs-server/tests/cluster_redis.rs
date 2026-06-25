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
use std::time::Duration;

use mcs_cluster::{EventBus, NodeInfo, NodeRegistry, RedisEventBus, RedisNodeRegistry};
use mcs_core::{GameStatus, PlayerView};
use mcs_domain::GameId;
use mcs_game::{spectator_topic, SpectatorFrame};
use tokio_stream::StreamExt;

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

/// Cross-node spectator broadcast over Redis pub/sub (#109): a
/// [`SpectatorFrame`] published on one [`RedisEventBus`] instance (the "owner
/// node") is received on a **second**, independent instance (a "spectator
/// node"). This is the bus property the WS spectator path relies on to serve a
/// watcher attached to a node that does not own the game.
#[tokio::test]
#[ignore = "requires a live Redis; set MCS_TEST_REDIS_URL to run"]
async fn spectator_frame_crosses_nodes_over_redis() {
    let Ok(url) = std::env::var("MCS_TEST_REDIS_URL") else {
        eprintln!("MCS_TEST_REDIS_URL unset; skipping");
        return;
    };

    // A unique prefix per run so concurrent invocations never cross-talk.
    let prefix = format!("mcs:test:bus:{}:", uuid::Uuid::new_v4());
    let game_id = GameId::new();
    let topic = spectator_topic(game_id);

    // Two independent bus instances stand in for two nodes.
    let owner = RedisEventBus::connect(&url)
        .await
        .expect("connect owner bus")
        .with_prefix(prefix.clone());
    let spectator = RedisEventBus::connect(&url)
        .await
        .expect("connect spectator bus")
        .with_prefix(prefix);

    // The spectator subscribes first (Redis pub/sub drops messages with no
    // current subscriber), then waits briefly for the SUBSCRIBE to register.
    let mut stream = spectator.subscribe(&topic).await.expect("subscribe");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The owner publishes a spectator-safe frame.
    let frame = SpectatorFrame::new(
        PlayerView::new(serde_json::json!({ "fen": "after-e4" })),
        GameStatus::Ongoing,
        None,
        1,
    );
    owner
        .publish(&topic, &frame.to_bytes().expect("serialize frame"))
        .await
        .expect("publish frame");

    // The spectator node receives and decodes the very same frame.
    let bytes = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a frame arrives before timeout")
        .expect("the stream yields a frame");
    let received = SpectatorFrame::from_bytes(&bytes).expect("decode frame");
    assert_eq!(received, frame, "the cross-node frame round-trips intact");
    assert_eq!(received.ply, 1);
}
