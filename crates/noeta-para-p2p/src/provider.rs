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
//! Because that map key is the whole guarantee, the directory is resolved **twice**: once when the
//! node is named ([`canonical_dir`], so every spelling of an existing directory is one key) and once
//! more when a key first misses the registry and is about to start a node ([`settle_key`], so a name
//! taken before its directory existed cannot start a *second* node against the same
//! `identity.key`/`store.db`). See [`backend_in`] for why the second resolution has to happen before
//! the node is built rather than after.
//!
//! The loopback broker is deliberately **not** keyed: on a host with no real networking every
//! config collapses onto [`NodeConfig::default`], so two replicas in one program still converge
//! through the one deterministic log (the sandbox's stand-in for two peers) and every oracle
//! fixture stays byte-identical.

use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    ///
    /// The directory is **canonicalized** first ([`canonical_dir`]), because this value is the map
    /// key: `/srv/a`, `/srv/a/`, `srv/a` from the right working directory and a symlink pointing at
    /// any of them are one node, and keying them separately would start several nodes against one
    /// `identity.key` and one `store.db`. That is a demonstrated corruption mode, not a theoretical
    /// one — two nodes sharing a store collide on the store's own migration
    /// (`index ux_orderer_pending_v1 already exists`).
    ///
    /// Canonicalizing here is what makes two handles *compare* equal; it is not what makes them
    /// reach one node. A name may be taken before the directory exists, and the filesystem can move
    /// under an unresolved tail afterwards, so the registry resolves the name once more before it
    /// starts anything ([`settle_key`]) — that, not this, is the guarantee. The consequence worth
    /// knowing: two handles opened either side of such a move reach the same live node but compare
    /// unequal, because equality is a pure function of the name and cannot re-walk the filesystem.
    ///
    /// A **relative** path is resolved against the working directory at the moment `open` is called,
    /// which is deliberate: that is what a relative path means everywhere else in the language, and
    /// a program that `chdir`s between two `open("data")` calls has named two genuinely different
    /// directories. Pinning the first one would make the meaning depend on invisible history rather
    /// than on the visible working directory.
    pub fn at(dir: impl Into<PathBuf>) -> NodeConfig {
        NodeConfig {
            app_id: None,
            data_dir: Some(canonical_dir(&dir.into())),
        }
    }

    /// Set the application namespace the default data dir keys on (no effect once `data_dir` is set).
    pub fn with_app(mut self, app_id: Option<String>) -> NodeConfig {
        self.app_id = app_id;
        self
    }
}

/// One spelling for one directory, so one directory is one node.
///
/// `fs::canonicalize` is the only thing that resolves symlinks, but it **fails on a path that does
/// not exist yet** — and naming a node whose directory has not been created is the normal case (a
/// first run creates it). So: make the path absolute, then canonicalize the longest **existing**
/// ancestor and re-append the segments below it. The existing part gets full symlink/`..`
/// resolution; the not-yet-created tail is carried literally, which is exactly right because there
/// is nothing there to resolve yet — and once it *is* created, every later spelling canonicalizes
/// through the same resolved ancestor and lands on the same key.
///
/// Chosen over **create-then-canonicalize** deliberately: naming a node should not have the side
/// effect of creating a directory, and it must not be able to *fail* — a name is not an operation.
/// The transport creates the directory when the node actually starts, which is where a permission
/// error belongs. (`.` segments and a trailing slash need no handling: [`Path`] compares and hashes
/// by component, so `/a/./b/` and `/a/b` are already the same key.)
///
/// # This is a resolution, not a decision — it is redone at first use
///
/// Carrying the tail literally is the best answer *available at naming time*, but it is not final:
/// it is an answer about a path that does not exist, and the filesystem can still move under it.
/// One spelling therefore still splits into two names if a **symlink appears at a not-yet-existing
/// tail segment between two `open`s** — measured, not hypothetical:
///
/// ```text
/// named before `link` exists : /root/link/alice          (tail carried literally)
/// symlink link -> elsewhere
/// named after                : /root/elsewhere/alice     (now resolved through it)
/// ```
///
/// Two names, one directory on disk — which is two p2panda nodes on one `identity.key` and one
/// `store.db` if the names are allowed to reach the registry as they are. [`settle_key`] is what
/// stops them: the registry re-runs this against the filesystem *as it is at first use*, and both
/// spellings above settle onto `/root/elsewhere/alice`.
fn canonical_dir(dir: &Path) -> PathBuf {
    let absolute = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = absolute.clone();
    loop {
        if let Ok(resolved) = probe.canonicalize() {
            let mut out = resolved;
            out.extend(tail.iter().rev());
            return out;
        }
        // Nothing at `probe` exists yet — step up and keep the segment for re-appending.
        let name = probe.file_name().map(|n| n.to_os_string());
        match (probe.parent().map(Path::to_path_buf), name) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                probe = parent;
            }
            // No ancestor resolved at all (the root itself is unreadable): the absolute path is the
            // best key available, and it is still stable for a given spelling.
            _ => return absolute,
        }
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

