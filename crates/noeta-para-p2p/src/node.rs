//! `para.p2p.Node` — a **named** p2p node, so one program can be several peers.
//!
//! Before this, "which node" was not a question a program could ask: the run had exactly one
//! backend, cached under one ctx-state key, and `p2p.publish` / `p2p.receive` / `p2p.identity` were
//! free functions over it. A node's identity and durable log could only be steered by
//! process-global environment variables, so several user identities in one process — a multi-user
//! desktop app, a multi-tenant server — was not expressible.
//!
//! A `Node` is **a name, not an owner**. The box below carries only a [`NodeConfig`]; the live
//! p2panda node lives in ctx state, keyed on that config ([`crate::provider::backend_for`]). So two
//! handles opened on one directory reach one node, a handle may be cloned, stored and dropped
//! freely, and the async `receive` descriptor still captures the backend `Arc` exactly as the free
//! function's does. Nothing owns a socket, so nothing has to be closed.
//!
//! - `p2p.open(dir)` — the node whose identity (`identity.key`), durable log (`store.db`) and
//!   encryption credentials live in `dir`. Naming it neither creates the directory nor starts the
//!   node: the node starts lazily on first use, exactly where it did before.
//! - `p2p.node()` — the default node, the one the free functions run on, so "my node" can be passed
//!   around uniformly rather than being reachable only through the module.
//!
//! There is deliberately **no capability check** on `open`: restricting which directories a program
//! may touch is a sandbox's job (container, flatpak, the host's own `FileSystem` policy), not this
//! layer's. A program with real filesystem access has real filesystem access, and inventing a
//! second, weaker gate here would only suggest otherwise.

use std::any::Any;
use std::cmp::Ordering;

use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    CtxError, CtxOut, ExternValue, NativeCtx, NativeValue, Slot, ctx_arity, no_method_error,
    type_error,
};

use crate::provider::{NodeConfig, receive_descriptor_for, with_node};

pub const NODE_TYPE_NAME: &str = "Node";

/// `Node`'s qualified runtime identity — registered under `para.p2p`.
pub const NODE_TYPE_IDENTITY: &str = "para.p2p.Node";

/// The extern box: just the node's name. Cloning it aliases the same live node (reference
/// semantics, like every other handle), and equality is by config — two `p2p.open` calls on one
/// directory produce equal handles, which is the program-visible form of "one directory, one node".
///
/// Equality is the *name's* answer, and it is the weaker of the two: it is a pure function of the
/// config and cannot re-walk the filesystem, so two handles opened either side of a symlink
/// appearing under a not-yet-created directory compare unequal while still reaching one live node
/// (the registry settles that at first use — see [`crate::provider::backend_in`]). Unequal handles
/// may share a node, never the reverse: equal handles always mean one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBox {
    /// The node this handle names, or `None` for "whatever the host permits" — the default node on
    /// a host with no real networking, which is the deterministic loopback broker.
    pub config: Option<NodeConfig>,
}

impl ExternValue for NodeBox {
    fn type_identity(&self) -> &'static str {
        NODE_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<NodeBox>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        match self.config.as_ref().and_then(|c| c.data_dir.as_deref()) {
            Some(dir) => write!(out, "<p2p node {}>", dir.display()),
            None => write!(out, "<p2p node default>"),
        }
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

/// The message union `publish` accepts — a string rides as its UTF-8 bytes, exactly as the free
/// `p2p.publish` accepts it.
const MESSAGE_SIG: SigType = SigType::Union(&[SigType::String, SigType::Bytes]);

/// `Node`'s methods mirror the `p2p` module's free functions one for one: the free function is this
/// method on the default node, so a program moving from one identity to several rewrites
/// `p2p.publish(t, m)` as `alice.publish(t, m)` and nothing else changes.
pub const NODE_CTX_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "publish",
        params: &[SigType::String, MESSAGE_SIG],
        ret: RetTy::Concrete(SigType::Unit),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        name: "receive",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Future(&SigType::Option(&SigType::Bytes))),
        ..ExtFn::DEFAULTS
    },
    ExtFn {
        name: "identity",
        params: &[],
        ret: RetTy::Concrete(SigType::Option(&SigType::String)),
        ..ExtFn::DEFAULTS
    },
    // `synced_signal(initial, topic, members?)` — a CRDT-backed signal replicating through *this*
    // node, the node-scoped twin of `para.synced.synced_signal`.
    //
    // MEASURED DIFFERENCE, not a guess: a non-CRDT initial value is still rejected at check time,
    // but as `E0007` (a type mismatch at the call) rather than the module form's `E0025` (the
    // named bound violation) — the checker seeds an extern-type method's signature variables from
    // the *receiver's* type arguments only (`noeta-check` `receiver_bindings`, reached from
    // `method_return`), and `Node` is not generic, so `Var(0)` never binds from the argument; the
    // bound-enforcement pass (`check_module_bounds` → `module_var_bounds`) is likewise wired for
    // module functions only. Both built-in and user-defined CRDTs are accepted, so the rejection
    // set is unchanged — only the diagnostic is less precise on this spelling.
    ExtFn {
        name: "synced_signal",
        params: &[
            SigType::BoundedVar(0, &["Mergeable", "Syncable"]),
            SigType::String,
            SigType::Optional(&SigType::List(&SigType::String)),
        ],
        ret: RetTy::Concrete(SigType::Generic(
            crate::synced::SYNCED_SIGNAL_TYPE_NAME,
            &[SigType::Var(0)],
        )),
        ..ExtFn::DEFAULTS
    },
];

