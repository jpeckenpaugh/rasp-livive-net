#!/usr/bin/env bash
#
# scripts/check-env.sh — verify the build environment described in
# docs/setup.md.
#
# Usage:
#   bash scripts/check-env.sh            # required checks decide the exit code
#   bash scripts/check-env.sh --strict   # optional (later-task) checks also fail
#
# Required checks cover everything a clean checkout needs to build, test, and
# build for wasm32. Optional checks cover tools that a later PLAN task needs;
# they are reported either way, but only affect the exit status under
# --strict so that F-02 does not fail on a tool W-02 will install.
#
# The script is plain bash (3.2-compatible), installs nothing, and works even
# when the tools it checks are missing: a missing command is reported as FAIL
# with the install hint, never as a crash.

set -u

strict=0
for arg in "$@"; do
  case "$arg" in
    --strict) strict=1 ;;
    -h | --help)
      printf 'usage: %s [--strict]\n' "${0##*/}"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$arg" >&2
      exit 2
      ;;
  esac
done

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 2
cd "$repo_root" || exit 2

# Must match docs/setup.md and the wasm-bindgen crate pin added in W-01.
WASM_BINDGEN_PIN="0.2.129"
NODE_MIN_MAJOR=20
PYTHON_MIN_MAJOR=3
PYTHON_MIN_MINOR=9

passed=0
required_failed=0
optional_failed=0

pass() {
  passed=$((passed + 1))
  printf 'PASS  %-26s %s\n' "$1" "$2"
}

fail() {
  required_failed=$((required_failed + 1))
  printf 'FAIL  %-26s %s\n' "$1" "$2"
}

pass_optional() {
  passed=$((passed + 1))
  printf 'PASS  %-26s %s\n' "$1" "$2"
}

fail_optional() {
  optional_failed=$((optional_failed + 1))
  printf 'FAIL  %-26s %s\n' "$1" "$2"
}

section() {
  printf '\n%s\n' "$1"
}

# ---------------------------------------------------------------------------
# Toolchain pin
# ---------------------------------------------------------------------------

section 'toolchain (rust-toolchain.toml)'

pinned_channel=""
if [ -f rust-toolchain.toml ]; then
  while IFS= read -r line; do
    case "$line" in
      channel*)
        pinned_channel=${line#*=}
        pinned_channel=${pinned_channel%%#*}
        pinned_channel=${pinned_channel//\"/}
        pinned_channel=${pinned_channel// /}
        pinned_channel=${pinned_channel//$'\t'/}
        ;;
    esac
  done < rust-toolchain.toml
fi

if [ -n "$pinned_channel" ]; then
  pass 'rust-toolchain.toml' "channel = $pinned_channel"
else
  fail 'rust-toolchain.toml' 'missing, or has no channel = "..." (see docs/setup.md)'
fi

if command -v rustup >/dev/null 2>&1; then
  rustup_out=$(rustup --version 2>/dev/null)
  rustup_out=${rustup_out%%$'\n'*}
  pass 'rustup' "$rustup_out"
else
  fail 'rustup' 'not found — install from https://rustup.rs/ (docs/setup.md §1)'
fi

if command -v rustc >/dev/null 2>&1; then
  rustc_out=$(rustc --version 2>&1)
  rustc_ver=${rustc_out#rustc }
  rustc_ver=${rustc_ver%% *}
  if [ -n "$pinned_channel" ] && [ "$rustc_ver" = "$pinned_channel" ]; then
    pass 'rustc' "$rustc_out"
  else
    fail 'rustc' "$rustc_out (expected ${pinned_channel:-the pinned channel})"
  fi
else
  fail 'rustc' 'not found — rustup toolchain install 1.99.1 (docs/setup.md §1)'
fi

if command -v cargo >/dev/null 2>&1; then
  pass 'cargo' "$(cargo --version 2>&1)"
else
  fail 'cargo' 'not found — install the pinned toolchain (docs/setup.md §1)'
fi

if command -v rustup >/dev/null 2>&1; then
  active=$(rustup show active-toolchain 2>/dev/null)
  if [ -n "$pinned_channel" ]; then
    case "$active" in
      "$pinned_channel"*) pass 'active toolchain' "$active" ;;
      *) fail 'active toolchain' "${active:-<none>} (expected ${pinned_channel}*) — remove stray overrides or +toolchain flags" ;;
    esac
  else
    fail 'active toolchain' 'cannot verify: no pinned channel in rust-toolchain.toml'
  fi

  components=$(rustup component list --installed 2>/dev/null)
  case "$components" in
    *rustfmt*) pass 'rustfmt component' 'installed' ;;
    *) fail 'rustfmt component' "missing — rustup component add rustfmt (docs/setup.md §1)" ;;
  esac
  case "$components" in
    *clippy*) pass 'clippy component' 'installed' ;;
    *) fail 'clippy component' 'missing — rustup component add clippy (docs/setup.md §1)' ;;
  esac

  targets=$(rustup target list --installed 2>/dev/null)
  case "$targets" in
    *wasm32-unknown-unknown*) pass 'wasm32 target' 'wasm32-unknown-unknown installed' ;;
    *) fail 'wasm32 target' 'missing — rustup target add wasm32-unknown-unknown (docs/setup.md §1)' ;;
  esac
