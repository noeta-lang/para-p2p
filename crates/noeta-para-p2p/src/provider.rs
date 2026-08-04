//! The **extension-owned** p2p backend (para-namespace follow-on F2b) — the seam that lets the
//! `para.p2p`/`para.synced` surface reach the [`P2p`] capability without any host implementing it.
//!
//! After F2b the transport lives entirely on the extension side: this module owns the run's
//! [`P2pBackend`]s in per-run ctx state, chosen at first use by the host's
//! [`real_p2p`](noeta_ext_abi::host::P2pProvider::real_p2p) policy:
//!
//! - **Loopback broker** ([`noeta_ext_abi::P2pBroker`]) — the deterministic in-process log, used on a
//!   host that permits no real networking (the sandbox, WASI, browser → `real_p2p()` is `None`), so a
//!   p2p/synced program is oracle-safe and terminates in-oracle.
//! - **Real node** ([`noeta_para_p2p_net::P2pNode`]) — a live p2panda-net node (gossip + log-sync
//!   over iroh/QUIC), used when the host permits real networking (`RealHost` → `Some(config)`) **and**
//!   this crate is built with the `ring-p2p` feature. Without the ring, even a real host falls back to
//!   the loopback broker (a `noeta run` still works locally, just without peers).
//!
//! Both implement [`P2p`]; the backend lives behind an `Arc<Mutex<…>>` ([`P2pBackend`]) because the
//! async `p2p.receive` leaf is `Send` while ctx state is `Rc`-based — the `Arc` is what crosses into
//! the receive descriptor.
//!
//! # One node per [`NodeConfig`], not one per run
//!
//! A p2p node **is** an identity plus the directory that identity and its log live in, so "which
//! node" is exactly "which [`NodeConfig`]". The ctx state therefore holds a *map* keyed on the
//! resolved config rather than a single cached backend: a run that only ever asks for the host's
//! config (every program today) gets exactly one node, as before, while a caller that names a
//! distinct data dir gets a distinct node — several user identities alive in one process, which is
//! already a tested p2panda configuration (each node builds its own runtime, endpoint, store and
//! signing key; only mDNS multicast is shared, and it is best-effort).
//!
//! The loopback broker is deliberately **not** keyed: on a host with no real networking every
//! config collapses onto [`NodeConfig::default`], so two replicas in one program still converge
//! through the one deterministic log (the sandbox's stand-in for two peers) and every oracle
//! fixture stays byte-identical.

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;

use noeta_ext_abi::host::P2p;
use noeta_ext_abi::{NativeCtx, P2pBackend, P2pReceiveIo, StdError};

/// The ctx-state key for this extension's per-run p2p backends (namespaced like every other
/// extension's state — `"std.reactive"`, `"std.cell"`, …).
pub const STATE_KEY: &str = "para.p2p";

/// Which node a p2p call runs on: the persistent state that *is* the node's identity.
///
/// A node's identity (`identity.key`), its durable log (`store.db`) and its group-encryption
/// credentials all live in one directory, so naming a directory names a node. `data_dir` set ⇒ that
/// exact directory; `data_dir` unset ⇒ the transport's per-user default for `app_id`
/// (`$NOETA_P2P_DIR`, else `$XDG_DATA_HOME/<app>/p2p`), which is what every program gets today.
///
/// Used as the ctx-state map key, so two calls naming the same config share one live node and two
/// calls naming different dirs get two.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct NodeConfig {
    /// The application namespace the *default* data dir is keyed on — the running program's package
    /// name, supplied by the host. Ignored when `data_dir` is set (the dir is already exact).
    pub app_id: Option<String>,
    /// The exact directory this node's identity and durable store live in, or `None` for the
    /// per-user default for [`Self::app_id`].
    pub data_dir: Option<PathBuf>,
}

impl NodeConfig {
    /// The node living in an exact directory — the multi-identity case (one directory per user).
    pub fn at(dir: impl Into<PathBuf>) -> NodeConfig {
        NodeConfig {
            app_id: None,
            data_dir: Some(dir.into()),
        }
    }

    /// Set the application namespace the default data dir keys on (no effect once `data_dir` is set).
    pub fn with_app(mut self, app_id: Option<String>) -> NodeConfig {
        self.app_id = app_id;
        self
    }
}

