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
* `--trim` is **off** by default and, when on, trims before rendering. The
  render preserves the (trimmed) length, so a trimmed render is shorter than the
  source by design — see `crates/rasp-dsp/src/endpoints.rs`. Compare it with the
  trimmed `natural` render, not with the untrimmed source; with `metrics.py`
  against the source, pass `--allow-length-delta <removed samples>`.

## Output contract

`--json` prints one object on **stdout**; the human summary goes to **stderr**.
Without `--json` the human report is on stdout. Exit codes: `0` success, `1`
work failed, `2` bad command line. Every failure is one line, no panics.

Per render: `file, fixture, language, preset, params, format, loudness, trim,
sampleRate, channels, sourceSamples, samplesIn, samplesOut, durationS, bytes,
peakDbfs, truePeakDbtp, sourceRmsDbfs, outputRmsDbfs, levelDeltaDb,
limiterMinGainDb, decodeMs, renderMs, writeMs, totalMs, msPerAudioSecond`.

`batch` writes `manifest.tsv` with:

```
fixture  step  cents  ratio  preset  pitch_st  formant  tilt_db_per_oct
file     samples  peak_dbfs  true_peak_dbtp  level_delta_db  language
```

`ratio` is the **pitch** ratio (`metrics.py`'s meaning); the formant ratio is
`formant`. Timings are deliberately absent, so the manifest and every render are
reproducible byte for byte.

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
