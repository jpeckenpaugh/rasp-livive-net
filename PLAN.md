# RASP — Build Plan

**Project:** `rasp-livive-net` — Rust Audio Shift Processor
**Goal:** a static, client-side voice transformer for browser playback. Rust → WASM inside an `AudioWorklet`, driven by the `dg-pp` corpus. Two product modes: **natural** (age/gender shift) and **robot** (robot-avatar voicing).
**Owner:** Jarad Peckenpaugh (also runs human testing) · **PM / integrator:** this session · **Last updated:** 2026-10-28

---

## 1. Scope

### In scope

- Decode Opus-in-WebM clips in Rust/WASM (no browser codec dependency).
- **Natural mode:** independent **pitch** and **formant** (vocal-tract-length) control, plus timbre trim.
- **Robot mode ("Robotize"):** a post-shift synthetic chain — channel vocoder, ring modulation, comb/servo resonance, monotone F0 flattening, grit — for the game's robot avatars. Works on top of the natural controls, so a robot can still read as small, large, or androgynous.
- Registry-driven demo that mirrors the game's hook names, with an **avatar switch (Human / Robot)**.
- Static deploy at `https://rasp.livive.net` (GitHub Pages, `CNAME` present).
- Human perceptual testing driven by the owner, using a built-in QA harness page.

### Out of scope (for v1)

- Live microphone / realtime conversational shifting. The DSP core is written to allow it later; the product is not.
- Voice conversion, neural models, speaker identity transfer, TTS.
- Server-side processing of any kind.
- Non-English UI. Corpus audio in all 8 languages is in scope; the UI is English-only.
- Voice design from scratch (formant synthesis of new speakers). Robot mode transforms an existing voice; it does not invent one.

### Non-negotiable constraints

| # | Constraint | Consequence |
|---|---|---|
| K1 | GitHub Pages, no custom headers | No `COOP`/`COEP`, therefore **no `SharedArrayBuffer`, no wasm threads**. Single-threaded design. |
| K2 | Static assets only | Registry + clips fetched over HTTP with CORS (confirmed `*` upstream) or vendored locally. |
| K3 | Clips are 0.42–2.19 s | Latency budget is per-clip render, not streaming. Full pre-roll is affordable. |
| K4 | 48 kHz mono source | Browsers often run `AudioContext` at 44.1 kHz; explicit resampling required. |
| K5 | MIT project, Apache-2.0 Kokoro upstream | Attribution file required before public launch. |
| K6 | Robot lines are the **same** corpus lines | Robot avatars reuse the 26 game phrases; robot character must come from processing, not new recordings. |

---

## 2. Architecture decisions (locked unless a spike disproves them)

| ID | Decision | Rationale |
|---|---|---|
| **D1** | **Two independent axes** for natural mode: `pitch` in semitones (−12…+12) and `formant` as a ratio (0.75…1.33). Age/gender presets are points in this 2-D space. | Formant-only and pitch-only shifts are the two physically meaningful dimensions of age/sex variation. One blended "shift" knob cannot express them. |
| **D2** | **Phase vocoder with identity phase locking**, `N=2048`, Hann, 75% overlap at 48 kHz. `N` configurable in `{1024, 2048, 4096}`. | Good quality/latency trade at speech F0; phase locking kills the classic "phasiness" on voiced speech. |
| **D3** | **Envelope/excitation separation**: cepstral liftering (≈40 ms quefrency cutoff), fallback LPC order 18. Shift excitation by pitch ratio, re-apply envelope, optionally warp envelope by formant ratio. | This is what "formant-preserving" actually means; without it, pitch shift drags the formants and the result sounds chipmunked. |
| **D4** | **AudioWorklet is the product path, `AudioBufferSourceNode` is the A/B path.** Worklet receives full clip PCM once, renders in hop-sized chunks with pre-roll, plays out with latency compensation. | Satisfies the README, keeps one audio graph, and still allows instant original-vs-shifted comparison. |
| **D5** | **Decode in Rust/WASM.** Spike first: pure-Rust decoder (`oporus` / `opus-pure` / `ruopus` via Symphonia adapter) preferred; `libopus` compiled to wasm (`audiopus` / `opus`) is the fallback. | Symphonia 0.6 core has **no** Opus decoder — this is the single biggest technical unknown. See R1. |
| **D6** | **Vendor the corpus into this repo** (or a Pages sibling path) rather than hot-linking `dg-pp`. | Demo must not break when the bake site is rebuilt or moved. 7 MB is acceptable for Pages. |
| **D7** | **No DSP tuning on language-mismatched voices.** Several non-English voices render English phrases phonetically (e.g. `hf_alpha`: `ˈʌb hˌəm dˈoːst hɛ̃!`). They are regression material only. | Tuning against them would optimize for the wrong signal. |
| **D8** | **Robotize is a separate chain after natural processing**, not a third slider on the same stage. Order: natural shift → robot chain → limiter. Robot chain has a `wet` **amount** (0–1) and a `style`. | Keeps the natural path untouched and calibrated, makes neat A/B possible, and lets pitch/formant still govern a robot's apparent size. |
| **D9** | **Three robot styles, one shared engine:** `vocoder` (band vocoder, synthetic pulse/saw carrier — classic android), `ring` (ring modulation — Dalek), `servo` (short comb + slow AM — machine in a housing). Optional `grit` (bit depth + sample-rate decimation) applied last. | Three recognisable archetypes cover the avatar range without maintaining three engines. |
| **D10** | **The vocoder reuses D3's envelope**: keep the shifted clip's spectral envelope, **replace the excitation with a carrier** locked to the target F0. | Nearly free — the expensive analysis already exists — and it is exactly why a vocoded robot stays intelligible. |
| **D11** | **Monotone F0 flattening is a robot cue but is language-gated.** Default `monotone`: 0 for `zh`, ≤ 0.3 for `ja`/`hi`, free range for `en`/`es`/`fr`/`it`/`pt`. | Mandarin tones, Japanese pitch accent and Hindi stress carry lexical meaning; flattening them turns words into mush. See R11. |
| **D12** | **Presets are avatar-framed, not knob-framed.** Human: `natural`, `child`, `elder`, `fem→masc`, `masc→fem`. Robot: `android`, `servo`, `dalek`, each with natural-mode pitch/formant underneath. | The consumer is the game; an avatar picker is the interface the runtime actually needs. |

---

## 3. Sub-agent roster and dispatch contract

| Role | Agent | Use for |
|---|---|---|
| **RECON** | `explore` | Read-only investigation, dependency archaeology, codec/browser capability checks, corpus measurement. Never writes product code. |
| **BUILD** | `general` | Implementation, scaffolding, test writing, refactors, CI config. |
| **VERIFY** | `general` (fresh) | Independent review of a finished task against its acceptance criteria. Never the same agent that built it. |
| **HUMAN** | Jarad | Perceptual verification, blind listening rounds, cross-device smoke, sign-off on "does this sound like a robot / like a person". Never delegated to an agent. |
| **PM** | this session | Sequencing, dispatch packets, integration, merge review, resolving blockers, updating this file. |

### Dispatch packet (required for every task)

