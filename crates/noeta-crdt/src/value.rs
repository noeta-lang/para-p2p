//! [`CrdtValue`] — the **data value** a value-carrying CRDT stores.
//!
//! The counters and the grow-only set hold primitive state chosen by the CRDT itself. A
//! last-write-wins register and an OR-Set hold whatever the *application* put in them, so they need
//! a value domain — and it cannot be "any language value at all", for a reason that has nothing to
//! do with the extern seam: a replicated value must reach a peer as **bytes**. A closure, a live
//! socket, or a reactive handle has no wire encoding, so no CRDT could carry one however the value
//! crossed into this crate.
//!
//! What is left after that constraint is exactly the tree the language already projects a value
//! onto when it crosses any boundary (a native call's arguments, `json.stringify`, an isolate
//! message): scalars, text, bytes, and lists/maps of them. That is what this enum is, kept in this
//! dependency-free crate so the convergence core still holds **no language values** — a `CrdtValue`
//! is owned, inert data, not a handle into a backend heap.
//!
//! # Two disciplines it must satisfy, and why
//!
//! 1. **A total, deterministic order.** Merges break ties by comparing values (see
//!    [`crate::LwwRegister`]), and an OR-Set keys its elements in a [`BTreeMap`]. Both must resolve
//!    identically on every replica, so ordering may not depend on hashing, insertion order, or
//!    float comparison's partiality. Floats therefore order by [`f64::total_cmp`], which is total
//!    over every bit pattern including NaN, and maps store their keys sorted.
//! 2. **Equality that agrees with that order.** `PartialEq` is defined *as* `cmp(…) == Equal`
//!    rather than derived, because a derived float comparison would make `NaN != NaN` while the
//!    ordering calls them equal — an [`Eq`] implementation that is not reflexive, which is exactly
//!    the inconsistency a `BTreeMap` key must not have. The visible consequence is that `0.0` and
//!    `-0.0` are *distinct* values here: bitwise identity is the only equality that is stable
//!    across replicas without a coordinator.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A value a CRDT can carry: inert, ordered, serializable data.
///
/// Mirrors the neutral value tree the language marshals across a native call, so a program's
/// `int`/`string`/`bytes`/list/map goes in and comes back out unchanged. A struct or class value
/// arrives as its field map — the projection the seam performs, the same one `json.stringify`
/// sees — and therefore reads back as a map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CrdtValue {
    /// The unit value — and what an absent optional (`none`) projects to.
    Unit,
    Bool(bool),
    Int(i64),
    /// A 64-bit float. Ordered by [`f64::total_cmp`], never by `<` (see the module docs).
    Float(f64),
    /// A 32-bit float, kept distinct from [`CrdtValue::Float`] so a `f32` round-trips as an `f32`.
    F32(f32),
    Str(String),
    Bytes(Vec<u8>),
    /// A list, tuple, or set — the language projects all three onto one ordered sequence.
    List(Vec<CrdtValue>),
    /// A string-keyed map, or a struct/class value as its fields. Sorted by key: the state must
    /// serialize and compare identically on every replica, whatever order the entries arrived in.
    Map(BTreeMap<String, CrdtValue>),
}

impl CrdtValue {
    /// This value's variant rank — the outer key of the total order. Two values of different kinds
    /// never compare equal, so a `1` and a `"1"` are distinct set members.
    fn rank(&self) -> u8 {
        match self {
            CrdtValue::Unit => 0,
            CrdtValue::Bool(_) => 1,
            CrdtValue::Int(_) => 2,
            CrdtValue::Float(_) => 3,
            CrdtValue::F32(_) => 4,
            CrdtValue::Str(_) => 5,
            CrdtValue::Bytes(_) => 6,
            CrdtValue::List(_) => 7,
            CrdtValue::Map(_) => 8,
        }
    }
}