/// Run `f` against the [`P2pBackend`] of the node `config` names — the node-scoped twin of
/// [`with_p2p`], which is this with the host's own config. `None` is the loopback broker.
pub fn with_node<C, R>(
    ctx: &mut C,
    config: Option<NodeConfig>,
    f: impl FnOnce(&mut dyn P2p) -> Result<R, StdError>,
) -> Result<R, StdError>
where
    C: NativeCtx + ?Sized,
{
    let backend = backend_for(ctx, config)?;
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
    // Whether this host permits real networking at all. It must be asked HERE rather than inferred
    // from `requested`: a program can name a node with `p2p.open` on any host, including the
    // deterministic sandbox, and a named node must not be what turns a live QUIC transport on.
    let real_permitted = ctx.host().real_p2p().is_some();
    // The registry rule itself lives in `backend_in`, spelled once; this is only where the map is
    // kept. Building a node touches no ctx state, so holding the borrow across it is safe.
    let state = ctx.state(STATE_KEY, new_state);
    let mut cell = state.borrow_mut();
    backend_in(nodes_mut(&mut **cell), requested, real_permitted)
}

/// The **node registry** itself, over a plain map — [`backend_for`] minus the ctx-state plumbing.
/// One directory reaches one live node because one key reaches one map entry; this is where that
/// happens, and it is separated out so the property is testable without a whole `NativeCtx`.
///
/// # A name is resolved twice: once when taken, once before it starts a node
///
/// A miss is the only moment a key can still be wrong, and it is exactly the moment being wrong
/// gets expensive — a miss is what *starts a node*. The key was resolved when the node was
/// **named**, possibly before its directory existed, so it may carry an unresolved tail
/// ([`canonical_dir`]); a symlink appearing at one of those segments in the meantime splits one
/// directory into two names. So on a miss, and only on a miss, the key is resolved again against
/// the filesystem as it is *now* ([`settle_key`]) — after which a handle opened before the
/// directory existed and one opened after are the same entry, in either order.
///
/// Two things follow from settling *before* [`create_backend`] rather than after, which is the
/// whole reason it happens here:
///
/// - **A settled key that collides with a live node yields that node.** The incumbent is this
///   directory's node, by definition — there is nothing to reconcile and nothing to fail. Because
///   nothing has been built yet, the collision is resolved by *not building*, which matters: a
///   second p2panda node on one `store.db` does not merely waste a socket, it collides on the
///   store's own migration (`index ux_orderer_pending_v1 already exists`), so "build, then discover
///   the collision, then tear down" would have to survive the very failure it is preventing.
/// - **A collision can never be discovered after building.** The settled key is looked up before
///   `create_backend` and the map cannot change across it (see below), so the insert below is
///   always into a vacant entry.
///
/// The stale key is recorded as an **alias** onto the same `Arc` rather than dropped, so a handle
/// that outran its directory costs one extra map entry instead of a filesystem walk on every
/// `publish`. It also pins that handle to the node it woke: re-settling on every call would let a
/// live handle migrate to a *different* node mid-run if the filesystem moved again.
///
/// # Racing
///
/// Nothing in one run can interleave with the settle→lookup→create→insert sequence: the registry
/// lives in ctx state behind a `RefCell` whose borrow is held across the whole of this call
/// ([`backend_for`]), and building a node touches no ctx state, so there is no re-entry point. A
/// concurrent `open` in the same run cannot observe a half-updated map, and cannot even reach one.
///
/// What this cannot serialize is a registry it does not own: a second run/isolate in the process
/// has its own ctx state, and another OS process has its own everything. Two of those naming one
/// directory are two nodes on one store, exactly as before — closing that needs an on-disk lock in
/// the transport, not a key in this map. Likewise the filesystem itself is not held still: another
/// process can plant a symlink between [`settle_key`] and the transport's own `create_dir_all`. The
/// window is two adjacent syscalls wide and needs an actively hostile neighbor, where the defect
/// this closes needed only a program that opened a handle a little early.
pub fn backend_in(
    nodes: &mut HashMap<Option<NodeConfig>, P2pBackend>,
    requested: Option<NodeConfig>,
    real_permitted: bool,
) -> Result<P2pBackend, StdError> {
    let key = backend_key(requested, real_permitted);
    if let Some(backend) = nodes.get(&key) {
        return Ok(backend.clone());
    }
    let settled = settle_key(key.clone());
    let moved = settled != key;
    if moved && let Some(backend) = nodes.get(&settled) {
        // This directory already has a live node under its settled name — that node *is* the
        // directory's node. Hand it back and alias the stale spelling onto it.
        let backend = backend.clone();
        nodes.insert(key, backend.clone());
        return Ok(backend);
    }
    let backend = create_backend(settled.clone())?;
    if moved {
        nodes.insert(key, backend.clone());
    }
    nodes.insert(settled, backend.clone());
    Ok(backend)
}

