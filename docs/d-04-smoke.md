# D-04 — Browser decode smoke

**Status:** complete · **Date:** 2026-10-28 · **Gate:** M1 (Stage D)

This is the page that proves the **whole path** — vendored `.weba` → Rust
(`rasp-opus`) → WebAssembly → browser audio — **before any DSP exists**. It
deliberately contains no phase vocoder, no worklet, no resampler and no
processing of any kind. If a later clip sounds wrong, this page is the control:
if it sounds right here, the DSP is the suspect.

Canonical task definition: [PLAN.md §5](../PLAN.md). Decode accuracy and framing
are owned by D-02 and documented in
[`crates/rasp-opus/README.md`](../crates/rasp-opus/README.md).

---

## 1. What is on the page

| Area | What it does |
|---|---|
| **Module** | `version()` from wasm, wasm raw + gzip size (gzip computed in the browser with `CompressionStream` when available), module init time, observed `AudioContext.sampleRate`, registry build id |
| **One clip** | Voice and phrase selects (54 × 26, from `registry.json`); changing either select decodes immediately; **Play decoded** plays the decoded `AudioBuffer` through a plain `AudioBufferSourceNode` |
| **All 1404** | Sequences every clip in the registry, decodes each in wasm, reports pass/fail, min/median/max/mean decode ms, ms per second of audio, realtime factor, wall time, and every failure by clip id with its error |

There is **no silent skip**. A fetch error, a registry byte-count mismatch, a
decode error, a non-finite sample, a wrong sample rate, a channel mismatch or an
implausible duration all become a line in the failures list.

Playback is unprocessed: the decoded buffer is created at **48 kHz** and
`AudioBufferSourceNode` lets the browser resample to its context rate. The
page does not include D-03's resampler; that belongs to the worklet path.

The wasm errors are typed, not traps: `decode()` throws an `Error` with
`name === "RaspDecodeError"` and a stable `code`
(`ContainerNotWebm`, `NoAudioTrack`, `UnsupportedCodec`,
`MalformedContainer`, `MalformedPacket`, `EmptyStream`, `Io`). The page shows
both fields; the headless verifier asserts the junk-input case.

## 2. Build

Prerequisites are the pinned toolchain in
[`docs/setup.md`](setup.md): Rust **1.99.1** with the `wasm32-unknown-unknown`
target, and the `wasm-bindgen` CLI at **exactly 0.2.129** (the crate is pinned
to the same version). `wasm-opt` (binaryen) is optional for the build but is
what the committed artifact uses.

```sh
bash scripts/build_wasm.sh
```

The script, from any directory:

1. fails with an actionable message if `wasm-bindgen --version` is not
   `0.2.129`;
2. `cargo build -p rasp-wasm --target wasm32-unknown-unknown --release`, with
   `RUSTFLAGS` remapping `~/.cargo`, `~/.rustup` and the repo root to neutral
   prefixes;
3. `wasm-bindgen --target web --out-dir web/wasm --out-name rasp`;
4. `wasm-opt -Oz` in place when available, printing raw/gzip size before and
   after;
5. prints the final artifact sizes.

The remap in step 2 matters: Rust stores panic locations as strings, so an
unremapped build embeds absolute build-machine paths in the committed wasm
(PLAN §8.7). After a build,
`strings web/wasm/rasp_bg.wasm | grep -c '/Users/'` must print `0`
(and generally no home-directory path). Changing the flags invalidates Cargo's
cache once; `scripts/verify_wasm_decode.mjs` uses the same flags so its
Node-target wasm is byte-identical to the shipped file.

`wasm-pack` is not used (PLAN F-02). The generated output is **not committed** (DECISIONS D18) — build it with `bash scripts/build_wasm.sh` before serving, and CI rebuilds it for deploys. The page works on any static host once built:

| File | Role |
|---|---|
| `web/wasm/rasp.js` | ES module glue (`--target web`) |
| `web/wasm/rasp_bg.wasm` | the module |
| `web/wasm/rasp.d.ts`, `web/wasm/rasp_bg.wasm.d.ts` | TypeScript declarations, no runtime role |

Rust gates from PLAN §8 still apply to the crate:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 3. Serve and open

```sh
bash scripts/serve.sh                 # http://127.0.0.1:8080/
# open http://127.0.0.1:8080/smoke.html
```