else
  fail 'active toolchain' 'skipped: rustup not found'
  fail 'rustfmt component' 'skipped: rustup not found'
  fail 'clippy component' 'skipped: rustup not found'
  fail 'wasm32 target' 'skipped: rustup not found'
fi

# ---------------------------------------------------------------------------
# wasm-bindgen CLI
# ---------------------------------------------------------------------------

section 'wasm-bindgen CLI (must equal the crate version)'

if command -v wasm-bindgen >/dev/null 2>&1; then
  bindgen_out=$(wasm-bindgen --version 2>&1)
  bindgen_ver=${bindgen_out##* }
  if [ "$bindgen_ver" = "$WASM_BINDGEN_PIN" ]; then
    pass 'wasm-bindgen' "$bindgen_out"
  else
    fail 'wasm-bindgen' "$bindgen_out (expected exactly $WASM_BINDGEN_PIN) — cargo install wasm-bindgen-cli --version $WASM_BINDGEN_PIN --locked"
  fi
else
  fail 'wasm-bindgen' "not found — cargo install wasm-bindgen-cli --version $WASM_BINDGEN_PIN --locked (docs/setup.md §2)"
fi

# ---------------------------------------------------------------------------
# Optional: needed by W-02 / X-08, not by this task
# ---------------------------------------------------------------------------

section 'optional for F-02, required by W-02 (use --strict to enforce)'

if command -v wasm-opt >/dev/null 2>&1; then
  pass_optional 'wasm-opt (binaryen)' "$(wasm-opt --version 2>&1)"
else
  fail_optional 'wasm-opt (binaryen)' 'not found — brew install binaryen (W-02 needs it; docs/setup.md §3)'
fi

# ---------------------------------------------------------------------------
# Runtime tooling
# ---------------------------------------------------------------------------

section 'runtime tooling'

if command -v node >/dev/null 2>&1; then
  node_out=$(node --version 2>&1)
  node_ver=${node_out#v}
  node_major=${node_ver%%.*}
  case "$node_major" in
    '' | *[!0-9]*) fail 'node' "$node_out — cannot parse major version" ;;
    *)
      if [ "$node_major" -ge "$NODE_MIN_MAJOR" ]; then
        pass 'node' "$node_out (reference v26.12.0)"
      else
        fail 'node' "$node_out — need >= v$NODE_MIN_MAJOR (brew install node)"
      fi
      ;;
  esac
else
  fail 'node' 'not found — brew install node (docs/setup.md §4)'
fi

if command -v python3 >/dev/null 2>&1; then
  py_out=$(python3 --version 2>&1)
  py_ver=${py_out#Python }
  py_major=${py_ver%%.*}
  py_rest=${py_ver#*.}
  py_minor=${py_rest%%.*}
  case "$py_major$py_minor" in
    '' | *[!0-9]*) fail 'python3' "$py_out — cannot parse version" ;;
    *)
      if [ "$py_major" -gt "$PYTHON_MIN_MAJOR" ] ||
        { [ "$py_major" -eq "$PYTHON_MIN_MAJOR" ] && [ "$py_minor" -ge "$PYTHON_MIN_MINOR" ]; }; then
        pass 'python3' "$py_out (reference 3.9.6)"
      else
        fail 'python3' "$py_out — need >= ${PYTHON_MIN_MAJOR}.${PYTHON_MIN_MINOR}"
      fi
      ;;
  esac
else
  fail 'python3' 'not found — xcode-select --install, or brew install python@3.12 (docs/setup.md §5)'
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

printf '\n%d check(s) passed, %d required failure(s), %d optional failure(s)\n' \
  "$passed" "$required_failed" "$optional_failed"

if [ "$required_failed" -eq 0 ] && { [ "$strict" -eq 0 ] || [ "$optional_failed" -eq 0 ]; }; then
  printf 'RESULT: PASS\n'
  exit 0
fi

printf 'RESULT: FAIL'
if [ "$optional_failed" -gt 0 ] && [ "$strict" -eq 1 ] && [ "$required_failed" -eq 0 ]; then
  printf ' (optional checks, --strict)'
fi
printf ' — fix the FAIL lines above; see docs/setup.md\n'
exit 1