impl Ord for CrdtValue {
    fn cmp(&self, other: &CrdtValue) -> Ordering {
        match (self, other) {
            (CrdtValue::Unit, CrdtValue::Unit) => Ordering::Equal,
            (CrdtValue::Bool(a), CrdtValue::Bool(b)) => a.cmp(b),
            (CrdtValue::Int(a), CrdtValue::Int(b)) => a.cmp(b),
            // `total_cmp`, not `partial_cmp`: NaN has to sit somewhere definite, or two replicas
            // holding the same set could disagree about which elements it has.
            (CrdtValue::Float(a), CrdtValue::Float(b)) => a.total_cmp(b),
            (CrdtValue::F32(a), CrdtValue::F32(b)) => a.total_cmp(b),
            (CrdtValue::Str(a), CrdtValue::Str(b)) => a.cmp(b),
            (CrdtValue::Bytes(a), CrdtValue::Bytes(b)) => a.cmp(b),
            (CrdtValue::List(a), CrdtValue::List(b)) => a.cmp(b),
            (CrdtValue::Map(a), CrdtValue::Map(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for CrdtValue {
    fn partial_cmp(&self, other: &CrdtValue) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for CrdtValue {
    /// Defined as the total order's equality, so `Eq` is reflexive even over NaN (see the module
    /// docs) and a `BTreeMap` keyed by a value behaves.
    fn eq(&self, other: &CrdtValue) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for CrdtValue {}

impl fmt::Display for CrdtValue {
    /// A compact, deterministic rendering for a CRDT's own display form (`<lww "hi">`). Strings are
    /// quoted because the values here are heterogeneous — an unquoted `1` would read the same as
    /// the integer beside it — and floats always carry a decimal point for the same reason.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrdtValue::Unit => write!(f, "unit"),
            CrdtValue::Bool(b) => write!(f, "{b}"),
            CrdtValue::Int(n) => write!(f, "{n}"),
            CrdtValue::Float(x) => write_float(f, *x),
            CrdtValue::F32(x) => write_float(f, f64::from(*x)),
            CrdtValue::Str(s) => write!(f, "\"{s}\""),
            CrdtValue::Bytes(b) => write!(f, "<{} bytes>", b.len()),
            CrdtValue::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            CrdtValue::Map(entries) => {
                write!(f, "{{")?;
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{key}: {value}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

/// Render a float with a decimal point even when it is integral (`2` prints as `2.0`), so a number
/// never reads as an integer it is not.
fn write_float(f: &mut fmt::Formatter<'_>, x: f64) -> fmt::Result {
    if x.is_finite() && x.fract() == 0.0 {
        write!(f, "{x:.1}")
    } else {
        write!(f, "{x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_never_collide_and_the_order_is_total() {
        let mut values = vec![
            CrdtValue::Str("1".to_string()),
            CrdtValue::Int(1),
            CrdtValue::Bool(true),
            CrdtValue::Unit,
        ];
        values.sort();
        assert_eq!(
            values,
            vec![
                CrdtValue::Unit,
                CrdtValue::Bool(true),
                CrdtValue::Int(1),
                CrdtValue::Str("1".to_string()),
            ]
        );
        // A `1` and a `"1"` are different values, so a set holds both.
        assert_ne!(CrdtValue::Int(1), CrdtValue::Str("1".to_string()));
    }

    #[test]
    fn nan_equals_itself_so_eq_is_reflexive() {
        let nan = CrdtValue::Float(f64::NAN);
        assert_eq!(nan, nan.clone());
        assert_eq!(nan.cmp(&nan.clone()), Ordering::Equal);
        // And it still orders definitely against an ordinary number, rather than being incomparable.
        assert_ne!(nan.cmp(&CrdtValue::Float(0.0)), Ordering::Equal);
    }

    #[test]
    fn a_map_compares_by_sorted_keys_regardless_of_insertion_order() {
        let one: BTreeMap<String, CrdtValue> = [
            ("a".to_string(), CrdtValue::Int(1)),
            ("b".to_string(), CrdtValue::Int(2)),
        ]
        .into_iter()
        .collect();
        let other: BTreeMap<String, CrdtValue> = [
            ("b".to_string(), CrdtValue::Int(2)),
            ("a".to_string(), CrdtValue::Int(1)),
        ]
        .into_iter()
        .collect();
        assert_eq!(CrdtValue::Map(one), CrdtValue::Map(other));
    }

    #[test]
    fn display_distinguishes_a_number_from_its_text() {
        assert_eq!(CrdtValue::Int(1).to_string(), "1");
        assert_eq!(CrdtValue::Str("1".to_string()).to_string(), "\"1\"");
        assert_eq!(CrdtValue::Float(2.0).to_string(), "2.0");
        assert_eq!(
            CrdtValue::List(vec![CrdtValue::Int(1), CrdtValue::Unit]).to_string(),
            "[1, unit]"
        );
        let map: BTreeMap<String, CrdtValue> = [("k".to_string(), CrdtValue::Bool(false))]
            .into_iter()
            .collect();
        assert_eq!(CrdtValue::Map(map).to_string(), "{k: false}");
    }
}
