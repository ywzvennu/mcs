//! A pluggable, topic-based event bus for cross-node fan-out (#109).
//!
//! The bus is the transport that lets one node publish a small frame on a named
//! topic and have it delivered to every subscriber of that topic — whether the
//! subscriber lives in the same process or on another node. Its motivating use
//! is **spectator broadcast**: the node that owns a game publishes a
//! spectator-safe snapshot frame after each move, and a spectator connected to
//! *any* node streams those frames by subscribing to the game's topic.
//!
//! ## Two implementations, one trait
//!
//! - [`LocalEventBus`] is the default. It keeps a process-local map of
//!   `tokio::broadcast` channels keyed by topic, so a single-node deployment —
//!   and the whole test suite — fans out with no external service and no Redis.
//!   Single-node behaviour is identical to before: the owner publishes and the
//!   in-process subscriber receives, all within one process.
//! - [`RedisEventBus`](crate::RedisEventBus) (behind the `redis` feature) maps
//!   each topic to a Redis pub/sub channel, so publishing on one node reaches
//!   subscribers on every other node.
//!
//! ## Delivery model & caveats
//!
//! The bus is **at-most-once** and **best-effort**, exactly like the underlying
//! `tokio::broadcast` and Redis pub/sub:
//!
//! - A subscriber that is not connected when a frame is published does not see
//!   it (there is no replay/backlog). Spectators bootstrap from a durable
//!   reconstruction, so a missed early frame is not fatal.
//! - A slow subscriber that falls behind the channel's buffer may **skip**
//!   frames (`tokio::broadcast` reports `Lagged`; the boxed stream simply
//!   resumes at the newest frame). Each frame is a *full* spectator snapshot, so
//!   a skipped frame self-heals on the next one.
//! - Ordering is per-topic FIFO for frames a subscriber actually observes; it
//!   makes no cross-topic ordering guarantee.
//!
//! Callers that need exactly-once or gap-free delivery must layer it themselves;
//! the spectator path deliberately does not, because a dropped intermediate
//! snapshot is harmless.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::error::ClusterError;

/// How many frames a topic's broadcast channel retains for a slow subscriber
/// before it begins skipping the oldest.
///
/// Each frame is a full spectator snapshot, so a subscriber that lags past this
/// simply resumes from the newest frame and is immediately consistent again. A
/// few hundred frames comfortably buffers a whole game for a briefly-stalled
/// watcher.
const TOPIC_CHANNEL_CAPACITY: usize = 256;

/// A boxed, owned stream of frames delivered on a subscribed topic.
///
/// Returned by [`EventBus::subscribe`]. Each item is one published frame's raw
/// bytes, in the order the subscriber observes them. The stream ends when the
/// bus can deliver nothing further (every publisher gone, or the backend
/// dropped); a transient per-frame decode hiccup is skipped rather than ending
/// the stream.
pub type TopicStream = Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>;

/// A topic-based publish/subscribe transport for cross-node fan-out.
///
/// Implementations move opaque `bytes` from a [`publish`](EventBus::publish) on
/// one node to every live [`subscribe`](EventBus::subscribe) of the same
/// `topic`, in the process or across the cluster. The payload is opaque to the
/// bus: callers serialize their own frames (the spectator path sends a JSON
/// snapshot) and the bus never inspects them.
///
/// See the [module docs](self) for the at-most-once / best-effort delivery
/// model.
#[async_trait]
pub trait EventBus: Send + Sync {
    /// Publishes `bytes` on `topic`, delivering them to every current
    /// subscriber.
    ///
    /// Delivery is best-effort: a publish with no subscribers succeeds and the
    /// frame is dropped. Returns once the frame has been handed to the backend,
    /// not once it has been received.
    ///
    /// # Errors
    ///
    /// Returns [`ClusterError::Backend`] only if the backend itself fails (e.g.
    /// a Redis publish error). The in-process [`LocalEventBus`] never errors.
    async fn publish(&self, topic: &str, bytes: &[u8]) -> Result<(), ClusterError>;

    /// Subscribes to `topic`, returning a stream of every frame published on it
    /// from now on.
    ///
    /// Frames published *before* this call are not replayed. A subscriber that
    /// falls behind may skip frames (see the [module docs](self)); since each
    /// spectator frame is a full snapshot, the next delivered frame resynchronises
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`ClusterError::Backend`] only if the backend cannot establish the
    /// subscription (e.g. Redis is unreachable). The in-process
    /// [`LocalEventBus`] never errors.
    async fn subscribe(&self, topic: &str) -> Result<TopicStream, ClusterError>;
}

