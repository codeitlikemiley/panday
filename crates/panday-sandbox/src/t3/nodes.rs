//! Pool nodes, and where a session lives (docs/22 §shape 2, M22.4).
//!
//! > "each stateless, horizontally scalable; session-actor affinity via consistent hashing on
//! > session_id at the LB (an actor lives on one node; failover = fold the log on another)."
//!
//! Two responsibilities, and the second is the interesting one:
//!
//! - **Placement.** A session hashes to a node, so every request for it lands where its actor
//!   already is. Consistent hashing rather than modulo: adding a node must move a *fraction* of
//!   sessions, not remap all of them, or every scale-up is a stampede of cold actors.
//! - **Failover.** When a node dies, its sessions are placed elsewhere and resume by folding their
//!   log (ADR-002). Nothing is migrated, because there is nothing to migrate — the log is the state.
//!   That is the whole reason this is cheap, and it is why "kill a node mid-exec" is a latency event
//!   rather than a data-loss event.
//!
//! **What a chaos test can prove without a cluster.** That placement is stable, that removing a node
//! moves only its own sessions, that a resumed session folds to the same state it had, and that a
//! session is never placed on a node known to be down. What needs real nodes is the *timing* — how
//! long a fold takes on a warm cache, and whether the LB notices the death before the client does.

use std::collections::{BTreeMap, BTreeSet};

/// A node in the pool, as the placer sees it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeId(pub String);

/// Where sessions may be placed.
///
/// Health is an input rather than something this discovers: liveness belongs to the thing that can
/// see the network, and a placer that made its own judgement would disagree with the load balancer
/// at exactly the wrong moment.
#[derive(Debug, Default)]
pub struct Ring {
    /// `hash -> node`, one entry per virtual node.
    points: BTreeMap<u64, NodeId>,
    nodes: BTreeSet<NodeId>,
    /// How many points each node gets. More points, smoother distribution; the cost is memory and
    /// nothing else.
    replicas: u32,
}

impl Ring {
    pub fn new(replicas: u32) -> Self {
        Self {
            points: BTreeMap::new(),
            nodes: BTreeSet::new(),
            replicas: replicas.max(1),
        }
    }

    pub fn with_nodes(replicas: u32, nodes: &[&str]) -> Self {
        let mut ring = Self::new(replicas);
        for node in nodes {
            ring.add(NodeId((*node).to_string()));
        }
        ring
    }

    pub fn add(&mut self, node: NodeId) {
        if !self.nodes.insert(node.clone()) {
            return;
        }
        for replica in 0..self.replicas {
            self.points
                .insert(hash(&format!("{}#{replica}", node.0)), node.clone());
        }
    }

    /// Remove a node — a death, a drain, or a scale-down. The sessions it held are simply placed
    /// again; nothing has to be told where they went.
    pub fn remove(&mut self, node: &NodeId) {
        if !self.nodes.remove(node) {
            return;
        }
        self.points.retain(|_, n| n != node);
    }

