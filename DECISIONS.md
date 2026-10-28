# DECISIONS

Owner: Jarad Peckenpaugh · Maintained by PM · Last updated: 2026-10-28

This file records *why* the architecture is what it is. Every entry has a date, the context that forced a choice, the alternatives that were rejected, and what would have to be true to reopen it. If code and this file disagree, this file is wrong — fix it or fix the code, but do not let them drift.

Canonical task and stage definitions live in [PLAN.md](PLAN.md). This file does not restate the backlog.

---

## D1 — Two independent axes for natural mode

**Date:** 2026-10-28 · **Status:** accepted

Pitch (±12 st) and formant ratio (0.75–1.33) are separate parameters. Age/gender presets are points in that 2-D space, not a mode.

*Rejected:* a single "shift" knob (cannot express "deeper without getting bigger"), separate hard-coded male/female chains (untestable, and the game's avatars are not binary).

**D-01 amendment, 2026-10-28 (P-03) — the knob space is not a rectangle.**
The pitch × formant plane is only validated in these regions:

| Pitch | Validated formant range |
|---|---|
| 0 st | 0.75 … 1.33 |
| ±7 st | 1.00 … 1.10 |

Outside that — specifically `(+7 st, 0.75)` and `(−7 st, 1.33)`, which ask the correction to move formants ~2× *against* an already-scaled excitation — the result fails badly (F1 error +46% / −52%, and worse on the low male). Two physical limits: the target formant spacing approaches the shifted harmonic spacing, and the excitation carries no energy where the correction is asked for, so the ±30 dB gain clamp saturates.

**Consequence:** P-05 and Q-02 presets must stay inside the validated region, and the UI must not present the full rectangle as equally trustworthy. Extreme opposite-corner combinations are a known unsupported area, not a bug to chase.

*Reopen if:* the game exposes only one slider to players — then a preset table over D1 is still the right implementation.

---

## D2 — Phase vocoder, N=2048, Hann, 75% overlap, identity phase locking

**Date:** 2026-10-28 · **Status:** accepted

`N` is configurable in `{1024, 2048, 4096}` for latency experiments; 2048 @ 48 kHz is the default because it resolves speech F0 down to ~90 Hz while keeping the window under 43 ms.

*Rejected:* plain overlap-add with no phase locking (audible phasiness), WSOLA/time-domain PSOLA (fails on 0.4 s clips without usable pitch marks).

*Reopen if:* P-02/P-03 cannot meet ±5 cents / 8% tolerances — then revisit `N` and phase-locking strategy before changing approach entirely.

---

## D3 — Envelope/excitation separation via cepstral liftering

**Date:** 2026-10-28 · **Status:** accepted

Lifter cutoff ≈ 40 ms quefrency; LPC order 18 as fallback. Pitch shifts the excitation; the envelope is re-applied, optionally warped by the formant ratio.

*Rejected:* pitch-shifting the whole spectrum (chipmunking), formant correction as a post-EQ (cannot move formants, only tilt).

**D-03 result, 2026-10-28 (P-03 amendment) — lifter cutoff is 4 ms, not 40 ms, plus true-envelope refinement.**
Measured on synthetic vowels with known formants: a 40 ms lifter keeps harmonic ripple (pitch quefrency is 3–11 ms across the corpus) inside the "envelope", so re-applying it cancels the harmonics it should preserve — F1 error **+11.8%** (male) and **+17.2%** (low male) on the formant-only cell, versus **+0.9%** at 4 ms. Three passes of iterative `max(log|X|, E)` refinement on top of 4 ms liftering removes the remaining downward bias. Implementation: `DEFAULT_LIFTER_SECONDS = 0.004`.

**LPC is not viable as the warping default.** It is temporally smoother (0.38–0.86 dB frame-to-frame vs 0.92–1.89 dB for cepstral) but that smoothness is insensitivity: at +7 st / 1.10 it applies only 3.2–6.7 dB of correction where 16.7–25.8 dB is needed, and F1/F2 errors land at **−31…+91%** versus ≤ 7.3% for cepstral. It stays behind the same API as a documented alternative, not the default.

**Composition path (b) chosen:** post-correct the P-02 output with a per-frame `E_target/E_observed` gain, rather than integrating the envelope into the vocoder. One formant rule for all cases, no risk to P-02's phase-locking loop, and P-02's window fallback keeps working. Cost: one extra STFT/ISTFT plus two envelope analyses (≈ +12.5 ms/s at +7 st, 31.6 ms/s total).

*Rejected:* LPC as default (numbers above), 40 ms liftering (wrong ruler for a 100–300 Hz F0 corpus).

**P-05 amendment, 2026-10-28 — the lifter must follow the pitch.**
A fixed 4 ms lifter exceeds the pitch period once the *output* F0 rises above ~250 Hz, so the "envelope" starts tracking individual harmonics and the correction chases them: measured HNR **−4.7…−8.9 dB** and p90 spread up to 1207 cents on upward presets. The lifter is now capped at `min(configured, 0.75 / F0)`.

This is inactive at every P-03 measured cell (output F0 ≤ 165 Hz), so P-03's formant table is byte-identical. It removed 13 of 14 preset D17 failures and all 30 octave outliers. The constant `0.75/F0` is a guard against lifter-over-period, not a tuning knob.

---

## D4 — AudioWorklet is the product path; AudioBufferSourceNode is the A/B path

**Date:** 2026-10-28 · **Status:** accepted

The worklet receives the full clip once, renders hop blocks with pre-roll, and plays out. A bypass path plays the same decoded buffer for comparison.

*Rejected:* processing in the main thread (blocking), full-file render then single `AudioBufferSourceNode` only (no realtime mode ever, and no per-hop visualization).

*Reopen if:* W-03 cannot hit < 40 ms start latency with zero underruns — then fall back to offline render + buffer playback for v1.

---

## D5 — Opus decode in Rust/WASM

**Date:** 2026-10-28 · **Status:** accepted, pending D-01 spike outcome

Preference order: pure-Rust decoder (`oporus` / `opus-pure` / `ruopus` via a Symphonia adapter), then `libopus` compiled to wasm.

*Context:* Symphonia 0.6 core has **no** Opus decoder — only adapters. This is the single largest unknown in the project (PLAN R1).

**D-01 result, 2026-10-28 — resolved: `symphonia` 0.6.1 (`mkv`) + `opus-pure` 0.2.2.**
Measured on 12 clips: 82.6–87.4 dB SNR against `ffmpeg -c:a libopus`, zero sample-count delta, 0.68 ms per second of audio native and 0.91 ms/s in wasm, 575 KiB wasm before `wasm-opt -Oz` → 467 KiB after (≈ 200 KiB gzipped). Crate is BSD-3, actively released, 134 `unsafe` blocks and 14 dependencies — the `unsafe` count is the main long-term concern and is contained behind our own safe API in `rasp-opus`.

*Rejected candidates:* `oporus` (non-conformant output: −9.19…+11.15 dB and clipping at ±1.0, plus LGPL-2.1), `ruopus` direct (works, 0.90 ms/s, but 41 dependencies and slower), `symphonia-adapter-opus-rs` (works, 42 deps, 748 KiB wasm), `libopus` via `opus`/`audiopus` (blocked: bundled build needs CMake, absent on this machine; wasm path also lacks `emcc`/`wasm-ld`).

*Consequence for D-02:* the accuracy target must name the reference decoder — plain `ffmpeg -i` uses FFmpeg's native Opus implementation, which is **not** sample-aligned with libopus (14.5–31.9 dB, peaking at 21.5 dB even with fractional-sinc alignment). Only `ffmpeg -c:a libopus` is a valid oracle.

*Rejected:* relying on `decodeAudioData` (Safari inconsistency, and it hides the sample rate), shipping PCM (138 MB for the corpus).

---

## D6 — Vendor the corpus, all 7 MB, all 8 languages

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ5)

