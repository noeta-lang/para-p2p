//! The **value seam** for the value-carrying CRDTs: a language value in, a [`CrdtValue`] out, and
//! back again.
//!
//! `LwwRegister` and `OrSet` hold whatever a program puts in them, which is a different shape of
//! argument from the counters' `(replica, amount)`. It needs no new machinery: the two types
//! declare [`ExtType::deep_marshal`](noeta_ext_abi::registry::ExtType::deep_marshal), so a list, a
//! map or a struct argument arrives as the recursive [`NativeValue`] tree instead of collapsing to
//! an opaque handle, and the result path already has the matching [`NativeOut`] shapes. Both
//! backends project a value onto that tree with the *same* code path they use for
//! `json.stringify`, so the differential holds for free — and the CRDT values stay plain `Send`
//! data with no arena entry and no ctx dispatch, exactly like the three beside them.
//!
//! # What a CRDT can hold, and why the line is where it is
//!
//! The line is not drawn by the seam; it is drawn by the wire. A replicated value's whole state has
//! to reach a peer as bytes, so a closure, a future, a channel or a live extern handle could not be
//! carried by *any* CRDT however it were stored. What is left is data — scalars, text, bytes, and
//! lists/maps of them — and that is what these two accept. A struct or class value arrives as its
//! field map (the projection the language performs at every boundary a value crosses) and therefore
//! reads back as a map; an enum value and an extern handle are **refused** rather than stored under
//! a shape they would not come back as. `none` and `some(x)` project through to `unit` and `x`,
//! following the same convention every native call sees.

use std::collections::BTreeMap;

use noeta_crdt::CrdtValue;
use noeta_ext_abi::registry::{NativeOut, NativeValue, Scalar};
use noeta_ext_abi::{ErrorKind, StdError};

/// Read a dispatch argument as the value a CRDT will store, or report why it cannot be stored.
pub fn from_native(method: &str, arg: &NativeValue) -> Result<CrdtValue, StdError> {
    match arg {
        NativeValue::Unit => Ok(CrdtValue::Unit),
        NativeValue::Scalar(Scalar::Bool(b)) => Ok(CrdtValue::Bool(*b)),
        NativeValue::Scalar(Scalar::Int(n)) => Ok(CrdtValue::Int(*n)),
        NativeValue::Scalar(Scalar::Float(x)) => Ok(CrdtValue::Float(*x)),
        NativeValue::Scalar(Scalar::F32(x)) => Ok(CrdtValue::F32(*x)),
        NativeValue::Str(s) => Ok(CrdtValue::Str(s.clone())),
        NativeValue::Bytes(b) => Ok(CrdtValue::Bytes(b.clone())),
        NativeValue::List(items) => items
            .iter()
            .map(|item| from_native(method, item))
            .collect::<Result<Vec<_>, _>>()
            .map(CrdtValue::List),
        // A map, a struct, or a class instance — one variant, because the language projects all
        // three onto keyed entries. A later key wins, as it would in the map literal itself.
        NativeValue::Map(entries) => {
            let mut map = BTreeMap::new();
            for (key, value) in entries {
                map.insert(key.clone(), from_native(method, value)?);
            }
            Ok(CrdtValue::Map(map))
        }
        // An enum value keeps its identity in `enum_name`/`variant`, and reconstructing it on the
        // way out would mean rebuilding a language type by name from inside a CRDT. Refusing is the
        // honest answer while that is not done: the alternative is storing a shape the program
        // would not get back.
        NativeValue::Variant { enum_name, .. } => Err(unstorable(method, enum_name)),
        NativeValue::Instance { class, .. } => Err(unstorable(method, class)),
        NativeValue::Extern(handle) => Err(unstorable(method, handle.type_identity())),
        NativeValue::Object { type_name, .. } => Err(unstorable(method, type_name)),
        NativeValue::Opaque(type_name) => Err(unstorable(method, type_name)),
    }
}

