//! [`OrSet`] — an **observed-remove set**: elements can be added, removed, and added *again*.
//!
//! A [`crate::GSet`] cannot remove, and the obvious repair — keeping a second set of removed
//! elements and subtracting it — is worse than no repair at all: once `"x"` is in the removed set,
//! every later add of `"x"` is silently swallowed, on every replica, forever. "Re-add after remove"
//! is not an edge case; it is the first thing a user does after deleting the wrong item.
//!
//! The fix is to stop treating the *element* as the thing that gets removed. Every insertion mints a
//! unique **tag** ([`Dot`]), the set is the elements with at least one live tag, and a remove
//! tombstones exactly the tags it could **observe** at the moment it ran — hence *observed-remove*.
//! A later add mints a tag no earlier remove could have named, so it survives; and an add that was
//! concurrent with a remove survives too, because the remove never saw it. Add wins over a
//! concurrent remove, which is the intuitive answer when two people edit at once: the item comes
//! back rather than vanishing under someone who never knew about it.
//!
//! Both halves are grow-only sets, and the merge is their union — so the three laws hold for the
//! same reason [`crate::GSet`]'s do, one level down. The price is **tombstones**: the tag of a
//! removed element is kept forever, because a replica that never saw the insertion cannot otherwise
//! be told the removal covered it. That is the standard cost of this type, and it is why it is a
//! set of application items rather than an event log.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Mergeable;
use crate::clock::Dot;

/// A set whose elements can be added and removed, converging without coordination.
///
/// An **immutable value** like every CRDT here: [`insert`](OrSet::insert) and
/// [`remove`](OrSet::remove) return a new set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "V: Ord + Serialize",
    deserialize = "V: Ord + Deserialize<'de>"
))]
pub struct OrSet<V> {
    /// Element → the tags of every insertion of it this replica has observed. Ordered, so merge,
    /// equality and [`members`](OrSet::members) are iteration-order-independent.
    adds: BTreeMap<V, BTreeSet<Dot>>,
    /// Every tag a remove has tombstoned. An element is present exactly while it has a tag that is
    /// not in here.
    removed: BTreeSet<Dot>,
}

impl<V> Default for OrSet<V> {
    fn default() -> OrSet<V> {
        OrSet {
            adds: BTreeMap::new(),
            removed: BTreeSet::new(),
        }
    }
}

impl<V: Ord + Clone> OrSet<V> {
    /// The empty set.
    pub fn new() -> OrSet<V> {
        OrSet::default()
    }

    /// A copy with `element` inserted, tagged by `replica` at one logical tick past every tag this
    /// state has seen.
    ///
    /// Inserting an element that is already present is **not** a no-op: it mints a second tag, so a
    /// remove that observed only the first one leaves the element in the set. That is the same
    /// "add wins over the remove that never saw it" rule the concurrent case follows.
    pub fn insert(&self, replica: &str, element: V) -> OrSet<V> {
        let tag = Dot::next_after(self.highest_counter(), replica);
        let mut adds = self.adds.clone();
        adds.entry(element).or_default().insert(tag);
        OrSet {
            adds,
            removed: self.removed.clone(),
        }
    }

    /// A copy with `element` removed — tombstoning exactly the tags this state can see.
    ///
    /// Removing an absent element does nothing: there is no tag to tombstone, and pre-emptively
    /// blocking a future insertion is the grow-only-set failure this type exists to avoid.
    pub fn remove(&self, element: &V) -> OrSet<V> {
        let mut removed = self.removed.clone();
        if let Some(tags) = self.adds.get(element) {
            removed.extend(tags.iter().cloned());
        }
        OrSet {
            adds: self.adds.clone(),
            removed,
        }
    }

    /// Whether `element` is a member — i.e. whether any insertion of it is still untombstoned.
    pub fn contains(&self, element: &V) -> bool {
        self.adds
            .get(element)
            .is_some_and(|tags| tags.iter().any(|tag| !self.removed.contains(tag)))
    }

    /// The number of members.
    pub fn len(&self) -> usize {
        self.members().len()
    }

    /// Whether the set has no members. (A set with only tombstoned elements is empty — it is not
    /// *equal* to a fresh set, because it still carries the history that makes the removal
    /// converge.)
    pub fn is_empty(&self) -> bool {
        self.members().is_empty()
    }

    /// The members, in sorted order — deterministic, so two converged replicas also *display*
    /// identically.
    pub fn members(&self) -> Vec<&V> {
        self.adds
            .iter()
            .filter(|(_, tags)| tags.iter().any(|tag| !self.removed.contains(tag)))
            .map(|(element, _)| element)
            .collect()
    }

    /// The highest logical time any tag in this state carries — what the next insertion ticks past.
    /// Scans the tombstones too, so a tag is never reissued after the insertion it named was
    /// removed and pruned from view.
    fn highest_counter(&self) -> u64 {
        let added = self
            .adds
            .values()
            .flat_map(|tags| tags.iter())
            .map(|tag| tag.counter);
        let removed = self.removed.iter().map(|tag| tag.counter);
        added.chain(removed).max().unwrap_or(0)
    }
}