pub const NODE_DOCS: &[(&str, &str)] = &[
    (
        "publish",
        "Publish `data` to the peer-to-peer `topic` from this node, delivering it to subscribed \
         peers.",
    ),
    (
        "receive",
        "Await the next message published to `topic` by a peer, as seen by this node; `none` when \
         the topic closes.",
    ),
    (
        "identity",
        "This node's public identity string if p2p networking is configured; `none` otherwise.",
    ),
    (
        "synced_signal",
        "A `SyncedSignal<T>` over a CRDT value, replicated through **this** node under `topic` \
         (optionally restricted to `peers`) — local edits merge conflict-free across the network.",
    ),
];

pub fn node_ctx_method_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let handle = handle_of(ctx, recv)?;
    match method {
        "publish" => {
            ctx_arity(method, args, 2)?;
            let topic = view_str(ctx, method, args, 0)?;
            let message = view_message(ctx, method, args, 1)?;
            with_node(ctx, handle.config, |p| p.p2p_publish(&topic, message))?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        "receive" => {
            ctx_arity(method, args, 1)?;
            let topic = view_str(ctx, method, args, 0)?;
            // WORK, not a value — same as the free `p2p.receive`, but the descriptor is built over
            // *this* node's backend, so two nodes drain two topics independently.
            let io = receive_descriptor_for(ctx, handle.config, topic)?;
            Ok(CtxOut::Slot(ctx.spawn_io(io)))
        }
        "identity" => {
            ctx_arity(method, args, 0)?;
            Ok(match with_node(ctx, handle.config, |p| p.p2p_identity())? {
                Some(hex) => CtxOut::Out(NativeOut::Some(Box::new(NativeOut::Str(hex)))),
                None => CtxOut::Out(NativeOut::None),
            })
        }
        // A signal replicating through this node — the same constructor `para.synced` exposes,
        // reached from the node it belongs to.
        "synced_signal" => crate::synced::create_synced_signal(ctx, handle.config, method, args),
        _ => Err(no_method_error(NODE_TYPE_NAME, method).into()),
    }
}

/// The receiver's name, read out of its extern box.
pub fn handle_of<C: NativeCtx + ?Sized>(ctx: &mut C, recv: Slot) -> Result<NodeBox, CtxError> {
    let mut handle = None;
    ctx.with_extern(recv, &mut |e| {
        handle = e.as_any().downcast_ref::<NodeBox>().cloned();
    })?;
    Ok(handle.expect("a Node receiver wraps a NodeBox"))
}

fn view_str<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Slot],
    index: usize,
) -> Result<String, CtxError> {
    match ctx.view(args[index])? {
        NativeValue::Str(s) => Ok(s),
        _ => Err(type_error(func, "string").into()),
    }
}

fn view_message<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Slot],
    index: usize,
) -> Result<Vec<u8>, CtxError> {
    match ctx.view(args[index])? {
        NativeValue::Str(s) => Ok(s.into_bytes()),
        NativeValue::Bytes(b) => Ok(b),
        _ => Err(type_error(func, "string|bytes").into()),
    }
}