`serve.sh` runs the prescribed
`python3 -m http.server 8080 --directory web` when the interpreter's
`mimetypes` table maps `.wasm` to `application/wasm`. If it does not, the
script starts an equivalent `ThreadingHTTPServer` with
`application/wasm` (and `text/javascript`/`text/html`) registered, and says so;
it never serves the wasm with a wrong type silently. On this machine
(Python **3.9.6**) the plain command is already correct:

```console
$ curl -sSI http://127.0.0.1:8080/wasm/rasp_bg.wasm
HTTP/1.0 200 OK
Content-type: application/wasm
Content-Length: 523270
```

> **Local note:** port `8080` was already occupied on the verification machine
> by an unrelated `dgen-livive-net` bench server bound to `127.0.0.1`, so the
> checks here used `PORT=8090 bash scripts/serve.sh`. The default is still
> `8080`; use `PORT=...` when something else owns it. This is an environment
> collision, not a project issue.

`web/index.html` is U-01's placeholder and is untouched by D-04.

## 4. Headless verification (no browser)

```sh
node scripts/verify_wasm_decode.mjs
# or, reusing an existing target/wasm-node-verify/:
node scripts/verify_wasm_decode.mjs --skip-build
```

Plain Node, no npm install. It builds the same crate, runs
`wasm-bindgen --target nodejs` into `target/wasm-node-verify/`, applies
`wasm-opt -Oz` when installed, and then asserts:

- all **1404** clips referenced by `web/registry.json` decode without throwing;
- 48 kHz, 1 channel, positive frame count, `samples().length === length() ×
  channels`, no NaN/Inf, peak ≤ 1.01;
- decoded duration is within 100 ms of the registry's `durationMs` (a display
  hint, **not** a sample-count oracle — see the `rasp-opus` README);
- the **12** fixture clips in `fixtures/manifest.json` have **exactly** the
  libopus sample counts (the manifest is generated, so a missing manifest is a
  printed SKIP with the regeneration command, not a pass);
- `decode(junk)` produces `RaspDecodeError(ContainerNotWebm)` — a typed error,
  never a trap;
- the Node-target wasm is byte-identical to the committed
  `web/wasm/rasp_bg.wasm` (same input, same `-Oz`).

Exit code is non-zero on any failure, and every failure prints its clip id.

### Fixtures

```sh
python3 scripts/make_fixtures.py      # regenerate the 12 reference clips
```

## 5. Measured (2026-10-28, macOS arm64, Node v26.12.0, rustc 1.99.1, ffmpeg 9.0.2 + libopus 1.6.1)

Build, from a clean `target/`:

| Step | Raw | gzip -9 |
|---|---|---|
| `wasm-bindgen --target web` | 671,591 B | 233,017 B |
| after `wasm-opt -Oz` | **523,270 B** (511.0 KiB) | **219,133 B** (214.0 KiB) |
| saved | 22.1% | 6.0% |

Other generated files: `rasp.js` 9,694 B, `rasp.d.ts` 2,988 B,
`rasp_bg.wasm.d.ts` 899 B.

Headless full-corpus run (decode + `Float32Array` copy, the same work the page
does):

| Metric | Result |
|---|---|
| Clips | **1404/1404**, zero failures |
| Audio decoded | 1432.969 s (68,782,530 frames) |
| Fixture sample counts | **12/12** exact vs `fixtures/manifest.json` |
| Decode ms/clip | min **0.43–0.49** · p50 **1.04–1.24** · mean **1.09–1.31** · p95 **1.59–2.01** · max **10.8–15.1** (four runs; the max is scheduler noise, not a code path) |
| Throughput | **1.07–1.28 ms per second of audio** (≈ 780–940× realtime) |
| Wall time | 1.76–2.13 s including 1404 file reads |
| Typed error | `RaspDecodeError(ContainerNotWebm)` on junk input |
| Verified wasm | byte-identical to `web/wasm/rasp_bg.wasm` |
| Embedded absolute paths | none (`strings web/wasm/rasp_bg.wasm \| grep -c '/Users/'` → 0) |

Browser run — headless Chrome (new headless mode, macOS arm64, driven over the
DevTools Protocol against `http://127.0.0.1:8090/smoke.html`):

