//! `para.crdt.AutoDoc` — an **Automerge-backed** document CRDT.
//!
//! The three hand-rolled CRDTs in [`crate::crdt`] are the classical primitives: two counters and a
//! grow-only set. They are small, dependency-free, and property-tested — and they cannot represent
//! **deletion or update**, because a grow-only lattice has no way to go back. That is the gap most
//! applications hit first: a chat log is fine, a document with editable, removable fields is not.
//!
//! Automerge answers exactly that, and p2panda is designed for it: `p2panda-core` advertises itself
//! as "compatible with any application data and CRDT" and ships none of its own, so the value layer
//! is the consumer's to choose. Its API also lines up with this package's contracts almost exactly —
//! `save`/`load` are [`crate::crdt::SYNCABLE_TRAIT`]'s encoding, and `merge` is
//! [`crate::crdt::MERGEABLE_TRAIT`]'s — so it drops in as a value rather than as a second sync
//! stack. The transport, log-sync and encryption stay p2panda's.
//!
//! The surface is deliberately a **string-valued map**, not all of Automerge. A document CRDT's full
//! shape (nested maps, lists, rich text, cursors) is a large API to expose through the extern seam,
//! and most of its value here is available from the map alone: concurrent writes to *different* keys
//! both survive, concurrent writes to the *same* key resolve deterministically on every replica, and
//! a key can be **removed**. Lists and text are the obvious follow-on, and nothing here forecloses
//! them.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;

use automerge::transaction::Transactable;
use automerge::{AutoCommit, ChangeHash, ReadDoc, ROOT, ScalarValue};
use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    ExternValue, Host, NativeValue, StdError, arity_error, no_method_error, type_error,
};

pub const AUTODOC_TYPE_NAME: &str = "AutoDoc";
pub const AUTODOC_TYPE_IDENTITY: &str = "para.crdt.AutoDoc";

const AUTODOC_SIG: SigType = SigType::Named(AUTODOC_TYPE_NAME);

/// An Automerge document, with its **heads** cached alongside.
///
/// The heads (the document's current change hashes) are what answer "did this actually change" —
/// which the sync engine asks after every peer merge, because a merge of state already reflected
/// must not rerun dependent effects. Automerge computes them behind `&mut self`, and equality is
/// `&self`, so they are computed once per constructed value instead of cloning the whole document
/// on every comparison.
#[derive(Debug, Clone)]
pub struct AutoDoc {
    doc: AutoCommit,
    heads: Vec<ChangeHash>,
}

impl AutoDoc {
    /// Wrap a document, caching its heads.
    fn new(mut doc: AutoCommit) -> AutoDoc {
        let heads = doc.get_heads();
        AutoDoc { doc, heads }
    }

    pub fn empty() -> AutoDoc {
        AutoDoc::new(AutoCommit::new())
    }

    /// This document's full state for the wire — [`crate::crdt::SYNCABLE_TRAIT`]'s `to_bytes`.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.doc.clone().save()
    }

    /// Decode a peer's document, or `None` if the bytes are not an Automerge document — untrusted
    /// input is an ordinary outcome, never a panic.
    pub fn from_bytes(bytes: &[u8]) -> Option<AutoDoc> {
        AutoCommit::load(bytes).ok().map(AutoDoc::new)
    }

    /// The convergent join of two documents. Automerge merges by history, so this is commutative,
    /// associative and idempotent for the same reason the hand-rolled lattices are: re-merging
    /// changes already present adds nothing.
    pub fn merge(&self, other: &AutoDoc) -> AutoDoc {
        let mut a = self.doc.clone();
        let mut b = other.doc.clone();
        // A merge failure here is a corrupt document rather than a divergence; keeping the local
        // state is the safe answer, and the caller's equality check reads it as "nothing changed".
        if a.merge(&mut b).is_err() {
            return self.clone();
        }
        AutoDoc::new(a)
    }

    fn put(&self, key: &str, value: &str) -> AutoDoc {
        let mut doc = self.doc.clone();
        if doc.put(ROOT, key, ScalarValue::Str(value.into())).is_err() {
            return self.clone();
        }
        AutoDoc::new(doc)
    }

    fn remove(&self, key: &str) -> AutoDoc {
        let mut doc = self.doc.clone();
        // Deleting an absent key is not an error worth surfacing — the post-state is the same.
        if doc.delete(ROOT, key).is_err() {
            return self.clone();
        }
        AutoDoc::new(doc)
    }

    fn get(&self, key: &str) -> Option<String> {
        match self.doc.get(ROOT, key) {
            Ok(Some((value, _))) => value.into_string().ok(),
            _ => None,
        }
    }

    /// The document's keys, sorted — a stable order matters because this feeds rendering, and two
    /// replicas that converged must also *display* identically.
    fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.doc.keys(ROOT).collect();
        keys.sort();
        keys
    }
}