1. **Objective** — one sentence, verb-first.
2. **Files in scope** — exact paths; explicit "do not touch" list.
3. **Constraints** — relevant decisions from §2 and anything discovered in earlier waves.
4. **Acceptance** — copy the task's acceptance criteria verbatim; no paraphrasing.
5. **Report format** — `done | blocked`, files changed, commands run + output summary, deviations, follow-ups.
6. **Timebox** — recon tasks get a hard stop; on expiry, report findings rather than expanding scope.

### Rules

- A task is not "done" until **VERIFY** confirms it, or the PM waives verification in writing (spikes only).
- Anything auditory that a metric cannot settle goes to **HUMAN**, batched into a round (§9). Never ask for one-off listening opinions without a harness recording the answer.
- Spikes produce a written recommendation, not production code. Spike code lives in `spikes/` and is deleted once the decision is recorded.
- No sub-agent may edit `PLAN.md`, `DECISIONS.md`, the registry artifacts, or `docs/qa/`. Those are PM- and HUMAN-owned.
- Wave boundaries are integration points: build + tests green + this file updated before the next wave starts.
- If acceptance criteria turn out to be wrong, the sub-agent stops and reports; it does not silently redefine success.

---

## 4. Stage map

```
M0 F ──► M1 D ──► M2 P ──► M3 B ──► M4 W ──► M5 U ──► M6 Q ──► M7 X ──► M8 S
Foundation  Decode   DSP      Robotize  WASM+     Demo     Calib     Perf     Ship
                     core               Worklet   app
```

| Stage | Milestone | Exit criteria |
|---|---|---|
| **F** | M0 Foundation | Repo builds; corpus vendored + integrity-verified; decisions recorded; fixtures generated |
| **D** | M1 Decode & playback | Any of 1404 clips decodes in Rust to 48 kHz f32 and plays in-browser, unmodified |
| **P** | M2 DSP core | Offline CLI shifts a clip by ±12 st and 0.75–1.33 formant with metrics passing |
| **B** | M3 Robotize | Vocoder / ring / servo styles and monotone produce recognisable robots that stay intelligible |
| **W** | M4 WASM + Worklet | Same output in-browser via `AudioWorklet`; no main-thread DSP; no underruns |
| **U** | M5 Demo app | Avatar switch, registry-driven selection, both mode control sets, A/B, QA harness |
| **Q** | M6 Calibration | Per-voice baselines; natural + robot presets calibrated; human rounds and regression suite green |
| **X** | M7 Perf & polish | Size, load, render, memory budgets met; robot CPU cost contained; cross-browser green |
| **S** | M8 Ship | CI deploys to `rasp.livive.net`; attribution, docs, rollback, human launch smoke |

---

## 5. Task backlog

Owner column = the agent to dispatch. `PM`/`HUMAN` items are not delegated to sub-agents.

### Stage F — Foundation (M0)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| F-01 | Scaffold Cargo workspace: `crates/rasp-dsp`, `crates/rasp-opus`, `crates/rasp-cli`, `web/`, `scripts/`, `fixtures/` (gitignored), `spikes/` | BUILD | — | `cargo test --workspace` runs; `web/index.html` placeholder exists |
| F-02 | Pin toolchain: `rust-toolchain.toml` (1.99.1, `wasm32-unknown-unknown`), pin `wasm-bindgen` CLI **0.2.129** (crate must match exactly), document `wasm-opt`/binaryen. **`wasm-pack` is not used** — the CLI path is sufficient | BUILD | F-01 | Clean checkout → `cargo build --target wasm32-unknown-unknown` succeeds; `scripts/check-env.sh --strict` passes |
| F-03 | Corpus vendoring script: fetch `registry.json`, verify every `sha256`/`bytes`, copy 1404 `.weba` into `web/audio/`, rewrite paths | BUILD | F-01 | Re-run is idempotent; any mismatch exits non-zero; ≈7.0 MB / 1404 files |
| F-04 | `DECISIONS.md`: record D1–D12, decode-spike outcome, deviations | PM | D-01 | Every decision has date, context, rejected alternatives |
| F-05 | Fixture set: 12 clips covering short/long, male/female, `en` + `zh` + `ja`, plus the Hindi-reading-English case. Decode to 48 kHz mono f32 via `ffmpeg` for native tests | BUILD | F-03 | Reproducible by script; names encode voice + phrase |
| F-06 | Corpus measurement: per-voice F0 range, duration, silence head/tail, peak/RMS/LUFS → `docs/corpus.md` | RECON | F-03 | Reproducible from committed script; flags clipping/DC offset |
| F-07 | CI skeleton: native `cargo test`, `fmt --check`, `clippy -D warnings`, wasm build, registry integrity | BUILD | F-01 | Green on first push; no deploy step yet |

### Stage D — Decode & playback (M1)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| D-01 | **Decode spike (timeboxed). — DONE:** `opus-pure` 0.2.2 + `symphonia` 0.6.1 (`mkv`). 82–87 dB vs libopus, Δ0 samples, 0.68 ms/s native, 0.91 ms/s wasm, 467 KiB `-Oz` (200 KiB gzip). `oporus` non-conformant + LGPL; `libopus`-to-wasm blocked on missing CMake | RECON | F-05 | ✅ `docs/spikes/decode.md` |
| D-02 | `rasp-opus`: `symphonia` WebM demux + `opus-pure` decode → f32 @ 48 kHz. Apply `pre_skip` (312) and Matroska `DiscardPadding` for sample-accurate framing; `durationMs` is **not** a sample-count oracle | BUILD | D-01 | Sample count equals libopus reference; **SNR > 60 dB vs `ffmpeg -c:a libopus`** (plain `ffmpeg -i` uses a different decoder and is not sample-aligned — an unpassable target); tests run against `web/audio/` and skip gracefully if ffmpeg is absent |
| D-02b | `DiscardPadding` handling for out-of-range values. Measured: libopus **ignores** an oversized discard (960 trims, 961 trims nothing), so the rule is "trim only if it fits the final packet", not a brute clamp — a clamp still failed the oracle at 961 | BUILD | D-02 | ✅ 960/961/1200/5000 all match the libopus oracle; corpus byte-identical (FNV digest + frame counts over all 1404) |
| D-02c | Error classification: `ContainerNotWebm` only when the EBML magic is absent; truncated-but-WebM → `MalformedContainer`; zero bytes → `EmptyStream` | BUILD | D-02 | ✅ Truncation sweep 0..3420: `MalformedContainer` 42 → **3416**, `ContainerNotWebm` 3378 → **3** (partial magic only), 0 traps |
| D-03 | Resampler 48 k ↔ arbitrary `AudioContext` rate (polyphase/windowed-sinc; `rubato` allowed) | BUILD | F-01 | Flat passband to 20 kHz, image rejection > 80 dB, length ±1 sample |
| D-04 | Browser smoke page: fetch registry, pick a clip, decode in WASM, play with **no** processing | BUILD | D-02 | 1404/1404 decode; no console errors; timings visible |
| D-05 | VERIFY stage D | VERIFY | D-02..D-04 | Independent Chrome + Safari run; report attached |
| **HR-1** | Human round: latency and decode feel, click-free starts, level parity vs `dg-pp` | HUMAN | D-04 | Pass/fail per clip in `docs/qa/round-1.md` |

