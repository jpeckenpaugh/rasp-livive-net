# Natural presets and loudness policy

**Tasks:** P-05 (natural parameters, tilt, presets) and P-08 (loudness policy) · **Date:** 2026-10-28
**Scope:** `crates/rasp-dsp` — `params`, `loudness`, and the tilt path in `transform`.
**Not in scope:** perceptual claims (those are HR-2), the robot chain (D8 is exposed, not wired), and per-voice calibration (Q-02).

This document is the evidence behind the four shipped natural presets. Every number here was produced by the committed code and the commands in [§10](#10-reproduce); nothing is hand-edited. If a number here disagrees with the code, the code wins and this file is wrong.

## 1. Operating points

D12 fixes the labels; this task fixes the points. A preset is a point in D1's pitch × formant plane plus a tilt.

| Preset | `pitch_st` | `formant_ratio` | `tilt_db_per_oct` | Physical intent |
|---|---|---|---|---|
| `natural` | 0 | 1.00 | 0 | neutral; bit-exact bypass, included so the UI row is complete |
| `shift-up` | +7 | 1.10 | +0.75 | **smaller speaker**: a fifth higher F0, a 10% shorter vocal tract, a little extra HF |
| `shift-down` | −3 | 0.88 | 0 | **larger adult**: a minor third lower F0 and a 12% longer tract |
| `bright` | 0 | 1.00 | +1.5 | **same speaker, brighter mic**: a shallower source tilt, explicitly no size change |
| `deep` | −4 | 0.85 | 0 | **large chest voice**: a fourth lower F0 and a 15% longer tract; the lowering *is* the darkening |

The size presets move pitch and formants **in the same direction**, which is the physically consistent one. The opposing corners are the ones D1 measured as broken and are deliberately absent.

`shift-up` is at D1's measured upward row (`+7 st`, formant 1.00–1.10). It is the only upward operating point inside that row that clears the D17 pitch gate on 11 of 12 fixtures; `+7 / 1.25` clears all 12 but leaves the D1 row, and its formant gate instead fails 4 of 8 synthetic cells (§8).

## 2. Method, and what "measured" means

Two rulers, both controlled — the P-02/P-07 lesson is that an uncontrolled estimator decides the answer:

* **F0** — the official D17 ruler: `scripts/metrics.py`, paired **same-time per-frame** ratios on a shared frame grid, median ≤ 5 c, p90 ≤ 25 c, **0 octave outliers**, every frame voiced in both signals. Cells whose expected output F0 leaves 80–550 Hz are `N/A`, never `PASS`.
* **Formants** — the P-03 harmonic-model fit (`testutil::fit_formants`), which recovers a synthetic vowel's formants to well under 1 % and rejects a 10 %-wrong truth. Each cell also fits an **analytic reference** synthesized at the expected F0/formants *with the same tilt*, so the ruler's free `C + α·log f` term must absorb the tilt exactly before the render is judged. The measured residual is reported as a conditioning diagnostic, not as a gate (processed speech is never a 3-resonator cascade, and a 1 dB residual gate would mark every processed cell `N/A`, including the ones whose formants are provably right).

A cell is **PASS** when both formants are within 8 % of the requested ratio. A cell is **UNSUPPORTED** when it is not: the measured number is still shown, the cause is stated, and the cell is excluded from the "passes every cell" claim. There are three such formant cells (§8); they are enumerated in code so an *un*documented failure still fails the test. The 8 % tolerance was not widened.

Formants are **not** reported on the 12 fixtures: D20 forbids publishing a formant number from a ruler that cannot certify real 32 kbps speech (the same ruler returns `N/A` there). The fixture evidence is pitch, level and true peak.

## 3. The validated region is a set of measured points, not a continuum

D1's amendment recorded two measured rows. This task adds measured points. The region is now:

| Point | What was measured | Result |
|---|---|---|
| pitch 0, formant 0.75 / 1.00 / 1.10 / 1.33 | P-03, 90/110 Hz synthetic vowels | all ≤ 1.6 % F1/F2 |
| pitch ±7, formant 1.00 / 1.10 | P-03, 90/110 Hz synthetic vowels | all ≤ 7.3 % F1/F2 |
| **pitch +7, formant 1.10, tilt +0.75** | this task, 90/110/150/220 Hz × 2 vowel sets | 6/8 cells; 2 unsupported (§8) |
| **pitch −3, formant 0.88, tilt 0** | this task, 90/110/150/220 Hz × 2 vowel sets | 7/8 cells; 1 unsupported (§8) |
| **pitch 0, formant 1.00, tilt +1.5** | this task, 90/110/150/220 Hz × 2 vowel sets | 8/8 cells, ≤ 0.6 % |
| **pitch −4, formant 0.85, tilt 0** | this task, 90/110/150/220 Hz × 2 vowel sets | 8/8 cells, ≤ 7.6 % |

**Interpolating between these rows is not evidence.** The scan behind §8 shows why: the same formant ratio can read 1 % at one pitch and 12 % one semitone away, because what matters is where the target formants fall relative to the output's harmonics, not how far the knobs are from a measured row. A new point is a new measurement.

The D1 rectangle remains *not* uniformly trustworthy: `(+7, 0.75)` and `(−7, 1.33)` still fail physically, and the cells in §8 show the upper-right corner of the plane is limited by harmonic resolution, not by a tunable parameter.

## 4. Synthetic vowels at each preset's own point

Rendered through the full chain (transform + loudness). `ctrl` is the analytic reference's worst F1/F2 error at the same cell — the ruler's control. `resid` is the fitted model's RMS log-amplitude residual (lower is better; ≤ 0.3 dB means the signal is model-exact, 4–10 dB means it no longer is). Level Δ is the loudness policy's A/B jump. The synthetic vowels are peak-normalised to 0.5, so the limiter is normally a no-op; `true peak` here is therefore the transform's peak.

| preset | vowel | F0 in | F0 out | err c | F1 | F2 | err F1% | err F2% | ctrl max% | fit resid dB | level Δ dB | peak dBFS | true peak dBTP | status |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| shift-up | 700/1220/2600 | 90 | 134.8 | +0.02 | 829 | 1394 | +7.6 | +3.9 | 0.3 | 5.01 | +0.00 | -3.81 | -3.81 | PASS |
| shift-up | 700/1220/2600 | 110 | 164.8 | +0.00 | 780 | 1363 | +1.2 | +1.6 | 0.3 | 3.80 | +0.00 | -5.20 | -5.20 | PASS |
| shift-up | 700/1220/2600 | 150 | 224.7 | +0.00 | 809 | 1367 | +5.0 | +1.9 | 0.0 | 6.46 | +0.00 | -5.62 | -5.62 | PASS |
| shift-up | 700/1220/2600 | 220 | 329.6 | +0.01 | 848 | 1482 | +10.1 | +10.4 | 0.3 | 8.37 | +0.00 | -5.33 | -5.32 | UNSUPPORTED |
| shift-up | 500/1500/2500 | 90 | 134.8 | -0.15 | 590 | 1681 | +7.3 | +1.9 | 0.6 | 5.71 | +0.00 | -3.14 | -3.13 | PASS |
| shift-up | 500/1500/2500 | 110 | 164.8 | -0.01 | 590 | 1696 | +7.3 | +2.8 | 0.6 | 6.62 | +0.00 | -4.49 | -4.49 | PASS |
| shift-up | 500/1500/2500 | 150 | 224.7 | +0.00 | 910 | 1722 | +65.4 | +4.4 | 0.0 | 8.53 | +0.00 | -4.85 | -4.84 | UNSUPPORTED |
| shift-up | 500/1500/2500 | 220 | 329.6 | +0.03 | 594 | 1735 | +8.0 | +5.1 | 0.9 | 10.20 | +0.00 | -6.11 | -6.11 | PASS |
| shift-down | 700/1220/2600 | 90 | 75.7 | +0.06 | 631 | 1084 | +2.5 | +0.9 | 0.0 | 3.76 | +0.00 | -3.18 | -3.18 | PASS |
| shift-down | 700/1220/2600 | 110 | 92.5 | -0.12 | 640 | 1067 | +3.9 | -0.6 | 0.3 | 4.54 | +0.00 | -4.03 | -4.03 | PASS |
| shift-down | 700/1220/2600 | 150 | 126.1 | -0.01 | 669 | 1077 | +8.6 | +0.3 | 0.0 | 7.70 | +0.00 | -4.49 | -4.49 | UNSUPPORTED |
| shift-down | 700/1220/2600 | 220 | 185.0 | +0.45 | 637 | 1121 | +3.4 | +4.4 | 0.3 | 2.05 | +0.00 | -2.87 | -2.87 | PASS |
| shift-down | 500/1500/2500 | 90 | 75.7 | +0.07 | 444 | 1316 | +0.9 | -0.3 | 0.0 | 3.60 | +0.00 | -3.51 | -3.51 | PASS |
| shift-down | 500/1500/2500 | 110 | 92.5 | -0.24 | 452 | 1332 | +2.8 | +0.9 | 0.6 | 4.35 | +0.00 | -4.82 | -4.82 | PASS |
| shift-down | 500/1500/2500 | 150 | 126.1 | +0.01 | 474 | 1320 | +7.8 | +0.0 | 0.3 | 7.47 | +0.00 | -4.84 | -4.84 | PASS |
| shift-down | 500/1500/2500 | 220 | 185.1 | +0.58 | 425 | 1361 | -3.4 | +3.1 | 0.6 | 2.88 | +0.00 | -4.22 | -4.22 | PASS |
| bright | 700/1220/2600 | 90 | 90.0 | +0.00 | 700 | 1220 | +0.0 | +0.0 | 0.0 | 0.22 | +0.00 | -5.50 | -5.50 | PASS |
| bright | 700/1220/2600 | 110 | 110.0 | +0.00 | 702 | 1220 | +0.3 | +0.0 | 0.3 | 0.21 | +0.00 | -5.58 | -5.57 | PASS |
| bright | 700/1220/2600 | 150 | 150.0 | -0.00 | 700 | 1220 | +0.0 | +0.0 | 0.0 | 0.20 | +0.00 | -5.73 | -5.73 | PASS |
| bright | 700/1220/2600 | 220 | 220.0 | +0.00 | 700 | 1220 | +0.0 | +0.0 | 0.0 | 0.21 | +0.00 | -5.84 | -5.84 | PASS |
| bright | 500/1500/2500 | 90 | 90.0 | +0.00 | 500 | 1500 | +0.0 | +0.0 | 0.0 | 0.22 | +0.00 | -5.22 | -5.22 | PASS |
| bright | 500/1500/2500 | 110 | 110.0 | +0.00 | 498 | 1495 | -0.3 | -0.3 | 0.3 | 0.21 | +0.00 | -5.31 | -5.31 | PASS |
| bright | 500/1500/2500 | 150 | 150.0 | +0.00 | 498 | 1500 | -0.3 | +0.0 | 0.3 | 0.20 | +0.00 | -5.37 | -5.37 | PASS |
| bright | 500/1500/2500 | 220 | 220.0 | +0.00 | 497 | 1495 | -0.6 | -0.3 | 0.6 | 0.20 | +0.00 | -5.53 | -5.52 | PASS |
| deep | 700/1220/2600 | 90 | 71.4 | +0.12 | 610 | 1037 | +2.5 | +0.0 | 0.0 | 7.71 | +0.00 | -2.48 | -2.48 | PASS |
| deep | 700/1220/2600 | 110 | 87.3 | +0.37 | 635 | 1053 | +6.7 | +1.6 | 0.0 | 6.86 | +0.00 | -2.59 | -2.59 | PASS |
| deep | 700/1220/2600 | 150 | 119.1 | +0.01 | 624 | 1018 | +4.8 | -1.9 | 0.0 | 8.24 | +0.00 | -3.84 | -3.84 | PASS |
| deep | 700/1220/2600 | 220 | 174.7 | +0.56 | 624 | 1116 | +4.8 | +7.6 | 0.3 | 6.55 | +0.00 | -2.67 | -2.66 | PASS |
| deep | 500/1500/2500 | 90 | 71.4 | +0.11 | 433 | 1271 | +1.9 | -0.3 | 0.0 | 7.88 | +0.00 | -2.21 | -2.21 | PASS |
| deep | 500/1500/2500 | 110 | 87.3 | +0.70 | 441 | 1283 | +3.8 | +0.6 | 0.0 | 6.58 | -0.00 | -3.04 | -3.04 | PASS |
| deep | 500/1500/2500 | 150 | 119.1 | +0.01 | 416 | 1263 | -2.2 | -0.9 | 0.0 | 9.49 | -0.00 | -4.53 | -4.53 | PASS |
| deep | 500/1500/2500 | 220 | 174.7 | +0.97 | 449 | 1311 | +5.6 | +2.8 | 0.0 | 7.12 | +0.00 | -3.27 | -3.27 | PASS |

Reading the table:

* **F0 is exact everywhere**: worst certified `|error|` is **0.97 c** (D17's synthetic cells: 0.00–0.07 c median). The pitch path is not the weak link.
* **`bright` is transparent to formants** (≤ 0.6 %, residual 0.2 dB — it is the only processed preset whose output is still model-exact, because the tilt is a pure per-bin gain that the ruler's tilt term absorbs).
* **The three unsupported cells** are the high-F0 / low-F1 ones. Their fits land F1 on a neighbouring formant or return a 65 % error; the residual says the corrected spectrum is no longer a 3-resonator shape. They are reported, not hidden.
* Level Δ is **0.000 dB** on all certified synthetic cells: the limiter does not engage at this input level.

## 5. Fixtures: D17 pitch gate

`scripts/metrics.py --renders <dir> --manifest <dir>/manifest.tsv --check` over (12 fixtures + 8 synthetic vowels) × 5 presets = **100 cells**:

| Preset | Fixture cells | Synthetic cells | PASS | FAIL | N/A | worst median c | worst p90 c | octave outliers |
|---|---|---|---|---|---|---|---|---|
| `natural` | 12 | 8 | 20 | 0 | 0 | 0.00 | 0.00 | 0 |
| `shift-up` | 12 | 8 | 19 | **1** | 0 | 2.67 | 28.58 | 0 |
| `shift-down` | 12 | 8 | 17 | 0 | 3 | 4.51 | 13.54 | 0 |
| `bright` | 12 | 8 | 20 | 0 | 0 | 1.74 | 10.68 | 0 |
| `deep` | 12 | 8 | 16 | 0 | 4 | 2.58 | 18.93 | 0 |
| **all** | 60 | 40 | **92** | **1** | **7** | 4.51 | 28.58 | **0** |

The `N/A` cells are the downward presets on the lowest voices: `shift-down`/−3 and `deep`/−4 take `am_onyx` (88 Hz) and `jm_kumo` (86 Hz) below the ruler's 80 Hz floor, and the 90/110 Hz synthetic vowels likewise. D17 says out-of-band is `N/A`, never `PASS`.

**The one FAIL** is `shift-up` on `am_onyx__got-time`: median −0.12 c, p90 **28.58 c** (limit 25), 0 octave outliers. The same clip through the pitch shifter alone (no formant correction) reads p90 **9.9 c**, so the correction pass — not the shifter — contributes the 18.7 c of per-frame spread; at `+7 / 1.25` (outside D1's row) it drops to 21.4 c. This is recorded as an unsupported cell for the lowest male voice at `shift-up`, not as a pass. `docs/qa/round-2.md` should listen for it specifically.

## 6. Fixtures: loudness and true peak

Full chain, 48 preset cells (the 12 fixtures × 4 measured presets; `natural` is the identity). Peak is the sample peak, true peak is **4× oversampled**.

| fixture | preset | source RMS dBFS | out RMS dBFS | level Δ dB | limiter dB | peak dBFS | true peak dBTP |
|---|---|---|---|---|---|---|---|
| af_heart__friends-now | shift-up | -15.88 | -15.88 | +0.00 | +0.00 | -3.26 | -3.24 |
| af_heart__friends-now | shift-down | -15.88 | -15.88 | +0.00 | +0.00 | -1.32 | -1.32 |
| af_heart__friends-now | bright | -15.88 | -16.10 | -0.22 | -2.56 | -1.50 | -1.50 |
| af_heart__friends-now | deep | -15.88 | -15.88 | +0.00 | +0.00 | -1.43 | -1.43 |
| af_aoede__you-got-me | shift-up | -14.61 | -14.61 | +0.00 | +0.00 | -3.09 | -3.07 |
| af_aoede__you-got-me | shift-down | -14.61 | -14.61 | +0.00 | +0.00 | -1.73 | -1.73 |
| af_aoede__you-got-me | bright | -14.61 | -14.77 | -0.16 | -0.99 | -1.50 | -1.50 |
| af_aoede__you-got-me | deep | -14.61 | -14.61 | +0.00 | +0.00 | -1.74 | -1.73 |
| bf_alice__maybe-next-time | shift-up | -19.39 | -19.39 | +0.00 | +0.00 | -5.80 | -5.76 |
| bf_alice__maybe-next-time | shift-down | -19.39 | -19.39 | +0.00 | +0.00 | -2.19 | -2.19 |
| bf_alice__maybe-next-time | bright | -19.39 | -19.43 | -0.04 | -1.15 | -1.50 | -1.50 |
| bf_alice__maybe-next-time | deep | -19.39 | -19.39 | +0.00 | +0.00 | -2.01 | -2.01 |
| am_onyx__got-time | shift-up | -19.77 | -20.00 | -0.23 | -1.81 | -1.50 | -1.50 |
| am_onyx__got-time | shift-down | -19.77 | -19.77 | +0.00 | +0.00 | -1.30 | -1.29 |
| am_onyx__got-time | bright | -19.77 | -19.90 | -0.12 | -1.37 | -1.50 | -1.50 |
| am_onyx__got-time | deep | -19.77 | -19.77 | +0.00 | +0.00 | -2.36 | -2.35 |
| am_eric__good-game | shift-up | -15.18 | -15.18 | +0.00 | +0.00 | -2.12 | -2.12 |
| am_eric__good-game | shift-down | -15.18 | -15.18 | +0.00 | +0.00 | -1.92 | -1.91 |
| am_eric__good-game | bright | -15.18 | -15.42 | -0.24 | -1.50 | -1.50 | -1.50 |
| am_eric__good-game | deep | -15.18 | -15.18 | +0.00 | +0.00 | -1.75 | -1.74 |
| bf_emma__nice-day | shift-up | -16.95 | -16.95 | +0.00 | +0.00 | -2.52 | -2.46 |
| bf_emma__nice-day | shift-down | -16.95 | -16.95 | +0.00 | +0.00 | -1.40 | -1.38 |
| bf_emma__nice-day | bright | -16.95 | -17.07 | -0.12 | -2.80 | -1.50 | -1.50 |
| bf_emma__nice-day | deep | -16.95 | -16.95 | -0.01 | -0.54 | -1.51 | -1.50 |
| zf_xiaoni__greet | shift-up | -15.68 | -15.68 | +0.00 | +0.00 | -2.02 | -1.99 |
| zf_xiaoni__greet | shift-down | -15.68 | -15.68 | +0.00 | +0.00 | -3.66 | -3.66 |
| zf_xiaoni__greet | bright | -15.68 | -15.71 | -0.02 | -0.59 | -1.52 | -1.50 |
| zf_xiaoni__greet | deep | -15.68 | -15.68 | +0.00 | +0.00 | -2.41 | -2.40 |
| zm_yunxi__no-worries | shift-up | -17.24 | -17.24 | -0.00 | +0.00 | -3.53 | -3.50 |
| zm_yunxi__no-worries | shift-down | -17.24 | -17.24 | +0.00 | +0.00 | -1.05 | -1.05 |
| zm_yunxi__no-worries | bright | -17.24 | -17.29 | -0.05 | -1.10 | -1.50 | -1.50 |
| zm_yunxi__no-worries | deep | -17.24 | -17.24 | +0.00 | +0.00 | -2.58 | -2.57 |
| jf_nezumi__be-friends | shift-up | -15.80 | -15.80 | +0.00 | +0.00 | -2.68 | -2.68 |
| jf_nezumi__be-friends | shift-down | -15.80 | -15.80 | +0.00 | +0.00 | -1.04 | -1.04 |
| jf_nezumi__be-friends | bright | -15.80 | -15.80 | +0.00 | +0.00 | -1.22 | -1.22 |
| jf_nezumi__be-friends | deep | -15.80 | -15.80 | +0.00 | +0.00 | -1.69 | -1.69 |
| jm_kumo__watch-this | shift-up | -20.66 | -20.66 | +0.00 | +0.00 | -5.17 | -5.15 |
| jm_kumo__watch-this | shift-down | -20.66 | -20.66 | -0.00 | +0.00 | -2.71 | -2.71 |
| jm_kumo__watch-this | bright | -20.66 | -20.81 | -0.15 | -1.37 | -1.50 | -1.50 |
| jm_kumo__watch-this | deep | -20.66 | -20.66 | +0.00 | +0.00 | -4.70 | -4.70 |
| hf_alpha__good-to-see | shift-up | -15.08 | -15.12 | -0.04 | -0.72 | -1.50 | -1.50 |
| hf_alpha__good-to-see | shift-down | -15.08 | -15.08 | +0.00 | +0.00 | -1.94 | -1.94 |
| hf_alpha__good-to-see | bright | -15.08 | -15.08 | +0.00 | +0.00 | -1.02 | -1.02 |
| hf_alpha__good-to-see | deep | -15.08 | -15.08 | +0.00 | +0.00 | -1.95 | -1.95 |
| ef_dora__see-you | shift-up | -19.49 | -19.49 | +0.00 | +0.00 | -3.31 | -3.31 |
| ef_dora__see-you | shift-down | -19.49 | -19.52 | -0.02 | -0.71 | -1.50 | -1.50 |
| ef_dora__see-you | bright | -19.49 | -19.52 | -0.03 | -0.94 | -1.55 | -1.50 |
| ef_dora__see-you | deep | -19.49 | -19.49 | +0.00 | +0.00 | -1.42 | -1.42 |

* **A/B level jump: worst 0.244 dB** (target < 1 dB). 34 of 48 cells are exact 0.00 dB (the limiter is a no-op and the RMS match is the only gain).
* **True peak: worst −1.016 dBTP**, i.e. never above −1 dBTP. Independently re-measured with SciPy's `resample_poly(..., 4, 1)` on all 60 fixture renders (including `natural`): worst **−1.016 dBTP**, all ≤ −1 dBTP.
* The limiter engaged on **14 of 48** cells; the largest gain reduction is **−2.8 dB** (`bright` on `bf_emma__nice-day`, a clip whose crest rises when the source tilt is flattened). `34/48` cells are bit-exact no-ops.

## 7. Loudness policy (P-08)

### 7.1 What is matched, and why whole-clip

The processed clip's **whole-clip RMS** is matched to the source clip's:

```
RMS = sqrt((1/N) · Σ x[n]²)     gain = RMS_source / RMS_processed
```

This is deliberately not LUFS and not a 3 s short-term window. The product renders **0.4–2.2 s one-shots**; BS.1770 needs 400 ms blocks with 100 ms hops and a relative gate, so a 0.4 s clip has one or two blocks and the relative gate is ill-defined — the number degenerates into a 400 ms short-term loudness that moves with the phrase. A 3 s short-term window is longer than every clip in the corpus. Whole-clip RMS is stable, deterministic, and exactly what the A/B comparison hears.

It is fair at these lengths because the transform **preserves length and silence**: source and render contain the same samples and the same head/tail silence, so the 11.7 % of the corpus that is silence contributes zero to both sides. K-weighting was rejected because it would make the measured level depend on the very spectral change a preset exists to make.

### 7.2 True-peak limiter

* **4× oversampled true peak**, not sample peak. A 12 kHz sine at 45° has every sample at −3.01 dBFS and a true peak of 0 dBFS; the test pins that a sample-peak meter misses it and this one does not.
* **Ceiling −1 dBTP**, with a **0.5 dB internal margin**, so the limiter targets −1.5 dBTP and the published gate has room for estimator/interpolation error. If a fast gain change leaves an interpolated residual, one **global trim** removes it exactly (a scalar cannot distort), and the measured output is then −1.5 dBTP.
* **Transparent below threshold.** If the post-RMS-match true peak is at or below −1 dBTP, the stage returns the signal **bit-exactly** and reports `limited = false`. No gain envelope is computed.
* **No pumping.** Gain reduction is pre-computed with a backward linear-attack pass (5 ms to traverse full scale) and a forward linear-release pass (100 ms); each sample's gain is the minimum of its own requirement and a slow rise, so the envelope cannot overshoot the requirement and one plosive cannot duck the next syllable. `stage_releases_between_bursts_without_pumping` measures the recovery in a quiet gap and the steadiness inside a burst.

The stage is `rasp_dsp::loudness::LoudnessStage` and is **reusable by D8**: give it the original clip as `source` and the robot output as `signal`, and the robot chain lands on the same policy. It is exposed here (`NaturalRenderer::loudness`) and deliberately **not wired** into a robot chain yet.

`LoudnessReport` states all of it per render: source/processed/output RMS in dBFS, the RMS gain applied, the A/B level delta, pre-limit and output true peak in dBTP, output sample peak, the minimum limiter gain (linear and dB), and whether the limiter acted.

## 8. Unsupported cells, and why they are unsupported

| Preset | Cell | Measured | Why |
|---|---|---|---|
| `shift-up` | 700/1220/2600 @ 220 Hz | F1 +10.1 %, F2 +10.4 %, residual 8.4 dB | output F0 330 Hz; F1 target 770 Hz is 2.3 harmonics up, and the correction must move formants *down* against a 1.5× excitation. The corrected spectrum is not model-exact, and the fit splits the difference between harmonics. |
| `shift-up` | 500/1500/2500 @ 150 Hz | F1 +65.4 %, residual 8.5 dB | output F0 225 Hz; F1 target 550 Hz is 2.4 harmonics up. The fit locks F1 onto the second formant (F1 = 910 Hz). A degenerate fit, not a usable number. |
| `shift-down` | 700/1220/2600 @ 150 Hz | F1 +8.6 %, residual 7.7 dB | output F0 126 Hz, F1 target 616 Hz sits between harmonics 4 and 5. The measured F1 is 8.2–8.6 % high for every formant ratio in 0.86–0.89; this is the ruler's conditioning limit at that F0, not a knob setting. |
| `shift-up` | `am_onyx__got-time`, D17 | median −0.12 c, p90 28.6 c | the correction pass adds per-frame spread (pitch-only: 9.9 c). The only upward point that clears it (`+7/1.25`) is outside D1's measured formant row. |

No upward operating point clears all eight synthetic cells. Scanned at `+7` with formant 1.05, 1.10, 1.15, 1.20, 1.25 and 1.33, and at `+5`/`+6` with 1.10–1.33: every one fails at least the high-F0 / 500 Hz-F1 cells, and moving the pitch down does not help because the limit follows the **output** F0 (a `+2 st` shift on a 220 Hz voice already leaves F1 at 2.6 harmonics). The cause is harmonic resolution in the correction pass, the same physical limit D1 identified for its failing corners — not a tolerance to widen.

**What would change the answer:** a correction rule that moves formants *down* in the output domain without relying on per-bin energy that the shifted excitation does not have (e.g. a time-mapped envelope with resynthesis rather than a per-frame gain), or accepting a narrower validated region for high-F0 voices. Both are DSP decisions above P-05's pay grade: they are reported here, not silently worked around.

## 9. One P-03 repair that P-05's measurements forced

The D17 gate exposed a defect in P-03's envelope that its own acceptance could not see, because P-03 measured 90/110 Hz vowels only.

The cepstral lifter was a fixed **4 ms**. The cepstral peak of a periodic signal sits at quefrency `1/F0`; at output F0 above ~250 Hz that period is shorter than 4 ms, so the pitch pulse enters the "envelope", and the correction chases harmonics instead of the vocal-tract shape. Measured on high female fixtures at `+7 st`: **HNR fell 4.7–8.9 dB** and the D17 ruler lost most of its voiced frames (af_heart: 8 paired frames, p90 1207 c). A pitch-only render of the same clips passes at p90 9.9 c, which isolates the correction.

Fix: cap the lifter at `0.75 / F0` of the signal the correction analyses (`LIFTER_PERIOD_FRACTION`). For output F0 ≤ 187 Hz the cap is above 4 ms and **nothing changes**; above it the envelope smooths with F0 (3 ms at 250 Hz, 2.3 ms at 330 Hz). Consequences, all measured:

* **P-03's formant table is unchanged** (its cells are 90/110 Hz inputs, output F0 ≤ 165 Hz; re-ran the table, numbers identical).
* D17 over the preset renders: the lifter fix took failures from **14 to 7**; moving `shift-down` and `deep` off the deepest pitch took them to **1**. Octave outliers **30 → 0**, worst p90 **1207 → 28.6 c**, worst median **21.8 → 4.5 c**.
* HNR on the affected fixtures **+3.4…+5.2 dB** relative to the 4 ms render.

This is a P-03-owned change made under P-05 because the preset measurement could not be honest without it. It is called out here for VERIFY and for the PM's decision log.

## 10. Reproduce

```sh
# unit + contract tests (debug, no fixtures needed; skips cleanly without them)
cargo test -p rasp-dsp

# the two measurement tables (release; ~5 s)
cargo test -p rasp-dsp --release preset_measurement_table -- --ignored --nocapture
cargo test -p rasp-dsp --release preset_fixture_table      -- --ignored --nocapture

# the official D17 ruler over every preset × fixture × synthetic cell
cargo test -p rasp-dsp --release write_preset_renders -- --ignored --nocapture
scripts/.venv/bin/python scripts/metrics.py \
  --renders "${TMPDIR:-/tmp}/rasp-p05-renders" \
  --manifest "${TMPDIR:-/tmp}/rasp-p05-renders/manifest.tsv" --check

# native CPU
cargo test -p rasp-dsp --release preset_cpu_cost -- --ignored --nocapture
```

The heavy measurements are `#[ignore]`d so a debug `cargo test` stays under the 60 s budget.

## 11. CPU (native, release, 1 s vowel)

| Path | ms per second of audio | × realtime |
|---|---|---|
| `natural` (bit-exact bypass) | 3.4 | 290 |
| `bright` (tilt only) | 10.4 | 96 |
| `shift-down` | 32.4 | 31 |
| `deep` | 31.7 | 32 |
| `shift-up` | 36.5 | 27 |
| `shift-up`, transform only | 33.4 | 30 |
| loudness stage only | 3.3 | 306 |

P-08 adds **≈ 3.2 ms/s** (~10 % of a shifted render, 306× realtime on its own): one 4× interpolated envelope pass plus, when the limiter engages, two linear envelope passes. P-05's tilt costs one `log2` per bin and one vector add per frame when a shift is already running; on its own (`bright`) it pays for one correction STFT/ISTFT because the tilt is applied in that domain. The dominant cost is still P-03's correction pass, as it was before.

## 12. Limits of this evidence

* **No perceptual claim.** "Smaller", "larger" and "brighter" are physical intent, not measured perception. HR-2 judges whether the presets sound like the same person.
* **Synthetic vowels are stationary.** They isolate pitch/formant behaviour with known ground truth; they do not exercise transients, glides or codec noise. P-04's transient numbers and the fixture D17 matrix cover those, and the fit residual column is the honest warning that a stationary synthetic vowel is already outside the 3-resonator model after correction.
* **The formant numbers are the P-03 ruler's.** It is exact on its analytic control and it reports where it is not: the three unsupported cells and the residual column. No fixture formant is published (D20).
* **The 150 Hz cells sit at the ruler's conditioning limit**, even where they pass: `shift-up` 500/90 and 500/110 read +7.3 % with a 5.7–6.6 dB residual. Treat "passes by 0.7 %" as "inside the gate with little margin", not as a comfortable result.
* **`shift-up`'s one D17 failure is a product caveat**: the lowest male voice (`am_onyx`, 88 Hz) at +7 st is outside the pitch gate by 3.6 c of p90. The other 11 fixtures pass.