/// The [`NodeConfig`] the *host* asks for — the default node of this run, and the one every
/// `p2p.publish` / `p2p.receive` / `synced_signal` uses today.
///
/// A host can currently name only the *app namespace*, never a directory: `RealP2pConfig` carries
/// `app_id` alone, so this maps to `data_dir: None` and the default node lands in the per-app
/// default location. The toolchain-side change that opens the seam is a
/// `pub data_dir: Option<PathBuf>` field on `noeta_ext_abi::host::RealP2pConfig`, filled by a
/// `RealHost::with_p2p_dir` builder; with it, the `None` below becomes `config.data_dir` and a host
/// — a multi-tenant server assigning one directory per signed-in user — steers the default node
/// with no other change on this side. Note the precedence that would follow: a directory named
/// here is a named node, so it wins over `$NOETA_P2P_DIR` rather than yielding to it, which is what
/// keeps one process-wide env var from collapsing every tenant onto a single identity and store.
pub fn host_node_config<C: NativeCtx + ?Sized>(ctx: &mut C) -> Option<NodeConfig> {
    ctx.host().real_p2p().map(|config| NodeConfig {
        app_id: config.app_id,
        data_dir: None,
    })
}

/// Run `f` against this run's default [`P2pBackend`], creating it on first use. A closure rather
/// than a returned `&mut dyn P2p` because the backend borrows through a `Mutex` guard that cannot
/// outlive the call.
pub fn with_p2p<C, R>(
    ctx: &mut C,
    f: impl FnOnce(&mut dyn P2p) -> Result<R, StdError>,
) -> Result<R, StdError>
where
    C: NativeCtx + ?Sized,
{
    let backend = p2p_backend(ctx)?;
    let mut guard = backend.lock().expect("p2p backend mutex poisoned");
    f(&mut *guard)
}

/// This run's default [`P2pBackend`] — the node the host's `real_p2p()` config names, created on
/// first access and cached in ctx state. An `Arc` clone the caller may keep past the ctx borrow (the
/// receive descriptor captures one; see [`receive_descriptor`]).
pub fn p2p_backend<C: NativeCtx + ?Sized>(ctx: &mut C) -> Result<P2pBackend, StdError> {
    let requested = host_node_config(ctx);
    backend_for(ctx, requested)
}

/// The [`P2pBackend`] for `requested` — `None` meaning "this host permits no real networking", which
/// is the loopback broker. Created on first use and cached in ctx state under its resolved key, so
/// asking twice for one node yields the same live node and asking for two data dirs yields two.
pub fn backend_for<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    requested: Option<NodeConfig>,
) -> Result<P2pBackend, StdError> {
    let key = backend_key(requested);
    // Fast path: this node already exists for this run.
    {
        let state = ctx.state(STATE_KEY, new_state);
        let cell = state.borrow();
        if let Some(backend) = nodes(&**cell).get(&key) {
            return Ok(backend.clone());
        }
    }
    // First use of this node: build it, then cache it under its key.
    let backend = create_backend(key.clone())?;
    let state = ctx.state(STATE_KEY, new_state);
    let mut cell = state.borrow_mut();
    nodes_mut(&mut **cell).insert(key, backend.clone());
    Ok(backend)
}

/// The ctx-state key a request resolves to. A real node keys on its own config (identity + dir);
/// every loopback request collapses onto one shared broker — `None` (no real networking) and, when
/// this build carries no transport ring, any request at all, since the backend it would get is the
/// deterministic broker either way.
fn backend_key(requested: Option<NodeConfig>) -> Option<NodeConfig> {
    match requested {
        Some(config) if cfg!(feature = "ring-p2p") => Some(config),
        _ => None,
    }
}

/// Build the backend for `key`: the real p2panda node when the host permits real networking and the
/// `ring-p2p` transport is compiled in, otherwise the deterministic loopback broker.
fn create_backend(key: Option<NodeConfig>) -> Result<P2pBackend, StdError> {
    match key {
        #[cfg(feature = "ring-p2p")]
        Some(config) => {
            let node = noeta_para_p2p_net::P2pNode::start_with_config(transport_config(&config))?;
            Ok(std::sync::Arc::new(std::sync::Mutex::new(node)) as P2pBackend)
        }
        // Real networking permitted but this build carries no transport ring: degrade to loopback
        // (a program still runs locally, just single-node). `backend_key` already collapsed the key,
        // so this arm is unreachable — it exists so the match is total without the ring.
        #[cfg(not(feature = "ring-p2p"))]
        Some(_config) => Ok(loopback_backend()),
        // No real networking (the deterministic sandbox and the minimal hosts).
        None => Ok(loopback_backend()),
    }
}

