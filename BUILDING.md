# Building

## Prerequisites

- [Rust via rustup](https://rustup.rs/). The exact toolchain is pinned in
  `rust-toolchain.toml` (currently **1.96**) and rustup will select it
  automatically — you do not need to install a version by hand. The workspace
  MSRV (`rust-version` in `Cargo.toml`) is **1.87**.
- **Sibling repo `langgraph-python-to-rust`.** This workspace has a path
  dependency on `../langgraph-python-to-rust/langgraph-core`, so it cannot be
  built standalone. Clone it next to this repo:

  ```bash
  git clone <host>/langgraph-python-to-rust ../langgraph-python-to-rust
  ```

## Build

```bash
cargo build --release
```

Binaries land in `target/release/`: `sentinel` (the CLI + daemon + in-repo MCP
host), `sentinel-git-interceptor`, and `sentinel-npx-interceptor`.

## Test

```bash
cargo test --workspace
```

## Verify before pushing

`scripts/verify.sh` is the canonical gate — it builds every target and runs the
whole test suite on the pinned toolchain. It is the source of truth while
GitHub Actions is dormant (CI is gated on the `CI_ENABLED` repo variable).

```bash
scripts/verify.sh
```

Enable the committed pre-push hook so this runs automatically and a red
workspace can't be pushed (the guard that catches a non-compiling `main`):

```bash
git config core.hooksPath .githooks
```

> Note: clippy's `-D warnings` bar is not yet enforced by the gate — the
> workspace carries a backlog of benign pedantic/nursery lints; `verify.sh`
> runs clippy as informational only. Tightening that to an enforced bar is a
> separate cleanup track.

## Install

```bash
cargo install --path crates/sentinel-cli
```
