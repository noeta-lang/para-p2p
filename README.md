# para/p2p

Local-first, collaborative state for Noeta — CRDTs, a peer-to-peer transport, and CRDT-backed reactive signals that converge across peers without coordination. A peer's change flows into the reactivity graph and reruns your `computed`/`effect`s exactly like a local edit — *signals that happen to be shared*.

This package is **fully native**: its whole surface is a Rust extension (there is no `.noe` layer), registered when a program depends on `para/p2p` and authorized in `[trust]`.

## What it provides

Three modules, all rooted at `para`:

- **`para.crdt`** — state-based CRDT value types: `crdt.gcounter()`, `crdt.pncounter()`, `crdt.gset()`, and the Automerge-backed document `crdt.automerge()`. Merge is commutative, associative, and idempotent, so independent replicas converge to the same value regardless of the order or duplication of the updates they exchange. The set is **open**: `Mergeable` and `Syncable` are ordinary traits your own type can implement.
- **`para.p2p`** — `publish` / `receive` / `identity` over the `P2p` host capability, on the default node or on one you `p2p.open(dir)` yourself (several peer identities in one program). With the real transport (the `ring-p2p` feature of the extension crate) that is a live p2panda node — gossip + log-sync over iroh/QUIC; without it, a deterministic in-process loopback broker, so plain builds and tests never link the networking tree.
- **`para.synced`** — `synced_signal(crdt, topic)`: a CRDT-backed signal that is a node in the **same** reactive graph as core `std.reactive` — a merge (local or from a peer) reruns dependent `computed`/`effect`s. Over the real transport it adds end-to-end group encryption and dynamic membership.

The Rust crates behind that surface:

