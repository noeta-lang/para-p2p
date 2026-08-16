//! [`LwwRegister`] — a **last-write-wins register**: one value, overwritten, converging on the
//! later write.
//!
//! The lattices beside it can only gain information, which is what makes their merge a join and
//! also what stops any of them from representing "the title is now *this*". A register can, at the
//! price the name states plainly: when two replicas write concurrently, one write is *lost*. That
//! is a real semantic, not a bug — it is what a document title, a cursor position, or a status flag
//! wants — and the honest way to offer it is to make the loss deterministic rather than
//! whoever-syncs-last.
//!
//! # Why the merge is still a join
//!
//! The state is a `((counter, replica), value)` triple and the merge is the **maximum** under a
//! total order on it. A max over a total order is commutative, associative and idempotent by
//! construction, so the three laws hold for every pair of states, including ones no legitimate
//! sequence of updates could produce. That matters more than it sounds: the property tests
//! reconcile states from arbitrary operation logs, and a merge that only converged for
//! "well-formed" inputs would be a merge that diverges the first time a replica id is reused.
//!
//! The order is [`Dot`] first (see its docs for why a Lamport counter and not a clock), and the
//! **value itself** as the final tie-break. The value tie-break is unreachable in correct use — two
//! writes with the same counter *and* the same replica id can only come from one node's id being
//! used on two machines — and it is exactly the case where a register without it would leave two
//! replicas permanently disagreeing while both believing they had converged.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Mergeable;
use crate::clock::Dot;

/// A register holding one value, stamped with the causal time it was written at.
///
/// An **immutable value** like every CRDT here: [`set`](LwwRegister::set) returns a new register.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LwwRegister<V> {
    /// The causal time of the write this register is holding. `counter == 0` means "never written".
    stamp: Dot,
    /// The value, absent until the first write — a register is legitimately empty before anyone
    /// writes to it, and `none` says so without asking the caller for a placeholder.
    value: Option<V>,
}

impl<V> Default for LwwRegister<V> {
    fn default() -> LwwRegister<V> {
        LwwRegister {
            stamp: Dot::default(),
            value: None,
        }
    }
}

impl<V> LwwRegister<V> {
    /// The empty register — no value, logical time 0.
    pub fn new() -> LwwRegister<V> {
        LwwRegister::default()
    }

    /// A copy of this register holding `value`, written by `replica` one logical tick past
    /// everything this state has seen (its own writes and every write merged into it).
    pub fn set(&self, replica: &str, value: V) -> LwwRegister<V> {
        LwwRegister {
            stamp: Dot::next_after(self.stamp.counter, replica),
            value: Some(value),
        }
    }

    /// The current value, or `None` if nothing has ever been written.
    pub fn get(&self) -> Option<&V> {
        self.value.as_ref()
    }

    /// The causal stamp of the write being held — its logical time and the replica that made it.
    pub fn stamp(&self) -> &Dot {
        &self.stamp
    }
}

