//! Integration tests for the Redis-backed registry.
//!
//! These require a live Redis. They are therefore **doubly gated**:
//!
//! * compiled only under `--features redis`;
//! * marked `#[ignore]` so plain `cargo test` stays green, and they short-circuit
//!   immediately unless `MCS_TEST_REDIS_URL` points at a reachable Redis.
//!
//! Run them with, e.g.:
//!
//! ```text
//! MCS_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!   cargo test -p mcs-cluster --features redis -- --ignored
//! ```

#![cfg(feature = "redis")]

use mcs_cluster::{owner, NodeInfo, NodeRegistry, RedisNodeRegistry};

/// Returns the configured Redis URL, or `None` to signal "skip this test".
fn redis_url() -> Option<String> {
    std::env::var("MCS_TEST_REDIS_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

/// A unique key prefix per test run so concurrent runs (and leftover keys from a
/// crashed run) never bleed into each other.
fn unique_prefix(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("mcs:test:{tag}:{nanos}:")
}

#[tokio::test]
#[ignore = "requires a live Redis at MCS_TEST_REDIS_URL"]
async fn two_nodes_see_each_other_and_agree_on_owner() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: MCS_TEST_REDIS_URL not set");
        return;
    };
    let prefix = unique_prefix("agree");

    let a = RedisNodeRegistry::connect(&url, NodeInfo::new("node-a", "http://a:8080"), 30)
        .await
        .unwrap()
        .with_prefix(&prefix);
    let b = RedisNodeRegistry::connect(&url, NodeInfo::new("node-b", "http://b:8080"), 30)
        .await
        .unwrap()
        .with_prefix(&prefix);

    a.register().await.unwrap();
    b.register().await.unwrap();

    let from_a = a.live_nodes().await.unwrap();
    let from_b = b.live_nodes().await.unwrap();

    // Both registries observe both nodes...
    assert_eq!(from_a.len(), 2, "node A should see both nodes");
    assert_eq!(from_b.len(), 2, "node B should see both nodes");

    // ...and compute the same HRW owner for a sample game id.
    let owner_a = owner("sample-game", &from_a).unwrap().id.clone();
    let owner_b = owner("sample-game", &from_b).unwrap().id.clone();
    assert_eq!(owner_a, owner_b, "both nodes must agree on the owner");

    a.leave().await.unwrap();
    b.leave().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a live Redis at MCS_TEST_REDIS_URL"]
async fn node_disappears_after_ttl_lapses() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: MCS_TEST_REDIS_URL not set");
        return;
    };
    let prefix = unique_prefix("ttl");

    // A short TTL keeps the test quick.
    let ttl_secs = 1;
    let stayer = RedisNodeRegistry::connect(&url, NodeInfo::new("stayer", "http://s:8080"), 30)
        .await
        .unwrap()
        .with_prefix(&prefix);
    let leaver =
        RedisNodeRegistry::connect(&url, NodeInfo::new("leaver", "http://l:8080"), ttl_secs)
            .await
            .unwrap()
            .with_prefix(&prefix);

    stayer.register().await.unwrap();
    leaver.register().await.unwrap();
    assert_eq!(stayer.live_nodes().await.unwrap().len(), 2);

    // The leaver stops heartbeating; wait past its TTL.
    tokio::time::sleep(std::time::Duration::from_secs(ttl_secs + 2)).await;

    let live = stayer.live_nodes().await.unwrap();
    assert_eq!(live.len(), 1, "expired node must drop out of live_nodes");
    assert_eq!(live[0].id.as_str(), "stayer");

    stayer.leave().await.unwrap();
}
