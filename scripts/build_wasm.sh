#!/usr/bin/env bash
#
# D-04: build `crates/rasp-wasm` and generate the browser module in `web/wasm/`.
#
# Reproducible by construction: pinned Rust toolchain (rust-toolchain.toml),
# pinned `wasm-bindgen` crate (`=0.2.129`) and the same `wasm-bindgen` CLI,
# then `wasm-opt -Oz` when binaryen is installed. Sizes are printed before and
# after optimisation so the size budget (W-02) has a baseline.
#
# Usage: bash scripts/build_wasm.sh
#        (run from anywhere; paths are resolved from this script's directory)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

EXPECTED_BINDGEN="0.2.129"
CRATE="rasp-wasm"
WASM_TARGET="wasm32-unknown-unknown"
OUT_DIR="web/wasm"
OUT_NAME="rasp"
RAW_WASM="target/${WASM_TARGET}/release/rasp_wasm.wasm"

# Panic locations embed source paths as data, so a plain build would ship the
# build machine's absolute paths in the committed wasm (PLAN §8.7). Remap them
# to neutral prefixes. scripts/verify_wasm_decode.mjs uses the same flags so the
# Node-target wasm stays byte-identical to the shipped one.
REMAP_FLAGS="--remap-path-prefix=${HOME}/.cargo=/cargo"
REMAP_FLAGS+=" --remap-path-prefix=${HOME}/.rustup=/rustup"
REMAP_FLAGS+=" --remap-path-prefix=${REPO_ROOT}=/rasp"
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }${REMAP_FLAGS}"

say() { printf '%s\n' "$*"; }

# Bytes in a file, with no dependence on `stat` flavour.
file_bytes() { wc -c <"$1" | tr -d ' '; }
gzip_bytes() { gzip -9 -c "$1" | wc -c | tr -d ' '; }
kib() { awk -v b="$1" 'BEGIN { printf "%.1f", b / 1024 }'; }

# --- Toolchain checks -------------------------------------------------------
if ! command -v wasm-bindgen >/dev/null 2>&1; then
  say "error: wasm-bindgen ${EXPECTED_BINDGEN} is not on PATH." >&2
  say "       cargo install wasm-bindgen-cli --version ${EXPECTED_BINDGEN} --locked" >&2
  exit 1
fi

HAVE_BINDGEN="$(wasm-bindgen --version | awk '{print $2}')"
if [[ "$HAVE_BINDGEN" != "$EXPECTED_BINDGEN" ]]; then
  say "error: wasm-bindgen CLI ${HAVE_BINDGEN} does not match the crate pin ${EXPECTED_BINDGEN}." >&2
  say "       The CLI and the crate must be identical; fix the CLI, not the wasm." >&2
  say "       cargo install wasm-bindgen-cli --version ${EXPECTED_BINDGEN} --locked" >&2
  exit 1
fi

say "==> cargo build -p ${CRATE} --target ${WASM_TARGET} --release"
cargo build -p "$CRATE" --target "$WASM_TARGET" --release

if [[ ! -f "$RAW_WASM" ]]; then
  say "error: expected ${RAW_WASM} was not produced." >&2
  exit 1
fi

# --- wasm-bindgen -----------------------------------------------------------
say ""
say "==> wasm-bindgen --target web --out-dir ${OUT_DIR} --out-name ${OUT_NAME}"
rm -rf "$OUT_DIR"
wasm-bindgen \
  --target web \
  --out-dir "$OUT_DIR" \
  --out-name "$OUT_NAME" \
  "$RAW_WASM"

WASM="${OUT_DIR}/${OUT_NAME}_bg.wasm"
BINDGEN_RAW="$(file_bytes "$WASM")"
BINDGEN_GZ="$(gzip_bytes "$WASM")"
say "    wasm-bindgen: ${BINDGEN_RAW} B raw / ${BINDGEN_GZ} B gzip ($(kib "$BINDGEN_RAW") / $(kib "$BINDGEN_GZ") KiB)"

# --- wasm-opt ---------------------------------------------------------------
FINAL_RAW="$BINDGEN_RAW"
FINAL_GZ="$BINDGEN_GZ"
if command -v wasm-opt >/dev/null 2>&1; then
  say ""
  say "==> wasm-opt -Oz (binaryen $(wasm-opt --version | awk '{print $3}'))"
  TMP_OPT="${WASM}.opt.wasm"
  if ! wasm-opt -Oz "$WASM" -o "$TMP_OPT"; then
    rm -f "$TMP_OPT"
    say "error: wasm-opt failed; refusing to ship a silently unoptimised bundle." >&2
    exit 1
  fi
  mv "$TMP_OPT" "$WASM"
  FINAL_RAW="$(file_bytes "$WASM")"
  FINAL_GZ="$(gzip_bytes "$WASM")"
  SAVED_RAW="$(awk -v a="$BINDGEN_RAW" -v b="$FINAL_RAW" 'BEGIN { printf "%.1f", (a - b) * 100 / a }')"
  SAVED_GZ="$(awk -v a="$BINDGEN_GZ" -v b="$FINAL_GZ" 'BEGIN { printf "%.1f", (a - b) * 100 / a }')"
  say "    after -Oz:    ${FINAL_RAW} B raw / ${FINAL_GZ} B gzip ($(kib "$FINAL_RAW") / $(kib "$FINAL_GZ") KiB)"
  say "    saved:        ${SAVED_RAW}% raw, ${SAVED_GZ}% gzip"
else
  say ""
  say "!!  wasm-opt not found (binaryen); shipping the unoptimised wasm."
  say "    Install it before W-02: brew install binaryen"
fi

# --- Final report -----------------------------------------------------------
say ""
say "==> final artifacts"
for f in "$OUT_DIR/${OUT_NAME}_bg.wasm" "$OUT_DIR/${OUT_NAME}.js" "$OUT_DIR/${OUT_NAME}.d.ts" "$OUT_DIR/${OUT_NAME}_bg.wasm.d.ts"; do
  if [[ -f "$f" ]]; then
    say "    $(printf '%-32s' "$f") $(printf '%10s' "$(file_bytes "$f")") B  ($(kib "$(file_bytes "$f")") KiB)"
  fi
done
say ""
say "wasm: ${FINAL_RAW} B raw / ${FINAL_GZ} B gzip ($(kib "$FINAL_RAW") / $(kib "$FINAL_GZ") KiB)"
say "Done. Serve with: bash scripts/serve.sh  ->  http://127.0.0.1:8080/smoke.html"
