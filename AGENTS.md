# AGENTS.md

Guidance for coding agents working in this repo — the standalone repo of the **para/p2p** Noeta package (local-first / peer-to-peer state: CRDTs, transport, synced signals; fully native), extracted from the noeta monorepo. Toolchain issues (the language, the `noeta` binary, `std.*`, the extension ABI) belong in the monorepo at github.com/noeta-lang/noeta, not here.

## Repo layout

- `noeta.toml` — the package manifest (`name = "para/p2p"`, `native = "native"` declares the Rust extension entry crate). There is no `.noe` surface — the package is fully native.
- `crates/noeta-crdt/` — the dependency-free CRDT convergence core (merge algebra, property tests).
- `crates/noeta-para-p2p/` — the extension crate: the `para.crdt`/`para.p2p`/`para.synced` modules, the reactive-graph seam, and the `.noe` conformance fixtures under `tests/conformance/`.
- `crates/noeta-para-p2p-net/` — the real p2panda transport (iroh/QUIC, group encryption), pulled in only by the extension's `ring-p2p` feature.
- `native/` — the thin entry crate the manifest's `native` key points at; re-exports `NOETA_EXTENSIONS`.
- `examples/para-p2p-demo/` — a standalone package depending on this repo via `para = { path = "../.." }`.
- `docs/Local-First-and-P2P.md` — the design write-up.
- `.github/workflows/` — CI (`ci.yml`) and the tag-triggered registry publish (`release.yml`).

## Build & test

- `cargo test` inside each `crates/*` member works standalone — the toolchain crates are git dependencies (currently the pre-publish `file:///home/niklas/Code/lang` form; flips to `https://github.com/noeta-lang/noeta` at publish). Plain builds/tests link **no** p2panda — the extension uses its dep-free loopback broker; `cargo check --features ring-p2p` in `crates/noeta-para-p2p` compiles the real-transport ring.
- Running the example needs the `noeta` binary and **composes a toolchain** (the native crates are compiled in). Set:
  - `NOETA_TOOLCHAIN_REPO=file:///home/niklas/Code/lang` — MUST equal the URL the crates' Cargo.toml declares, or the composed build links two copies of the extension ABI and every impl fails with a two-`Extension`-traits E0308;
  - optionally `NOETA_TOOLCHAIN_SRC=<path to a noeta checkout>` to skip the git fetch.
- Then `noeta check` / `noeta test` / `noeta run` in `examples/para-p2p-demo/`.

## Conventions

- Rust code is `cargo fmt` and `cargo clippy --all-targets -- -D warnings` clean (toolchain pinned at 1.97.0 in CI).
- `noeta.lock` files under `examples/` **are committed** when present — leave resolved locks in place.
- Markdown never hard-wraps lines; American English throughout.
- Conventional commits. Never move a published `v*` tag — a release is a new tag.

## CI

`ci.yml` gates the Rust crates (fmt/clippy/test + the `ring-p2p` check, mirroring the monorepo's old gate) and the example (pinned released `noeta`); `release.yml` re-runs the crate gate then publishes the tag to the hosted registry (`noeta publish`, keyless Sigstore provenance via GitHub OIDC). Both go green only once the toolchain repo is published under github.com/noeta-lang/noeta and the `file:///` deps are flipped.