### Stage P — DSP core, native (M2)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| P-01 | STFT/ISTFT primitives: Hann, `N ∈ {1024,2048,4096}`, hop `N/4`, COLA-correct overlap-add, framing with delay bookkeeping. Document that window choice follows **the signal being analysed** (D19): 2048 resolves the corpus's 79–323 Hz input F0; a pass that analyses a shifted-down output needs 4096 | BUILD | F-01 | Perfect reconstruction < −100 dB on noise and fixtures; delay reported numerically |
| P-02 | Pitch shifter: phase-vocoder time-stretch + resample, instantaneous-frequency estimation, identity phase locking, **per-frame ratio curve** (needed by B-04) | BUILD | P-01 | ✅ **Met by per-frame measurement** (≤ 5.4 c worst, typically < 2 c, no octave errors in band). The median-F0 harness could not arbitrate — see P-07b |
| P-02b | Harden the P-02 API: `process_into` must not `expect` on a render error (W-01 promises "no panics across the boundary"); decide hysteresis for curve dips below the 0.6 window threshold | BUILD | P-02 | Fallible render path; no `expect`/`unwrap` reachable from public API; documented behaviour for a brief curve dip |
| P-02c | Repair pitch accuracy. **Not a DSP repair in the end** — P-07b's ruler paired voiced frames by rank, not by time, and every catastrophic cell had a voiced-count mismatch. `am_onyx` +7: **+105.94 c** (rank) vs **+2.60 c** (frame-indexed). P-02b's panic fix landed; the window-policy half moved to **P-03b** under D19 | BUILD | P-02, P-07b | Closed as "blocked on ruler" — see P-07d; re-scored after it |
| P-07d | **Fix the pitch ruler's pairing.** Pair by frame index on the shared grid, not by voiced rank; enforce p90 + octave clauses, not just the median; apply per-frame reliability and sub-harmonic octave rejection before the statistics; add a *glide* mismatch control (a constant-F0 control cannot see rank pairing) | BUILD | P-07b, P-07c | ✅ **54 PASS / 1 FAIL / 8 N/A**, 0 octave outliers, worst median **3.34 c**, worst p90 **33.25 c** (one cell). Rank-pairing negative control reads −32 c and is rejected. Frame drop 12.7%, false-rejection rate 8.0% on real speech, 0 on synthetic |
| P-03 | Spectral envelope extraction + excitation/resynthesis; formant ratio warp; expose the envelope as a reusable analysis object (for D10) | BUILD | P-01 | ✅ Required cells: F1/F2 within **8%** (worst 7.3%), 4 ms lifter + true-envelope refinement. Corners `(+7, 0.75)` / `(−7, 1.33)` fail physically — see D1 amendment |
| P-03b | **Decouple the correction window from the shifter (D19).** `VoiceTransformer` must stop reusing `pitch.window_size()`; its correction pass analyses the shifted output and needs `N=4096` when that output F0 is low, while the shifter's front end should stay at 2048 for this corpus | BUILD | P-03, P-02c | ✅ `pitch_window_size()` / `correction_window_size()`; correction picks 4096 below a 100 Hz proxy. Formant table unchanged (worst 7.3%); `af_heart` −12 pitch **+15.65 → +2.15 c**; worst anchor median **2.81 c**. Cost +11% only on cells that need 4096 |
| P-07c | Port P-03's synthetic-reference **control** into the formant check | BUILD | P-03, P-07b | ✅ Harmonic-model ruler exact on synthetic (worst **0.208%** vs LPC 10–60%); negative control rejects a 10%-wrong truth 12/12; D1 region enforced; **real speech → `N/A`** (see P-07e) |
| P-07e | **Make the formant ruler usable on real 32 kbps speech, or define what Q-01 may publish.** Measured the codec floor by round-tripping synthetic vowels through Opus@32k, then tested model extensions against the clean-synthetic control | BUILD | P-07c | ✅ Outcome **B**: 3.5 kHz band floor 0.58 dB median / 2.23 p90 per frame; every extension that lowered fixture residual failed the synthetic control (2.58% / 15.13% / 39.82% worst); 0/12 fixtures certifiable → 95% intervals implemented, all formant fields `uncalibrated`, F0 only |
| P-04 | Transient handling: phase reset / peak-lock on onsets; endpoint detection (not fixed padding) + 5 ms fades | BUILD | P-02, P-03 | ✅ Endpoints match F-06 exactly (median head 40 ms / tail 64.6 ms; max 524 ms); **0/1404 clips lose > 0.1% energy**, none empty. Onset region-reset: pooled pre-echo **−2.9 dB**, rise time −6 ms, impulse centroid +359 samples at 0.5 (inside bound). Pitch matrix now **36/36 anchors pass** |
| P-05 | Natural params & presets: `pitch_st`, `formant_ratio`, `tilt_db_per_oct`, smoothing; presets `shift-up`, `shift-down`, `bright`, `deep`. Each preset is **measured at its own operating point** — D1's validated region is sparse (pitch 0 and ±7 rows only), so interpolating between rows is not evidence | BUILD | P-02, P-03, P-04 | Params validated/clamped; every shipped preset measured on synthetic vowels + 12 fixtures; `docs/presets.md` records operating point, expected F0/formant deltas, and measured error |
| P-06 | `rasp-cli`: `rasp render --in x.weba --pitch 4 --formant 1.1 --out y.wav`, batch mode over fixtures | BUILD | P-02..P-05 | Deterministic bytes; 1 s clip < 50 ms native |
| P-07 | Objective metrics harness (`scripts/metrics.py`, pinned venv): F0 (YIN/autocorr), LPC formants, centroid, HNR, LUFS, null test for bypass | BUILD | P-06 | ✅ Consolidates F-06 primitives; corpus doc regenerates byte-identical; identity null ≤ −200 dB |
| P-07b | **Fix the pitch check so it can arbitrate a ±5-cent claim.** Median-F0-vs-median-F0 fails even on a mathematically exact resample (−20…+38 c) because voiced-frame selection changes. Use same-time per-frame ratios (40 ms frames, paired), widen/derive the tracker band per cell, mark out-of-band cells N/A instead of FAIL, and add `--self-test` that runs a control (SciPy `resample_poly`, exact) which must pass | BUILD | P-02, P-07 | Control passes at every in-band cell; P-02 renders pass; out-of-band cells report N/A; `--self-test` wired into CI |
| P-08 | Loudness policy: match shifted RMS to source; true-peak limiter as the output stage (also used by the robot chain) | BUILD | P-06 | A/B level jump < 1 dB; no overs > −1 dBTP; holds at every preset |
| P-09 | VERIFY stage P | VERIFY | P-01..P-08 | Reproduces metric table from clean checkout; reviews 12 fixtures for artifacts |
| **HR-2** | Human round: blind natural-mode A/B — "same person, different age?" vs "chipmunk?" | HUMAN | P-09 | Ratings per preset in `docs/qa/round-2.md` |