`web/audio/<voice>/<phrase>.weba` plus a pinned `web/registry.json` live in this repo. GitHub Pages allows it; the alternative is a demo that breaks when the bake is rebuilt.

*Rejected:* hot-linking `dg-pp` (churn, third-party dependency for a shipped product), English-only vendoring (saves 3.4 MB, costs a language feature).

*Reopen if:* repo clone time becomes a real complaint, or Pages bandwidth becomes a problem.

---

## D7 — No DSP tuning on language-mismatched voices

**Date:** 2026-10-28 · **Status:** accepted

Several non-English Kokoro voices read the English phrases phonetically (e.g. `hf_alpha` → `ˈʌb hˌəm dˈoːst hɛ̃!`). They are regression material, never calibration targets.

---

## D8 — Robotize is a separate chain after natural processing

**Date:** 2026-10-28 · **Status:** accepted

Fixed order: natural shift → robot chain → loudness/limiter. Robot controls are `style`, `wet` (0–1), `grit`, `monotone`, `jitter`.

*Rejected:* a third slider on the same stage (entangles calibration, makes A/B meaningless), robotizing before the pitch shift (carrier would be shifted twice).

*Reopen if:* R13 shows the vocoder is unstable across the pitch range — then reorder and document why.

---

## D9 — Three robot styles on one engine

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ1)

