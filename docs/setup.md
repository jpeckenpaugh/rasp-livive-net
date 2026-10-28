# Setup — macOS

Verified on macOS (Apple Silicon) on **2026-10-28** with the versions below.
Everything here is copy-pasteable and checkable: `bash scripts/check-env.sh`
reports PASS/FAIL for each item, and `--strict` also fails on the tools that
only a later task needs.

Target: a clean checkout builds for both the host and
`wasm32-unknown-unknown`, and the PLAN §8 gate commands pass.

## Reference versions

| Tool | Reference version | Pinned by | Needed for |
|---|---|---|---|
| rustc / cargo | **1.99.1** | `rust-toolchain.toml` | everything |
| rustfmt, clippy | matching components | `rust-toolchain.toml` | PLAN §8 gates |
| `wasm32-unknown-unknown` | installed target | `rust-toolchain.toml` | all WASM work |
| `wasm-bindgen` CLI | **0.2.129** (exact) | `docs/setup.md` + the crate pin added in W-01 | W-01 onward |
| `wasm-opt` (binaryen) | *not installed here* — install before W-02 | Homebrew | W-02, X-08 |
| Node.js | **v26.12.0** (npm 11.20.0) | none; reference only | U-01, U-12 |
| Python 3 | **3.9.6** (Apple Command Line Tools) | none; reference only | P-07, F-03, F-06 |

`wasm-pack` is **not** used and not required. The build calls the
`wasm-bindgen` CLI directly; a wasm-pack install is not part of this
toolchain. Do not add one without updating this file and W-01.

## 0. Prerequisites

```bash
xcode-select --install          # Command Line Tools: linker, system python3
brew --version                  # Homebrew; install from https://brew.sh if missing
```

## 1. Rust toolchain (pinned)

`rust-toolchain.toml` at the repo root pins the compiler, the two
components, and the wasm target. `rustup` applies it automatically to every
command run inside the repo — you do not need `rustup override`, and you
should not pass `+stable`.

A clean machine can either let rustup install the pin on first use, or
install it explicitly:

```bash
rustup toolchain install 1.99.1 \
  --component rustfmt clippy \
  --target wasm32-unknown-unknown
```

Verify:

```bash
rustup show active-toolchain
# 1.99.1-aarch64-apple-darwin (overridden by '.../rust-toolchain.toml')

rustc --version
# rustc 1.99.1 (...)
```

If rustup reports a different toolchain, you have a global override or a
`+toolchain` argument somewhere; the file in the repo root wins otherwise.

## 2. `wasm-bindgen` CLI — pin the exact version

The CLI major/minor/patch must match the `wasm-bindgen` **crate** version
that the wasm binary was built against. The crate pin is added as
`wasm-bindgen = "=0.2.129"` when W-01 introduces the bindings; this project
standardises on **0.2.129** for both sides.

```bash
cargo install wasm-bindgen-cli --version 0.2.129 --locked
wasm-bindgen --version
# wasm-bindgen 0.2.129
```

This is the classic breakage: a `wasm-bindgen` CLI newer or older than the
crate fails at generation time with a message like

```
it looks like the Rust project used to create this wasm file was linked
against version of wasm-bindgen that uses a different bindgen format
```

That error is a version mismatch, **not** a corrupt build. Fix it by pinning
both sides to the same exact version — never by regenerating the wasm and
hoping. `wasm-bindgen-test-runner` ships from the same install and must also
report 0.2.129.

## 3. `wasm-opt` (binaryen)

**Status on the reference machine: not installed.** It is not needed to
build or test the workspace, so F-02 does not block on it, but W-02
(`wasm-opt -Oz` size budget) and X-08 (CI size gate) do.

```bash
brew install binaryen
wasm-opt --version
```

`scripts/check-env.sh` reports this as an **optional** check and only fails
on it with `--strict`. Treat a missing `wasm-opt` as a W-02 blocker, not an
F-02 one.

## 4. Node.js

Reference install: **v26.12.0** with npm 11.20.0. The demo app is static
(no framework build step unless U-01 justifies one), so any current Node
LTS or newer works; `scripts/check-env.sh` requires Node ≥ 20.