### Stage B — Robotize (M3)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| B-01 | Carrier generator: pulse/saw/square at target F0 (48 k), `jitter` (0 = pure machine) and `drift` params; band-limited to avoid aliasing | BUILD | P-01 | Aliasing < −60 dB; F0 accurate to ±1 cent; switches style without phase discontinuity |
| B-02 | **Channel vocoder**: 24–32 log-spaced bands (80 Hz–8 kHz), envelope followers with attack/release, carrier excitation from D10, wet/dry | BUILD | B-01, P-03 | Band-envelope correlation with source > 0.9 on fixtures; no pumping at syllable rate; intelligible on all 12 fixtures |
| B-03 | `ring` and `servo` styles: ring mod with selectable carrier (20–120 Hz), comb with tunable delay + slow AM | BUILD | B-01 | Ring sideband structure matches theory; comb notches land where expected; no clipping at full wet |
| B-04 | Monotone F0 flattening as a per-frame pitch ratio, `amount` 0–1, using the existing F0 tracker; language-aware defaults per D11 | BUILD | P-02 | At amount 1, F0 std reduced ≥ 80% on `en` fixtures with formants intact; no warble; zh/ja gates verified by a language test |
| B-05 | Robot params & presets: `style`, `wet`, `grit` (bit depth + decimation), `monotone`, `jitter`; presets `android`, `servo`, `dalek` with natural-mode underlays | BUILD | B-02..B-04 | Params clamped; each preset documented with target metrics; CPU ≤ 1.5× natural chain |
| B-06 | Metrics extension for robot mode: band-envelope correlation, spectral flatness, F0 stability, grit quantization noise floor | BUILD | B-02, P-07 | One command emits natural + robot tables for a batch |
| B-07 | VERIFY stage B | VERIFY | B-01..B-06 | Re-runs the full batch; confirms style separation in metrics (styles not converging to the same sound) |
| **HR-3** | Human round: **blind robot identification** — which style is which, and is it a robot or a person? Plus a first listen for "cheesy" | HUMAN | B-07 | ≥ 90% style ID, "robot" rated ≥ 4/5 plausibility, notes in `docs/qa/round-3.md` |

### Stage W — WASM + AudioWorklet (M4)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| W-01 | `wasm-bindgen` surface in **`crates/rasp-wasm`** (created by D-04): `init`, `decode(bytes) -> ClipHandle`, `render(clip, natural, robot) -> Float32Array`, explicit `free`; no panics across the boundary; `#![forbid(unsafe_code)]`; document that **callers must `free()`** (D-05 measured 494 MB growth over 1000 unfreed decodes — the generated `FinalizationRegistry` does not save you) | BUILD | D-02, P-06, B-05 | Errors returned as typed results, never traps; 10k init/free cycles with stable RSS |
| W-02 | Size/ABI budget: CI builds the artifact from source (D18 — nothing generated is committed), `wasm-opt -Oz`, audit exports, strip debug; record baseline; enforce ≤ 1.5 MB gzipped | BUILD | W-01 | Size gate in CI, not a manual script; baseline documented in `docs/perf.md` |
| W-03 | `AudioWorkletProcessor`: receives clip PCM + params once, renders hop blocks with pre-roll, plays out, reports `underruns`/`processMs` via `port` | BUILD | W-01, D-03 | Zero underruns over 100 plays; start latency < 40 ms |
| W-04 | Worklet loading: wasm instantiation inside worklet scope, no main-thread dependency after init | BUILD | W-03 | Works Chrome/Safari/Firefox; graceful error if `addModule` fails |
| W-05 | A/B path: `AudioBufferSourceNode` bypass sharing the same decoded clip, sample-aligned switching | BUILD | W-03 | Switching mid-line phase-continuous within 1 sample; no click |
| W-06 | Worker offload: decode + offline render in a Web Worker; transfer `Float32Array` ownership (no clone copies) | BUILD | W-01 | Main thread blocked < 16 ms during a 2 s render |
| W-07 | Params marshalling for both modes: one frozen struct, validated once, versioned so old cached JS cannot send new fields | BUILD | W-01, B-05 | Unknown/missing fields rejected with a typed error, never UB |
| W-08 | VERIFY stage W | VERIFY | W-01..W-07 | Chrome + Safari + Firefox run-through; leak check; underrun log attached |

### Stage U — Demo app (M5)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| U-01 | App shell: static `web/`, no framework build step (or Vite, if justified and documented); dark UI consistent with the `dg-pp` bench | BUILD | F-01 | `npm run dev` + static build both work; no runtime CDN deps |
| U-02 | Registry loader + typed models: `Voice`, `Phrase`, `Hook`, `ClipEntry`; filters by language/gender/locale/grade | BUILD | U-01 | 54 voices × 26 phrases browsable; missing entries surfaced, never crash |
| U-03 | Natural controls: pitch (−12…+12 st), formant (0.75…1.33), tilt; numeric readout in cents/%; reset | BUILD | U-01 | Applies on next render without reload; clamps; keyboard-operable |
| U-04 | Natural preset row: Original / Child / Elder / Fem→Masc / Masc→Fem | BUILD | U-03, P-05 | Preset ↔ slider state bidirectional and stable |
| U-05 | A/B compare: hold-to-hear-original, plus waveform + spectrum overlay of original vs processed | BUILD | U-02, W-05 | Sample-aligned; overlay matches what is heard |
| U-06 | Hook player: pick a hook, sample a candidate phrase as the runtime would, play processed | BUILD | U-02, W-03 | All 17 hooks playable; sampling varies per hook's candidate list |
| U-07 | Diagnostics: decode ms, render ms, wasm size, sample rate, underruns, build id, registry hash, active mode | BUILD | U-02 | Matches `docs/perf.md` within noise |
| U-08 | **Avatar switch**: Human / Robot. Selecting Robot loads the robot preset row and keeps natural controls as the underlying base | BUILD | U-03, W-07 | Mode switch preserves the phrase and re-renders without reload; state is shareable via URL |
| U-09 | Robot controls: style (`vocoder`/`ring`/`servo`), `wet`, `grit`, `monotone`, `jitter`; mono-pitch warning when the selected voice is `zh`/`ja` | BUILD | U-08, B-05 | Sliders map 1:1 to DSP params; warning appears only where D11 gates apply |
| U-10 | Robot preset row: Android / Servo / Dalek, each shown with its natural underlay | BUILD | U-09, B-05 | Preset restores both layers; switching style does not reset pitch/formant |
| U-11 | Hook player routes by avatar: robot avatar samples the same hook through the robot preset | BUILD | U-06, U-08 | Same hook, two avatars, clearly different characters, same words |
| U-12 | **QA harness page** (`web/qa.html`, noindex): randomized blind trials — A/B/X, style ID, phrase ID — 5-point ratings, artifact tagging, CSV export + `localStorage` resume | BUILD | U-05, U-09 | A 15-minute round is completable without instructions; CSV imports into `docs/qa/` unchanged |
| U-13 | VERIFY stage U | VERIFY | U-01..U-12 | Full click-through against acceptance; a11y pass (tab order, labels, contrast) |
| **HR-4** | Human round: full demo click-through on desktop + phone, both avatars, all presets | HUMAN | U-13 | Issue list triaged into tasks in `docs/qa/round-4.md` |