`vocoder` (android), `ring` (Dalek), `servo` (comb + slow AM). All three ship. **If one must be cut, cut `servo`** — it is the most subtle and the most redundant once `vocoder` exists.

*Rejected:* one "robot" button (the game's avatars are not all the same machine), four-plus styles (calibration cost).

---

## D10 — The vocoder reuses D3's spectral envelope

**Date:** 2026-10-28 · **Status:** accepted

Keep the shifted clip's envelope, replace the excitation with a band-limited carrier locked to the target F0. This is both the cheapest and the most intelligible option, and it is why B-02 depends on P-03 rather than re-deriving its own analysis.

*Rejected:* an independent filter bank fed by the raw signal (double analysis, worse phase coherence), noise-vocoding (intelligible but not recognisably "robot").

---

## D11 — Monotone F0 flattening is language-gated

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ3)

Default `monotone`: **0 for `zh`**, **≤ 0.3 for `ja`/`hi`**, unrestricted for `en`/`es`/`fr`/`it`/`pt`. Mandarin tones, Japanese pitch accent, and Hindi stress are lexical; flattening them destroys words.

The UI must show the gate as a visible constraint with an explicit opt-in override, not a silently clamped slider.

*Rejected:* flattening everywhere (breaks three languages), disabling monotone entirely (loses the strongest robot cue).

---

## D12 — Presets are avatar-framed; labels are player-facing and neutral

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ4)

Human presets: `natural`, `shift-up`, `shift-down`, `bright`, `deep`. Robot presets: `android`, `servo`, `dalek`.

Age/gender framing stays internal (docs, calibration tables, commit messages) but **not** in the UI, because the game may expose these to players and calling a 30-year-old character "elder" is a design bug.

*Rejected:* Child/Elder/Fem→Masc labels (loaded, and wrong for many characters), numeric-only labels (unusable by a designer).

---

## D13 — Robotizable voice set: all 54 functional, 12 curated for calibration

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ2)

Every voice remains robotizable. Calibration, QA rounds, and the HR-5 preference test run on a curated set of 12: 6 English (**4 F / 2 M**, 79–248 Hz — deliberately female-heavy, because the low-male pitch range is the hard case for the vocoder and one low anchor plus one mid male is enough to expose it), 2 Mandarin, 2 Japanese, 1 Hindi, 1 Spanish. Curation reduces human testing time without removing capability.

**F-06 amendment, 2026-10-28.** Measured F0 medians span 79–323 Hz. The set is confirmed with two English substitutions: `bm_lewis` → **`am_onyx`** (88 Hz, the low anchor) and `am_santa` → **`am_eric`**. Final curation: low `am_onyx`, high `af_aoede` (221 Hz), stress `bf_alice` (248 Hz), plus `af_heart`, `am_eric`, `bf_emma`; `zf_xiaoni`, `zm_yunxi`; `jf_nezumi`, `jm_kumo`; `hf_alpha`; `ef_dora`. Weak slots are flagged rather than filled: Mandarin male (`zm_yunjian`, 6.7 dB HNR) and Spanish (`ef_dora`, 7.9 dB HNR) — interpret their results with that in mind.

*Rejected:* curating the product to 12 voices (arbitrary, and the game may add avatars), calibrating all 54 by ear (unaffordable).

---

## D14 — v1 ships a fixed demo; the WASM API is written as if it were embeddable

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ6)

No `<script>` bundle, no npm package, no versioned embed contract in v1. But W-01's surface (`init` / `decode` / `render` / `free`, one frozen params struct) is documented and kept clean enough that packaging later is a small job.

*Rejected:* building the embed story now (premature; the API will change once calibration lands).

---

## D15 — Attribution in both places, minimally

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ7)

`ATTRIBUTION.md` with the full Kokoro-82M Apache-2.0 notice, plus one footer line in the UI: `Voices: Kokoro-82M (Apache-2.0)`. No modal, no banner.