impl ExternValue for AutoDoc {
    fn type_identity(&self) -> &'static str {
        AUTODOC_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        // Equal history ⇒ equal document. Comparing heads rather than serialized bytes is what
        // makes this answer "did the state change", which is the only question asked of it.
        other
            .as_any()
            .downcast_ref::<AutoDoc>()
            .is_some_and(|o| o.heads == self.heads)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0
    }
    fn display(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "<autodoc [{}]>", self.keys().join(", "))
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// `AutoDoc`'s methods. `merge` satisfies `Mergeable`; `to_bytes`/`merge_bytes` satisfy `Syncable`;
/// the rest are the map surface.
pub const AUTODOC_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &["key", "value"],
        name: "put",
        params: &[SigType::String, SigType::String],
        ret: RetTy::Concrete(AUTODOC_SIG),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        param_names: &["key"],
        name: "get",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Option(&SigType::String)),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        param_names: &["key"],
        name: "remove",
        params: &[SigType::String],
        ret: RetTy::Concrete(AUTODOC_SIG),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        param_names: &[],
        name: "keys",
        params: &[],
        ret: RetTy::Concrete(SigType::List(&SigType::String)),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        param_names: &["other"],
        name: "merge",
        params: &[AUTODOC_SIG],
        ret: RetTy::Concrete(AUTODOC_SIG),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        param_names: &[],
        name: "to_bytes",
        params: &[],
        ret: RetTy::Concrete(SigType::Bytes),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        param_names: &["other"],
        name: "merge_bytes",
        params: &[SigType::Bytes],
        ret: RetTy::Concrete(AUTODOC_SIG),
        ..ExtFn::DEFAULTS
    },
];

fn want_str(method: &str, args: &[NativeValue], index: usize) -> Result<String, StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s.clone()),
        _ => Err(type_error(method, "string")),
    }
}

fn want_bytes(method: &str, args: &[NativeValue]) -> Result<Vec<u8>, StdError> {
    match args.first() {
        Some(NativeValue::Bytes(b)) => Ok(b.clone()),
        _ => Err(type_error(method, "bytes")),
    }
}

fn doc_out(doc: AutoDoc) -> NativeOut {
    NativeOut::Extern(noeta_ext_abi::ExternBox::new(doc))
}

