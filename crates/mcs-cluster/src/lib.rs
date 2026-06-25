//! # mcs-cluster
//!
//! Optional clustering primitives that let several MCS server instances agree —
//! with **no chatter between them** — on which node owns (runs the actor for)
//! each game.
//!
//! Two pieces compose to make that work:
//!
//! 1. A [`NodeRegistry`] tracks which nodes are currently alive, using a TTL so
//!    a crashed node disappears on its own.
//! 2. A rendezvous-hash [`Directory`] turns that live set into a deterministic
//!    per-game owner. Because the mapping is a pure function every node computes
//!    identically, agreement is free: there is no leader, no gossip, no lock.
//!
//! ## Cross-node event bus (#109)
//!
//! Beyond ownership, the crate provides a pluggable [`EventBus`]: a topic-based
//! publish/subscribe transport that fans a frame out to every subscriber of a
//! topic — in the process or across the cluster. Its motivating use is
//! **spectator broadcast**: a game's owner node publishes a spectator-safe
//! snapshot after each move and a watcher on any node streams it. The default
//! [`LocalEventBus`] is in-process (no Redis); a [`RedisEventBus`] behind the
//! `redis` feature carries frames between nodes over Redis pub/sub.
//!
//! ## No Redis required
//!
//! The crate's default features are empty. The pure directory, the single-node
//! [`LocalRegistry`], and the in-process [`LocalEventBus`] need no external
//! services, so a one-node deployment — and the entire test suite — runs with
//! nothing installed. The Redis-backed [`RedisNodeRegistry`] and
//! [`RedisEventBus`] are available behind the `redis` feature for horizontal
//! scaling.
//!
//! ## Example
//!
//! ```
//! use mcs_cluster::{HrwDirectory, Directory, NodeInfo};
//!
//! let nodes = vec![
//!     NodeInfo::new("node-a", "http://10.0.0.1:8080"),
//!     NodeInfo::new("node-b", "http://10.0.0.2:8080"),
//!     NodeInfo::new("node-c", "http://10.0.0.3:8080"),
//! ];
//!
//! let dir = HrwDirectory::new();
//! let owner = dir.owner("game-12345", &nodes).unwrap();
//! // Every node, given the same live set, picks the same owner for this game.
//! assert!(dir.is_owner("game-12345", &owner.id, &nodes));
//! ```

mod bus;
mod directory;
mod error;
mod registry;
mod types;

#[cfg(feature = "redis")]
mod redis;

#[cfg(feature = "redis")]
mod redis_bus;

pub use bus::{EventBus, LocalEventBus, TopicStream};
pub use directory::{is_owner, owner, Directory, HrwDirectory};
pub use error::ClusterError;
pub use registry::{LocalRegistry, NodeRegistry};
pub use types::{NodeId, NodeInfo};

#[cfg(feature = "redis")]
pub use redis::RedisNodeRegistry;

#[cfg(feature = "redis")]
pub use redis_bus::RedisEventBus;
