//! [`Dot`] — the **causal timestamp** the value-carrying CRDTs stamp their updates with.
//!
//! A grow-only counter needs no clock: `max` per replica is already a join. A register that
//! *overwrites* does — "the later write wins" is only meaningful against some notion of later, and
//! it may not be the wall clock. Two replicas' clocks disagree by seconds routinely and by hours
//! after a laptop wakes up in another timezone, so a wall-clock register loses whichever write
//! happened on the slow machine, arbitrarily and invisibly. A **Lamport** counter instead moves
//! forward on every local update and past everything a merge has ever shown this replica, so it
//! respects the causal order it can actually observe: if A's write reached B before B wrote, B's
//! write is later, and that is the only ordering claim a CRDT is entitled to make.
//!
//! Concurrent writes get the same counter, so the counter alone is not a total order. Pairing it
//! with the **replica id** completes it: `(counter, replica)` compared lexicographically is total
//! and identical on every replica, which is what makes an arbitrary-but-deterministic winner an
//! answer rather than a divergence. The replica id is the caller's own string, exactly as it is for
//! [`crate::GCounter`] — one id per node, supplied explicitly, never derived from anything local.

use serde::{Deserialize, Serialize};

/// A Lamport counter paired with the replica that stamped it.
///
/// Ordered by counter first, then replica id — the derive is load-bearing, and the field order with
/// it: swapping them would rank a replica named `"A"` above every write anyone else ever made.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dot {
    /// The logical time of the update. `0` is the "nothing has happened here" stamp.
    pub counter: u64,
    /// The replica that made the update — the tie-break among concurrent writes, and (with the
    /// counter) the identity of a single observed insertion in an [`crate::OrSet`].
    pub replica: String,
}

impl Dot {
    /// The stamp for an update made by `replica` at logical time `counter`.
    pub fn new(counter: u64, replica: &str) -> Dot {
        Dot {
            counter,
            replica: replica.to_string(),
        }
    }

    /// The stamp `replica` gets for an update applied to a state whose highest observed counter is
    /// `seen` — one tick past everything this replica knows about.
    ///
    /// Saturating, so the operation stays total: a counter that reached [`u64::MAX`] stops
    /// advancing rather than wrapping back into the past, which is the failure that would actually
    /// corrupt the order. (Reaching it requires 2⁶⁴ updates.)
    pub fn next_after(seen: u64, replica: &str) -> Dot {
        Dot::new(seen.saturating_add(1), replica)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_counter_outranks_the_replica_id() {
        // "A" < "Z" as text, but the counter is the outer key: a later write wins whoever made it.
        assert!(Dot::new(2, "A") > Dot::new(1, "Z"));
    }

    #[test]
    fn concurrent_stamps_break_the_tie_on_replica_id() {
        assert!(Dot::new(1, "B") > Dot::new(1, "A"));
        // Total and deterministic: every replica computes the same answer from the same pair.
        assert_eq!(
            Dot::new(1, "A").cmp(&Dot::new(1, "B")),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn the_next_stamp_passes_everything_seen_and_never_wraps() {
        assert_eq!(Dot::next_after(4, "A"), Dot::new(5, "A"));
        assert_eq!(Dot::next_after(u64::MAX, "A").counter, u64::MAX);
    }
}