### Stage Q — Calibration & quality (M6)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| Q-01 | Per-voice baselines: **median F0 for all 54 voices**; formants only where the controlled ruler certifies them (D20/P-07e: currently none) → `web/data/voice-baselines.json`. Medians only — p10/p90 carry octave ambiguity below HNR 8 | BUILD | F-06, P-07, P-07e | Deterministic; every field either `graded` with a 95% interval or `null` + `uncalibrated` + reason. No number published without a passing control |
| Q-02 | Preset calibration **as relative transforms** (D21): per cohort, F0 and level deltas only; formant deltas are fixed per preset and validated on synthetic ground truth. No "this voice should land here" targets | BUILD | Q-01, P-05 | F0 delta within 10% of target per cohort; level within 1 dB; no formant claim against real speech |
| Q-03 | Regression suite: golden renders for 12 fixtures × 5 natural presets; metric bounds, not bytes | BUILD | P-07, Q-02 | Fails on deliberately broken DSP; passes on refactor |
| Q-04 | ABX scoring pipeline: consumes U-12 CSV, computes per-preset accuracy, significance, and a trend line across rounds | BUILD | U-12 | One command turns a round into `docs/listening.md` rows |
| Q-05 | Artifact hunt: all 1404 clips × 5 natural presets, ranked by anomaly score, review worst 30 | BUILD | Q-03 | Each anomaly fixed or explicitly accepted with a note |
| Q-06 | VERIFY stage Q (natural) | VERIFY | Q-01..Q-05 | Re-runs full-corpus pass; no regressions vs golden metrics |
| Q-07 | **Robot calibration**: per style, tune band count, carrier, comb delay, `grit` so the three styles are measurably distinct and intelligible; robot regression set (12 fixtures × 3 styles × 3 natural underlays) | BUILD | B-06, Q-03 | Styles separate on ≥ 3 metrics; regression suite covers robot mode; targets in `docs/presets.md` |
| Q-08 | **Language safety pass for robot mode**: all 8 languages × 3 styles with the D11 monotone gates; flag any locale where intelligibility collapses | BUILD | Q-07 | Report in `docs/robot-languages.md`; every language has a recommended `monotone` ceiling |
| Q-09 | VERIFY stage Q (robot) | VERIFY | Q-07, Q-08 | Independent full-corpus robot pass; confirms style separation and language gates |
| **HR-5** | Human round: **blind phrase identification** (robot mode, all 3 styles, en + zh + ja), and natural-vs-robot preference for each of the 26 game lines | HUMAN | Q-09 | Robot phrase ID ≥ 90%; zh/ja gate validated; results in `docs/qa/round-5.md` |

### Stage X — Performance & polish (M7)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| X-01 | Profile and optimize: SIMD128 where profitable, buffer reuse, no per-frame allocation | BUILD | W-02, P-06 | ≥ 3× faster than the M4 baseline, or ≥ 5× realtime on an M1; documented |
| X-02 | Load strategy: registry 508 KB, on-demand clips, prefetch hook candidates (~2 clips), cache headers, optional service worker | BUILD | W-06 | Cold interactive < 2 s on cable; repeat visit < 300 ms |
| X-03 | Memory audit: peak RSS over 10 sequential renders, wasm heap growth, `free` discipline | BUILD | W-01 | No monotonic growth over 100 renders; peak < 250 MB |
| X-04 | Cross-browser matrix: Chrome, Safari, Firefox, iOS Safari; `AudioWorklet` + wasm + 44.1 kHz resample | BUILD | U-13 | All pass playback, A/B, mode switch; gaps documented |
| X-05 | Accessibility + mobile: labels, focus order, reduced motion, touch targets, 360 px layout | BUILD | U-13 | Lighthouse a11y ≥ 95; usable one-handed at 360 px |
| X-06 | Docs: `README.md`, `docs/architecture.md`, `docs/corpus.md`, `docs/perf.md`, `docs/presets.md`, `docs/robot-languages.md`, `ATTRIBUTION.md` | BUILD | X-04 | A new contributor can build, calibrate, and deploy from docs alone |
| X-07 | VERIFY stage X | VERIFY | X-01..X-06 | Independent measurements reproduce `docs/perf.md`; doc commands actually work |
| X-08 | **Robot CPU/size containment**: 32-band vocoder cost, carrier generation, per-style memory; enforce a wasm size gate in CI | BUILD | X-01, Q-07 | Robot render ≤ 1.5× natural in wasm; total bundle still ≤ 1.5 MB gzipped |

### Stage S — Ship (M8)

| ID | Task | Owner | Depends | Acceptance |
|---|---|---|---|---|
| S-01 | Pages deploy workflow: build wasm, integrity check, publish `web/`, keep `CNAME`, verify `application/wasm` | BUILD | X-07, X-08 | Push to `main` publishes; size + MIME checked in CI |
| S-02 | Release checklist + rollback: tag, header/cache verification, previous artifact retrievable → `docs/release.md` | PM | S-01 | Executed once end-to-end and committed |
| S-03 | Launch smoke: 5 real devices/browsers, cold + warm, both avatars, all presets, 3 languages | HUMAN | S-02 | Signed-off table in `docs/release.md` |

---

## 6. Wave plan (dispatch schedule)

| Wave | Parallel tasks | Gate to next wave |
|---|---|---|
| **1** | F-01, F-03, F-06, D-01 (spike), F-02 | Repo builds; corpus local + verified; decode recommendation written |
| **2** | D-02, D-03, F-05, F-07, F-04 (PM) | M1: 1404/1404 decode, browser playback, **HR-1** |
| **3** | P-01, P-06 (skeleton), P-07 (skeleton) | STFT round-trips |
| **4** | P-02, P-03 (parallel, different files), P-04 | Pitch and formant tolerances met |
| **5** | P-05, P-08, B-01 | CLI + metrics green on fixtures, **HR-2** |
| **6** | B-02, B-03, B-06 | Vocoder + styles render; robot metrics exist |
| **7** | B-04, B-05, W-01 | M3: three styles, separate in metrics, **HR-3** |
| **8** | W-02, W-03, W-06, W-07 | M4: worklet plays shifted and robot audio |
| **9** | U-01, U-02, W-04, W-05 | App shell lists all voices/phrases |
| **10** | U-03, U-04, U-05, U-06, U-07 | Natural demo works |
| **11** | U-08, U-09, U-10, U-11, U-12 | M5: both avatars playable, QA harness live, **HR-4** |
| **12** | Q-01, Q-03, Q-05, X-01 (early profile) | Baselines + natural artifact list |
| **13** | Q-02, Q-04, Q-07, Q-08 | M6: presets calibrated, language gates measured |
| **14** | X-02, X-03, X-04, X-05, X-06, X-08 | M7: budgets + browser matrix |
| **15** | S-01, S-02, S-03 | M8: live at `rasp.livive.net`, **HR-5** signed off |

VERIFY tasks (D-05, P-09, B-07, W-08, U-13, Q-06/Q-09, X-07) run at each stage gate in a fresh `general` context, and are never parallel with the stage they verify.

---

## 7. Risk register

