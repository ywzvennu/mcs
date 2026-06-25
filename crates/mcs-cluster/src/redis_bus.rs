//! Redis pub/sub-backed [`EventBus`] (enabled by the `redis` feature).
//!
//! Each topic maps directly to a Redis pub/sub channel, namespaced under a
//! prefix so multiple logical clusters can share one Redis. Publishing on one
//! node `PUBLISH`es to the channel; subscribing on any other node opens a
//! dedicated `SUBSCRIBE` connection and streams every message — this is what
//! carries a spectator frame from the game's owner node to a watcher attached to
//! a different node.
//!
//! ## Why two connection kinds
//!
//! The async `redis` crate's [`ConnectionManager`] multiplexes ordinary
//! commands (so `PUBLISH` shares the same auto-reconnecting connection as the
//! node registry), but a `SUBSCRIBE` puts a connection into pub/sub mode where
//! it can no longer issue ordinary commands. So each [`subscribe`](RedisEventBus::subscribe)
//! takes a *fresh* dedicated connection from the client and drives it as a
//! [`PubSub`], leaving the shared manager free for publishes.
//!
//! See the [`EventBus`] / module docs for the at-most-once, best-effort
//! delivery model: Redis pub/sub drops messages for a subscriber that is not
//! connected at publish time, which the spectator path tolerates by
//! bootstrapping from a durable reconstruction.

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use tokio_stream::StreamExt;

use crate::bus::{EventBus, TopicStream};
use crate::error::ClusterError;

/// Default channel-name prefix for bus topics. Keeps spectator channels from
/// colliding with the membership keys (`mcs:cluster:node:*`) or another
/// cluster's traffic on a shared Redis.
const DEFAULT_PREFIX: &str = "mcs:bus:";

/// An [`EventBus`] backed by Redis pub/sub.
///
/// Construct one per process with [`connect`](RedisEventBus::connect). It holds
/// the [`Client`](redis::Client) (used to open a fresh dedicated connection per
/// subscription) and a shared [`ConnectionManager`] (used for publishes, and so
/// auto-reconnecting). Cloning is cheap and shares both, so the one bus slots
/// into the actor-spawn path and `AppState` like the other shared handles.
#[derive(Clone)]
pub struct RedisEventBus {
    client: redis::Client,
    publish_conn: ConnectionManager,
    prefix: String,
}

impl std::fmt::Debug for RedisEventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither the client nor the `ConnectionManager` is `Debug`; summarize.
        f.debug_struct("RedisEventBus")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl RedisEventBus {
    /// Connects to Redis at `url`, preparing the shared publish connection.
    ///
    /// The [`ConnectionManager`] used for publishes reconnects transparently, so
    /// the bus tolerates brief Redis outages on the publish side. Subscriptions
    /// each open their own connection on demand.
    ///
    /// # Errors
    ///
    /// Returns [`ClusterError::Backend`] if the URL is invalid or the initial
    /// connection cannot be established.
    pub async fn connect(url: &str) -> Result<Self, ClusterError> {
        let client = redis::Client::open(url)?;
        let publish_conn = client.get_connection_manager().await?;
        Ok(Self {
            client,
            publish_conn,
            prefix: DEFAULT_PREFIX.to_owned(),
        })
    }

    /// Overrides the channel-name prefix (default `mcs:bus:`). Useful to isolate
    /// multiple logical clusters that share one Redis.
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The fully-qualified Redis channel name for a logical `topic`.
    fn channel(&self, topic: &str) -> String {
        format!("{}{}", self.prefix, topic)
    }
}

#[async_trait]
impl EventBus for RedisEventBus {
    async fn publish(&self, topic: &str, bytes: &[u8]) -> Result<(), ClusterError> {
        let mut conn = self.publish_conn.clone();
        // The integer reply (number of clients that received it) is irrelevant;
        // a publish with zero subscribers is a success, per the best-effort model.
        conn.publish::<_, _, ()>(self.channel(topic), bytes).await?;
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<TopicStream, ClusterError> {
        // Take a fresh dedicated connection: a SUBSCRIBE monopolizes its
        // connection, so it must not be the shared publish manager.
        let mut pubsub = self.client.get_async_pubsub().await?;
        pubsub.subscribe(self.channel(topic)).await?;

        // `into_on_message` yields a stream of pub/sub messages for the life of
        // the connection; map each to its raw payload bytes. The `PubSub`
        // connection is owned by the stream, so it stays subscribed until the
        // stream (and thus this subscriber) is dropped.
        let stream = pubsub
            .into_on_message()
            .map(|msg| msg.get_payload_bytes().to_vec());
        Ok(Box::pin(stream))
    }
}
