# AGENTS.md

Guidance for coding agents working in this repo — the standalone repo of the **para/p2p** Noeta package (local-first / peer-to-peer state: CRDTs, transport, synced signals), extracted from the noeta monorepo.

Toolchain issues (the language, the `noeta` binary, `std.*`, the extension ABI) belong in the monorepo at github.com/noeta-lang/noeta, not here.

## Layout

The package is **fully native** — there is no `.noe` surface. `noeta.toml`'s `native = "native"` points at a thin entry crate that re-exports `NOETA_EXTENSIONS`; the surface (`para.crdt` / `para.p2p` / `para.synced`) lives in `crates/noeta-para-p2p`, over the dep-free convergence core in `crates/noeta-crdt` and the optional p2panda transport in `crates/noeta-para-p2p-net`.

Every Cargo.toml here carries its rationale inline — read it before changing a dependency. `docs/Local-First-and-P2P.md` is the design write-up; `README.md` is the user-facing surface documentation.

## Build & test

- **There is no root workspace.** Each crate under `crates/` and `native/` ends its manifest with a bare `[workspace]` and is its own root, so the composed toolchain can adopt it as a path dependency. `cargo` at the repo root does nothing useful — `cd` into a crate, or loop over `crates/*/ native/` the way `ci.yml` does.
- **Do not delete `[patch.crates-io]` in `crates/noeta-para-p2p/Cargo.toml`.** It looks redundant and is not: the contract crates ship both on crates.io (what `[dependencies]` names) and as path members of the toolchain repo (what the git-pinned dev-dependencies drag in). Without the patch a test build links two copies of the ABI and every `impl Extension` fails E0277. Cargo honors `[patch]` only in the root it builds, so it governs this repo's tests and is ignored by consumers.
- **The real-transport ring is off by default.** Plain builds link no p2panda — the extension uses its dep-free loopback broker. CI *runs* `cargo test --features ring-p2p` in `crates/noeta-para-p2p` rather than merely checking it, because `cargo check` never builds a test target and ring-only behavior would otherwise have zero coverage.
- **The `.noe` conformance fixtures never touch the network, ring or no ring.** They run on `SandboxHost`, whose `P2pProvider::real_p2p()` is the default `None`, so the extension serves them the loopback broker and no p2panda node ever starts in a test. That the corpus passes identically with and without the ring is the property being gated.
- **Running `examples/para-p2p-demo/` composes a toolchain** (the native crates are compiled in), so it needs the `noeta` binary and a Rust toolchain; then `noeta check` / `noeta test` / `noeta run` there. Usually set nothing — the compose `[patch]` key defaults to the binary's baked repository URL, which already equals the URL the crates' git deps declare. If you override it to a fork or local clone, `NOETA_TOOLCHAIN_REPO` MUST equal the declared URL or the composed build links two copies of the extension ABI (a two-`Extension`-traits E0308). `NOETA_TOOLCHAIN_SRC=<path to a noeta checkout>` skips the git fetch.

## Toolchain version coupling

The supported toolchain is spelled in four places that must move together, and CI asserts the last two agree:

- `noeta-ext-abi` / `noeta-reactive-abi` as a **crates.io range** (`"0.6"`) in the three crates that use them — published and semver-stable, so a *patch* toolchain release costs this repo no edit at all.
- Every other toolchain crate (`noeta-conformance`, `noeta-stdlib`) is internal and unpublished, so it stays a git dependency pinned to the **exact release tag** — as do the two `[patch.crates-io]` entries.
- `package.toolchain = ">=0.6"` in `noeta.toml` — the minimum a consumer resolves against.
- `NOETA_VERSION` (a GitHub org variable) in `ci.yml` and `release.yml`.

`toolchain-pin.yml` runs on a toolchain release, rewrites the pins, builds against the new toolchain and opens a PR — the build is the point, the PR carries the verdict. It deliberately will not touch `toolchain = ">=X.Y"` or the package version: those are consumer-facing calls for a human.

## Pitfalls in the p2p node registry

- **One directory must be one map key, or data is silently corrupted.** `provider.rs` keys live nodes on a resolved `NodeConfig`, and resolves the directory twice — once when a node is named (`canonical_dir`) and again on a registry miss before anything starts (`settle_key`). Two keys for one directory means two p2panda nodes on one `identity.key` and one `store.db`: overlapping starts collide loudly on a store migration, but **staggered starts come up fine and report nothing**, both presenting the same peer id. Preserve that invariant if you touch the keying.
- **A locally-set `$NOETA_P2P_DIR` or `$NOETA_P2P_APP` silently voids a test.** `default_data_dir_is_namespaced_per_app` returns early when either is set, because an env override collapses the namespace it asserts on. Unset them before trusting a green run of `crates/noeta-para-p2p-net`.
- **The host cannot yet steer the default node's directory, but the ABI no longer blocks it.** `provider.rs::host_node_config` hardcodes `data_dir: None`, so the default node resolves only through `$NOETA_P2P_DIR` / `$XDG_DATA_HOME`. Its doc comment says the seam needs a `data_dir` field on `noeta_ext_abi::host::RealP2pConfig` — that field **exists as of the pinned v0.6.0 ABI**, so the comment is stale and the change is now a one-liner on this side. Programs are unaffected: `p2p.open(dir)` already names a node per directory, and an explicitly named directory beats the environment.

## Conventions

- **Nothing is locked in git.** `.gitignore` covers `target/`, `Cargo.lock` and `examples/*/noeta.lock`; no lockfile is tracked. Leave it that way — examples are demos, not package roots.
- Rust: `cargo clippy --all-targets -- -D warnings` clean, zero compiler warnings, `cargo fmt` clean. The CI compiler is **pinned at 1.97.0** — lint against that locally, since a floating `@stable` surfaces lints CI does not have yet, and vice versa.
- Markdown never hard-wraps lines. **American English** throughout, code and prose alike (`behavior`, not `behaviour`).
- **Conventional commits** for every commit title. Commit each green slice as it completes, but **never `git push` without explicit authorization**. Never move a published `v*` tag — a release is a new tag.
- Implement in full — no stubs or TODOs; new functionality lands with tests.
- Keep `README.md` and this file current when layout or behavior changes.

## CI

`ci.yml` gates the Rust crates (fmt / clippy / test per crate directory, plus the `ring-p2p` test run) and the examples (pinned released `noeta`, after a toolchain-tag consistency check). `release.yml` fires on a `v*` tag, reuses `ci.yml` as its gate, then `noeta publish`es to the hosted registry with keyless Sigstore provenance via GitHub OIDC. `docs-backfill.yml` is manually dispatched to regenerate the docs artifact for an already-published tag (`--docs-only`, no version bump).
