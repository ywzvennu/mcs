//! Rendezvous (highest-random-weight) ownership directory.
//!
//! Every node in the cluster runs the *same* pure function over the *same* set
//! of live nodes to decide which node owns a given game. Because the decision
//! is a deterministic function of `(node_id, game_id)`, nodes agree without any
//! coordination traffic: there is no leader, no gossip, no lock.
//!
//! ## Why rendezvous hashing
//!
//! For each candidate node we compute a weight `hash(node_id, game_id)` and pick
//! the node with the largest weight. This is [rendezvous hashing][hrw], also
//! called highest-random-weight (HRW) hashing.
//!
//! Its defining property is *minimal disruption*: when a node joins or leaves,
//! only the games that map to (or away from) that node move. Every other game
//! keeps its owner, because the relative ordering of the surviving nodes'
//! weights is unaffected. A naive `hash(game_id) % n` scheme, by contrast,
//! reshuffles almost everything whenever `n` changes.
//!
//! [hrw]: https://en.wikipedia.org/wiki/Rendezvous_hashing
//!
//! ## The hash function
//!
//! Weights are computed with **FNV-1a (64-bit)**, a small, well-specified,
//! seedless hash. Determinism across processes and machines is the whole point,
//! so a seeded or randomized hasher (such as the standard library's
//! `RandomState`) must not be used — two nodes seeded differently would
//! disagree on owners. FNV-1a is chosen over `DefaultHasher` because the latter
//! is explicitly documented as not guaranteed stable across Rust versions,
//! whereas FNV-1a is a fixed, externally specified algorithm we implement here.

use crate::types::{NodeId, NodeInfo};

/// 64-bit FNV-1a offset basis (see <http://www.isthe.com/chongo/tech/comp/fnv/>).
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// 64-bit FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Computes the rendezvous weight of `(node_id, game_id)` with FNV-1a.
///
/// The two inputs are folded into one stream separated by a `0xff` byte (which
/// cannot appear inside a UTF-8 scalar value) so that, for example,
/// `("ab", "c")` and `("a", "bc")` hash differently.
fn weight(node_id: &str, game_id: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    let mut mix = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    mix(node_id.as_bytes());
    mix(&[0xff]);
    mix(game_id.as_bytes());
    hash
}

/// Returns the owner of `game_id` from `nodes` using rendezvous hashing.
///
/// The owner is the node maximizing `weight(node.id, game_id)`. Ties (which are
/// astronomically unlikely for a 64-bit hash) are broken by `NodeId` ordering so
/// the result stays deterministic. Returns `None` when `nodes` is empty.
#[must_use]
pub fn owner<'a>(game_id: &str, nodes: &'a [NodeInfo]) -> Option<&'a NodeInfo> {
    nodes.iter().max_by(|a, b| {
        let wa = weight(a.id.as_str(), game_id);
        let wb = weight(b.id.as_str(), game_id);
        // Break weight ties on the id so ordering is total and stable.
        wa.cmp(&wb).then_with(|| a.id.cmp(&b.id))
    })
}

/// Returns `true` when `this` is the rendezvous owner of `game_id`.
///
/// Equivalent to `owner(game_id, nodes).map(|n| &n.id) == Some(this)`, but spelled
/// out as the question each node actually asks: *do I own this game?*
#[must_use]
pub fn is_owner(game_id: &str, this: &NodeId, nodes: &[NodeInfo]) -> bool {
    owner(game_id, nodes).is_some_and(|n| &n.id == this)
}

/// Zero-state rendezvous directory.
///
/// The ownership logic is pure and holds no state, so this struct exists only to
/// give callers a value to pass around (for dependency injection, or to leave
/// room for a future stateful directory) and to provide a [`Directory`] trait
/// implementation. The inherent and trait methods both delegate to the free
/// functions [`owner`] and [`is_owner`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HrwDirectory;

impl HrwDirectory {
    /// Constructs the directory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Maps games to owning nodes.
///
/// Implementations must be pure and deterministic: the same `(game_id, nodes)`
/// must always yield the same owner, on every node, with no I/O.
pub trait Directory {
    /// Returns the owner of `game_id`, or `None` if `nodes` is empty.
    fn owner<'a>(&self, game_id: &str, nodes: &'a [NodeInfo]) -> Option<&'a NodeInfo>;