---

## D16 — Realtime microphone input is out of scope for v1

**Date:** 2026-10-28 · **Status:** accepted (resolves OQ8, closes PLAN R6)

The DSP core is written to permit a streaming mode later (D2/D4 framing and D8 chain order already allow it). v1 processes whole clips. New realtime requests go to a `v2` list.

---

## D17 — Pitch accuracy is gated on the whole clip, not a chosen slice

**Date:** 2026-10-28 · **Status:** accepted (PM ruling on the P-07b escalation)

**Context.** P-02's original acceptance measured one high-energy 4096-sample slice per render. P-07b's new paired per-frame metric scores the whole clip with a ruler that passes an exact control at 0.66 cents. Under that ruler, 13 of 63 P-02 cells fail, worst **+105.9 cents** at `am_onyx` +7/+12 st. The DSP is not meeting the stated target; the earlier slice-based view was flattering it.

**Decision.** The gate is:

| Condition | Threshold |
|---|---|
| Median absolute per-frame error | ≤ 5 cents |
| p90 absolute per-frame error | ≤ 25 cents |
| Octave outliers | 0 |
| Frames counted | every frame voiced in **both** signals, whole clip |

No steady-state or high-energy subset. Cells whose expected output F0 falls below 80 Hz or above 550 Hz are `N/A` (the ruler cannot certify a cent there), never `PASS`.

**Why.** A player hears the whole line, glides included. Excluding glides would hide exactly the artifact class most likely to be audible — pitch drift and wobble on intonation contours — and P-02's largest errors are on those regions, not on steady vowels. The ruler is now trustworthy (exact control at sub-cent), so a failure is information, not noise.

**Consequence.** P-02 does **not** meet acceptance as shipped. Task **P-02c** repairs it. Widening the tolerance is explicitly rejected: ±25 cents is roughly a quarter-tone and is audible as wrongness on sustained notes.

**D17 amendment, 2026-10-28 (P-02c report) — the gate stands; its first verdict was void.**
The rule is unchanged (median ≤ 5 c, p90 ≤ 25 c, 0 octave outliers, whole clip, frames voiced in both signals). But P-07b's implementation paired the **compacted voiced-only arrays by rank**, not by frame index:

```python
paired_count = min(len(ref_f0), len(test_f0))
ref_paired = ref_f0[:paired_count]   # n-th voiced ref frame vs n-th voiced test frame
```

When the two signals disagree about one marginal frame, every later pair slips. Evidence: `am_onyx` +7 st has 48 voiced reference frames and 52 voiced render frames, and reports **+105.94 c** by rank-pairing versus **+2.60 c** when frames are paired by index. Every catastrophic cell had a voiced-count mismatch; the one cell whose counts matched (`af_heart` −12) read the same under both methods. So the "13 failures" verdict is a ruler defect, not a DSP defect.

Two further defects in the same task: `--renders` enforced only the median, so a cell with **21 octave outliers and p90 731 c** was reported `PASS`; and the `--self-test` control could not catch rank-pairing because its synthetic control has identical voiced counts on both sides.

**Consequence:** **P-07d** fixes the ruler (pair by frame index, enforce p90/octave, per-frame reliability band, and a control that deliberately mismatches voiced counts). The gate is re-scored after that; the shifter is not re-tuned against a broken measurement.

*Rejected:* accepting the 13 failures as real (they are not), relaxing p90 (it is the guard against exactly this class of slip).

---

## D18 — Generated wasm is not committed

**Date:** 2026-10-28 · **Status:** accepted (PM ruling on D-05 finding F4)

`web/wasm/` is build output. It is gitignored, produced by `scripts/build_wasm.sh`, and rebuilt by CI for deploys (S-01). Serving a checkout requires running the build first.

**Why.** A committed binary drifts from its source silently; the only thing that caught it today was a manual script. CI rebuilding from source makes "the shipped wasm matches the Rust" true by construction, and keeps the 523 KB artifact out of the diff. It also settles the ambiguity D-05 found: `docs/d-04-smoke.md` claimed the output was committed while nothing under `web/` was tracked.

*Consequence:* the wasm size gate (W-02) and the deploy (S-01) must both build in CI, and the drift check moves from "manual" to "the build is the artifact".

*Rejected:* committing the artifact for zero-build hosting (drift risk, review noise).

## D19 — Window size follows the signal being analysed, not the requested ratio

