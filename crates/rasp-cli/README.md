# `rasp-cli` — the `rasp` command

PLAN **P-06**. The offline renderer that makes the DSP usable: listening files
for HR-2, regression renders for Q-03, and the `--renders` input for
`scripts/metrics.py`.

```sh
cargo build -p rasp-cli --release     # -> target/release/rasp
./target/release/rasp --help
```

No external dependencies: `rasp-dsp` and `rasp-opus` are in-workspace crates,
and argument parsing plus the fixture-manifest JSON reader are hand-rolled
(`args.rs`, `json.rs`).

## Commands

| Command | Purpose |
|---|---|
| `rasp render` | one clip through the natural chain (preset and/or explicit knobs) |
| `rasp batch` | fixtures × presets → renders + `manifest.tsv` for `metrics.py` |
| `rasp decode` | `.weba` → `.f32`/`.wav`, no DSP — the reference path |
| `rasp info` | frames, rate, channels, duration, peak, true peak, endpoint head/tail |

`rasp <command> --help` carries the full option list and examples. The important
behaviour:

* Input is **48 kHz mono** `.weba`, raw little-endian `.f32`, or `.wav`. Other
  rates/channel counts are rejected, not resampled (the corpus is 48 kHz mono).
* `--format wav16` (listening default) / `wavf32` / `f32` (batch default, and
  what `metrics.py` compares).
* `--preset natural|shift-up|shift-down|bright|deep` fills all three values;
  `--pitch/--formant/--tilt` override individually. Out-of-range values are
  rejected (exit 2) rather than silently clamped.
* `--loudness on` (default) is the P-08 policy: whole-clip RMS match, then the
  −1 dBTP true-peak limiter. `off` is the raw transform, for measurement.
* `--headroom on` (default) is D22: `effective = nominal × w`, with `w` from the
  clip's measured median F0 against the corpus envelope (79–323 Hz, reference
  178 Hz). Pitch, formant log-ratio and tilt all scale, so a voice with no room
  gets no tract change. `off` reproduces the pre-D22 fixed presets for A/B and
  for the nominal D17 matrix. `natural`/`bright` are unaffected (no pitch
  direction → `w = 1`).
* `--trim` is **off** by default and, when on, trims before rendering. The
  render preserves the (trimmed) length, so a trimmed render is shorter than the
  source by design — see `crates/rasp-dsp/src/endpoints.rs`. Compare it with the
  trimmed `natural` render, not with the untrimmed source; with `metrics.py`
  against the source, pass `--allow-length-delta <removed samples>`.

## Output contract

`--json` prints one object on **stdout**; the human summary goes to **stderr**.
Without `--json` the human report is on stdout. Exit codes: `0` success, `1`
work failed, `2` bad command line. Every failure is one line, no panics.

Per render: `file, fixture, language, preset, params, headroom, format, loudness,
trim, sampleRate, channels, sourceSamples, samplesIn, samplesOut, durationS,
bytes, peakDbfs, truePeakDbtp, sourceRmsDbfs, outputRmsDbfs, levelDeltaDb,
limiterMinGainDb, decodeMs, renderMs, writeMs, totalMs, msPerAudioSecond`.

`params` is the **nominal** point (the preset / knob values). `headroom` is the
D22 mapping actually applied:

```json
"headroom": {
  "mode": "on", "weight": 0.0788, "measuredF0Hz": 84.223,
  "effectivePitchSt": -0.3152, "effectiveFormantRatio": 0.9873,
  "effectiveTiltDbPerOct": 0.0
}
```

`measuredF0Hz` is `null` when the clip had no voiced frames (the weight is then
`1.0`, the documented fallback) or when the mode is `off`. `weight: 0` means the
preset is a bit-exact bypass — the UI should say "no headroom", not "no change".

`batch` writes `manifest.tsv` with:

```
fixture  step  cents  ratio  preset  pitch_st  formant  tilt_db_per_oct
nominal_pitch_st  nominal_formant  weight  f0_hz
file     samples  peak_dbfs  true_peak_dbtp  level_delta_db  language
```

`step`/`cents`/`ratio`/`pitch_st`/`formant` are the **effective** values the DSP
applied, so `metrics.py` scores the render against the shift it actually got;
`nominal_*` is the preset point and `weight` is D22's `w`. With `--headroom off`
the two pairs are identical. Timings are deliberately absent, so the manifest
and every render are reproducible byte for byte.

Batch renders sequentially in manifest order. Parallelism was rejected: it would
buy wall-clock and risk nondeterministic output, and 60 renders already take
under 2 s.

## Measured

Release build, 2026-10-28, macOS (M-series), 0.990 s fixture, median of 5 runs,
including decode and write:

| preset | total ms | ms per audio second |
|---|---|---|
| `natural` | 3.4 | 3.4 |
| `bright` | 14.1 | 14.3 |
| `shift-down` | 38.5 | 38.8 |
| `deep` | 38.1 | 38.5 |
| `shift-up` | 48.9 | 49.4 |
| `shift-up`, `--headroom off` | 48.9 | 49.4 |

All under the 50 ms gate for a 1 s clip; `shift-up` is the tight one (P-05's
warm DSP-only number is 36.1 ms/s, and the corpus's transients plus one-shot
chain construction account for the rest). The 60-render batch averages
**27.5 ms/s** because chain setup and caches amortise across files.

## Listening pack for HR-2

```sh
cargo build -p rasp-cli --release
./target/release/rasp batch --all --presets all --format wav16 \
    --out-dir "$HOME/rasp-hr2-pack"
```

That is 12 fixtures × 5 presets = 60 WAVs named
`<voice>__<phrase>.<preset>.wav`, plus `manifest.tsv`. For a full-corpus pass,
point `--manifest` at a manifest of that corpus; `--voice`/`--phrase` narrow it.
