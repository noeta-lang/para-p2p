# para/p2p

Local-first, collaborative state for Noeta — CRDTs, a peer-to-peer transport, and CRDT-backed reactive signals that converge across peers without coordination. A peer's change flows into the reactivity graph and reruns your `computed`/`effect`s exactly like a local edit — *signals that happen to be shared*.

This package is **fully native**: its whole surface is a Rust extension (there is no `.noe` layer), registered when a program depends on `para/p2p` and authorized in `[trust]`.

## What it provides

Three modules, all rooted at `para`:

- **`para.crdt`** — state-based CRDT value types: `crdt.gcounter()`, `crdt.pncounter()`, `crdt.gset()`. Merge is commutative, associative, and idempotent, so independent replicas converge to the same value regardless of the order or duplication of the updates they exchange.
- **`para.p2p`** — `publish` / `receive` / `identity` over the `P2p` host capability. With the real transport (the `ring-p2p` feature of the extension crate) that is a live p2panda node — gossip + log-sync over iroh/QUIC; without it, a deterministic in-process loopback broker, so plain builds and tests never link the networking tree.
- **`para.synced`** — `synced_signal(crdt, topic)`: a CRDT-backed signal that is a node in the **same** reactive graph as core `std.reactive` — a merge (local or from a peer) reruns dependent `computed`/`effect`s. Over the real transport it adds end-to-end group encryption and dynamic membership.

The Rust crates behind that surface:

- `crates/noeta-crdt` — the dependency-free convergence core (the merge algebra, property-tested for the three CRDT laws).
- `crates/noeta-para-p2p` — the extension crate (modules, extern types, the reactive-graph seam).
- `crates/noeta-para-p2p-net` — the p2panda transport (optional, behind the extension's `ring-p2p` feature).

## Installation

```toml
[dependencies]
para = { version = "^0.1", package = "para/p2p" }

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

## Examples

- [`examples/para-p2p-demo/`](examples/para-p2p-demo) — CRDTs, a synced signal, and the reactivity integration in one program.
- [`crates/noeta-para-p2p/tests/conformance/`](crates/noeta-para-p2p/tests/conformance) — the `.noe` conformance fixtures (crdt / p2p / synced) the crate's test harness runs with the extension registered.

The full design write-up is in [`docs/Local-First-and-P2P.md`](docs/Local-First-and-P2P.md).

## Requirements

Consumers compile this package's native crates locally: `cargo` and a Rust toolchain (1.95+) must be on `PATH`. The Noeta toolchain composes and builds them automatically on first use. Real peer networking rides the `ring-p2p` feature (enabled by the composed toolchain/runner; a `--native` build includes it only when the program imports `para.p2p`/`para.synced`).

## Development

- `cargo test` in each `crates/*` member (standalone; `noeta-para-p2p`'s harness runs the `.noe` conformance fixtures).
- `cargo check --features ring-p2p` in `crates/noeta-para-p2p` compiles the real-transport ring.
- `noeta run` / `noeta test` the program under `examples/`.

See [AGENTS.md](AGENTS.md) for the repo layout and the toolchain environment the examples need.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
