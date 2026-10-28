#!/usr/bin/env bash
# Everything that must pass before a commit in this repo.
# Usage: bash scripts/test_all.sh [--quick]
#   --quick  skip the wasm build and the Node full-corpus pass (~40 s saved)
set -euo pipefail

cd "$(dirname "$0")/.."

QUICK=0
[[ "${1:-}" == "--quick" ]] && QUICK=1

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
fail() { printf '\033[31mFAILED: %s\033[0m\n' "$*" >&2; exit 1; }

VENV="scripts/.venv/bin/python"
if [[ ! -x "$VENV" ]]; then
  echo "Setting up the metrics venv (one-time)…"
  python3 -m venv scripts/.venv
  scripts/.venv/bin/pip install -q -r scripts/requirements.txt
fi

step "format"
cargo fmt --all --check || fail "cargo fmt"

step "clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings || fail "clippy"

step "rust tests"
cargo test --workspace || fail "cargo test"

step "corpus integrity (offline)"
python3 scripts/vendor_corpus.py --check || fail "corpus integrity"

step "metrics controls"
"$VENV" scripts/metrics.py --self-test --check || fail "pitch ruler self-test"
"$VENV" scripts/metrics.py --formant-self-test || fail "formant ruler self-test"
if [[ -f fixtures/manifest.json ]]; then
  "$VENV" scripts/metrics.py --manifest --check || fail "fixture metrics"
else
  echo "fixtures absent — skipping --manifest (run: python3 scripts/make_fixtures.py)"
fi

if [[ "$QUICK" == "0" ]]; then
  step "wasm build"
  bash scripts/build_wasm.sh || fail "wasm build"

  step "decode all 1404 clips in wasm (Node)"
  node scripts/verify_wasm_decode.mjs || fail "wasm decode verification"
fi

printf '\n\033[32mAll checks passed.\033[0m\n'