| ID | Risk | Likelihood | Impact | Mitigation | Owner |
|---|---|---|---|---|---|
| **R1** | No pure-Rust Opus decoder in Symphonia 0.6 core; adapters may be immature or bloat wasm | High | High | D-01 spike before any DSP work; `libopus`→wasm fallback; worst case build-time PCM only for tests while keeping Opus in production | PM |
| **R2** | Pages has no COOP/COEP → no shared memory, no threads | Certain | Medium | Single-threaded by design; budgets assume it; no `SharedArrayBuffer` in any task | PM |
| **R3** | Phase-vocoder artifacts on 0.4–2.2 s one-shots (pre-echo, phasiness, transient smear) | High | High | Phase locking (D2), transient reset (P-04), full-corpus hunt (Q-05), A/B default in UI | BUILD |
| **R4** | 32 kbps Opus makes formant estimation noisy; knobs unstable near extremes | Medium | High | Envelope smoothing, clipped ranges, tune on `en` cohort only (D7) | BUILD |
| **R5** | No perceptual ground truth → optimizing numbers, sounding worse | Medium | High | QA harness (U-12) + rounds before calibration; human sign-off at every gate | PM + HUMAN |
| **R6** | Scope creep toward realtime mic / voice conversion | Medium | Medium | §1 out-of-scope list; new asks go to a `v2` list, not the backlog | PM |
| **R7** | Safari `AudioWorklet`/wasm quirks (44.1 kHz, module MIME, autoplay) | Medium | Medium | X-04 matrix; W-04 error surface; A/B `AudioBufferSourceNode` as fallback demo | BUILD |
| **R8** | Corpus churn: bake rebuilds change hashes and paths | Low | Medium | D6 vendoring + pinned snapshot; CI integrity check fails loudly on drift | BUILD |
| **R9** | wasm size creep blows the load budget | Medium | Medium | W-02 baseline, CI size gate, `wasm-opt -Oz`, no formatting/panic machinery | BUILD |
| **R10** | **Robot mode destroys intelligibility** — vocoder smears consonants, ring mod eats the fundamental | High | High | D10 (keep the source envelope), band count tuned in Q-07, blind phrase-ID gate ≥ 90% (HR-5), `wet` default < 1 | BUILD |
| **R11** | **Monotone flattening breaks tonal languages** — Mandarin tones and Japanese pitch accent are lexical | High | High | D11 language gates; Q-08 per-language pass; UI warning (U-09); default `monotone` = 0 for `zh` | PM |
| **R12** | **Robot aesthetic mismatch** — reads as B-movie gimmick rather than "this character is a machine" | Medium | High | Three distinct styles (D9), human plausibility rating at HR-3, game-line preference test at HR-5; accept that one style may be cut | HUMAN |
| **R13** | Robot and natural layers interact — pitch/formant knobs reintroduce artifacts into the vocoder | Medium | Medium | Fixed chain order (D8), robot regression set includes 3 natural underlays (Q-07) | BUILD |
| **R14** | Human testing becomes the bottleneck (one tester, perceptual rounds are serial) | Medium | Medium | 15-minute harness (U-12), batched rounds, CSV import, objective metrics pre-filtered so human time goes to decisions only | PM |

---

## 8. Quality gates (definition of done)

A task is done only if **all** apply:

1. Acceptance criteria met, demonstrated with a command or click-through — not asserted.
2. `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace` pass.
3. No new browser console warnings on a clean load.
4. Behaviour-changing work updates the relevant doc (`docs/architecture.md`, `docs/perf.md`, `docs/presets.md`, `docs/robot-languages.md`, or this file).
5. Independent VERIFY pass at the stage gate.
6. Perceptual claims confirmed by a HUMAN round, not by a metric.
7. No secrets, no absolute local paths, no committed fixtures over 1 MB.

**Measurable targets carried through the project**

| Metric | Target |
|---|---|
| Pitch accuracy | median ≤ **5 cents**, p90 ≤ 25 c, 0 octave outliers — whole clip, all frames voiced in both signals (D17). Out-of-band cells are `N/A`, never `PASS` |
| Formant accuracy (pitch bypassed) | within 8% of requested F1/F2 ratio, **inside the D1 validated region only** |
| Perfect-reconstruction error | < −100 dB |
| Realtime factor (wasm, M1-class laptop) | ≥ 5× realtime offline render |
| Playback start latency | < 40 ms |
| Worklet underruns | 0 over 100 plays |
| wasm bundle | ≤ 1.5 MB gzipped |
| Cold interactive load | < 2 s |
| Peak memory, 10 clips | < 250 MB |
| A/B level jump | < 1 dB |
| Robot band-envelope correlation | > 0.9 vs source |
| Robot blind phrase identification | ≥ 90% |
| Robot style identification | ≥ 90% |
| Robot render cost | ≤ 1.5× natural |
| Natural blind phrase identification | ≥ 95% |

---

## 9. Human testing protocol

The owner tests. The job of the build is to make that cheap and unambiguous.

**Rounds** (each produces `docs/qa/round-N.md`, appended, never rewritten)

| Round | Gate | Duration | What is judged | Passing looks like |
|---|---|---|---|---|
| HR-1 | M1 | 10 min | Decode + playback feel, clicks, level parity | No artifacts; levels match `dg-pp` |
| HR-2 | M2 | 20 min | Natural mode: same-person plausibility per preset | ≥ 4/5 "sounds like the same person" for `child`/`elder` |
| HR-3 | M3 | 20 min | Robot styles: identification + plausibility + cheese factor | ≥ 90% style ID; ≥ 4/5 robot plausibility |
| HR-4 | M5 | 20 min | Full app: flow, latency, mobile, controls | No blocking issues |
| HR-5 | M6 | 30 min | Blind phrase ID (robot, en/zh/ja); best style per game line | ≥ 90% robot phrase ID; zh/ja gates hold |
| HR-6 | M8 | 15 min | Launch smoke on real devices | Sign-off |

**Method rules**

- Blind by default: the harness randomizes order and hides labels until rated (U-12).
- Always paired with the original clip. Absolute judgement of synthetic speech is unreliable; relative judgement is not.
- Ratings are 5-point and comparative ("more like a person than X"), never free-floating.
- Any artifact gets a timestamp, clip id, voice id, mode, preset, and a one-line description. "Sounds bad" is not a defect report.
- Findings become tasks with IDs `QA-<round>-<n>` and a severity: `blocker` (blocks the gate), `major` (fix before next gate), `minor` (backlog), `wontfix` (recorded with reason).
- The tester does not test after a long session; rounds are short by design. Fatigue produces false passes.

---

## 10. Open questions for the owner

1. **Robot archetype priority.** If only two of `vocoder` / `ring` / `servo` ship, which is least useful for the game's avatars? (Default assumption: `servo` is the cut candidate.)
2. **Robot base voices.** All 54 voices robotizable, or a curated subset that reads best as machines? Curating would cut calibration work and improve quality.
3. **Monotone for Mandarin.** Accept "robots speak Mandarin without tones" as a stylistic choice, or keep tones and rely on vocoder/ring only for `zh`?
4. **Preset naming.** Age/gender-framed (Child, Elder, Fem→Masc) or neutral (Shift Up, Shift Down)? The game may expose these to players.
5. **Corpus hosting.** Vendor all 7 MB, or English only (~3.6 MB) and hot-link the rest?
6. **Public scope.** Fixed demo at a URL, or an embeddable artifact (`<script>` + worklet) other pages can use? This changes W-01's API surface.
7. **Attribution.** In-UI "voice model: Kokoro-82M" line, or `ATTRIBUTION.md` only?
8. **Realtime.** Confirm "later, not now" for microphone input so R6 stays closed.

---

## 11. Status tracker

Update on every wave gate. `☐` not started · `◐` in progress · `☑` done & verified · `✖` blocked

