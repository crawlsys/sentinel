#!/usr/bin/env bash
#
# Canonical verification gate — the source of truth while GitHub Actions is
# dormant (CI is gated on the CI_ENABLED repo variable; see .github/workflows/
# test.yml). Builds and tests the WHOLE workspace on the pinned toolchain
# (rust-toolchain.toml). Run it before every push; the committed
# .githooks/pre-push hook runs it automatically once you set
#   git config core.hooksPath .githooks
#
# Scope note: clippy's `-D warnings` bar is intentionally NOT enforced here yet.
# The workspace currently carries ~200 benign pedantic/nursery lints; driving
# them to a real, enforceable bar is a separate cleanup track. We run clippy as
# INFORMATIONAL (non-fatal) so the count stays visible without blocking the gate
# that actually matters today: "does it build and do the tests pass."
set -euo pipefail
cd "$(dirname "$0")/.."

# The workspace has a path-dependency on a sibling langgraph repo (see the
# root Cargo.toml). It cannot build without it — fail early with a clear
# message instead of a cryptic manifest error.
if [ ! -d "../langgraph-python-to-rust/langgraph-core" ]; then
  echo "ERROR: sibling repo ../langgraph-python-to-rust not found." >&2
  echo "       This workspace has a path-dependency on it and cannot build standalone." >&2
  echo "       Clone it next to this repo:" >&2
  echo "         git clone <host>/langgraph-python-to-rust ../langgraph-python-to-rust" >&2
  exit 1
fi

echo "==> cargo build --workspace --all-targets"
cargo build --workspace --all-targets

echo "==> cargo test --workspace"
cargo test --workspace

echo "==> cargo clippy --workspace  (informational; -D warnings deferred to the lint-cleanup track)"
cargo clippy --workspace || true

echo
echo "OK: workspace builds and all tests pass on $(rustc --version)."