**Date:** 2026-10-28 · **Status:** accepted (P-02c finding)

The phase vocoder's analysis window must resolve the F0 of **the signal it actually analyses**. P-02's rule — "switch to `N=4096` when the pitch ratio drops below 0.6" — is based on the *output* F0, but the shift is realised in the read-out resampler, and the front-end analysis sees the **input**. For the corpus (79–323 Hz) `N=2048` resolves the input fine even at −12 st. Measured with correct frame pairing: fixed `N=2048` gives 43 pass / 12 fail (worst 2.81 c) and `af_heart` −12 improves from +15.6 c to **+2.15 c**; `N=4096` gives 39/16 (worst 15.65 c).

The converse is true for P-03's correction pass: it analyses the **shifted output**, so it genuinely needs the wider window when that output F0 is low. Today it inherits the shifter's `N`, which is the wrong direction of coupling.

**Decision.** Two independent windows:

| Stage | Window chosen from |
|---|---|
| `PitchShifter` front end | input signal F0 (default 2048 for this corpus) |
| `VoiceTransformer` correction pass | shifted output F0 (4096 when it is low) |

**Consequence:** `VoiceTransformer` must stop reusing `pitch.window_size()`; P-03's formant table is re-verified after decoupling (task **P-03b**).

*Rejected:* one shared window (forces 4096 where it is unnecessary and 2048 where it is harmful), and choosing by ratio at both stages (wrong signal at both).

---

## D20 — No baseline is published from an uncontrolled ruler

**Date:** 2026-10-28 · **Status:** accepted (P-07c finding)

The harmonic-model formant ruler is exact on synthetic vowels (worst **0.208%** across F0 90–300 Hz) but returns `N/A` on **every real fixture**: the 3-resonator + tilt model fits synthetic vowels to 0.01–0.02 dB RMS and real 32 kbps speech to 3–8 dB (anti-resonances, source ripple, F4/F5 unmodelled). LPC would produce numbers on the same clips, but biased by **10–60%** and sometimes failing to resolve three peaks at all.

**Decision.** Q-01 publishes a formant baseline only where a controlled ruler certifies it. Ungraded voices are marked `uncalibrated` in the data file with the reason, and presets calibrated against them are labelled approximate. Silently substituting LPC numbers to fill a table is prohibited.

**Why.** Presets are the product. A baseline that is 20% off produces a preset that is 20% off, and nothing downstream would reveal it — the same failure mode as D17's rank-pairing, one layer deeper.

