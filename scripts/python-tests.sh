#!/bin/sh
# Build the node and the Python package, then run the Python tests
# against a node started by the tests. Needs `uv` (https://docs.astral.sh/uv/)
# and the Rust toolchain; everything else is fetched into
# crates/djbod-python/.venv.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"
cargo build -q -p djbod-node
cd "$root/crates/djbod-python"
[ -d .venv ] || uv venv --quiet .venv
uv pip install --quiet --python .venv maturin pytest
.venv/bin/maturin develop --quiet
DJBOD_NODE_BIN="$target/debug/djbod-node" .venv/bin/pytest -q "$@"