impl<V> Mergeable for LwwRegister<V>
where
    V: Ord + Clone + Serialize + DeserializeOwned,
{
    /// The later write, by the total order `(stamp, value)`. See the module docs for why the value
    /// participates in the comparison at all.
    fn merge(&self, other: &LwwRegister<V>) -> LwwRegister<V> {
        if (&other.stamp, &other.value) > (&self.stamp, &self.value) {
            other.clone()
        } else {
            self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::laws::assert_lattice_laws;
    use proptest::prelude::*;

    /// A register over plain strings — the laws are about the algebra, not about the payload type,
    /// and the extern surface's own value type is exercised through the package's fixtures.
    type Reg = LwwRegister<String>;

    fn set(reg: &Reg, replica: &str, value: &str) -> Reg {
        reg.set(replica, value.to_string())
    }

    #[test]
    fn a_fresh_register_is_empty_and_the_first_write_lands() {
        let empty = Reg::new();
        assert_eq!(empty.get(), None);
        assert_eq!(empty.stamp().counter, 0);

        let one = set(&empty, "A", "hello");
        assert_eq!(one.get().map(String::as_str), Some("hello"));
        assert_eq!(one.stamp(), &Dot::new(1, "A"));
    }

    #[test]
    fn the_later_write_wins_whichever_replica_made_it() {
        let first = set(&Reg::new(), "A", "first");
        // B writes with knowledge of A's write (it merged first), so B's write is causally later.
        let second = set(&first, "B", "second");
        assert_eq!(
            first.merge(&second).get().map(String::as_str),
            Some("second")
        );
        assert_eq!(
            second.merge(&first).get().map(String::as_str),
            Some("second")
        );
    }

    #[test]
    fn concurrent_writes_resolve_the_same_way_on_every_replica() {
        // Both replicas write from the same base state, so both stamp counter 1 — genuinely
        // concurrent. The replica id decides, and it decides identically on both sides.
        let base = Reg::new();
        let a = set(&base, "A", "from-a");
        let b = set(&base, "B", "from-b");
        assert_eq!(a.stamp().counter, b.stamp().counter);
        assert_eq!(a.merge(&b).get().map(String::as_str), Some("from-b"));
        assert_eq!(b.merge(&a).get().map(String::as_str), Some("from-b"));
    }

    #[test]
    fn a_reused_replica_id_still_converges_rather_than_splitting() {
        // The misuse case: one replica id on two nodes, so the stamps collide exactly. Without the
        // value tie-break each node would keep its own value and never notice.
        let base = Reg::new();
        let a = set(&base, "A", "left");
        let b = set(&base, "A", "right");
        assert_eq!(a.stamp(), b.stamp());
        assert_eq!(a.merge(&b), b.merge(&a));
        assert_eq!(a.merge(&b).get().map(String::as_str), Some("right"));
    }

    #[test]
    fn the_counter_passes_everything_merged_in_so_a_later_write_really_is_later() {
        let ahead = set(&set(&set(&Reg::new(), "A", "1"), "A", "2"), "A", "3");
        assert_eq!(ahead.stamp().counter, 3);
        // B has only ever written once, but it merged A's state first — so its next write outranks.
        let b = set(&Reg::new().merge(&ahead), "B", "later");
        assert_eq!(b.stamp().counter, 4);
        assert_eq!(ahead.merge(&b).get().map(String::as_str), Some("later"));
    }

    #[test]
    fn state_round_trips_through_the_wire_and_merges_the_same() {
        let a = set(&Reg::new(), "A", "local");
        let b = set(&a, "B", "remote");
        let decoded = Reg::from_bytes(&b.to_bytes()).expect("decodes");
        assert_eq!(decoded, b);
        assert_eq!(a.merge(&decoded), a.merge(&b));
        assert_eq!(Reg::from_bytes(&[0xFF, 0xFF, 0xFF, 0xFF]), None);
    }

    proptest! {
        /// The three laws plus convergence, over three replicas that each saw a different subset of
        /// an arbitrary write log — including concurrent writes at equal logical times, which is
        /// where a register's tie-break is the only thing standing between the replicas and a
        /// permanent disagreement.
        #[test]
        fn lww_laws(
            writes in prop::collection::vec(
                (prop::sample::select(vec!["A", "B", "C"]), "[a-z]{1,4}"),
                0..40,
            ),
        ) {
            let apply = |reg: Reg, (replica, value): &(&str, String)| reg.set(replica, value.clone());
            let a = writes.iter().step_by(2).fold(Reg::new(), apply);
            let b = writes.iter().skip(1).step_by(2).fold(Reg::new(), apply);
            let c = writes.iter().step_by(3).fold(Reg::new(), apply);

            let converged = assert_lattice_laws(&a, &b, &c);
            // The winner is one of the writes actually made — a register invents nothing.
            if !writes.is_empty() {
                let written: Vec<&String> = writes.iter().map(|(_, v)| v).collect();
                prop_assert!(converged.get().is_some_and(|v| written.contains(&v)));
            } else {
                prop_assert_eq!(converged.get(), None);
            }
        }

        /// Convergence again, but with the merges themselves interleaved into the log the way a
        /// live network delivers them: a replica that has merged a peer writes *after* it, and the
        /// causal counter has to carry that forward or a later write would lose to an earlier one.
        #[test]
        fn a_write_after_a_merge_outranks_what_it_saw(
            values in prop::collection::vec("[a-z]{1,4}", 1..20),
        ) {
            let mut a = Reg::new();
            let mut b = Reg::new();
            for (i, value) in values.iter().enumerate() {
                if i % 2 == 0 {
                    a = a.merge(&b).set("A", value.clone());
                } else {
                    b = b.merge(&a).set("B", value.clone());
                }
            }
            // Every write saw the previous one, so the last write is the converged value.
            let last = values.last().expect("non-empty");
            let (ab, ba) = (a.merge(&b), b.merge(&a));
            prop_assert_eq!(ab.get(), Some(last));
            prop_assert_eq!(ba.get(), Some(last));
        }
    }
}