    /// Returns `true` when `this` owns `game_id`.
    fn is_owner(&self, game_id: &str, this: &NodeId, nodes: &[NodeInfo]) -> bool {
        self.owner(game_id, nodes).is_some_and(|n| &n.id == this)
    }
}

impl Directory for HrwDirectory {
    fn owner<'a>(&self, game_id: &str, nodes: &'a [NodeInfo]) -> Option<&'a NodeInfo> {
        owner(game_id, nodes)
    }

    fn is_owner(&self, game_id: &str, this: &NodeId, nodes: &[NodeInfo]) -> bool {
        is_owner(game_id, this, nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn nodes(ids: &[&str]) -> Vec<NodeInfo> {
        ids.iter()
            .map(|id| NodeInfo::new(*id, format!("http://{id}:8080")))
            .collect()
    }

    #[test]
    fn empty_node_set_has_no_owner() {
        assert!(owner("game-1", &[]).is_none());
        assert!(!is_owner("game-1", &NodeId::from("a"), &[]));
    }

    #[test]
    fn ownership_is_deterministic() {
        let ns = nodes(&["a", "b", "c"]);
        let first = owner("game-42", &ns).unwrap().id.clone();
        for _ in 0..100 {
            assert_eq!(owner("game-42", &ns).unwrap().id, first);
        }
        // And the trait impl agrees with the free function.
        let dir = HrwDirectory::new();
        assert_eq!(dir.owner("game-42", &ns).unwrap().id, first);
    }

    #[test]
    fn order_of_nodes_does_not_change_owner() {
        let ascending = nodes(&["a", "b", "c"]);
        let descending = nodes(&["c", "b", "a"]);
        for g in 0..200 {
            let game = format!("game-{g}");
            assert_eq!(
                owner(&game, &ascending).unwrap().id,
                owner(&game, &descending).unwrap().id,
                "ordering of the live set must not affect ownership"
            );
        }
    }

    #[test]
    fn every_game_maps_to_some_node() {
        let ns = nodes(&["a", "b", "c"]);
        for g in 0..1000 {
            let game = format!("game-{g}");
            assert!(owner(&game, &ns).is_some());
        }
    }

    #[test]
    fn distribution_is_reasonably_even() {
        let ns = nodes(&["a", "b", "c"]);
        let total = 30_000;
        let mut counts: HashMap<NodeId, u32> = HashMap::new();
        for g in 0..total {
            let game = format!("game-{g}");
            let id = owner(&game, &ns).unwrap().id.clone();
            *counts.entry(id).or_default() += 1;
        }
        let expected = total / ns.len() as u32;
        // Allow a generous 15% band; FNV-1a spreads these inputs well within it.
        let tolerance = (f64::from(expected) * 0.15) as u32;
        for node in &ns {
            let got = counts.get(&node.id).copied().unwrap_or(0);
            assert!(
                got.abs_diff(expected) <= tolerance,
                "node {} owned {got} games, expected ~{expected} (±{tolerance})",
                node.id
            );
        }
    }

    #[test]
    fn removing_a_node_only_reassigns_its_own_games() {
        let before = nodes(&["a", "b", "c"]);
        let after = nodes(&["a", "b"]); // node "c" left
        let removed = NodeId::from("c");

        let total = 5_000;
        for g in 0..total {
            let game = format!("game-{g}");
            let owner_before = owner(&game, &before).unwrap().id.clone();
            let owner_after = owner(&game, &after).unwrap().id.clone();

            if owner_before == removed {
                // Games owned by the departed node must move to a survivor...
                assert_ne!(owner_after, removed);
                assert!(owner_after == NodeId::from("a") || owner_after == NodeId::from("b"));
            } else {
                // ...and every other game keeps its previous owner (minimal movement).
                assert_eq!(
                    owner_before, owner_after,
                    "game {game} moved despite its owner staying live"
                );
            }
        }
    }

    #[test]
    fn is_owner_agrees_with_owner() {
        let ns = nodes(&["a", "b", "c"]);
        for g in 0..500 {
            let game = format!("game-{g}");
            let winner = owner(&game, &ns).unwrap().id.clone();
            for node in &ns {
                assert_eq!(is_owner(&game, &node.id, &ns), node.id == winner);
            }
        }
    }

    #[test]
    fn weight_separates_concatenation_ambiguity() {
        // ("ab","c") and ("a","bc") must not collide via naive concatenation.
        assert_ne!(weight("ab", "c"), weight("a", "bc"));
    }
}