/// Resolve a key against the filesystem **as it is now** — [`canonical_dir`] re-run at first use,
/// which is what makes a name taken before its directory existed land on the same node as one taken
/// after.
///
/// Total and side-effect-free, like the naming it repeats: it creates nothing, and a path it still
/// cannot resolve comes back unchanged (a key that stays literal is at least stable). Only a key
/// that *names a directory* does any work — the loopback broker's `None` key and the default node's
/// `data_dir: None` walk no filesystem at all, so a build without the ring (where
/// [`backend_key`] collapses every request onto `None`) never touches the disk here.
fn settle_key(key: Option<NodeConfig>) -> Option<NodeConfig> {
    let mut config = key?;
    if let Some(dir) = config.data_dir.take() {
        config.data_dir = Some(canonical_dir(&dir));
    }
    Some(config)
}

/// The ctx-state key a request resolves to. A real node keys on its own config (identity + dir);
/// every loopback request collapses onto one shared broker.
///
/// Two conditions must BOTH hold for a request to name a node of its own: the host permits real
/// networking (`real_permitted`), and this build carries the transport ring. Otherwise the backend
/// would be the deterministic broker either way, and keying it per config would fragment the one
/// shared log — so the key collapses to `None`.
///
/// The host condition is load-bearing and easy to lose: naming a node (`p2p.open`) is something a
/// program may do on *any* host, so if the key ignored the host policy, a sandboxed or oracle run
/// that opened a node would start a live p2panda node over QUIC. The conformance corpus under
/// `--features ring-p2p` is what catches that, and did.
fn backend_key(requested: Option<NodeConfig>, real_permitted: bool) -> Option<NodeConfig> {
    match requested {
        Some(config) if real_permitted && cfg!(feature = "ring-p2p") => Some(config),
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
    let config = host_node_config(ctx);
    receive_descriptor_for(ctx, config, topic)
}

/// [`receive_descriptor`] against the node `config` names, for `Node.receive`.
pub fn receive_descriptor_for<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    config: Option<NodeConfig>,
    topic: String,
) -> Result<Box<dyn noeta_ext_abi::ExternIo>, StdError> {
    let backend = backend_for(ctx, config)?;
    Ok(Box::new(P2pReceiveIo { backend, topic }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory tree under the OS temp dir, removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "noeta-p2p-provider-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            TempDir(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Naming a directory names a node: two dirs are two keys, the same dir is one key — the
    /// property the ctx-state map turns into "two live nodes" / "one shared node".
    #[test]
    fn a_node_is_identified_by_its_data_dir() {
        let root = TempDir::new("identity");
        let alice = NodeConfig::at(root.0.join("alice"));
        let bob = NodeConfig::at(root.0.join("bob"));
        assert_ne!(alice, bob);
        assert_eq!(alice, NodeConfig::at(root.0.join("alice")));
        // The app namespace only picks the *default* location, so it is part of the identity too.
        assert_ne!(
            NodeConfig::default().with_app(Some("acme/chat".into())),
            NodeConfig::default().with_app(Some("acme/wiki".into()))
        );
    }

    /// **One directory is one node, however it is spelled.** Keying the live-node map on the raw
    /// path would let a trailing slash, a `.` segment, a relative path or a symlink start several
    /// nodes against one `identity.key` and one `store.db` — a store-level collision, not a
    /// cosmetic one. Every spelling below must produce the identical key.
    #[test]
    fn every_spelling_of_one_directory_is_one_node() {
        let root = TempDir::new("spelling");
        let real = root.0.join("alice");
        std::fs::create_dir_all(&real).expect("node dir");
        let canonical = NodeConfig::at(&real);

        // A trailing separator, and a `.` segment in the middle.
        assert_eq!(canonical, NodeConfig::at(format!("{}/", real.display())));
        assert_eq!(canonical, NodeConfig::at(root.0.join(".").join("alice")));
        // A `..` that walks back through a real directory.
        assert_eq!(
            canonical,
            NodeConfig::at(root.0.join("alice").join("..").join("alice"))
        );
        // A symlink pointing at the same directory — the case only `canonicalize` catches.
        #[cfg(unix)]
        {
            let link = root.0.join("alice-link");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            assert_eq!(canonical, NodeConfig::at(&link));
        }
        // A relative path resolved against the current working directory.
        let cwd = std::env::current_dir().expect("cwd");
        if let Ok(relative) = real.strip_prefix(&cwd) {
            assert_eq!(canonical, NodeConfig::at(relative));
        }

        // A directory that does not exist yet still gets a stable, absolute key — naming a node
        // must not require creating it, and must not fail.
        let unborn = root.0.join("bob").join("deep").join("not-yet");
        assert_eq!(NodeConfig::at(&unborn), NodeConfig::at(&unborn));
        assert!(
            NodeConfig::at(&unborn)
                .data_dir
                .expect("a named node has a dir")
                .is_absolute()
        );
        assert!(!unborn.exists(), "naming a node must not create it");
        // …and once it *is* created, a fresh spelling still lands on that same key.
        let before = NodeConfig::at(&unborn);
        std::fs::create_dir_all(&unborn).expect("create the named dir");
        assert_eq!(before, NodeConfig::at(&unborn));
    }

    /// The loopback broker is shared, never keyed: with no real networking (or no transport ring)
    /// every request collapses onto one deterministic log, so two replicas in one program still
    /// converge and the oracle sees exactly the behavior it saw before nodes were nameable.
    #[test]
    fn loopback_requests_collapse_onto_one_broker() {
        let root = TempDir::new("collapse");
        let alice = NodeConfig::at(root.0.join("alice"));
        assert_eq!(backend_key(None, true), None);
        // A named node on a host that permits no real networking is still the broker — naming a
        // node must never be what enables a live transport.
        assert_eq!(backend_key(Some(alice.clone()), false), None);
        let named = backend_key(Some(alice.clone()), true);
        if cfg!(feature = "ring-p2p") {
            assert_eq!(named, Some(alice));
        } else {
            assert_eq!(named, None, "no ring ⇒ the broker, whatever was asked for");
        }
    }

    /// **Opening one directory twice reaches one live node.** The registry is what guarantees it:
    /// two requests naming the same directory (in any spelling) resolve to one key and therefore
    /// one entry — asserted on pointer identity, since two `P2pBackend`s that merely compare equal
    /// would still be two nodes on one store.
    #[test]
    fn opening_one_directory_twice_reaches_one_node() {
        let root = TempDir::new("reuse");
        let alice = root.0.join("alice");
        std::fs::create_dir_all(&alice).expect("node dir");
        let mut nodes = HashMap::new();

        let first = backend_in(&mut nodes, Some(NodeConfig::at(&alice)), true).expect("first open");
        // A different spelling of the same directory — the collision this must not create.
        let again = backend_in(
            &mut nodes,
            Some(NodeConfig::at(format!("{}/", alice.display()))),
            true,
        )
        .expect("second open");
        assert!(
            std::sync::Arc::ptr_eq(&first, &again),
            "one directory must reach one live node"
        );
        assert_eq!(nodes.len(), 1, "and must occupy one registry entry");
    }

    /// A directory named **before it exists** and the same directory named after are one node.
    ///
    /// This is the hole [`canonical_dir`] alone cannot close: a name taken before the directory
    /// existed carries its tail literally, so a symlink appearing at one of those segments in
    /// between makes the second spelling resolve somewhere the first does not — two keys for one
    /// directory on disk, which under the ring is two p2panda nodes on one `identity.key` and one
    /// `store.db`. Asserted in **both** orders (early handle used first, late handle used first),
    /// and on pointer identity, because two backends that merely compare equal would still be two
    /// nodes on one store.
    #[cfg(unix)]
    #[test]
    fn a_name_taken_before_its_directory_existed_reaches_one_node() {
        for (tag, early_first) in [("race-early", true), ("race-late", false)] {
            let root = TempDir::new(tag);
            let target = root.0.join("elsewhere");
            std::fs::create_dir_all(&target).expect("the directory the link will point at");
            let real = target.canonicalize().expect("the link target resolves");
            let spelling = root.0.join("link").join("alice");

            // Named while nothing exists at `link`: the tail is carried literally.
            let early = NodeConfig::at(&spelling);
            // …then a symlink appears at exactly that not-yet-existing segment.
            std::os::unix::fs::symlink(&target, root.0.join("link")).expect("symlink");
            // The same spelling, named again — now resolved through the link.
            let late = NodeConfig::at(&spelling);
            assert_ne!(
                early, late,
                "the premise: two names for one directory, which is what the registry must absorb"
            );
            let through_link = spelling
                .parent()
                .expect("the linked segment")
                .canonicalize()
                .expect("the link resolves");
            assert_eq!(
                through_link, real,
                "and they really are one directory on disk"
            );

            let mut nodes = HashMap::new();
            let (first, second) = if early_first {
                (early, late)
            } else {
                (late, early)
            };
            let a = backend_in(&mut nodes, Some(first), true).expect("first open");
            let b = backend_in(&mut nodes, Some(second), true).expect("second open");
            assert!(
                std::sync::Arc::ptr_eq(&a, &b),
                "one directory must reach one live node, however early its name was taken"
            );
            // Under the ring that node is a *real* p2panda node, not the broker standing in for one
            // — so this is the store-sharing case being pinned, not a loopback look-alike. (The
            // broker has no identity, which is what distinguishes them.)
            let identity = a
                .lock()
                .expect("p2p backend mutex")
                .p2p_identity()
                .expect("identity");
            assert_eq!(identity.is_some(), cfg!(feature = "ring-p2p"));
        }
    }

    /// The rule the registry leans on, on its own: re-resolving a key against the filesystem as it
    /// is *now* collapses a name that outran its directory onto the name of the directory itself.
    /// Build-independent — without the ring every key is `None`, so this is the only place the
    /// property is observable there.
    #[cfg(unix)]
    #[test]
    fn settling_a_key_collapses_a_name_that_outran_its_directory() {
        let root = TempDir::new("settle");
        let target = root.0.join("elsewhere");
        std::fs::create_dir_all(&target).expect("link target");
        let spelling = root.0.join("link").join("alice");

        let early = NodeConfig::at(&spelling);
        std::os::unix::fs::symlink(&target, root.0.join("link")).expect("symlink");
        let late = NodeConfig::at(&spelling);
        assert_ne!(early, late);

        assert_eq!(
            settle_key(Some(early.clone())),
            settle_key(Some(late.clone())),
            "both names settle onto the one directory"
        );
        // The settled name is the link's *target*, spelled with std's own `canonicalize` — an
        // oracle independent of `canonical_dir` — and settling it again changes nothing.
        let settled = settle_key(Some(early)).expect("a named node has a config");
        let expected = target.canonicalize().expect("target").join("alice");
        assert_eq!(settled.data_dir.as_deref(), Some(expected.as_path()));
        assert_eq!(settle_key(Some(settled.clone())), Some(settled));
        // And it resolves nothing it is not asked to: the broker's key and the default node's
        // dir-less config come back untouched, so a loopback build walks no filesystem.
        assert_eq!(settle_key(None), None);
        let default = NodeConfig::default().with_app(Some("acme/chat".into()));
        assert_eq!(settle_key(Some(default.clone())), Some(default));
    }

    /// Two distinct directories in one process are two nodes — the multi-identity property, at the
    /// registry level. Under the ring they are two live p2panda nodes; without it both collapse
    /// onto the one deterministic broker, which is the documented sandbox behavior.
    #[test]
    fn two_directories_in_one_process_are_two_nodes() {
        let root = TempDir::new("two");
        let (alice, bob) = (root.0.join("alice"), root.0.join("bob"));
        std::fs::create_dir_all(&alice).expect("alice dir");
        std::fs::create_dir_all(&bob).expect("bob dir");
        let mut nodes = HashMap::new();

        let a = backend_in(&mut nodes, Some(NodeConfig::at(&alice)), true).expect("alice");
        let b = backend_in(&mut nodes, Some(NodeConfig::at(&bob)), true).expect("bob");
        if cfg!(feature = "ring-p2p") {
            assert!(
                !std::sync::Arc::ptr_eq(&a, &b),
                "two directories are two nodes"
            );
            assert_eq!(nodes.len(), 2);
        } else {
            assert!(
                std::sync::Arc::ptr_eq(&a, &b),
                "no ring ⇒ one shared broker, whatever was named"
            );
            assert_eq!(nodes.len(), 1);
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
        let root = TempDir::new("transport");
        let dir = root.0.join("alice");
        std::fs::create_dir_all(&dir).expect("node dir");
        let named = transport_config(&NodeConfig::at(&dir));
        // Compared against std's own `canonicalize` — an oracle independent of `canonical_dir`.
        assert_eq!(named.data_dir, Some(dir.canonicalize().expect("canonical")));
        assert!(named.persist);

        let default = transport_config(&NodeConfig::default().with_app(Some("acme/chat".into())));
        assert_eq!(default.data_dir, None);
        assert_eq!(default.app_id.as_deref(), Some("acme/chat"));
    }
}
