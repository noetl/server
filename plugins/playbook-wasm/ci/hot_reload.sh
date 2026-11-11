#!/usr/bin/env bash
# Compile two versions of examples/sum to WASM, prove an impure playbook does
# not compile, then run the hot-reload host test (noetl/ai-meta#548 Phase 3).
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${OUT:-$PWD/target/pbc}; mkdir -p "$OUT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
build() {  # build <yaml> <out.wasm>
  NOETL_PBC_PLAYBOOK="$PWD/$1" cargo build -q --release --target wasm32-unknown-unknown
  cp "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/noetl_playbook_wasm.wasm" "$2"
  echo "compiled $1 -> $2 ($(wc -c < "$2" | tr -d ' ') bytes, sha256 $(shasum -a 256 "$2" | cut -c1-16)…)"
}
build examples/sum.yaml "$OUT/sum_v1.wasm"
build examples/sum_v2.yaml "$OUT/sum_v2.wasm"
if NOETL_PBC_PLAYBOOK="$PWD/examples/impure.yaml" cargo build -q --release --target wasm32-unknown-unknown 2> "$OUT/impure.log"; then
  echo "FAIL: an impure playbook compiled"; exit 1
fi
grep -o "cannot be compiled to WASM: .*" "$OUT/impure.log" | head -1 | sed 's/^/refused as expected: /'
PBC_V1="$OUT/sum_v1.wasm" PBC_V2="$OUT/sum_v2.wasm" cargo test -q --test hot_reload -- --nocapture