/// Materialize a stored value back into the language.
pub fn to_out(value: &CrdtValue) -> NativeOut {
    match value {
        CrdtValue::Unit => NativeOut::Unit,
        CrdtValue::Bool(b) => NativeOut::Scalar(Scalar::Bool(*b)),
        CrdtValue::Int(n) => NativeOut::Scalar(Scalar::Int(*n)),
        CrdtValue::Float(x) => NativeOut::Scalar(Scalar::Float(*x)),
        CrdtValue::F32(x) => NativeOut::Scalar(Scalar::F32(*x)),
        CrdtValue::Str(s) => NativeOut::Str(s.clone()),
        CrdtValue::Bytes(b) => NativeOut::Bytes(b.clone()),
        CrdtValue::List(items) => NativeOut::List(items.iter().map(to_out).collect()),
        CrdtValue::Map(entries) => NativeOut::Map(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), to_out(value)))
                .collect(),
        ),
    }
}

/// The refusal, phrased as the constraint that actually causes it — a value a peer could never be
/// sent is not a value this replica may pretend to hold.
fn unstorable(method: &str, type_name: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!(
            "`{method}` stores data — a number, bool, string, bytes, or a list or map of them — \
             and `{type_name}` is not data a peer could be sent"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole seam rests on: what goes in comes back out. A stored value that
    /// changed shape on the way home would be a silent data loss no type would catch.
    #[test]
    fn every_data_shape_round_trips() {
        let cases = [
            NativeValue::Unit,
            NativeValue::Scalar(Scalar::Bool(true)),
            NativeValue::Scalar(Scalar::Int(-7)),
            NativeValue::Scalar(Scalar::Float(1.5)),
            NativeValue::Scalar(Scalar::F32(0.25)),
            NativeValue::Str("hi".to_string()),
            NativeValue::Bytes(vec![1, 2, 3]),
            NativeValue::List(vec![
                NativeValue::Scalar(Scalar::Int(1)),
                NativeValue::Str("two".to_string()),
            ]),
            NativeValue::Map(vec![
                ("b".to_string(), NativeValue::Scalar(Scalar::Int(2))),
                ("a".to_string(), NativeValue::Unit),
            ]),
        ];
        for case in cases {
            let stored = from_native("set", &case).expect("data is storable");
            let out = to_out(&stored);
            let expected = match &case {
                // A map normalizes to sorted keys on the way in (the state has to compare and
                // serialize identically on every replica), so the round trip is up to key order.
                NativeValue::Map(_) => NativeOut::Map(vec![
                    ("a".to_string(), NativeOut::Unit),
                    ("b".to_string(), NativeOut::Scalar(Scalar::Int(2))),
                ]),
                other => native_as_out(other),
            };
            assert_eq!(out, expected, "round trip changed {case:?}");
        }
    }

    /// The mirror of the argument view, for the cases that map one-to-one.
    fn native_as_out(value: &NativeValue) -> NativeOut {
        match value {
            NativeValue::Unit => NativeOut::Unit,
            NativeValue::Scalar(s) => NativeOut::Scalar(*s),
            NativeValue::Str(s) => NativeOut::Str(s.clone()),
            NativeValue::Bytes(b) => NativeOut::Bytes(b.clone()),
            NativeValue::List(items) => NativeOut::List(items.iter().map(native_as_out).collect()),
            other => panic!("not a one-to-one shape: {other:?}"),
        }
    }

    #[test]
    fn a_value_with_no_wire_encoding_is_refused_by_name() {
        let err = from_native(
            "set",
            &NativeValue::Variant {
                enum_name: "Color".to_string(),
                variant: "red".to_string(),
                variant_index: 0,
                fields: vec![],
            },
        )
        .expect_err("an enum value is not storable");
        assert!(err.message.contains("Color"), "{}", err.message);
        assert!(matches!(err.kind, ErrorKind::ArgType));
    }

    #[test]
    fn a_nested_unstorable_value_is_refused_rather_than_dropped() {
        // The recursion has to carry the refusal out, or a list would silently lose an element.
        let err = from_native(
            "insert",
            &NativeValue::List(vec![NativeValue::Opaque("Future")]),
        )
        .expect_err("a list containing a future is not storable");
        assert!(err.message.contains("Future"), "{}", err.message);
    }
}