pub fn autodoc_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(doc) = recv.as_any().downcast_ref::<AutoDoc>() else {
        return Err(type_error(method, AUTODOC_TYPE_NAME));
    };
    match method {
        "put" => {
            if args.len() != 2 {
                return Err(arity_error(method, 2, args.len()));
            }
            let key = want_str(method, args, 0)?;
            let value = want_str(method, args, 1)?;
            Ok(doc_out(doc.put(&key, &value)))
        }
        "get" => {
            if args.len() != 1 {
                return Err(arity_error(method, 1, args.len()));
            }
            let key = want_str(method, args, 0)?;
            Ok(match doc.get(&key) {
                Some(v) => NativeOut::Some(Box::new(NativeOut::Str(v))),
                None => NativeOut::None,
            })
        }
        "remove" => {
            if args.len() != 1 {
                return Err(arity_error(method, 1, args.len()));
            }
            let key = want_str(method, args, 0)?;
            Ok(doc_out(doc.remove(&key)))
        }
        "keys" => {
            if !args.is_empty() {
                return Err(arity_error(method, 0, args.len()));
            }
            Ok(NativeOut::List(
                doc.keys().into_iter().map(NativeOut::Str).collect(),
            ))
        }
        "merge" => {
            if args.len() != 1 {
                return Err(arity_error(method, 1, args.len()));
            }
            let NativeValue::Extern(other) = &args[0] else {
                return Err(type_error(method, AUTODOC_TYPE_NAME));
            };
            let Some(other) = other.as_any().downcast_ref::<AutoDoc>() else {
                return Err(type_error(method, AUTODOC_TYPE_NAME));
            };
            Ok(doc_out(doc.merge(other)))
        }
        "to_bytes" => {
            if !args.is_empty() {
                return Err(arity_error(method, 0, args.len()));
            }
            Ok(NativeOut::Bytes(doc.to_bytes()))
        }
        "merge_bytes" => {
            if args.len() != 1 {
                return Err(arity_error(method, 1, args.len()));
            }
            let bytes = want_bytes(method, args)?;
            // Untrusted input: an undecodable payload leaves this replica exactly as it was, which
            // the sync engine's equality check then reads as "nothing changed".
            Ok(doc_out(match AutoDoc::from_bytes(&bytes) {
                Some(peer) => doc.merge(&peer),
                None => doc.clone(),
            }))
        }
        _ => Err(no_method_error(AUTODOC_TYPE_NAME, method)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three CRDT laws, on the type this package now recommends for anything needing deletion.
    /// Automerge merges by history, so they hold for the same reason the hand-rolled lattices'
    /// do — but "it should" is not evidence, and this is the package's own claim to keep honest.
    #[test]
    fn merge_is_commutative_associative_and_idempotent() {
        let a = AutoDoc::empty().put("x", "1");
        let b = AutoDoc::empty().put("y", "2");
        let c = AutoDoc::empty().put("z", "3");

        let ab = a.merge(&b);
        let ba = b.merge(&a);
        assert_eq!(ab.keys(), ba.keys(), "commutative");

        let left = ab.merge(&c);
        let right = a.merge(&b.merge(&c));
        assert_eq!(left.keys(), right.keys(), "associative");

        assert_eq!(ab.merge(&ab).keys(), ab.keys(), "idempotent");
        assert!(
            ab.merge(&ab).eq_value(&ab),
            "re-merging own state changes nothing, so dependents must not wake"
        );
    }

    /// The capability the built-in three cannot express at all.
    #[test]
    fn a_key_can_be_removed_and_the_removal_converges() {
        let seeded = AutoDoc::empty().put("keep", "a").put("drop", "b");
        let removed = seeded.remove("drop");
        assert_eq!(removed.keys(), vec!["keep".to_string()]);

        // A peer that never saw the removal still converges on it, rather than resurrecting it.
        let merged = seeded.merge(&removed);
        assert_eq!(merged.keys(), vec!["keep".to_string()]);
    }

    #[test]
    fn state_round_trips_through_the_wire_encoding() {
        let doc = AutoDoc::empty().put("k", "v");
        let peer = AutoDoc::from_bytes(&doc.to_bytes()).expect("decodes");
        assert_eq!(peer.get("k").as_deref(), Some("v"));
        assert!(peer.eq_value(&doc), "a round trip preserves history");
    }

    #[test]
    fn undecodable_bytes_are_rejected_rather_than_panicking() {
        assert!(AutoDoc::from_bytes(b"not an automerge document").is_none());
    }
}
