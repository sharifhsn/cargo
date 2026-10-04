# Experimental active configuration artifact retention

This branch contains the isolated Cargo prototype for automatic collection of obsolete configuration artifacts.
The implementation retains complete units that sessions, published outputs, and execution leases still need.
It preserves the previous session roots after compilation failure.
The state commit occurs after compilation and warning validation, before test or application execution.

The five matched project histories saved 61.1% to 65.5% of target allocation with full debug information and incremental compilation enabled.
A first return to an evicted configuration costs recompilation.

Read the [results, scope, and reproduction](https://github.com/sharifhsn/rust/blob/codex/target-size-research/TARGET-SIZE.md).
The code is in [active_artifacts.rs](src/compiler/active_artifacts.rs).
The [focused suite](tests/testsuite/active_artifacts.rs) contains 29 retention regressions.

## Build and small reproduction

Use the pinned Cargo base and a compatible Rust toolchain.
The measured applications used Rust 1.99.0 on Linux.

```sh
cargo test --locked --test testsuite active_artifacts::
cargo build --locked --release --bin cargo
```

Get the small reproduction from the Rust fork.
Pass the built Cargo binary and the toolchain's `bin` directory.

```sh
python3 ../rust/tools/artifact-retention/reviewer-repro.py   --cargo "$PWD/target/release/cargo"   --toolchain /absolute/path/to/toolchain/bin   --out /absolute/path/to/new-reproduction
```

The reproduction checks two feature configurations, warm reuse, old-unit deletion, and recompilation after return.
It has no external dependencies and uses Cargo's internal nightly test override.

## Limits and authorship

The flags and interface are provisional.
The prototype requires coarse build-directory locks and rejects NFS and fine-grain locking.
Direct application and debugger launches do not acquire Cargo execution leases.
Native Windows lifetime, full-workspace concurrency, arbitrary old-target adoption, and power-failure durability remain unverified.
Temporary replacement needs room for both configurations.

This is an LLM-assisted prototype, with LLM-assisted documentation, for discussion in a personal fork.
The source matches isolation commit `950bb0729f3a80377d5c47d24c4b03eb10b4fd5e` and the frozen patch SHA-256 `c74be69ecb97bf6c78fbf4a36d3782700836d1ed6501aabf62b85e4434103ff4`.
No upstream proposal, PR, or reviewer agreement exists.
The upstream contribution process requires separate design, authorship, and reviewer steps.