| Metric | Result |
|---|---|
| Module init | 7.1 ms (`init()` through `version()`) |
| Initial decode-on-select | `af_heart/greet` · 31,578 frames · 0.658 s · peak 0.8828 · **9.0 ms** (cold first decode) |
| Play decoded | `AudioContext.sampleRate` **48000 Hz**; status reports “Playing … at 48000 Hz, resampled by the browser to 48000 Hz” (headless has no audible output device) |
| Decode all | **1404/1404**, 0 failures |
| Decode ms/clip (browser) | min **0.50** · median **1.10** · mean **1.10** · max **5.10** |
| Throughput | 1.08 ms per second of audio (≈ 925× realtime) |
| Wall time | 2.42 s including all 1404 fetches |
| Console | **0 messages** (no errors, warnings, MIME warnings or 404s) |
| Page-reported gzip | 221,850 B via `CompressionStream` (system `gzip -9` reports 219,133 B; compression implementations differ slightly) |

The first decode in a fresh page is ~9 ms (cold wasm call/JIT); the steady-state
per-clip cost is the ~1 ms median. This is consistent with the Node numbers in
the table above, which is the same V8 engine.

## 6. Pass definition

D-04 passes when **all** of these hold:

1. `bash scripts/build_wasm.sh` from a clean `target/` exits 0 and prints the
   sizes above (or smaller).
2. `node scripts/verify_wasm_decode.mjs` prints `PASS: 1404/1404 clips decoded,
   zero failures.` and `12/12` fixture counts when the fixtures exist.
3. `curl -sSI http://127.0.0.1:8080/wasm/rasp_bg.wasm` reports
   `Content-type: application/wasm`.
4. In a browser, the page loads with **no console errors**, shows
   `version() = "rasp-wasm 0.1.0"`, decodes on selection, and plays a clip
   audibly and unprocessed.
5. **Decode all** ends at `1404/1404 passed · 0 failed`; if anything fails, the
   failing id and error are visible (a listed failure is a pass for honesty and
   a fail for acceptance).

## 7. What a browser/human must still confirm

The page was exercised in **headless Chrome** (load, decode-on-select, the play
code path, and the full 1404-clip run), and the Node verifier covers the same
decode path. Still needing real eyes/ears/devices:

- **Audible playback on real hardware.** Headless Chrome has no output device;
  it confirmed `AudioContext` runs at 48 kHz and `source.start()` executed, but
  not that a human hears clean, unmodified speech. Listen for clicks at the
  start/end beyond the source clip. Level parity against `dg-pp` is **HR-1**,
  not D-04.
- **Console is clean on load in a non-headless browser** (the CDP run saw zero
  messages, including no autoplay warning and no favicon/MIME 404s).
  Note any warning verbatim with its URL.
- **Initial decode-on-select** populates clip/frames/duration/format/decode/peak
  for `af_heart` + `greet`, and changing either select re-decodes.
- **Observed `AudioContext.sampleRate`** on the target browser (48000 vs 44100)
  and that playback still runs at the right speed — the browser resamples the
  48 kHz buffer; the page does not. Chrome reported 48000.
- **Safari and Firefox** page load + playback (D-05 owns the full matrix).
- **Failure honesty spot-check:** temporarily rename one `.weba` (or serve from
  a directory missing one) and confirm the run ends with a visible failure line
  naming that clip and an HTTP error, not a silent 1403/1404.

## 8. Deviations from the task packet

- **`samples()` returns a copy**, not a view: wasm-bindgen's `Vec<f32>` return
  becomes a new `Float32Array`. This is documented in the crate docs and means
  the caller may hold it after `free()`. `length()` is **frames per channel**,
  not the interleaved sample count; for the mono corpus they are equal.
- **The headless verifier also runs `wasm-opt -Oz`** on the Node-target wasm so
  it checks the artifact that ships, and compares it byte-for-byte with
  `web/wasm/rasp_bg.wasm`. The task only required `--target nodejs` into a temp
  dir; this makes the check stricter, not looser.
- **`serve.sh` has an automatic MIME fallback.** The prescribed command is used
  unchanged when Python already reports `application/wasm`; otherwise the
  equivalent server with the type registered is started. Documented above.
- **`version()` returns `"rasp-wasm <crate version>"`**, not a bare number, so a
  page screenshot identifies the crate as well as the version.
- **Build paths are remapped in `RUSTFLAGS`** (`~/.cargo`→`/cargo`,
  `~/.rustup`→`/rustup`, repo root→`/rasp`) so the committed wasm contains no
  absolute local paths. This is a build-script detail, not an API change; the
  verifier mirrors it.