impl<V> Mergeable for OrSet<V>
where
    V: Ord + Clone + Serialize + DeserializeOwned,
{
    /// Union both halves: the tags of every observed insertion, and every tombstone. Commutative,
    /// associative and idempotent because union is — and an element's membership is then a pure
    /// function of the merged state, so the replicas agree on the set as well as on the bytes.
    fn merge(&self, other: &OrSet<V>) -> OrSet<V> {
        let mut adds = self.adds.clone();
        for (element, tags) in &other.adds {
            adds.entry(element.clone())
                .or_default()
                .extend(tags.iter().cloned());
        }
        let mut removed = self.removed.clone();
        removed.extend(other.removed.iter().cloned());
        OrSet { adds, removed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::laws::assert_lattice_laws;
    use proptest::prelude::*;

    type Set = OrSet<String>;

    fn insert(set: &Set, replica: &str, element: &str) -> Set {
        set.insert(replica, element.to_string())
    }

    fn remove(set: &Set, element: &str) -> Set {
        set.remove(&element.to_string())
    }

    fn members(set: &Set) -> Vec<&str> {
        set.members().into_iter().map(String::as_str).collect()
    }

    /// The whole reason this type exists: a naive add/remove set cannot express it, because its
    /// removal is keyed on the element rather than on the insertion it saw.
    #[test]
    fn an_element_re_added_after_removal_is_present() {
        let set = insert(&Set::new(), "A", "x");
        let gone = remove(&set, "x");
        assert!(!gone.contains(&"x".to_string()));

        let back = insert(&gone, "A", "x");
        assert!(back.contains(&"x".to_string()));
        assert_eq!(members(&back), vec!["x"]);

        // And the re-add survives reconciliation with the replica that only saw the removal.
        assert!(back.merge(&gone).contains(&"x".to_string()));
        assert!(gone.merge(&back).contains(&"x".to_string()));
    }

    #[test]
    fn a_remove_only_covers_the_insertions_it_observed() {
        // A adds "x" and removes it. B, which never saw either, adds "x" concurrently.
        let a = remove(&insert(&Set::new(), "A", "x"), "x");
        let b = insert(&Set::new(), "B", "x");
        // Add wins over the remove that could not have known about it — on both replicas.
        assert!(a.merge(&b).contains(&"x".to_string()));
        assert!(b.merge(&a).contains(&"x".to_string()));
    }

    #[test]
    fn a_removal_converges_rather_than_being_resurrected() {
        // The other direction: a peer that never saw the removal must not bring the element back
        // just by merging its older state in.
        let seeded = insert(&insert(&Set::new(), "A", "keep"), "A", "drop");
        let trimmed = remove(&seeded, "drop");
        assert_eq!(members(&trimmed), vec!["keep"]);
        assert_eq!(members(&seeded.merge(&trimmed)), vec!["keep"]);
        assert_eq!(members(&trimmed.merge(&seeded)), vec!["keep"]);
    }

    #[test]
    fn merge_is_union_and_members_come_back_sorted() {
        let a = insert(&insert(&Set::new(), "A", "x"), "A", "y");
        let b = insert(&insert(&Set::new(), "B", "z"), "B", "y");
        assert_eq!(members(&a.merge(&b)), vec!["x", "y", "z"]);
        assert_eq!(a.merge(&b), b.merge(&a));
        assert_eq!(a.merge(&b).len(), 3);
    }

    #[test]
    fn removing_an_absent_element_does_not_block_adding_it_later() {
        // The grow-only-plus-tombstones failure, asserted as the thing that must NOT happen.
        let empty = remove(&Set::new(), "x");
        assert!(insert(&empty, "A", "x").contains(&"x".to_string()));
    }

    #[test]
    fn an_emptied_set_is_empty_but_keeps_the_history_that_makes_it_converge() {
        let emptied = remove(&insert(&Set::new(), "A", "x"), "x");
        assert!(emptied.is_empty());
        assert_ne!(emptied, Set::new());
    }

    #[test]
    fn state_round_trips_through_the_wire_and_merges_the_same() {
        let a = insert(&Set::new(), "A", "x");
        let b = remove(&insert(&a, "B", "y"), "x");
        let decoded = Set::from_bytes(&b.to_bytes()).expect("decodes");
        assert_eq!(decoded, b);
        assert_eq!(a.merge(&decoded), a.merge(&b));
        assert_eq!(Set::from_bytes(&[0xFF, 0xFF, 0xFF, 0xFF]), None);
    }

    proptest! {
        /// The three laws plus convergence, over three replicas folding different subsets of an
        /// arbitrary add/remove log — the case a grow-only set never has to face, since here the
        /// two halves of the state have to stay consistent with each other under every ordering.
        #[test]
        fn orset_laws(
            ops in prop::collection::vec(
                (prop::sample::select(vec!["A", "B", "C"]), "[a-z]{1,2}", any::<bool>()),
                0..40,
            ),
        ) {
            let apply = |set: Set, (replica, element, add): &(&str, String, bool)| {
                if *add {
                    set.insert(replica, element.clone())
                } else {
                    set.remove(element)
                }
            };
            let a = ops.iter().step_by(2).fold(Set::new(), apply);
            let b = ops.iter().skip(1).step_by(2).fold(Set::new(), apply);
            let c = ops.iter().step_by(3).fold(Set::new(), apply);

            let converged = assert_lattice_laws(&a, &b, &c);
            // Membership is a function of the converged state, so every replica also agrees on the
            // *set* — not merely on the bytes.
            for element in converged.members() {
                prop_assert!(a.merge(&b).merge(&c).contains(element));
                prop_assert!(c.merge(&b).merge(&a).contains(element));
            }
        }

        /// Re-adding after a remove keeps working however many rounds it takes, and with the merges
        /// interleaved the way a network delivers them.
        #[test]
        fn a_re_add_survives_any_number_of_rounds(rounds in 1usize..8) {
            let mut a = insert(&Set::new(), "A", "x");
            let mut b = Set::new();
            for _ in 0..rounds {
                b = b.merge(&a);
                b = remove(&b, "x");
                a = insert(&a.merge(&b), "A", "x");
            }
            prop_assert!(a.merge(&b).contains(&"x".to_string()));
            prop_assert!(b.merge(&a).contains(&"x".to_string()));
        }
    }
}