*Rejected:* publishing LPC baselines with a footnote (footnotes don't survive into a preset table), widening the residual gate until real speech passes (that turns `N/A` into a confident wrong number).

*Path out:* task **P-07e** — extend the model (anti-resonances, F4/F5, source-ripple term) and/or measure the codec's own contribution by round-tripping synthetic vowels through Opus at 32 kbps. If the floor is above the gate, the ruler reports ranges, not points.

**P-07e result, 2026-10-28 — outcome B: real speech is not certifiable at this model order.**

The decisive number is the codec's own contribution, measured by round-tripping synthetic vowels with known formants through Opus@32k:

| Fit band | Clean residual | Coded residual (median / max) | Worst formant error |
|---|---|---|---|
| 3.0 kHz | 0.02 dB | 0.25 / 0.38 dB | 0.94% |
| 3.5 kHz | 0.02 dB | **0.40 / 0.82 dB** | 1.08% |
| 4.0 kHz | 0.02 dB | 1.43 / 1.92 dB | 5.01% |
| 6.0 kHz | 0.02 dB | 4.03 / 5.21 dB | 11.82% |

At 3.5 kHz the per-frame floor is 0.58 dB median / 2.23 dB p90 — already above the 1.0 dB gate the model was held to. Above 3.5 kHz the codec, not the model, sets the floor.

Every model extension that lowered the fixture residual **failed the clean-synthetic control** at high F0: +2 zero pairs 2.58% worst, 5 resonators 15.13%, 5 res + 2 zeros + curvature 39.82%. At F0 300 Hz there are 11 harmonics below 3.5 kHz against 12–17 free parameters — the optimiser reaches zero residual by moving F1/F2 arbitrarily. The fits also wander: frame-to-frame F1 SD **90–400 Hz**.

**0 of 12 fixtures** pass the 1.5 dB calibration gate (best 2.74 dB), so **0 of 36 formant fields are publishable**. Implemented policy: a formant is `graded` only when residual ≤ 1.5 dB, ≥ 3 usable frames, dof > 0, condition number < 1e8, and each 95% half-width ≤ 8%; otherwise `null` + `"status": "uncalibrated"` + reason. On this corpus that means F0 only.

*Rejected:* widening the gate until fixtures pass (turns `uncalibrated` into a confident wrong number), LPC as a fallback (10–60% biased).

**Reopening condition:** a pitch-synchronous or cepstral estimator with its own ground truth *and* a codec control, or a source model that explains glottal ripple.

---

## D21 — Presets are relative transforms; there is no absolute voice calibration in v1

**Date:** 2026-10-28 · **Status:** accepted (P-07e outcome B)

Absolute per-voice formant baselines are unmeasurable on this corpus (0 of 36 fields publishable). Therefore Q-02 must not calibrate presets against "this voice should land here" targets — those targets do not exist.

**Decision.** A preset is a **relative transform** with a stated operating point (`pitch_st`, `formant_ratio`, `tilt`), validated on two grounds:

1. **Synthetic ground truth** — vowels with known formants, where the transform's effect on F1/F2 is measurable to under 1%.
2. **Real fixtures** — F0 and level deltas only, where the ruler is certified.

**Consequence.** Presets are described by what they *do*, not by who they *sound like*. D12's neutral player-facing labels already fit; "child/elder" framing would imply absolute targets we cannot verify, which is why it stays internal. Q-02's cohort table reduces to F0 and level per cohort, with formant deltas fixed per preset.

*Rejected:* interpolating formant targets from gender labels (the labels are not measurements — F-06 found `am_santa` at 268 Hz and `zm_yunxia` at 315 Hz), faux-precision ranges presented as calibration.

---

## D22 — Preset strength is proportional to the headroom that voice actually has

**Date:** 2026-10-28 · **Status:** accepted (owner listening feedback + measurement)

**The complaint.** "A deep voice should not be pushed deeper, and a high voice should not be pushed higher."

**It is measurable.** Presets are fixed-direction pushes that ignore where the voice already sits. Against the corpus's own measured F0 envelope (79–323 Hz over 54 voices, median 178 Hz):

| Preset | Nominal | Voices outside the envelope | Worst case |
|---|---|---|---|
| `shift-up` | +7 st (×1.50) | **16 / 54 above 323 Hz** | `bf_alice` 248 → **372 Hz**; max 484 Hz |
| `deep` | −4 st (×0.79) | **3 / 54 below 79 Hz** | `am_onyx` 88 → **70 Hz**; `bm_lewis` 79 → 63 |
| `shift-down` | −3 st (×0.84) | 3 / 54 below | `jm_kumo` 86 → 68 |

It is **not** "presets are too strong in general": the median voice under `shift-up` lands at 267 Hz, the 90th percentile of the corpus — aggressive but inside the population. The failures are purely directional headroom at the extremes.

**Decision.** Effective shift = nominal × `w`, where

```
room_up(f)   = 12·log2(HI / f)        HI = 323 Hz
room_down(f) = 12·log2(f / LO)        LO =  79 Hz
w            = clamp(room_direction(f) / room_direction(178 Hz), 0, 1)
```

`f` is the clip's measured median F0 (±3% required), 178 Hz is the corpus median, and the bounds are the corpus's measured envelope — documented as a population envelope, **not** physiology. The same `w` scales the formant log-ratio and tilt, so a voice with no room does not get a tract change either.

**Consequence.** For the deepest voice, `deep` becomes essentially a no-op (−0.55 st at 88 Hz, w 0.14); for the highest, `shift-up` becomes a no-op. Mid voices keep the full nominal shift. The UI must show this honestly ("no headroom") rather than silently applying nothing.

*Rejected:* a global strength reduction (leaves the extremes broken and weakens the middle), a hard clamp at the bounds (voices bunch against the wall), an absolute target percentile (makes `shift-up` inert for every female voice, ~half the cast).

*Reopen if:* HR-2 shows the middle of the range is also too strong — then reduce the nominal strengths, not the mapping.

1. Write the proposed change in a new entry (do not edit history).
2. State what evidence forces it (a spike result, a metric, a human round).
3. Update PLAN.md's decision table and any affected acceptance criteria in the same commit.
4. PM signs off. Sub-agents may not change decisions mid-task; they stop and report instead.