- `crates/noeta-crdt` — the dependency-free convergence core (the merge algebra, property-tested for the three CRDT laws).
- `crates/noeta-para-p2p` — the extension crate (modules, extern types, the reactive-graph seam).
- `crates/noeta-para-p2p-net` — the p2panda transport (optional, behind the extension's `ring-p2p` feature).

## Installation

```toml
[dependencies]
para = { version = "^0.3", package = "para/p2p" }

[trust]
native = ["para/p2p"]   # authorizes the package's native extension
```

The package is keyed `para`, so its modules address as `para.crdt`, `para.p2p`, and `para.synced`.

## Usage

```noeta
use para.{crdt}
use para.synced.{synced_signal}
use std.reactive.{effect}

// Two replicas of a grow-only counter converge on merge.
a = crdt.gcounter().increment("A", 3).increment("A", 2)
b = crdt.gcounter().increment("B", 4)
echo "merged=${a.merge(b).value()}"

// A CRDT-backed signal in the shared reactive graph: a merge reruns the effect.
x = synced_signal(crdt.gcounter().increment("A", 1), "demo")
seen = effect(fn() {
    echo "synced: x = ${x.get().value()}"
})
x.merge(crdt.gcounter().increment("B", 5))
```

## CRDTs — values that merge without coordination

A CRDT is a value whose concurrent edits **merge** into the same result regardless of the order they arrive in, so replicas converge without a coordinator — and duplicated or out-of-order messages are harmless. CRDTs are ordinary immutable values: an update returns a *new* value, equality is by content, and they print for debugging (`<gcounter 7>`, `<gset [x, y]>`). Each carries a **replica id** — a string identifying the node that made a change — which you supply explicitly.

| Constructor | Type | What it is |
| --- | --- | --- |
| `crdt.gcounter()` | `GCounter` | A grow-only counter (only increments). |
| `crdt.pncounter()` | `PnCounter` | A counter that also decrements. |
| `crdt.gset()` | `GSet` | A grow-only set of strings. |
| `crdt.automerge()` | `AutoDoc` | An Automerge document — a string map that can **update and delete**. |

`AutoDoc` is the one to reach for when a grow-only lattice cannot express your data. The three above can only ever *gain* information — that is what makes their merge a lattice join — so none of them can represent removing or overwriting a value. `AutoDoc` can: `.put(key, value)`, `.get(key): ?string`, `.remove(key)`, `.keys(): List<string>`. Concurrent writes to different keys both survive; concurrent writes to the same key resolve identically on every replica.

```noeta
doc = crdt.automerge().put("title", "draft").put("author", "ada")
echo doc.remove("author").keys()          // ["title"] — a deletion that converges
```

It is backed by [Automerge](https://automerge.org), not hand-rolled: p2panda ships no application CRDT by design (`p2panda-core` is "compatible with any application data and CRDT"), so the value layer is the consumer's to choose, and Automerge's `save`/`load`/`merge` match this package's `Syncable`/`Mergeable` contracts directly. The transport, log-sync and encryption stay p2panda's. The surface is deliberately a string map for now — nested maps, lists and rich text are the obvious follow-on.

Every CRDT has `.merge(other)` (returning the converged value) and a reader: `GCounter`/`PnCounter` expose `.value(): int`; `GSet` exposes `.contains(e): bool`, `.len(): int`, and `.members(): List<string>` (sorted). Counters take `.increment(replica, by = 1)`, and `PnCounter` also `.decrement(replica, by = 1)`; amounts must be non-negative — a counter that needs to go down is a `PnCounter`, whose `value()` nets increments against decrements and may go negative.

```noeta
use para.{crdt}

// A grow-only counter: merge takes the per-replica max, so independent increments sum.
a = crdt.gcounter().increment("A", 3)
b = crdt.gcounter().increment("B", 4)
echo a.merge(b).value()                     // 7 — and a.merge(b) == b.merge(a)

// A PN-counter tracks both directions per replica.
c = crdt.pncounter().increment("A", 10).decrement("A", 3)
echo c.value()                              // 7

// A grow-only set converges by union.
s = crdt.gset().insert("x").insert("y").merge(crdt.gset().insert("z"))
echo s.members()                            // ["x", "y", "z"]
```

`.merge` only accepts the **same** CRDT type — `gcounter().merge(gset())` is a compile error, not a runtime surprise.

## Your own CRDT — `Mergeable` and `Syncable`

The four above are not the whole world, and the traits behind them are ordinary ones your type can implement:

```noeta
use para.crdt.{Mergeable, Syncable}

// A last-write-wins register, which this package does not ship.
class Lww {
    pub at: int
    pub value: string

    fn new(at: int, value: string): Lww { return Lww { at: at, value: value } }

    impl Mergeable {
        fn merge(other: Lww): Lww {
            return if other.at > self.at then other else self
        }
    }
    impl Syncable {
        fn to_bytes(): bytes { return "${self.at}|${self.value}".to_bytes() }
        fn merge_bytes(other: bytes): Lww {
            parts = (other.decode() ?? "").split("|", 2)
            at = if parts.len() > 0 then parts[0].to_int() ?? 0 else 0
            text = if parts.len() > 1 then parts[1] else ""
            return self.merge(Lww.new(at, text))
        }
    }
}

x = synced_signal(Lww.new(1, "hello"), "topic")   // accepted like any built-in
```

**Why two traits.** Converging and being replicated are different capabilities. A value can merge usefully in-process and never leave it, and making that value justify a wire encoding would tax the common case. So `Mergeable` is `merge` alone, `Syncable` adds the wire, and `synced_signal` asks for both — a type that merges but has no encoding is refused *naming `Syncable`*, rather than with a vague "not a CRDT".

`Syncable`'s contract is instance-only for a concrete reason: a trait method has a `Self` receiver, so there is nowhere to hang a static `from_bytes`. Decoding folds into `merge_bytes` — "decode a peer's state and merge it into me" — which the engine can always call, because it holds the current value when peer state arrives. It also degrades well: answer a malformed payload by returning yourself unchanged, and the engine reads that as "nothing changed".

> [!WARNING]
> The checker enforces that you **supplied** a `merge`, not that it is commutative, associative and idempotent. No type system can check that. The four built-ins are property-tested for the three laws in `crates/noeta-crdt` and `autodoc.rs`; your type deserves the same treatment, because a merge that violates them diverges silently — replicas simply stop agreeing, with nothing to catch it.

## Peer-to-peer messaging — `para.p2p`

`para.p2p` is the transport underneath synced state: publish a message to a **topic**, receive messages other peers published to it. Messages are opaque bytes (a string rides as its UTF-8), so any payload — including serialized CRDT state — travels over it.

| Function | Signature | Behavior |
| --- | --- | --- |
| `p2p.publish` | `(topic: string, message: string \| bytes)` | Broadcast to everyone subscribed to the topic. |
| `p2p.receive` | `(topic: string) -> Future<?bytes>` | The next message — `.await` it; `none` once the topic has drained. |
| `p2p.identity` | `() -> ?string` | This node's stable identity (the hex Ed25519 public key it signs with); `none` under the loopback broker, which has no network identity. |
| `p2p.open` | `(dir: string) -> Node` | The node whose identity and durable store live in `dir` — how one program acts as several peers. |
| `p2p.node` | `() -> Node` | The default node, the one the three functions above run on, as a value you can pass around. |

Topics are independent channels: every subscriber sees every message, and receiving from an empty topic yields `none` immediately, so a drain loop terminates:

```noeta
use para.{p2p}

async fn drain(): void {
    p2p.publish("room", "hello")
    p2p.publish("room", "world")
    mut running = true
    while running {
        msg = p2p.receive("room").await
        (hex, keep) = match msg {
            some(bytes) => (bytes.to_hex(), true),
            none => ("", false),
        }
        if keep { echo hex }
        running = keep
    }
}
```

### Several identities in one program

The three functions above run on the program's **default** node. That is the right shape for an app that is one peer, but a multi-user desktop app or a multi-tenant server is several peers at once, and a node's identity is exactly the directory its `identity.key` and `store.db` live in. `p2p.open(dir)` names such a node; every function above is also a method on it.

```noeta
use para.{crdt}
use para.{p2p}

alice = p2p.open("/srv/app/users/alice")
bob = p2p.open("/srv/app/users/bob")

alice.publish("room", "hello from alice")
echo alice.identity()                       // alice's own Ed25519 key, not bob's
counter = bob.synced_signal(crdt.gcounter(), "tally")
```

A `Node` is **a name, not a resource**: it holds no socket, so nothing has to be closed, and `p2p.open` neither creates the directory nor starts the node — the node starts on first use, like the default one always has. Opening one directory twice reaches one live node however you spell the path (`/srv/a`, `/srv/a/`, a relative path, a symlink all resolve to the same node), because two nodes sharing one store would corrupt it.

`p2p.node()` is the default node as a value, so a function can take a `Node` and work for either.

> [!NOTE]
> `p2p.open` is not a permission boundary and does not try to be one. If your program can write to a directory, it can open a node there; restricting that is a sandbox's job (a container, a flatpak), not this library's.

> [!NOTE]
> `node.synced_signal(…)` accepts exactly the same values as `para.synced.synced_signal(…)` — both built-in and user-defined CRDTs, and both reject a non-CRDT at compile time — but the method form reports the rejection as a plain type error (`E0007`) rather than the named bound violation (`E0025`). The bound is declared identically; the checker only resolves a signature variable from a *generic receiver's* type arguments, and `Node` is not generic.

## Synced signals — reactive state shared across peers

A `synced_signal(initial, topic)` — on the default node, or `node.synced_signal(initial, topic)` on one you opened — fuses the layers: a reactive signal whose value is a CRDT and whose changes replicate over a p2p topic. Its value type must be `Mergeable + Syncable` — it has to converge *and* know how to cross the wire — enforced at compile time, so you can never sync a value with no convergence story (`synced_signal(42, "t")` is a type error), nor one that converges but cannot be transmitted. Because a synced signal is a node in the *same* reactive graph as `signal`/`computed`/`effect`, a peer's merge propagates to dependents exactly like a local update.

The surface is a signal you converge rather than overwrite:

| Method | Behavior |
| --- | --- |
| `.get(): T` | The current merged value; a read inside a `computed`/`effect` subscribes to it. |
| `.merge(delta: T)` | Merge `delta` into the local value, rerun dependents, and **publish** the new state to peers. |
| `.sync()` | **Pull**: drain the topic, merge every peer state in, and rerun dependents once if anything changed. |
| `.status(): string` | This replica's convergence state for its topic: `"synced"` / `"syncing"` / `"offline"`. |

`.sync()` is deliberately explicit — the network boundary stays visible, so it is legible in your code exactly where remote state enters, rather than hiding behind every read. Two `synced_signal`s on one topic in the same program are two replicas that converge through the transport:

```noeta
use para.{crdt}
use std.reactive.{effect}
use para.synced.{synced_signal}

a = synced_signal(crdt.gcounter().increment("A", 1), "counter")
b = synced_signal(crdt.gcounter().increment("B", 2), "counter")

e = effect(fn() {
    echo "effect sees a=${a.get().value()}"     // prints 1 on creation
})

a.sync()                                        // merges b's state → 3; the effect reruns
b.sync()
echo "final a=${a.get().value()} b=${b.get().value()}"   // 3 == 3
```

`.status()` is always `"synced"` under the loopback broker (a single node never lags); over a real network it reports genuine offline/sync state — e.g. an `effect` rendering "working offline".

## Encrypted groups — end-to-end encryption with dynamic membership

A third argument — a **member set** of peer-id strings — makes a synced signal end-to-end encrypted to exactly those peers. Every state it publishes crosses the wire encrypted; a node outside the set sees only ciphertext. The members are given at construction, so the very first state announced to the topic is already encrypted — there is no window where it goes out in the clear.

```noeta
use para.{crdt}
use para.synced.{synced_signal}

members = ["alice", "bob"]                  // peer ids — their p2p.identity() strings
tally = synced_signal(crdt.gcounter(), "team/tally", members)
tally.merge(crdt.gcounter().increment("alice", 1))   // encrypted before it leaves the node
tally.sync()                                          // decrypts peers' state in

tally.add_member("carol")     // carol can now read new state
tally.remove_member("bob")    // the group key rotates — bob stops decrypting anything published from now on
```

`.add_member(peer_id)` admits a peer; `.remove_member(peer_id)` revokes one and **rotates the group key**. The group creator is authoritative over membership. Under the real transport this is backed by p2panda's group encryption (a symmetric group key — XChaCha20-Poly1305 — with the property that a member who joins late can still decrypt prior state, exactly what a convergent CRDT needs). Encryption is transparent to your program: `.get()` returns the same converged value it would without it, so the deterministic loopback treats an encrypted signal as a pass-through and an encrypted program behaves identically on both backends.

> [!WARNING]
> `.add_member` / `.remove_member` are only valid on an encrypted signal — one created with a members list. Calling them on a plaintext signal is a runtime error. There is no way to declare members without encryption: members-imply-encryption is the safe default.

## Convergence semantics — what merge guarantees

Merge is a lattice join, property-tested in `crates/noeta-crdt` for the three CRDT laws:

1. **commutativity** — `a.merge(b) == b.merge(a)`
2. **associativity** — `(a.merge(b)).merge(c) == a.merge(b.merge(c))`
3. **idempotence** — `a.merge(a) == a`

Together these mean replicas may exchange state in any order, with any duplication (a node re-merging its own echoes included), and still land on the same value. Practical consequences:

- Merging a state already reflected is a no-op, so `.sync()` never spuriously reruns your effects — dependents wake only when the value actually changed.
- A malformed or cross-type message received from a peer is untrusted input: `.sync()` skips it rather than aborting.
- Merging two *different* CRDT types is rejected — statically on direct `.merge` calls, and as a clean runtime error (`cannot merge CRDT values of different types`) at the sync engine's dynamic seam.
- There is no "set": a CRDT-backed signal has no way to overwrite, only to converge. Deletion needs a CRDT that can represent it (`PnCounter` can go down; `GSet` cannot forget).

## Transport & persistence — the loopback broker and the real node

The backend is chosen per run by the host's real-networking policy, and both implement the same `P2p` capability, so your program is identical either way:

- **Loopback broker** — a deterministic in-process, per-topic FIFO log. Used on hosts that permit no real networking (the sandbox, tests) and in builds without the `ring-p2p` feature. Publish-then-receive drains in publish order and terminates, so p2p programs are testable and reproducible; two synced signals on one topic model two peers deterministically.
- **Real p2panda node** (`ring-p2p`) — a live p2panda-net node: mDNS peer discovery, NAT-traversing QUIC via iroh, gossip pub/sub, and eventual-consistency **log-sync** backing `synced_signal` — every state a replica publishes appends to a durable, signed operation log, so a late-joining peer syncs the full history and converges from it.

The real node is **persistent by default**: its Ed25519 identity (`identity.key`, the value `p2p.identity()` returns) and durable operation store (`store.db`) live in a per-app data directory, so a `noeta run` of a p2p program keeps its identity and synced logs across restarts with zero configuration. Two Noeta apps on one machine never share an identity or store.

A node **is** its directory — identity, durable log and encryption credentials all live there — so naming a directory names a node, and the precedence follows from that. A node opened at an explicit directory uses that directory, full stop: no environment variable overrides it, because overriding it would silently collapse several named nodes onto one identity and store. A node nobody named — the default node a plain `p2p.publish` runs on — resolves its directory as `$NOETA_P2P_DIR` if set (an absolute path), else `$XDG_DATA_HOME/<app>/p2p`, where `<app>` is `$NOETA_P2P_APP` if set, else the project's package name (supplied by the toolchain), else the running binary's own file stem.

> [!NOTE]
> `p2p.identity()` returns `none` and `.status()` is always `"synced"` under the loopback broker — deliberate, so a program that reads them behaves identically on both backends.
>
> The broker likewise does **not** model node isolation: every node in a run shares one message bus, whatever directory it names. Convergence is therefore testable in the oracle — two nodes exchange state exactly as two real peers would — but isolation is not: a program cannot verify under loopback that one node fails to see another's topic, because there it always sees it. That is a deliberate trade of fidelity for determinism and oracle-safety, and it is true only of the broker; real nodes are genuinely separate, each with its own identity, store and encryption credentials.

## Examples

- [`examples/para-p2p-demo/`](examples/para-p2p-demo) — CRDTs, a synced signal, and the reactivity integration in one program.
- [`crates/noeta-para-p2p/tests/conformance/`](crates/noeta-para-p2p/tests/conformance) — the `.noe` conformance fixtures (crdt / p2p / synced) the crate's test harness runs with the extension registered; a runnable spec of every behavior above, convergence and encryption included.

The full design write-up is in [`docs/Local-First-and-P2P.md`](docs/Local-First-and-P2P.md).

## Requirements

Consumers compile this package's native crates locally: `cargo` and a Rust toolchain (1.95+) must be on `PATH`. The Noeta toolchain composes and builds them automatically on first use. Real peer networking rides the `ring-p2p` feature (enabled by the composed toolchain/runner; a `--native` build includes it only when the program imports `para.p2p`/`para.synced`).

## Development

- `cargo test` in each `crates/*` member (standalone; `noeta-para-p2p`'s harness runs the `.noe` conformance fixtures).
- `cargo test --features ring-p2p` in `crates/noeta-para-p2p` builds and exercises the real-transport ring (the ring-gated unit tests, plus the conformance fixtures — which stay on the loopback broker even with the ring linked, since the sandbox host permits no real networking).
- `noeta run` / `noeta test` the program under `examples/`.

See [AGENTS.md](AGENTS.md) for the repo layout and the toolchain environment the examples need.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