/// The process-local default [`EventBus`]: a registry of `tokio::broadcast`
/// channels keyed by topic.
///
/// Needs no external backend, so it is what a single-node deployment and the
/// whole test suite run on. Publishing on a topic sends to the in-process
/// broadcast channel for that topic; subscribing returns a stream over a fresh
/// receiver on it. A topic's channel is created lazily on first publish or
/// subscribe and kept for the bus's lifetime (the count of distinct game topics
/// is bounded by live games), so a late subscriber to an active topic still
/// attaches to the same channel the publisher uses.
///
/// Cheap to clone — every clone shares the one channel registry through an
/// [`Arc`] — so it slots into the actor-spawn path and `AppState` like the other
/// shared handles.
#[derive(Clone, Default)]
pub struct LocalEventBus {
    /// One broadcast sender per topic. Guarded by a `std::sync::Mutex` held only
    /// for the brief lookup-or-create; it is never held across an `.await`.
    topics: Arc<Mutex<HashMap<String, broadcast::Sender<Vec<u8>>>>>,
}

impl LocalEventBus {
    /// Creates an empty local bus with no topics yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the broadcast sender for `topic`, creating it on first use.
    ///
    /// The lock is held only for the map lookup/insert, never across the channel
    /// operations the caller then performs.
    fn sender(&self, topic: &str) -> broadcast::Sender<Vec<u8>> {
        let mut topics = self.topics.lock().expect("event bus lock poisoned");
        if let Some(sender) = topics.get(topic) {
            return sender.clone();
        }
        let (sender, _) = broadcast::channel(TOPIC_CHANNEL_CAPACITY);
        topics.insert(topic.to_owned(), sender.clone());
        sender
    }
}

impl std::fmt::Debug for LocalEventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let topics = self.topics.lock().map(|t| t.len()).unwrap_or(0);
        f.debug_struct("LocalEventBus")
            .field("topics", &topics)
            .finish()
    }
}

#[async_trait]
impl EventBus for LocalEventBus {
    async fn publish(&self, topic: &str, bytes: &[u8]) -> Result<(), ClusterError> {
        // A send error means there are simply no subscribers right now, which is
        // not a failure: the frame is dropped, exactly the best-effort contract.
        let _ = self.sender(topic).send(bytes.to_vec());
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<TopicStream, ClusterError> {
        let receiver = self.sender(topic).subscribe();
        // Map the broadcast receiver into a byte stream, silently skipping a
        // `Lagged` gap: the next full snapshot frame resynchronises the watcher.
        let stream = BroadcastStream::new(receiver).filter_map(Result::ok);
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_a_published_frame_to_a_subscriber() {
        let bus = LocalEventBus::new();
        let mut stream = bus.subscribe("game:1:spectator").await.unwrap();

        bus.publish("game:1:spectator", b"hello").await.unwrap();

        let frame = stream.next().await.expect("a frame");
        assert_eq!(frame, b"hello");
    }

    #[tokio::test]
    async fn fans_out_to_every_subscriber_of_a_topic() {
        let bus = LocalEventBus::new();
        let mut a = bus.subscribe("t").await.unwrap();
        let mut b = bus.subscribe("t").await.unwrap();

        bus.publish("t", b"frame").await.unwrap();

        assert_eq!(a.next().await.unwrap(), b"frame");
        assert_eq!(b.next().await.unwrap(), b"frame");
    }

    #[tokio::test]
    async fn topics_are_isolated() {
        let bus = LocalEventBus::new();
        let mut watcher = bus.subscribe("game:1:spectator").await.unwrap();

        // A frame on a different topic must never reach this subscriber.
        bus.publish("game:2:spectator", b"other").await.unwrap();
        bus.publish("game:1:spectator", b"mine").await.unwrap();

        assert_eq!(watcher.next().await.unwrap(), b"mine");
    }

    #[tokio::test]
    async fn publish_without_subscribers_succeeds_and_is_dropped() {
        let bus = LocalEventBus::new();
        // No subscriber yet: the frame is dropped, but the publish still succeeds.
        bus.publish("lonely", b"void").await.unwrap();

        // A subscriber that attaches afterward sees only frames from now on.
        let mut late = bus.subscribe("lonely").await.unwrap();
        bus.publish("lonely", b"seen").await.unwrap();
        assert_eq!(late.next().await.unwrap(), b"seen");
    }

    #[tokio::test]
    async fn a_cloned_bus_shares_the_same_topics() {
        let bus = LocalEventBus::new();
        let clone = bus.clone();
        let mut stream = bus.subscribe("shared").await.unwrap();

        // Publishing through the clone reaches a subscriber of the original.
        clone.publish("shared", b"via-clone").await.unwrap();
        assert_eq!(stream.next().await.unwrap(), b"via-clone");
    }
}
