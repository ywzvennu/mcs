//! Identity types shared across the clustering primitives.

use serde::{Deserialize, Serialize};

/// Stable identifier for a server instance (a "node") in the cluster.
///
/// The string is opaque to this crate: any value that is unique and stable for
/// the lifetime of a process works. A common choice is a hostname, a pod name,
/// or a freshly minted UUID generated at boot. The same `NodeId` must be used
/// for registration *and* ownership decisions, otherwise the rendezvous hash
/// will not agree across nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// Borrows the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for NodeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for NodeId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A live node together with the address peers use to reach it.
///
/// `address` is whatever a peer needs to forward work to this node — typically
/// an HTTP base URL such as `http://10.0.0.7:8080`. This crate never dials the
/// address itself; it only stores and hands it back so the caller can route to
/// the owning node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Stable identity of the node.
    pub id: NodeId,
    /// How peers reach this node (e.g. `http://host:port`).
    pub address: String,
}

impl NodeInfo {
    /// Builds a [`NodeInfo`] from any [`NodeId`]-convertible value and an address.
    pub fn new(id: impl Into<NodeId>, address: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            address: address.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_roundtrips_through_serde() {
        let id = NodeId::from("alpha");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"alpha\"");
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn node_info_roundtrips_through_serde() {
        let info = NodeInfo::new("alpha", "http://host:8080");
        let json = serde_json::to_string(&info).unwrap();
        let back: NodeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, info);
    }

    #[test]
    fn node_id_display_and_as_str_match() {
        let id = NodeId::from("beta");
        assert_eq!(id.as_str(), "beta");
        assert_eq!(id.to_string(), "beta");
    }
}
