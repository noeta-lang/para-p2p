//! The **lattice-law harness** every CRDT in this crate is held to (test-only).
//!
//! The three laws are the whole contract — a type that upholds them replicates safely, and one that
//! does not corrupts state in a way no test of its *methods* would catch. Asserting them once, here,
//! rather than per type is not only DRY: it is what stops a new CRDT from being checked more weakly
//! than the ones beside it, which is exactly how a merge bug reaches a network.

use std::fmt::Debug;

use crate::Mergeable;

/// Assert the three laws — commutativity, associativity, idempotence — over three replica states,
/// then the property they exist for: **convergence**. Every one of the six orders in which three
/// states can be reconciled must produce the same value, and re-merging a state already reflected
/// must change nothing (so a duplicated or out-of-order delivery is harmless).
///
/// Returns the converged state, so a caller can additionally assert *what* it converged to — the
/// laws say the replicas agree, not that they agree on the right answer.
pub(crate) fn assert_lattice_laws<T>(a: &T, b: &T, c: &T) -> T
where
    T: Mergeable + Clone + PartialEq + Debug,
{
    // 1. Commutative, over every pair: the order two states meet in cannot matter, because the
    //    network decides it.
    for (x, y) in [(a, b), (b, c), (a, c)] {
        assert_eq!(x.merge(y), y.merge(x), "merge is not commutative");
    }

    // 2. Associative: how a replica groups the states it received cannot matter either.
    assert_eq!(
        a.merge(b).merge(c),
        a.merge(&b.merge(c)),
        "merge is not associative"
    );

    // 3. Idempotent: a state merged with itself is itself, which is what makes a redelivery free.
    for x in [a, b, c] {
        assert_eq!(&x.merge(x), x, "merge is not idempotent");
    }

    // Convergence — the point of the three. All six orderings of three replicas' states.
    let converged = a.merge(b).merge(c);
    let orders = [
        a.merge(c).merge(b),
        b.merge(a).merge(c),
        b.merge(c).merge(a),
        c.merge(a).merge(b),
        c.merge(b).merge(a),
    ];
    for (i, other) in orders.iter().enumerate() {
        assert_eq!(
            &converged,
            other,
            "replicas diverged: reconciliation order {} disagrees",
            i + 1
        );
    }

    // A stale copy arriving late (a duplicate, a slow peer) adds nothing to the converged state.
    for stale in [a, b, c] {
        assert_eq!(
            converged.merge(stale),
            converged,
            "re-merging an already-seen state changed the value"
        );
    }

    converged
}