/// Project a [`NodeConfig`] onto the transport's own config: a persistent node in the named
/// directory when one is given, else the per-app default location (the pre-multi-node behavior).
/// `persist` is always on — an ephemeral node has no identity to be *this user*, which is the whole
/// point of naming a node.
#[cfg(feature = "ring-p2p")]
fn transport_config(config: &NodeConfig) -> noeta_para_p2p_net::P2pConfig {
    let base = match &config.data_dir {
        Some(dir) => noeta_para_p2p_net::P2pConfig::at(dir),
        None => noeta_para_p2p_net::P2pConfig::persistent(),
    };
    base.with_app(config.app_id.clone())
}

/// A fresh, empty node map — the ctx-state initializer.
fn new_state() -> Box<dyn Any> {
    Box::new(HashMap::<Option<NodeConfig>, P2pBackend>::new())
}

fn nodes(state: &dyn Any) -> &HashMap<Option<NodeConfig>, P2pBackend> {
    state
        .downcast_ref()
        .expect("para.p2p state is a node map keyed on NodeConfig")
}

fn nodes_mut(state: &mut dyn Any) -> &mut HashMap<Option<NodeConfig>, P2pBackend> {
    state
        .downcast_mut()
        .expect("para.p2p state is a node map keyed on NodeConfig")
}

/// A fresh loopback-broker backend — the always-available, dep-free default.
fn loopback_backend() -> P2pBackend {
    std::sync::Arc::new(std::sync::Mutex::new(noeta_ext_abi::P2pBroker::default())) as P2pBackend
}

/// Build the async `receive` descriptor for `topic`: a [`P2pReceiveIo`] over this run's backend (a
/// captured `Arc` clone — the `Send` handle that lets the receive leaf resolve without any host p2p
/// state). Creating the backend on first use may fail (a real node binds sockets), so this is
/// fallible, like every other p2p op.
pub fn receive_descriptor<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    topic: String,
) -> Result<Box<dyn noeta_ext_abi::ExternIo>, StdError> {
    let backend = p2p_backend(ctx)?;
    Ok(Box::new(P2pReceiveIo { backend, topic }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Naming a directory names a node: two dirs are two keys, the same dir is one key — the
    /// property the ctx-state map turns into "two live nodes" / "one shared node".
    #[test]
    fn a_node_is_identified_by_its_data_dir() {
        let alice = NodeConfig::at("/var/lib/app/alice");
        let bob = NodeConfig::at("/var/lib/app/bob");
        assert_ne!(alice, bob);
        assert_eq!(alice, NodeConfig::at("/var/lib/app/alice"));
        // The app namespace only picks the *default* location, so it is part of the identity too.
        assert_ne!(
            NodeConfig::default().with_app(Some("acme/chat".into())),
            NodeConfig::default().with_app(Some("acme/wiki".into()))
        );
        assert_eq!(
            NodeConfig::at("/d").data_dir.as_deref(),
            Some(Path::new("/d"))
        );
    }

    /// The loopback broker is shared, never keyed: with no real networking (or no transport ring)
    /// every request collapses onto one deterministic log, so two replicas in one program still
    /// converge and the oracle sees exactly the behavior it saw before nodes were nameable.
    #[test]
    fn loopback_requests_collapse_onto_one_broker() {
        assert_eq!(backend_key(None), None);
        let named = backend_key(Some(NodeConfig::at("/var/lib/app/alice")));
        if cfg!(feature = "ring-p2p") {
            assert_eq!(named, Some(NodeConfig::at("/var/lib/app/alice")));
        } else {
            assert_eq!(named, None, "no ring ⇒ the broker, whatever was asked for");
        }
    }

    /// A host that permits no real networking yields the broker, which has no identity — the
    /// oracle-safe default every sandbox run gets.
    #[test]
    fn no_real_networking_yields_the_broker() {
        let backend = create_backend(None).expect("the broker always starts");
        let mut guard = backend.lock().expect("broker mutex");
        assert_eq!(guard.p2p_identity().expect("identity"), None);
    }

    /// The dir a caller names reaches the transport instead of being discarded: a named node is
    /// persistent *there*, an unnamed one falls back to the per-app default location.
    #[cfg(feature = "ring-p2p")]
    #[test]
    fn a_named_data_dir_reaches_the_transport() {
        let named = transport_config(&NodeConfig::at("/var/lib/app/alice"));
        assert_eq!(
            named.data_dir.as_deref(),
            Some(Path::new("/var/lib/app/alice"))
        );
        assert!(named.persist);

        let default = transport_config(&NodeConfig::default().with_app(Some("acme/chat".into())));
        assert_eq!(default.data_dir, None);
        assert_eq!(default.app_id.as_deref(), Some("acme/chat"));
    }
}
