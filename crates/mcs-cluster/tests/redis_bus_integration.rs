//! Integration test for the Redis pub/sub-backed [`EventBus`] (#109).
//!
//! It simulates two nodes by constructing **two independent** `RedisEventBus`
//! instances over the same Redis: one publishes a frame, the other (subscribed
//! first) must receive it — the cross-node spectator-broadcast property.
//!
//! Like the registry's Redis tests it is **doubly gated**:
//!
//! * compiled only under `--features redis`;
//! * marked `#[ignore]` so plain `cargo test` stays green, and it short-circuits
//!   unless `MCS_TEST_REDIS_URL` points at a reachable Redis.
//!
//! Run it with, e.g.:
//!
//! ```text
//! MCS_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!   cargo test -p mcs-cluster --features redis -- --ignored
//! ```

#![cfg(feature = "redis")]

use std::time::Duration;

use mcs_cluster::{EventBus, RedisEventBus};
use tokio_stream::StreamExt;

/// Returns the configured Redis URL, or `None` to signal "skip this test".
fn redis_url() -> Option<String> {
    std::env::var("MCS_TEST_REDIS_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

/// A unique topic per test run so concurrent runs (and leftover traffic) never
/// bleed into each other.
fn unique_topic(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test:{tag}:{nanos}:spectator")
}

#[tokio::test]
#[ignore = "requires a live Redis at MCS_TEST_REDIS_URL"]
async fn a_frame_published_on_one_bus_reaches_a_subscriber_on_another() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: MCS_TEST_REDIS_URL not set");
        return;
    };
    let topic = unique_topic("xnode");

    // Two independent bus instances over the same Redis stand in for two nodes.
    let publisher = RedisEventBus::connect(&url).await.unwrap();
    let subscriber = RedisEventBus::connect(&url).await.unwrap();

    // Subscribe *before* publishing: Redis pub/sub drops messages for a channel
    // with no current subscribers, exactly the at-most-once model.
    let mut stream = subscriber.subscribe(&topic).await.unwrap();

    // Give the SUBSCRIBE a moment to register on the server before publishing.
    tokio::time::sleep(Duration::from_millis(100)).await;

    publisher.publish(&topic, b"spectator-frame").await.unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a frame should arrive before the timeout")
        .expect("the stream should yield a frame");
    assert_eq!(frame, b"spectator-frame");
}

#[tokio::test]
#[ignore = "requires a live Redis at MCS_TEST_REDIS_URL"]
async fn topics_are_isolated_across_the_bus() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: MCS_TEST_REDIS_URL not set");
        return;
    };
    let mine = unique_topic("mine");
    let other = unique_topic("other");

    let publisher = RedisEventBus::connect(&url).await.unwrap();
    let subscriber = RedisEventBus::connect(&url).await.unwrap();

    let mut stream = subscriber.subscribe(&mine).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A frame on a different topic must never reach this subscriber...
    publisher.publish(&other, b"nope").await.unwrap();
    // ...only the one published on the subscribed topic.
    publisher.publish(&mine, b"yes").await.unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a frame should arrive before the timeout")
        .expect("the stream should yield a frame");
    assert_eq!(frame, b"yes");
}