```bash
brew install node
node --version    # v26.12.0
npm --version     # 11.20.0
```

## 5. Python 3

The system `/usr/bin/python3` is **3.9.6** (Apple Command Line Tools) and is
enough to run the repo scripts. P-07's metrics harness wants numpy/scipy in a
pinned venv; those are happier on a newer interpreter, so if you are doing
P-07 work prefer:

```bash
brew install python@3.12
python3 --version
```

Do not commit the venv (`.venv/` is gitignored). F-02 only requires python3
to be present and ≥ 3.9.

## 6. Build and verify

From the repo root, the four gate commands from PLAN §8 — the last one is
also F-02's acceptance criterion:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --target wasm32-unknown-unknown -p rasp-dsp
```

And the environment check itself:

```bash
bash scripts/check-env.sh
bash scripts/check-env.sh --strict   # also requires wasm-opt (W-02)
```

A clean checkout is only "set up" when all four exit 0 and
`bash scripts/check-env.sh` reports `RESULT: PASS`. `check-env.sh` is
bash-only, installs nothing, and reports missing tools instead of failing to
run.

## 7. CI

`.github/workflows/ci.yml` runs on every push to `main`, on every pull request,
and on demand (`workflow_dispatch`). It is read-only (`permissions: contents:
read`), has no secrets and no deploy step — Pages publishing is S-01. Superseded
runs on the same ref are cancelled.

| Job | Runs | Guards against |
|---|---|---|
| `lint-test` | `cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo test --workspace` | unformatted code, clippy regressions, broken tests |
| `wasm-build` | `cargo build --workspace --target wasm32-unknown-unknown --release`, then prints raw + gzip size of any `.wasm` | wasm build breakage |
| `corpus-integrity` | `python3 scripts/vendor_corpus.py --check` | corpus corruption or drift; offline, ~0.1 s, stdlib Python only, no Rust |
| `fixtures-smoke` | `python3 scripts/make_fixtures.py`, then asserts the manifest lists 12 clips | the ffmpeg fixture path rotting (F-05) |

The compiler is the pinned one, not the runner's default: the workflow resolves
`rust-toolchain.toml` with `rustup show active-toolchain`, then installs that
exact toolchain with the `rustfmt` and `clippy` components and the
`wasm32-unknown-unknown` target. Cargo registry and `target/` are cached with
`Swatinem/rust-cache`; its key includes `Cargo.lock` and `rust-toolchain.toml`,
so a dependency or toolchain bump invalidates the cache.

`fixtures-smoke` probes for `scripts/make_fixtures.py` first. While that script
is absent it warns and runs nothing; now that F-05 has landed it is a hard gate
(`continue-on-error: false`, the default) and runs on every push. If the script
is ever removed the job fails loudly rather than silently passing.

Before pushing, run the same commands locally, in the same order:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --target wasm32-unknown-unknown --release
python3 scripts/vendor_corpus.py --check
```

Deliberately not in CI yet:

- **Deploy.** S-01 owns the Pages workflow and its permissions; this skeleton
  must not publish.
- **`wasm-bindgen-cli` 0.2.129 and `wasm-opt` (binaryen).** No crate depends on
  `wasm-bindgen` until W-01, and the size budget is W-02, so installing them
  now would only add minutes and a version-mismatch failure mode. When W-02
  lands, add both with cached installs at pinned versions and enforce the
  ≤ 1.5 MB gzipped gate in the `wasm-build` job; the `TODO(W-02)` comment there
  is the checklist. Today that job only *prints* sizes.
- **`vendor_corpus.py --compare-upstream`.** It needs the network and exits
  non-zero when upstream rebakes, which is not a defect in the PR under review.
  It runs in `.github/workflows/corpus-upstream.yml`, weekly (Mondays 06:17 UTC)
  and on demand. Red there means "upstream changed, decide whether to
  re-vendor", not "the committed corpus is broken" — `--check` owns that.
- **Python/JS lint and the browser matrix.** No Python or JS lint config exists
  yet, and the cross-browser matrix is X-04 (needs the real app from U-13).
- **`scripts/check-env.sh`.** It is a local onboarding check of developer
  machine state; CI installs what it needs instead of checking for it.
