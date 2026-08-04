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

- `cargo test` inside each `crates/*` member works standalone — the toolchain crates are git dependencies on `https://github.com/noeta-lang/noeta` (rev-pinned; flips to tag pins once a toolchain release tag exists). Plain builds/tests link **no** p2panda — the extension uses its dep-free loopback broker; `cargo test --features ring-p2p` in `crates/noeta-para-p2p` builds **and runs** the real-transport ring. CI gates that, not merely `cargo check` — a `check` never builds a test target, so ring-only behavior would otherwise have zero automated coverage. The `.noe` conformance fixtures stay on the loopback broker even under that feature: they run on `SandboxHost`, whose `real_p2p()` is `None`, so no p2panda node ever starts in a test.
- Running the example needs the `noeta` binary and **composes a toolchain** (the native crates are compiled in). Set:
  - nothing, in the common case: the compose `[patch]` key defaults to the binary's baked repository URL (`https://github.com/noeta-lang/noeta`), which now equals the URL the crates' Cargo.toml declares. When overriding to a fork or local clone, `NOETA_TOOLCHAIN_REPO` MUST equal the declared URL, or the composed build links two copies of the extension ABI and every impl fails with a two-`Extension`-traits E0308;
  - optionally `NOETA_TOOLCHAIN_SRC=<path to a noeta checkout>` to skip the git fetch.
- Then `noeta check` / `noeta test` / `noeta run` in `examples/para-p2p-demo/`.

## Conventions

- `noeta.lock` files under `examples/` **are committed** when present — leave resolved locks in place.
- Rust: default `rustfmt` style (no `rustfmt.toml`), `cargo clippy --all-targets -- -D warnings` clean, zero compiler warnings; the CI toolchain is pinned at 1.97.0 — lint against it locally (a floating `@stable` surfaces lints CI doesn't have yet, and vice versa).
- Rust naming: `snake_case` files/functions, `PascalCase` types, `SCREAMING_SNAKE_CASE` constants; prefer enums and constants over magic strings.
- Markdown never hard-wraps lines.
- **American English** throughout — code, comments, and docs (`behavior`, not `behaviour`).
- **Conventional commits** for all commit titles. Commit each green slice as it completes, but **never `git push` without explicit authorization**. Never move a published `v*` tag — a release is a new tag.
- Implement in full — no stubs or TODOs; new functionality lands with tests.
- Keep `README.md` and this file up to date when layout or behavior changes.

## CI

`ci.yml` gates the Rust crates (fmt/clippy/test + the `ring-p2p` check, mirroring the monorepo's old gate) and the example (pinned released `noeta`); `release.yml` re-runs the crate gate then publishes the tag to the hosted registry (`noeta publish`, keyless Sigstore provenance via GitHub OIDC). Both go green only once the toolchain repo is published under github.com/noeta-lang/noeta and the `file:///` deps are flipped.