| Stage | Tasks | Status | Notes |
|---|---|---|---|
| F Foundation | 7 | ☑ | F-01..F-03, F-06 done; F-05/F-07 move to Wave 2 |
| D Decode | 5 | ☑ | All done. **HR-1 PASS** (Chrome + Safari, 1404/1404, levels match `dg-pp`) — see `docs/qa/round-1.md` |
| P DSP core | 12 | ◐ | P-01..P-04, P-07/07b/07c/07d ✅; **pitch 36/36 anchors, formants ≤ 7.3%**. P-05/P-08 next, P-07e running |
| B Robotize | 7 | ☐ | New; depends on P-03's envelope |
| W WASM/Worklet | 8 | ☐ | |
| U Demo app | 13 | ☐ | includes QA harness |
| Q Calibration | 10 | ☐ | Q-01 reduced to F0 only (D20/P-07e); Q-02 is relative-transform calibration (D21) |
| X Perf/polish | 8 | ☐ | |
| S Ship | 3 | ☐ | |

**Critical path:** D-01 → D-02 → P-01 → P-02/P-03 → B-01/B-02 → W-01 → W-03 → U-08 → Q-07 → X-04 → S-01.
**Highest-schedule-risk item:** ~~D-01~~ **closed** — decode works, ahead of schedule. Now D-02 (framing correctness) is the schedule risk.
**Second risk:** R11 — the language gate for `monotone` is easy to skip and expensive to discover late.

---

## 12. Wave log

Append-only. One block per wave: what ran, what it produced, what changed in the plan.

### Wave 1 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| F-01 | ✅ | Workspace scaffolded; `cargo test/fmt/clippy/build` green; `spikes/*` excluded from the workspace |
| F-02 | ✅ | `rust-toolchain.toml` (1.99.1), `docs/setup.md`, `scripts/check-env.sh`. **wasm-pack dropped** in favour of the `wasm-bindgen` CLI 0.2.129 |
| F-03 | ✅ | 1404 clips / 7,005,583 B vendored, hashes verified; `--check` catches 1-byte corruption; idempotent |
| F-06 | ✅ | `docs/corpus.md`: F0 medians 79–323 Hz, 11.7% of the corpus is silence, 170 clips with > 100 ms tail, no clipping/DC |
| D-01 | ✅ | **`opus-pure` + `symphonia` chosen.** 0.91 ms/s in wasm, Δ0 samples, 200 KiB gzipped |

**Plan amendments from Wave 1**

- D-02's SNR oracle corrected to `ffmpeg -c:a libopus` (plain `ffmpeg -i` is a different decoder and unpassable).
- P-01 gains the measured window guidance (4096 needed for −12 st on a low male).
- P-04 gains the measured trim distribution (worst tail 524 ms).
- Q-01 constrained to median F0 only (p10/p90 unreliable below HNR 8).
- F-02 rewritten: no `wasm-pack`, `wasm-bindgen` CLI 0.2.129 pinned.
- D13 curation amended: `am_onyx` and `am_eric` replace `bm_lewis` and `am_santa`.
- `binaryen`/`wasm-opt` 133 installed on the dev machine (W-02 preflight now passes).

### Wave 2 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| D-02 | ✅ | `decode_weba`: 66/66 exact sample counts vs `ffmpeg -c:a libopus`, SNR **79.4–89.5 dB** (target 60), 0.65 ms/s native, full corpus 1404/1404, wasm 366 KB → 166 KB gzip after `-Oz` |
| D-03 | ✅ | In-house Kaiser polyphase resampler, zero deps: worst passband error **0.0001 dB**, stopband **−100 dB**, chunked ≡ whole-buffer bit-exact, 113× realtime, 1.6 ms latency reported |
| F-05 | ✅ | 12 fixtures over all 8 groups, 0.394–2.058 s, manifest + reference PCM from `-c:a libopus`; audio stays out of git |
| F-07 | ✅ | 4-job CI (lint/test, wasm, offline corpus check, fixtures smoke) + weekly upstream sentinel; `actionlint` clean; every `run:` block executed locally |

**Plan amendments from Wave 2**

- `crates/rasp-wasm` is created by D-04 and extended by W-01 (previously W-01 assumed it existed).
- F-07's `wasm-build` job only *prints* size; the real gate stays W-02, as planned.
- `docs/setup.md` §7 corrected — the fixtures job is now a hard gate, not a warning.
- Fixtures are **not** golden data: f32/reference bytes move with the ffmpeg build, so Q-03 asserts metric bounds only (already the plan; now explicit).

### Wave 3 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| D-04 | ✅ | `crates/rasp-wasm` + `web/smoke.html`; **1404/1404 decoded in headless Chrome**, 1.1 ms/clip, wasm 523 KB raw / 219 KB gzip, 12/12 sample-count match vs libopus |
| P-01 | ✅ | In-house radix-2 real FFT, zero deps. Round-trip **−138 dB** worst (target −100), COLA spread 1.8e-7, chunked ≡ whole-buffer bit-identical, delay = N exactly |
| P-07 | ✅ | `audio_metrics.py` + `metrics.py` (pair compare, `--manifest`, `--json`, `--check`); identity null ≤ −200 dB; **`docs/corpus.md` regenerated byte-identical** after the refactor |

**Plan amendments from Wave 3**

- M1 is met pending HR-1. `web/wasm/` is a generated artifact in the tree — S-01 decides whether to publish CI-built wasm instead of committing it.
- P-02 will expose a **per-frame ratio curve**, not just a scalar, because B-04's monotone flattening needs time-varying pitch. Cheaper to design in now.
- The metrics CLI is the official arbiter for P-02/P-03/Q-03 numbers; DSP tests assert internally, and the CLI confirms end-to-end.

### Human round 1 — 2026-10-28

HR-1 is ready: unprocessed corpus playback through the WASM decode path, served at `http://127.0.0.1:8090/smoke.html`. Scope deliberately narrow — this round validates decode, not DSP, so that any later artifact is unambiguously ours.

**Result: PASS.** Chrome and Safari, 1404/1404 decoded, ~2.5 s wall, 1.10 ms median per clip, clips sound correct and match `dg-pp` levels. Logged in `docs/qa/round-1.md`. Firefox and the 44.1 kHz context remain unexercised — both are X-04 scope.

### Wave 4 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| P-02 | ✅ after repair | Time-stretch + resample PV with identity phase locking. Per-frame pitch error ≤ **5.4 c** worst (typically < 2 c), no octave errors in band; `N=4096` auto-selected only at −12 st |
| — | ⚠️ incident | The first P-02 agent died mid-run (`timed out waiting for response headers`). The code was unverified, and the "hanging test suite" turned out to be **block-buffered stdout**, not a hang: serial suite is 43 s, parallel 12.7 s. Nothing was wrong with the DSP. |

**Plan amendments from Wave 4**

- **P-02b added:** `process_into` can panic on an unreachable render error. W-01's "no panics across the boundary" makes that a defect, not a nit.
- **P-07b added:** the metrics harness's pitch check is unsound — median-F0-vs-median-F0 reports −20…+38 c for a *mathematically exact* resample, and ±1200 c when the target leaves its 60–400 Hz band. It cannot certify a ±5-cent claim. Fix before Q-03 relies on it.
- Acceptance wording: pitch accuracy is judged on **same-time per-frame ratios**, not median F0. Recorded here because it contradicts the naive reading of the §8 gate table.
- The dead agent wrote 300 renders into `target/`; cleaned up. Metric renders now go to a temp dir only.