    pub fn nodes(&self) -> Vec<NodeId> {
        self.nodes.iter().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Which node owns this session.
    ///
    /// The first point clockwise of the session's hash, wrapping — the standard construction, and
    /// the reason removing a node only moves that node's own keys.
    pub fn place(&self, session_id: &str) -> Option<NodeId> {
        if self.points.is_empty() {
            return None;
        }
        let at = hash(session_id);
        self.points
            .range(at..)
            .next()
            .or_else(|| self.points.iter().next())
            .map(|(_, node)| node.clone())
    }
}

/// FNV-1a with a finalizing avalanche. Small, dependency-free, and — with the second half —
/// uniform enough for a ring. A ring wants uniformity, not cryptographic resistance: nobody is
/// choosing session ids to collide with a node boundary.
///
/// **The avalanche is not optional, and a test proved it.** Plain FNV-1a over sequential inputs
/// (`session-0000`, `session-0001`, …) varies almost entirely in the low bits, so every one of them
/// landed in the same arc of the ring: 1,000 sessions split 200/100/500/200 across four nodes, and
/// adding a fifth node moved *zero* keys — a ring with none of the properties a ring is for. The
/// fmix64 finalizer spreads the low-bit differences across all 64 bits, which is the whole job.
fn hash(value: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // fmix64 (murmur3's finalizer).
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// What happened to a session when its node went away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reassignment {
    pub session_id: String,
    pub from: NodeId,
    pub to: NodeId,
}

/// Move every session off a dead node.
///
/// Returns the reassignments so a caller can log them and a test can assert on them — a failover
/// that happens silently is one nobody can distinguish from a failover that did not happen.
///
/// Sessions on *other* nodes are deliberately not touched. A drain that reshuffled healthy sessions
/// would turn one node's death into every session's cold start, which is the failure mode this whole
/// construction exists to avoid.
pub fn drain(ring: &mut Ring, dead: &NodeId, sessions: &[String]) -> Vec<Reassignment> {
    let before: Vec<(String, Option<NodeId>)> = sessions
        .iter()
        .map(|s| (s.clone(), ring.place(s)))
        .collect();

    ring.remove(dead);

    before
        .into_iter()
        .filter_map(|(session_id, was)| {
            let was = was?;
            if &was != dead {
                return None;
            }
            let now = ring.place(&session_id)?;
            Some(Reassignment {
                session_id,
                from: was,
                to: now,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("session-{i:04}")).collect()
    }

    #[test]
    fn placement_is_stable() {
        // The property the load balancer depends on: the same session always lands on the same node,
        // or every request is a cold actor.
        let ring = Ring::with_nodes(64, &["a", "b", "c"]);
        for session in sessions(50) {
            let first = ring.place(&session);
            assert!(first.is_some());
            assert_eq!(first, ring.place(&session));
        }
    }

    #[test]
    fn an_empty_ring_places_nothing_rather_than_panicking() {
        // A pool with no healthy nodes is a real state — a rolling deploy, a bad region — and the
        // honest answer is "nowhere", not a modulo by zero.
        assert_eq!(Ring::new(64).place("session-1"), None);
    }

    #[test]
    fn adding_a_node_moves_a_fraction_rather_than_everything() {
        // Consistent hashing's entire reason for existing here. With modulo, adding a fourth node
        // remaps three quarters of sessions and every one of them cold-starts.
        let all = sessions(400);
        let before = Ring::with_nodes(128, &["a", "b", "c"]);
        let mut after = Ring::with_nodes(128, &["a", "b", "c"]);
        after.add(NodeId("d".into()));

        let moved = all
            .iter()
            .filter(|s| before.place(s) != after.place(s))
            .count();
        // Ideally 1/4 of keys move. Allow slack for hash variance; the point is that it is nowhere
        // near all of them.
        assert!(
            (40..=160).contains(&moved),
            "{moved} of {} sessions moved when a fourth node joined",
            all.len()
        );
    }

    #[test]
    fn a_dead_node_moves_only_its_own_sessions() {
        // One node's death must not be every session's cold start.
        let all = sessions(400);
        let mut ring = Ring::with_nodes(128, &["a", "b", "c", "d"]);
        let placed_before: Vec<_> = all.iter().map(|s| ring.place(s)).collect();

        let dead = NodeId("c".into());
        let moves = drain(&mut ring, &dead, &all);

        // Everything that was on the dead node moved, and nothing else did.
        for (session, before) in all.iter().zip(&placed_before) {
            let after = ring.place(session);
            if before.as_ref() == Some(&dead) {
                assert_ne!(after.as_ref(), Some(&dead));
                assert!(moves.iter().any(|m| &m.session_id == session));
            } else {
                assert_eq!(&after, before, "a healthy session was reshuffled");
            }
        }
        assert!(!moves.is_empty(), "the dead node held nothing to move");
        assert!(moves.iter().all(|m| m.from == dead && m.to != dead));
    }

    #[test]
    fn draining_the_last_node_reports_nothing_rather_than_inventing_a_home() {
        // A one-node pool losing its node has nowhere to place anything, and saying so beats
        // reporting a reassignment to a node that does not exist.
        let mut ring = Ring::with_nodes(64, &["only"]);
        let moves = drain(&mut ring, &NodeId("only".into()), &sessions(10));
        assert!(moves.is_empty());
        assert!(ring.is_empty());
        assert_eq!(ring.place("session-0"), None);
    }

    #[test]
    fn a_session_is_never_placed_on_a_removed_node() {
        // The failover invariant. If this can fail, "kill a node" becomes a black hole rather than a
        // latency event.
        let mut ring = Ring::with_nodes(128, &["a", "b", "c"]);
        ring.remove(&NodeId("b".into()));
        for session in sessions(200) {
            assert_ne!(ring.place(&session), Some(NodeId("b".into())));
        }
    }

    #[test]
    fn removing_something_that_was_never_there_changes_nothing() {
        let mut ring = Ring::with_nodes(64, &["a", "b"]);
        let before: Vec<_> = sessions(20).iter().map(|s| ring.place(s)).collect();
        ring.remove(&NodeId("ghost".into()));
        let after: Vec<_> = sessions(20).iter().map(|s| ring.place(s)).collect();
        assert_eq!(before, after);
        assert_eq!(ring.nodes().len(), 2);
    }

    #[test]
    fn the_distribution_is_not_wildly_lopsided() {
        // A ring that put 90% of sessions on one node would satisfy every test above and be useless.
        let ring = Ring::with_nodes(128, &["a", "b", "c", "d"]);
        let mut counts: BTreeMap<NodeId, usize> = BTreeMap::new();
        for session in sessions(1_000) {
            *counts.entry(ring.place(&session).unwrap()).or_default() += 1;
        }
        assert_eq!(counts.len(), 4, "a node received nothing at all");
        let (min, max) = (
            *counts.values().min().unwrap(),
            *counts.values().max().unwrap(),
        );
        assert!(max < min * 3, "lopsided: {counts:?}");
    }
}