### Wave 5 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| P-07b | ✅ | Pitch check rebuilt: paired per-frame ratios, 80–550 Hz reliable band with explicit `N/A`, `--renders`, `--allow-length-delta`, and `--self-test` (exact `resample_poly` control) wired into CI. Control worst **0.66 c** (legacy median method: **1249 c** on the same control) |
| P-03 | ◐ | In flight — envelope/excitation + formant ratio |

**Escalation and ruling (D17).** With a ruler that passes a perfect control, **P-02 fails its own acceptance**: 42 pass / 13 fail / 8 N/A, worst **+105.9 c** (`am_onyx` +7 st). PM ruling: the gate is the whole clip (median ≤ 5 c, p90 ≤ 25 c, no octave outliers), not a high-energy slice — the slice view is what let this through. Added **P-02c** to repair the shifter; **tolerance will not be widened**.

**Harness incident, second instance.** This is the same failure mode as P-02's "hanging tests": a measurement that looked authoritative and was wrong. Two rules now stand: every metric needs a known-exact control, and every acceptance number must name the analysis window and frame policy.

### Wave 6 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| P-03 | ✅ | Envelope/excitation split + formant warp + `VoiceTransformer`. Required cells pass: F1/F2 ≤ **7.3%** error (target 8%). Pitch held at ±7 st with formants fixed: **+0.18 / −0.01 cents** |
| — | ⚠️ two decision amendments | D3: lifter is **4 ms**, not 40 — 40 ms cancels the harmonics it should keep (+11.8% / +17.2% F1 error). LPC rejected as the warping default (−31…+91% error; its smoothness is insensitivity). D1: the knob space is **not a rectangle** |

**New limits recorded**

- **D1 amendment:** validated region is pitch 0 with formant 0.75–1.33, and ±7 st with formant 1.00–1.10. `(+7, 0.75)` and `(−7, 1.33)` fail physically (formant spacing approaching harmonic spacing; excitation has no energy to correct). Presets must stay inside; the UI must not imply the full rectangle is equally trustworthy.
- **P-07c added:** the formant ruler has no control either. P-03 showed an LPC ruler reports 586 Hz for a true 700 Hz formant at F0 220 Hz. Q-01 cannot publish baselines on an uncontrolled metric.
- P-03 measured the pitch-only path at **31.6 ms/s** (+7 st) — 31× realtime native, still far above the 5× realtime wasm target, but the double-STFT correction pass is now the dominant cost and X-01 should look at it.
- Formant correction cannot yet be combined with a per-frame pitch curve (the two time axes diverge). B-04 (monotone) is therefore either "curve without formant correction" or needs a time-mapped envelope. Logged for B-04.

### D-05 verification — Stage D (2026-10-28) · `pass-with-findings`

Every Stage D claim reproduced independently: 1404/1404 in Node and Chrome, wasm byte-identical to source, 66/66 oracle clips exact, **191/191** independent oracle clips exact (SNR 77.4–89.0 dB), 3,600 mutated inputs → **0 traps**, 3,000 decode+free cycles → **0 B** heap growth, and a deliberately deleted clip correctly reports `1403/1404 FAILED` rather than a silent pass.

Findings, all minor, now tasks:

| Finding | Disposition |
|---|---|
| `DiscardPadding` > one packet diverges from libopus (961+ samples) | **D-02b** — clamp to the final packet |
| Truncated WebM reported as `ContainerNotWebm` instead of `MalformedContainer` | **D-02c** — only claim "not WebM" when the magic is absent |
| `playSelected()` throws outside a `try` behind a `void` handler | Folded into **U-01** (the real app replaces this page; no unhandled rejections allowed) |
| CI never builds the shipped wasm; stale comment claiming otherwise | **D18** (wasm not committed) + **W-02** (CI builds and gates it) |
| `rasp-wasm` lacks `#![forbid(unsafe_code)]` | Folded into **W-01** |

Notable positive: `af_sarah/tough-one.weba` contains a literal `75 A2` **inside audio payload** — the structural EBML walk ignores it, while a naive `rfind` would erase the clip. That is the corpus defending its own decoder.

Unverifiable here and still open: Safari/Firefox, a real 44.1 kHz context, audible level parity (HR-1 covers the last one). All remain X-04 scope.

**D-02b/D-02c closed.** The interesting part: a literal clamp to the final packet (what PLAN originally specified) still diverges at 961 samples — libopus ignores an *oversized* discard entirely rather than clamping it. The implemented rule is "trim only if it fits the final packet", which reproduces the 960→961 discontinuity exactly. The plan wording was wrong; the oracle was right. Corpus output proven byte-identical by FNV digest over all 1404 clips.

### Wave 7 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| P-07c | ✅ | Harmonic-model formant ruler: **0.208%** worst on synthetic ground truth across F0 90–300 Hz, where LPC is 10–60% off and fails to resolve 3 peaks on 4/12 cells. Negative control rejects a 10%-wrong truth 12/12. D1 region enforced |
| P-03b | ◐ | Window decoupling under D19, in flight |
| P-07d | ◐ | Ruler pairing repair, dispatched |

**The uncomfortable result:** the new formant ruler is excellent on synthetic vowels and **`N/A` on every real fixture** — the 3-resonator model fits synthetic to 0.02 dB and real 32 kbps speech to 3–8 dB. So **Q-01 is blocked**, and **D20** now forbids filling the baseline table with LPC numbers that are 10–60% biased. Q-01 will publish `uncalibrated` instead of a confident wrong number; **P-07e** tries to make the ruler fit real speech (anti-resonances, F4/F5, codec floor measurement) before that becomes permanent.

That is the third ruler finding this week, and the pattern is consistent: the estimator is the weakest link, not the DSP. Every number that reaches a preset now has to survive a control first.

### Wave 8 — 2026-10-28

| Task | Result | Notable output |
|---|---|---|
| P-04 | ✅ | Endpoint trim matches F-06 exactly; **0/1404** clips lose > 0.1% energy. Onset region-reset: pre-echo **−2.9 dB** pooled, rise time −6 ms, pitch matrix **36/36 anchors**, 0 octave outliers |
| P-07d | ✅ | Frame-index pairing + full D17 gate + per-frame reliability + octave rejection. **54 PASS / 1 FAIL / 8 N/A**; the remaining cell was fixed by P-04. Mismatch control rejects rank pairing at −32 c |
| P-07e | ✅ | **Outcome B.** Codec floor at 3.5 kHz is 0.58 dB median / 2.23 p90 per frame; every model extension that lowered fixture residual failed the synthetic control; **0/36 formant fields publishable** |
| P-05 / P-08 | ◐ | Presets + loudness, in flight |

**Two decisions from P-07e**

- **D20 amendment:** real 32 kbps speech is not certifiable at this model order. Formants are published only as `graded` values with 95% intervals; on this corpus that means none, and Q-01 publishes **F0 only**.
- **D21:** presets are **relative transforms**, not absolute voice reconstructions. Each preset is measured at its own operating point on synthetic ground truth (formants) plus fixtures (F0, level). This is why "child/elder" framing stays internal — it implies absolute targets we cannot verify.

The product consequence is small and worth stating plainly: we can promise "this transform does X to F0 and Y to the vocal-tract envelope", not "this is what an 8-year-old sounds like". For a game with robot avatars and stylised characters, that is the more honest and more useful contract.
