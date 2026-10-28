# Testing

How the test suite is laid out, what it may assume, and how to get a machine
into a state where the audio tests actually run. The rule that matters most:
**audio fixtures are generated, never committed**. A test that needs them must
pass cleanly when they are absent and say how to make them.

Canonical task definitions live in [PLAN.md](../PLAN.md); the corpus numbers are
in [docs/corpus.md](corpus.md); setup is in [docs/setup.md](setup.md).

## 1. Test tiers

| tier | needs | must do when the input is missing |
|---|---|---|
| unit / property | nothing | — |
| fixture-based (numeric) | `fixtures/manifest.json` + ffmpeg to regenerate | **skip, exit 0**, print the fix command to stderr |
| corpus-wide (1404 clips) | `web/audio/` committed | skip if the corpus is not there |
| browser / perceptual | a running app, a human | not part of `cargo test` |

There is no test that silently pretends to have run. A skip prints its reason
and the command that removes the skip, so a green CI log still says whether the
audio path was exercised.

## 2. Where fixtures live

Everything is under `fixtures/` at the repo root, and that whole directory is
gitignored except `fixtures/README.md` and the `.gitkeep` placeholder:

```
fixtures/wav/<voice>__<phrase>.wav         48 kHz mono 16-bit PCM
fixtures/f32/<voice>__<phrase>.f32         48 kHz mono raw little-endian f32
fixtures/reference/<voice>__<phrase>.f32   same, decoded by libopus
fixtures/manifest.json                     provenance + hashes + sample counts
```

12 clips, one per voice from the D13 set, 646,948 samples, 13.48 s in total,
≈ 6.2 MiB on disk. `<voice>__<phrase>` is a double underscore because both ids
already contain single underscores. `fixtures/README.md` has the clip table and
the full manifest contract; `scripts/make_fixtures.py` is the generator and the
source of truth for which clips were chosen and why.

Clips are decoded whole — no trimming, no fades, no normalisation. Trimming and
fading are the thing under test (P-04), not a preprocessing step.

### `reference/` is the oracle; `f32/` is not

`fixtures/f32/` comes from FFmpeg's **native** Opus decoder. `fixtures/reference/`
comes from **libopus** (`ffmpeg -c:a libopus -i`). They are different waveforms
from the same `.weba` — 14.5–31.9 dB apart, with no constant delay, so alignment
cannot rescue a comparison.

- Compare a Rust decoder's output against `reference/`. Target from D-02:
  sample count identical, SNR > 60 dB.
- `f32/` is for things that do not care which decoder produced the samples:
  levels, envelopes, F0, trim/endpoint tests, listening.
- Never write a test that asserts `f32/` and `reference/` agree.

## 3. Make them

Prerequisites: `python3` (3.8+, standard library only) and `ffmpeg` built with
libopus (macOS: `brew install ffmpeg`; Debian/Ubuntu: `apt-get install ffmpeg`).
The script checks for the `libopus` decoder up front and fails with an install
hint rather than producing a non-oracle reference tree.

```sh
python3 scripts/make_fixtures.py           # build what is missing, reuse the rest
python3 scripts/make_fixtures.py --force   # rebuild all 36 files
```

It reads `web/registry.json`, verifies the `sha256` and byte count of every
source clip, then decodes with `ffmpeg`. The twelve source clips must be
vendored; a missing or modified `.weba` is fatal and the message names the
repair command. Nothing is downloaded.

Second run: nothing decoded, nothing written, `0 built · 12 cached`, exit 0.
It still re-hashes the 36 outputs and asks ffmpeg for its version — that is what
makes the cache trustworthy. The manifest records the source hash, the
ffmpeg+libopus signature and the hash of every generated file, so a stale cache
is detected three ways: changed source, changed decoder, or a modified/deleted
output. A different ffmpeg version rebuilds everything, because the reference
bytes are decoder-specific.

Output is one table plus a summary:

```
voice      phrase           group          sec      bytes  sha256        state
af_heart   friends-now      friendship   0.990    190,112  96d9b8e842ad  cached
...
12 clips · 646,948 samples · 13.48 s · 6,470,008 bytes · en/es/hi/ja/zh · groups ...
decoder ffmpeg 9.0.2 + libopus · registry build 3e81bd1f069b
0 built · 12 cached
```

`bytes` is the f32 payload size; the `sha256` column is the first 12 hex digits
of the f32 digest. The full hashes are in the manifest.

## 4. The contract for Rust tests

Any test that reads a fixture **must**:

1. Locate `fixtures/` from `CARGO_MANIFEST_DIR` (`../../fixtures` for a crate in
   `crates/<name>`) or take it from a helper. Never hard-code an absolute path.
2. Treat `fixtures/manifest.json` as the switch. Absent → skip, exit 0.
3. Print one line to stderr on skip, including the command that fixes it.
4. Read `sampleCount` and per-file `sha256` from the manifest, not by listing
   the directory, so a half-deleted or stale tree fails loudly instead of
   producing wrong numbers.

```rust
use std::path::{Path, PathBuf};

/// The fixture root, or `None` when the fixtures have not been generated.
fn fixtures_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    dir.join("manifest.json").is_file().then_some(dir)
}

#[test]
fn decoded_pcm_matches_libopus() {
    let Some(dir) = fixtures_dir() else {
        eprintln!(
            "skipping: fixtures/manifest.json is missing - \
             run `python3 scripts/make_fixtures.py` to generate it"
        );
        return;
    };
    // ... read manifest.json, then fixtures/<clip>/reference/*.f32
}
```

Notes that keep this honest:

- A skipped test is a **pass**. That is deliberate: `cargo test --workspace`
  must be green on a checkout with no audio. A CI job that wants the numbers
  runs `scripts/make_fixtures.py` first (see `fixtures-smoke` in
  `.github/workflows/ci.yml`), so a broken generator shows up as a red job
  rather than as 12 silent skips.
- `eprintln!` output only reaches the terminal under `cargo test -- --nocapture`,
  so a plain run shows a pass either way. That is the intended behaviour; the
  skip line is for the person debugging, not for the suite.
- Fixtures are pinned. If `files[].sha256` no longer matches a file, fail — do
  not re-baseline an expectation to make a test pass. Regenerate with
  `--force` and, if the corpus or ffmpeg changed, treat the numeric change as a
  deliberate re-baseline and say so in the commit.
- Tests must not write into `fixtures/`. Render output goes to `target/` or a
  `tempfile::tempdir()`.
- Do not add a test that only checks that fixtures exist — that is what the CI
  job is for.

Corpus-wide tests (D-02's 1404-clip pass, D-04) use `web/audio/` directly and
skip when it is absent, with the same stderr convention. They do not need
`fixtures/`.

## 5. Fresh checkout

```sh
git clone <repo> && cd rasp-livive-net

# 1. the committed corpus is intact (offline, ~0.1 s)
python3 scripts/vendor_corpus.py --check

# 2. generate the fixtures (needs ffmpeg with libopus)
python3 scripts/make_fixtures.py

# 3. the manifest is the contract for the Rust tests
python3 -c "import json;d=json.load(open('fixtures/manifest.json'));print(len(d['clips']))"
# 12

# 4. the gates
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Step 2 is optional for a plain `cargo test`: without it the fixture tests skip.
Step 3 is the cheap way to prove the fixture set really was built.

## 6. CI

The `fixtures-smoke` job in `.github/workflows/ci.yml` generates the fixtures
and then asserts the manifest lists 12 clips. The essential commands:

```sh
python3 scripts/make_fixtures.py
python3 - <<'PY'
import json
d = json.load(open("fixtures/manifest.json"))
assert len(d["clips"]) == 12, len(d["clips"])
PY
```

It uses the vendored corpus and `ubuntu-latest`'s ffmpeg (which is built with
libopus); nothing is downloaded and no cache is restored, so the job proves the
generator works from a clean checkout. `corpus-integrity` runs
`python3 scripts/vendor_corpus.py --check` in parallel.

If a Rust job later needs fixtures for numeric assertions, it must generate
them in the same job before `cargo test` — a cached `fixtures/` directory is
acceptable only when the cache key includes `web/registry.json` and
`scripts/make_fixtures.py`. Without that, a stale cache would silently change
what the tests measure.

## 7. When it breaks

| symptom | cause | fix |
|---|---|---|
| `missing corpus clip ...` | the vendored `.weba` is gone | `python3 scripts/vendor_corpus.py`, then `--check` |
| `sha256 mismatch for web/audio/...` | local corruption, or a deliberate re-bake | vendor script; if upstream rebaked, re-vendor with `--refresh-registry` |
| `this ffmpeg has no libopus decoder` | ffmpeg built without `--enable-libopus` | install a build with it; never swap in the native decoder |
| `decoders disagree on sample count` | ffmpeg not honouring `pre_skip`/`DiscardPadding` | check the ffmpeg build; do not ship misaligned fixtures |
| everything rebuilds on every run | ffmpeg version changed (the signature differs) | expected; the reference bytes are decoder-specific |
| fixture test skipped in CI | `fixtures-smoke` did not run before the test job | generate fixtures in that job, do not ignore the skip |

## 8. Do not

- Commit anything under `fixtures/` except `.gitkeep` and `README.md` (PLAN §8).
- Point a test at `fixtures/f32/` and call it a conformance check.
- Compare waveforms without checking `sampleCount` first.
- Resample or trim the fixture inputs inside a test harness.
- Write absolute paths into scripts, manifests or docs.

## 9. Metrics harness (P-07)

`scripts/metrics.py` is the one command that turns two audio files into a
comparison table. `scripts/audio_metrics.py` holds the measurement primitives;
`scripts/measure_corpus.py` imports the same functions for `docs/corpus.md`, so
there is no second F0/LUFS/centroid implementation to drift.

### 9.1 Run it

```sh
# bootstrap once (numpy 1.26.4 / scipy 1.13.1, pinned in requirements.txt)
python3 -m venv scripts/.venv
scripts/.venv/bin/pip install -r scripts/requirements.txt

# identity / bypass: 0 cents, 0% formant delta, null <= -100 dB
scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \
                   --test fixtures/f32/af_heart__friends-now.f32 --check

# actual decoder difference (reference = libopus, f32 = ffmpeg native)
scripts/metrics.py --ref fixtures/reference/af_heart__friends-now.f32 \
                   --test fixtures/f32/af_heart__friends-now.f32

# a render: requested +4 st and a 1.10 formant ratio
scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \
                   --test out.wav --pitch-st 4 --formant-ratio 1.10 --check

# all 12 fixtures
scripts/metrics.py --manifest --check
```

Running `scripts/metrics.py` directly is supported: it re-execs into
`scripts/.venv` when that venv exists and the current interpreter is not it.
`--help` repeats the examples and the target list.

### 9.2 What each number means

| field | meaning | target |
|---|---|---|
| samples, duration | decoded length; `.f32` is headerless f32le and **48 kHz is assumed** | equal to the reference |
| F0 median | YIN over 40 ms frames / 10 ms hop, 60–400 Hz, 5-frame median smoothing | within **±5 cents** of the requested shift |
| F1/F2/F3 | median of order-18 LPC spectral-envelope peaks, 25 ms frames at 8 kHz | F1/F2 within **8%** of the requested ratio; F3 shown only |
| centroid | median spectral centroid over 25 ms Hann frames above −55 dBFS | descriptive; no project target |
| HNR | `10*log10(r/(1-r))` from the autocorrelation at the detected lag | descriptive; compare voices, not an absolute figure |
| LUFS-I | BS.1770-4 K-weighting, 400 ms blocks / 100 ms hop, −70 / −10 LU gating | P-08's A/B level jump < 1 dB |
| peak / RMS / DC / clipped | whole-clip level, sample mean, `\|x\| >= 0.999` count | sanity flags; true-peak policy is P-08 |
| null residual | `20*log10(RMS(ref-test) / RMS(ref))`, more negative is better | **≤ −100 dB** for identity or `--expect-null` |

The comparison table also shows the per-sample maximum residual in the JSON
(`null.max_residual_db`), not in the human table.

### 9.3 PASS/FAIL rules

- **Always gated:** both clips finite (no NaN/Inf) and equal sample count.
- **Pitch gated** when `--pitch-st N` is given. When `--ref` and `--test` are
  the same bytes, identity implies 0 cents and is gated automatically.
- **Formants gated** when `--formant-ratio R` is given; identity implies 1.0.
  Only F1/F2 are gated, per PLAN §8.
- **Null gated** for identical bytes or `--expect-null`. Two different Opus
  decoders are *expected* to differ, so their residual is reported, not failed
  (the current `af_heart` pair sits at −13.6 dB while agreeing to −0.2 cents
  on F0).
- `--check` exits **1** when an applicable check fails and **0** otherwise.
  Without `--check` the verdict is still printed but the exit status is 0.
  Usage errors and unreadable/wrong-format input exit **2**.
- `--json` emits exactly one JSON object on stdout (no table). Non-finite
  values become `null`, so the output is valid JSON for CI:

  ```sh
  scripts/metrics.py --manifest --json --check \
    | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['ok']"
  ```

### 9.4 Fixture self-check (`--manifest`)

`--manifest` reads `fixtures/manifest.json` and compares each clip's `f32`
(ffmpeg native decoder) against its `reference` (libopus). Those are two
different decoders; the check is deliberately not a waveform match. Per clip
it requires:

1. identical sample count,
2. no NaN/Inf,
3. a nonzero, finite SNR (the files must actually differ).

All 12 must pass; the current build prints median SNR ≈ 14.7 dB. That SNR is
`-null.residual_db` on the raw, unaligned waveforms, so it is not the same
statistic as the "14.5–31.9 dB apart" figure in §2; it exists to prove the two
decoder outputs really differ. A missing manifest is exit 2 with the
`python3 scripts/make_fixtures.py` command, because this mode is an explicit
request, not a test that may silently skip.

### 9.5 Goldens are metric bounds, not hashes

**`fixtures/f32/` and `fixtures/reference/` bytes are decoder-build-dependent.**
`f32/` moves with the FFmpeg native Opus decoder; `reference/` moves with the
libopus version; `scripts/make_fixtures.py` stores the decoder signature and
rebuilds everything when it changes. The same applies to the 32 kbps source
Opus: it is the input, not something this repo controls.

Therefore Q-03's golden renders, and any CI assertion about audio output, must
be **bounds on these metrics** — F0 within ±5 cents, F1/F2 within 8%, null
≤ −100 dB for identity, identical sample count, no NaN, level within the P-08
window — and never a `sha256` of a `.f32`/`.wav` fixture or of a render that is
compared against one. A hash golden would fail on a legitimate FFmpeg upgrade
while proving nothing about the DSP. Hash the *scripts* and the manifest
contract if provenance is needed, not the PCM.

### 9.6 Limits

- YIN can still make octave errors below HNR ≈ 8 dB; use medians only, which
  is why Q-01 is constrained to median F0.
- LPC formants on 32 kbps Opus are noisy. Use them to compare two renders of
  the same clip (the 8% gate is a ratio); do not read them as clinical
  formants.
- Everything assumes **48 kHz mono**. WAV at another rate or with more than
  one channel is rejected with a resample hint instead of being silently
  measured. `.f32` has no header, so a wrong-rate file cannot be detected.
- LUFS-I needs at least one 400 ms block; shorter clips report NaN. Digital
  silence reports −inf in the table and `null` in JSON.


### 9.7 Paired per-frame pitch (P-07b) — this supersedes the "F0 median" row in §9.2

The pitch check no longer compares two whole-clip medians. It frames the
reference and the render on the **same 40 ms grid with 50% overlap**, estimates
F0 independently per frame, keeps only frames **voiced in both** signals, and
reports the median/IQR/p10/p90 of `1200*log2(test/ref)` in cents plus the paired
frame count. The frame-level estimator is the existing YIN tracker with a wider
50–600 Hz band and a spectral-peak cross-check: a frame is dropped when no
harmonic peak confirms YIN's octave, which also removes digital silence (it has
no peaks). A render is expected to be **time-aligned**; the check does not
search for a delay or warp the grid. A renderer latency is a separate defect,
not a pitch error, and must be reported as one. The same tracker
defaults (60–400 Hz, 10 ms hop, no refinement) are unchanged for
`docs/corpus.md` and Q-01, so regenerating that document is still
byte-identical.

**Why median-vs-median was wrong.** A pitch-shifted clip is not a different
speech act: the frames that survive a voicing test are chosen independently on
each side, and a resampler or vocoder changes which frames pass and where the
F0 contour lands. On a mathematically exact `resample_poly` control the old
check reports tens to hundreds of cents — −20…+38 c on small shifts, ±1200 c
when the shifted F0 leaves the 60–400 Hz band — for a process whose ratio is
exact by construction. It failed a perfect result and would have hidden a
broken one. The paired ratio removes the frame-selection degree of freedom:
each frame is compared with the frame at the same time in the other signal.

**`N/A` means the ruler cannot reach, not that the DSP failed.** The check is
`FAIL` only when the paired median misses the request by more than 5 cents in a
cell the tracker can certify. A cell is `N/A` (never `PASS`, never `FAIL`) when
any of these holds, and the reason is printed and put in JSON under
`metrics.pitch.naReason` / `cells[].naReason`:

- the expected output F0 is below **80 Hz** or above **550 Hz** (the reliable
  band for a 40 ms window inside the 50–600 Hz tracker);
- fewer than 4 frames are voiced in both signals;
- no expected shift was supplied (e.g. the D-02 decoder pair with no
  `--pitch-st`; the paired number is still shown, descriptively).

So `am_onyx` at −12 st (≈41 Hz) is `N/A`, not a failure; `af_heart` at +12 st
(≈458 Hz) is in band and gated.

**Run the control (`--self-test`).** It loads the 12 fixtures, builds an exact
`scipy.signal.resample_poly` control at every semitone −12…+12, and asserts the
harness reports **≤ 1 cent** in every in-band cell. Two controls are used:

| control | construction | measured on |
|---|---|---|
| `sameTime` | stationary harmonic at the fixture's median F0, resampled | the normal same-time 40 ms grid (the render path) |
| `resample` | the fixture itself, resampled exactly | the resampler's own time base with window, hop and lag band scaled by the ratio |

The second control is the one that would have caught the old bug: the current
build reports a worst **0.66 c** paired error, while the legacy median method on
the same controls is off by up to **1249 c** (228 of 279 in-band cells exceed
5 c). That legacy column is printed as `legacy max c` for exactly this reason.
`--self-test` always exits non-zero on a regression (it implies `--check`);
runtime is ≈ 12 s on the 12 fixtures.

```sh
scripts/metrics.py --self-test --check
scripts/metrics.py --self-test --json | python3 -m json.tool
```

**Batch renders (`--renders`).** Point it at a directory of `PitchShifter`
renders. If the directory has a `manifest.tsv` (P-02's ignored test writes one)
it is used automatically; `--manifest TSV` names one explicitly. Without a
manifest, `<fixture>.<semitone>.f32` names are inferred and the reference is
looked up in the directory first, then `fixtures/f32/`. The command accepts
`--allow-length-delta N` and emits one JSON object with a per-cell status:

```sh
scripts/metrics.py --renders /tmp/rasp-p02-renders --check
scripts/metrics.py --renders out/ --manifest out/manifest.tsv --json --check
scripts/metrics.py --renders out/ --allow-length-delta 48 --check
```

`--allow-length-delta N` (default 0) is for P-06's trim/pad. A mismatch within
N samples passes with the delta printed and recorded in JSON; pitch and null
use the common region. A larger mismatch still fails the sample-count check —
the decision is explicit, never silent. This tolerates a **length** difference,
not a **time offset**: a head trim shifts the render relative to the reference
and must be reported separately (the paired grid starts at sample 0), so do not
use the flag to paper over it.

**P-02 whole-clip result, recorded honestly.** P-02's original acceptance
measured a single highest-energy 4096-sample slice. That view is much kinder
(most cells land within a few cents), but it is not the whole clip. The paired
whole-clip batch on the current `PitchShifter` renders is **42 PASS / 13 FAIL /
8 N/A**, worst `+105.9 c` (`am_onyx__got-time` at +7 and +12 st); other
failures include `am_onyx` +1/+4 (+27.9/+92.9 c), `af_aoede` −12/+12
(+40.2/+31.8 c), `af_heart` −12 (+15.2 c) and `bf_alice` +12 (−41.9 c). Those
are real per-frame ratio errors on gliding/low-F0 regions, not a ruler
artifact — the `--self-test` controls are exact to ≤ 1 c and the same harness
catches a deliberately mislabelled render by 400 c. Q-03 must decide whether
the ±5 c gate applies to the whole clip (then P-02 needs work) or only to a
defined steady-state subset; do not silently widen the tolerance. The 8 `N/A`
cells in that batch are exactly the expected out-of-band cases (low anchor at
−12…−1 st, synthetic 88/150 Hz at −12 st).

### 9.8 Harmonic-model formants (P-07c) — this supersedes the F1/F2/F3 row in §9.2

The formant check no longer gates on LPC. F1/F2 come from a fit of P-03's
**generative model** — three two-pole resonators with free bandwidths plus a
free `C + α·log f` source tilt — to the harmonic amplitudes of 100 ms frames,
the same method as `crates/rasp-dsp/src/testutil.rs::fit_formants`. LPC is kept
beside it as the **legacy** estimator so the two can be compared, but it is
never gated.

**Why LPC was not sufficient.** It is not merely noisier; it is *biased*, and
the bias is largest exactly where the female voices live. On synthetic vowels
whose formants are known by construction, `--formant-self-test` reports:

| F0 | true F1/F2/F3 | LPC estimate | LPC error | harmonic estimate | harmonic error |
|---|---|---|---|---|---|
| 90 | 700/1220/2600 | unresolved | — | 700/1220/2601 | -0.01/-0.04/+0.03% |
| 90 | 500/1500/2500 | 508/1506/2498 | +1.6/+0.4/-0.1% | 500/1500/2500 | -0.02/-0.02/+0.02% |
| 110 | 700/1220/2600 | unresolved | — | 700/1220/2601 | -0.01/-0.04/+0.03% |
| 110 | 500/1500/2500 | 516/1511/2502 | +3.1/+0.7/+0.1% | 500/1500/2500 | -0.02/-0.02/+0.02% |
| 150 | 700/1220/2600 | unresolved | — | 700/1220/2601 | +0.01/-0.04/+0.03% |
| 150 | 500/1500/2500 | 455/1502/2500 | -9.0/+0.1/+0.0% | 500/1500/2500 | +0.05/-0.02/+0.02% |
| 220 | 700/1220/2600 | 664/1027/1295 | -5.1/-15.8/-50.2% | 699/1220/2599 | -0.21/-0.04/-0.05% |
| 220 | 500/1500/2500 | 432/1541/2418 | -13.7/+2.7/-3.3% | 500/1500/2501 | +0.04/+0.00/+0.02% |
| 250 | 700/1220/2600 | unresolved | — | 699/1221/2600 | -0.12/+0.09/+0.00% |
| 250 | 500/1500/2500 | 502/1500/2502 | +0.4/+0.0/+0.1% | 500/1500/2500 | -0.01/-0.02/+0.02% |
| 300 | 700/1220/2600 | 598/930/1205 | -14.6/-23.8/-53.7% | 700/1218/2600 | -0.07/-0.14/+0.00% |
| 300 | 500/1500/2500 | 311/611/1076 | -37.9/-59.2/-57.0% | 500/1500/2501 | +0.04/+0.00/+0.02% |

Worst harmonic error **0.21%** (gate 1.00%); worst LPC error **59.2%**, with
7 of 12 cells over 1% and 4 of 12 where LPC cannot even produce three peaks.
That is the P-03 observation in table form: at F0 = 220 Hz it reports 664 Hz for
a true 700 Hz formant and puts F2 at 1027 Hz; at 300 Hz it is 53% low on F3.
Publishing per-voice baselines from that estimator would bake the error into the
product, which is why Q-01 must use the certified one.

**What is measured now.** Each 100 ms frame is voiced-checked with YIN, its
harmonic amplitudes are projected at the exact harmonic frequencies, and the
6-parameter fit is solved (coarse multi-start grid, then coordinate descent with
halving steps; later frames warm-start from the previous one). The reported
number is the median over frames that pass the gates. The search is bounded to
an adult vocal tract (F1 200–1200, F2 600–3000, F3 1500–3600 Hz) and to the
ordering F2 > 1.15·F1, F3 > 1.15·F2. The table shows the harmonic rows first and
the LPC rows labelled `legacy` beside them; `--json` keeps both under
`metrics.formantsHz` and `metrics.formantsHz.legacyLpc`.

**`N/A` means the ruler cannot certify a formant — not that the DSP failed.**
A formant check is never `PASS` and never `FAIL` when:

- the fit does not explain the harmonics: the RMS log-amplitude residual is
  above **1.0 dB** per harmonic. A synthetic vowel built from the model sits at
  0.01–0.05 dB; real 32 kbps speech sits at 3–8 dB, because it has
  anti-resonances, source ripple and a fourth/fifth formant the 3-resonator
  cascade cannot represent. In that case the fit can place F1 at 200 Hz (its
  bound) and mean nothing;
- fewer than **3** frames survive the fit and voiced gates;
- F1 lies below the first harmonic (F0): the harmonic grid cannot shape a
  formant there, so it would be extrapolation;
- fewer than **6** harmonics lie below F3: F3 is then extrapolated past the
  observed grid rather than measured;
- fewer than **8** harmonics are available at all;
- the requested ratio is **outside the D1 validated region** for the pitch
  position (below).

The reason is printed and stored under `metrics.formantsHz.naReason`, and the
per-frame rejection counts are available from `audio_metrics.harmonic_formants`.
The practical consequence, stated plainly: **on real 32 kbps corpus speech the
harmonic ruler reports N/A for every fixture**, so Q-01 cannot publish F1/F2/F3
baselines from it yet. That is a true statement about the ruler, not a DSP
defect, and the alternative — publishing LPC numbers with a 10–60% bias — is
what P-07c exists to prevent. Improving the model (more resonators, a
pitch-synchronous or cepstral envelope with a certified control) is the open
work.

**The validated region (D1).** The ratio is gated only where D1 validated the
knob space:

| pitch position | gated formant ratio |
|---|---|
| 0 st (identity, or `\|pitch\| ≤ 0.5 st`) | 0.75 … 1.33 |
| `\|pitch\| ≤ 7 st` | 1.00 … 1.10 |
| `\|pitch\| > 7 st` | no validated region → `N/A` |

The pitch position is the requested `--pitch-st` when supplied, `0` for
identical bytes, and otherwise the measured paired pitch. Outside the row, or
outside the row's ratio range, the check prints
`formants: N/A - requested formant ratio 1.33 is outside the D1 validated region
1.00-1.10 for pitch +7.0 st`. It does not fail, and it does not pass.

**Run the control (`--formant-self-test`).** It synthesizes 12 vowels (6 F0
values × the two P-03 formant sets, 0.6 s each) from the same 6-parameter model
the estimator fits, so the true formants are known by construction and no
estimator is in the loop when the expectation is written. It requires every
harmonic estimate within **1%**, prints the LPC column for comparison, and also
runs a source → synthetic 1.10× target pair through the same D1/ratio logic
`--formant-ratio` uses. A 10% wrong expectation is also fed to the gate on every
cell and must be rejected, so the control cannot silently become unfalsifiable.
It exits non-zero on any regression and takes ≈ 7 s (it needs no fixtures).

```sh
scripts/metrics.py --formant-self-test
scripts/metrics.py --formant-self-test --json | python3 -m json.tool
```

**End-to-end check.** A synthetic source and a 1.10× target (formants moved by
exactly 10%, e.g. two `audio_metrics.synthesize_vowel` calls, or a P-03
`VoiceTransformer` render when `rasp-cli`/the crate build is available) score as
expected:

```sh
scripts/metrics.py --ref vowel-src.f32 --test vowel-1.10.f32 \
                   --formant-ratio 1.10 --check      # F1 +10.0%, F2 +10.1% → PASS
scripts/metrics.py --ref vowel-src.f32 --test vowel-1.10.f32 \
                   --formant-ratio 1.20 --check      # → FAIL (exit 1)
```

`--formant-ratio` still gates only F1/F2, per PLAN §8; F3 is printed.


### 9.9 The pitch ruler's pairing, reliability and whole gate (P-07d) — this supersedes the P-02 result and frame policy in §9.7

P-07b's paired ratio had three defects. All three are fixed; the tolerance was
not touched.

**1. Rank pairing.** `f0_track` returns *compacted* voiced-only arrays, and
`paired_f0_ratio` then did

```python
paired_count = min(len(ref_f0), len(test_f0))
ref_paired = ref_f0[:paired_count]      # n-th voiced ref frame ...
test_paired = test_f0[:paired_count]    # ... against n-th voiced test frame
```

so one marginal-frame disagreement shifted every later pair. `am_onyx` +7 st
reported **+105.94 c** that way and **+2.60 c** when paired by time. Frames are
now paired **by frame index** — the same instant on the shared grid — after
intersecting `ref_voiced ∩ test_voiced` **on those indices**
(`_indexed_track`). `f0_track` gained `return_indices=True` (the frame ordinal,
`start = index*hop`); its defaults, and therefore `docs/corpus.md`, are
unchanged. The resampler control's index mapping is now explicit rather than
assumed: reference frame `i` starts at `i*hop*scale` seconds of content and
output frame `j` at `j*hop` seconds, i.e. `scale*j*hop` of content, so
`i = round(j*scale*hop_test/hop_ref)`. With `hop_ref = round(hop_s*scale*fs)`
the factor is within a hop-rounding error of 1.

**2. Only the median was gated.** `--renders` called a cell `PASS` when the
median was within 5 c, so `synthetic_vowel228` passed with 21 octave outliers
and a 731-cent p90. `--renders` (and `--ref/--test`) now enforce **all three**
D17 clauses: median `|error| <= 5 c` **and** `p90 |error| <= 25 c` **and**
`octave outliers == 0`, and the failing clause is named in the table
(`p90 |error| 33.25 c (limit 25 c)`), in `cells[].failedClauses` and in
`summary.failuresByClause`. Out-of-band cells stay `N/A` — never `PASS`.

**3. No per-frame reliability gate, and a cross-check that could not say no.**
Three separate faults:

- Frames were inside the statistics regardless of where their *measured* F0
  sat. The ruler now drops, per index and **before** median/p90/octave, any
  frame whose reference or render F0 is outside the reliable 80–550 Hz band,
  any frame whose **expected** output F0 (`ref_f0 x ratio`) is outside it (a
  per-frame form of the `N/A` rule: at −12 st a reference frame that dips to
  82 Hz should render at 41 Hz, and the only in-band reading there is an
  octave artifact), any frame the spectral cross-check rejects, and any
  isolated octave jump on the *reference* (`_reference_unreliable_mask`).
- `_spectral_peaks` returned **every** local maximum, and a Hann window's
  sidelobes are local maxima ~31 dB down. The cross-check was therefore
  "confirming" readings the frame did not support (347.7 Hz on a 228 Hz
  vowel; 173.7 Hz on the same vowel shifted to 114 Hz). Peaks now need
  prominence (`PITCH_PEAK_PROMINENCE = 0.02` of the frame peak, −34 dB).
  `--formant-self-test` is unchanged to the last digit (worst 0.208%).
- YIN itself chose a spurious dip: on a stationary 228 Hz synthetic vowel it
  reported **347.77 Hz on 11/99 frames**, with `agreement=True`. Neither an
  absolute band nor a sub-harmonic test fixes that (347.77 is not an integer
  multiple of 228), so `_frame_f0_supported` scores candidate fundamentals
  against the frame's prominent peaks: the octave family of the reading
  (`f/3, f/2, f, 2f, 3f`) plus the sub-multiples of the lowest prominent peak
  (`p/h, h = 1..4`). A candidate's score is
  `captured peak-energy fraction x harmonic hit rate`; either term alone is
  degenerate (captured energy always prefers a sub-harmonic because it has
  more teeth; hit rate alone always prefers a low harmonic because every tooth
  lands on one). A reading is rejected when a candidate more than a semitone
  away (100 c) wins by 1.5x with a score >= 0.05. This is applied to **both**
  signals: a render that genuinely moves an octave keeps its energy there, so
  the moved value wins and the error is measured and counted — it is never
  dropped. The `510.65 Hz` reading on an 88 Hz male frame that produced a
  −2788 c outlier is removed by the band/contour/support gates, and the count
  is reported.

Every drop is counted in `metrics.pitch.dropped*` and printed
(`N frame(s) dropped as unreliable ...`). On the P-02 matrix, 433 of the 3403
frames voiced in both signals in the 55 in-band cells are dropped (12.7%):
reference band 117, render band 24, expected band 30, cross-check 114/58,
reference contour 8, harmonic support 71/11. Paired evidence falls from 3167
frames (old ruler) to 2970.

**The false-rejection rate, measured.** On all 413 voiced frames of the 12
fixtures the harmonic-support gate rejects **33 (8.0%)** — 2–16% per fixture,
concentrated on low-HNR and transition frames. On the four real-speech
*identity* cells (where both signals are the same bytes, so every drop is
unnecessary by definition) the gates drop 39 of 148 both-voiced frames (26%):
22 by the 80 Hz band (19 of them on the 82 Hz low anchor), 10 by harmonic
support, 4 by the cross-check, 3 by the reference contour check. The two clean
synthetic vowels drop **0 of 198**; the 11 dropped on `synthetic_vowel228` are
the real 347.77 Hz artifacts. Rejections can only remove evidence — they never
turn a failing number into a passing one — and they are reported, not silent.

**The pairing control, and it can fail.** `--self-test` gained
`pairingMismatch`: a 1.2 s, 150→260 Hz glide where the reference has four whole
frames silenced and the render two, so the voiced sets differ
(`refVoicedFrames`/`testVoicedFrames` are reported and a mismatch of at least 2
is required). Outside the silences the two signals are **sample-identical**, so
index pairing must read exactly 0.000 c — the control does not depend on the
tracker's accuracy. It also carries a runtime **negative control**: the median
the discarded rank-pairing rule would report on the same two signals must
exceed 1 c, or the control is not exercising anything and fails. Current
result: index-paired **0.000 c**, rank-paired **−32.00 c**.

Verified by deliberately reverting to rank pairing in a scratch copy of the two
scripts (the repo copy untouched): the control reads **−32.002 c**, `--self-test`
exits **1**, and `--renders` collapses to **38 PASS / 17 FAIL / 8 N/A**, worst
median **+105.88 c** (`am_onyx` +12 st) — i.e. the old behaviour returns, and
the control catches it. Restoring the shipped scripts returns 54/1/8 and exit 0.

**Re-scored P-02 matrix** (`write_metric_render_matrix`, 7 fixtures × 9 steps =
63 cells, `metrics.py --renders`): **54 PASS / 1 FAIL / 8 N/A**, 0 octave
outliers, worst median **3.34 c** (`am_onyx` +12), worst p90 **33.25 c**. The
old *pairing and gate* on the same renders (rank pairing, median-only, today's
peak picking so the columns isolate the P-07d pairing/gate change): 46/9/8;
**17** cells fail the full D17 gate under rank pairing.

| cell | st | rank median | rank p90 | rank oct | indexed median | indexed p90 | indexed oct | now |
|---|---|---|---|---|---|---|---|---|
| af_heart__friends-now | −4 | +0.51 | 65.85 | 0 | +0.77 | 33.25 | 0 | **FAIL (p90)** |
| af_aoede__you-got-me | −12 | −7.98 | 235.32 | 2 | −1.14 | 18.89 | 0 | PASS |
| af_aoede__you-got-me | −7 | −10.53 | 269.64 | 2 | −1.44 | 10.27 | 0 | PASS |
| af_aoede__you-got-me | −1 | +18.36 | 156.38 | 2 | −0.68 | 6.08 | 0 | PASS |
| af_aoede__you-got-me | +4 | −0.70 | 8.46 | 1 | −0.70 | 5.21 | 0 | PASS |
| af_aoede__you-got-me | +7 | +0.56 | 13.17 | 1 | +0.28 | 6.28 | 0 | PASS |
| af_aoede__you-got-me | +12 | +31.80 | 159.81 | 1 | +0.12 | 8.85 | 0 | PASS |
| bf_alice__maybe-next-time | −12 | +0.36 | 156.13 | 2 | −1.39 | 13.47 | 0 | PASS |
| bf_alice__maybe-next-time | −7 | −0.65 | 142.16 | 0 | −0.83 | 9.74 | 0 | PASS |
| bf_alice__maybe-next-time | −4 | −2.69 | 151.28 | 0 | −0.71 | 7.54 | 0 | PASS |
| bf_alice__maybe-next-time | −1 | −0.17 | 159.68 | 0 | +0.13 | 7.61 | 0 | PASS |
| bf_alice__maybe-next-time | +7 | −2.40 | 84.03 | 0 | −0.31 | 14.49 | 0 | PASS |
| bf_alice__maybe-next-time | +12 | −15.54 | 346.79 | 2 | −0.30 | 17.55 | 0 | PASS |
| am_onyx__got-time | +1 | +51.33 | 315.58 | 4 | +0.90 | 5.97 | 0 | PASS |
| am_onyx__got-time | +4 | +81.86 | 298.99 | 2 | +0.94 | 7.92 | 0 | PASS |
| am_onyx__got-time | +7 | +48.47 | 251.10 | 2 | +2.25 | 11.24 | 0 | PASS |
| am_onyx__got-time | +12 | +105.88 | 324.36 | 2 | +3.34 | 14.23 | 0 | PASS |

Per fixture, worst `|median|` and worst p90 over its in-band cells:

| fixture | cells (in band) | worst median | worst p90 |
|---|---|---|---|
| af_heart__friends-now | 9 | 1.63 | 33.25 |
| af_aoede__you-got-me | 9 | 1.44 | 18.89 |
| bf_alice__maybe-next-time | 9 | 1.39 | 17.55 |
| am_onyx__got-time | 5 (+4 N/A) | 3.34 | 14.23 |
| synthetic_vowel88 | 6 (+3 N/A) | 0.04 | 0.36 |
| synthetic_vowel150 | 8 (+1 N/A) | 0.02 | 0.04 |
| synthetic_vowel228 | 9 | 0.09 | 0.09 |

**The one remaining failure is `af_heart` −4 st (p90 33.25 c, median +0.77 c,
0 octave outliers).** 21 paired frames; two of them carry the tail (+76 c at
t≈0.50 s, −69 c at t≈0.60 s). Both sit on a steep glide into a voicing hole
(the reference contour moves ~100–180 c per 20 ms frame there and has an
isolated voiced frame at 0.51 s). Re-measuring the same region on a 10 ms grid
shows the render following the reference contour within ±10 c except at those
same two instants, and the signed error at those instants **flips sign between
step −4 (+76 c) and step −7 (−33 c)** — not the signature of a ratio error.
The evidence points at **window smearing across the transition in the
estimator**, not at the shifter; if P-04's transient work changes this region it
should be re-scored, and the cell is not "fixed" by widening anything.

**Not changed.** `docs/corpus.md` regenerates byte-identical
(`sha256 a3d0ac63183c94992a69b6be769bedc72a19b823e65ecb60342320cf36500029`);
`--formant-self-test` still passes 12/12 at worst 0.208%; `--manifest` still
reports 12/12, median SNR 14.7 dB. `--self-test` now runs 279/279 in-band cells
at worst same-time 0.07 c and worst resample **0.93 c** (was 0.66 c: the extra
0.27 c is the reliability gates removing frames, not a pairing change), plus the
pairing control.

```sh
scripts/metrics.py --self-test --check            # controls: 279 cells + pairing
scripts/metrics.py --renders "$TMPDIR/rasp-p02-renders" \
                   --manifest "$TMPDIR/rasp-p02-renders/manifest.tsv" --check
```

**Open, deliberately not done here.** (a) The render side is never gated on its
own contour consistency — that would delete exactly the octave artifacts D17
wants counted; only spectral *support* is required. (b) The `af_heart` −4
transition is an estimator-vs-transient ambiguity; resolving it needs a
shorter-window or pitch-synchronous estimator with its own control, which is a
new task, not a tolerance change. (c) `harmonic_formants` still shares
`_spectral_peaks`; its synthetic control is unchanged, but P-07e should re-run
`--formant-self-test` if it touches peak picking.

### 9.10 Formant calibration on real 32 kbps speech (P-07e) — the codec floor and what Q-01 may publish

P-07c's ruler is exact on the model it was built from and returns `N/A` on every
real fixture. P-07e asked whether a larger model or the codec itself explains
the gap, and — if not — what Q-01 may publish under D20.

**The codec floor is the single most important number.** Synthetic vowels with
formants known by construction were encoded with the corpus settings
(`ffmpeg -c:a libopus -b:a 32k -ar 48000 -ac 1 -frame_duration 20`, WebM),
decoded with libopus, and fitted with the installed ruler. 0.8 s vowels, all
frames, four F0 values (90, 150, 220, 300 Hz) × two vowel sets:

| fit band | clean residual (median) | coded residual (median / max) | coded worst F1/F2/F3 error |
|---|---|---|---|
| 3.0 kHz | 0.02 dB | **0.25 / 0.38 dB** | 0.94% |
| 3.5 kHz | 0.02 dB | **0.40 / 0.82 dB** | 1.08% |
| 4.0 kHz | 0.02 dB | 1.43 / 1.92 dB | 5.01% |
| 6.0 kHz | 0.02 dB | 4.03 / 5.21 dB | 11.82% |

The 6 kHz band is where the **model** is exact; it is not where 32 kbps Opus is
transparent. Above ~3.5 kHz the codec's noise filling and band coding dominate
the harmonic amplitudes, and a larger model cannot remove that: on one coded
150 Hz vowel, the best 3-resonator fit is 3.44 dB and the *true* model evaluated
on the same amplitudes is 3.50 dB. The codec, not the model, sets the floor.

At 3.5 kHz the per-frame floor over all 156 control frames is **0.58 dB median,
2.23 dB p90, 4.67 dB max**. That is already above the synthetic-model gate
(1.0 dB) on a per-frame basis, so any real-speech gate has to be above the
codec's own contribution rather than at the synthetic model's 0.01 dB.

**Model extensions were tried and rejected by the control that can see them.**
Adding 2 anti-resonance (zero) pairs, F4/F5 (5 resonators), a source-tilt
curvature term and a per-frame gain were fitted as alternatives. (The per-frame
gain already exists: the free intercept `C` in `C + alpha·log f` is exactly a
per-frame level.) The 1% synthetic control is the admission test, and it is
where the extensions fail:

| variant | clean synthetic worst error | coded control worst error | coded residual median | verdict |
|---|---|---|---|---|
| 3 resonators + linear tilt (shipped) | **0.005%** | 4.34%* | 1.49 dB | admissible |
| + 2 zero pairs + linear tilt | 2.58% | 10.73% | 1.04 dB | fails at F0 = 300 Hz |
| 5 resonators + linear tilt | 15.13% | 22.79% | 1.63 dB | fails at F0 ≥ 250 Hz |
| 5 resonators + 2 zeros + quadratic tilt | 39.82% | 107.66% | 0.68 dB | fails everywhere |

\* 3-frame sample; the full-frame codec control in `--formant-calibration`
reports a worst *graded* error of 2.52%.

The failure is not numerical. At F0 = 300 Hz there are only 11 harmonics below
3.5 kHz. The shipped model has 8 free parameters (three formants, three
bandwidths, two tilt coefficients); the 2-zero model has 12 and the
5-resonator + 2-zero + curvature model has 17 (10 + 4 + 3). Above 11 free
parameters the problem is underdetermined, and the optimiser fits the residual
to zero by moving F1/F2 anywhere — on the **clean** synthetic vowel, with no
codec in the loop. Even the 2-zero model is over-parameterised at the top of the
corpus F0 range (2.58% error at 300 Hz, 0.05% at 150 Hz). A model that cannot
reproduce ground truth it was built from is not a ruler, so the extensions are
not shipped.

**On the 12 fixtures, the extensions lower the residual by at most ~2.5 dB and
do it by wandering.** Per-frame residual (dB RMS log-amplitude, 3.5 kHz band,
12 frames per fixture, one common optimiser so the columns are comparable;
median over frames). The `3res` column is that optimiser's convergence on the
shipped model; the shipped coordinate-descent fit is slightly better at **4.31
dB median (2.74–5.60)**, so the extension gain below is, if anything, flattered:

| fixture | F0 | 3res | +2 zeros | 5res | 5res+2z+quad |
|---|---|---|---|---|---|
| af_heart__friends-now | 246 | 3.94 | 1.68 | 1.91 | 2.22 |
| af_aoede__you-got-me | 234 | 7.02 | 2.54 | 3.24 | 1.45 |
| bf_alice__maybe-next-time | 242 | 3.48 | 2.33 | 3.60 | 1.36 |
| am_onyx__got-time | 93 | 5.32 | 4.07 | 3.98 | 3.74 |
| am_eric__good-game | 179 | 5.56 | 4.15 | 4.37 | 2.96 |
| bf_emma__nice-day | 188 | 5.24 | 3.75 | 4.89 | 3.40 |
| zf_xiaoni__greet | 222 | 2.74 | 1.17 | 2.34 | 0.65 |
| zm_yunxi__no-worries | 174 | 5.32 | 2.78 | 3.33 | 2.54 |
| jf_nezumi__be-friends | 233 | 3.60 | 1.57 | 2.54 | 1.58 |
| jm_kumo__watch-this | 104 | 4.94 | 3.02 | 3.78 | 3.21 |
| hf_alpha__good-to-see | 224 | 4.65 | 2.24 | 2.92 | 1.06 |
| ef_dora__see-you | 186 | 5.67 | 3.85 | 3.76 | 3.37 |
| **median** | — | **5.09** | **2.66** | **3.46** | **2.38** |

Even the best variant leaves 7 of 12 fixtures above 1.5 dB, and it does so with
three extra resonators whose frequencies are not stable.

Frame-to-frame stability is the second, independent refusal. F1's standard
deviation is **90–400 Hz** and its full range **205–1000 Hz** on 10-frame
fixtures for every extended variant, against an F1 of 300–800 Hz; the largest
single-frame step is 178–928 Hz. The fit is not measuring a formant; it is
choosing one of several near-equivalent minima per frame. The shipped
3-resonator fit is no better on real speech: `--formant-calibration` reports F1
95% intervals of 27–104% of the value.

**Decision: (B), it does not certify.** The codec floor is above the synthetic
gate; the extensions that lower the fixture residual cannot reproduce synthetic
ground truth; and the fits that do lower it move hundreds of Hz frame to frame.
No fixture passes the 1.5 dB residual gate at either 3.0 or 3.5 kHz (best
fixture: 2.74 dB at 3.5 kHz, 1.99 dB at 3.0 kHz), so **0/12 fixtures and 0/36
per-formant fields are publishable**. The honest output is `uncalibrated` with a
reason, not an LPC number (D20).

**The uncertainty policy (implemented in `audio_metrics.formant_calibration`).**
The Q-01 ruler fits the same model in the 3.5 kHz band where the codec is
transparent, and reports a **95% interval from the fit's own residual
sensitivity**: `Cov = sigma^2 (J^T J)^-1`, `sigma^2 = RSS/dof`, with `J` the
numerical Jacobian of the tilt-profiled residual with respect to the three
formants and three bandwidths (`_formant_covariance`). The interval is the
linearised lower bound: it assumes the model is true. On real speech the
residual is systematic, so the true error can exceed it — but a fit whose
*optimistic* interval is already wider than 8% cannot be published on any
reading.

A formant is `graded` only when **all** of these hold, otherwise it is
`uncalibrated` with the first reason that failed:

1. median residual over fitted frames ≤ **1.5 dB** (the worst graded
   synthetic-through-codec cell is 1.45 dB; 1.5 dB is above the codec's own
   contribution and below every real fixture);
2. at least **3** frames survive that gate with a usable covariance;
3. the fit has positive residual degrees of freedom (at F0 = 300 Hz, 11
   harmonics − 8 parameters leaves 3: enough for a number, not enough for a
   tight one);
4. the normal matrix is conditioned below **1e8**;
5. the 95% half-width is ≤ **8%** of the value (PLAN §8's formant target).

The frame-to-frame step (median and SD per formant) is reported as a
diagnostic, not gated: real speech changes formants between vowels, so a stable
point estimate is not the same as a stable voice. It is what makes the
extensions' failure visible.

**Controls that can fail (`scripts/metrics.py --formant-calibration`, ≈16 s).**
It runs both controls before touching the fixtures:

- **clean synthetic ground truth**: 12/12 graded, worst error **0.172%** (1%
  gate), with a 10%-wrong expectation rejected 12/12;
- **synthetic → Opus@32k → fit**: **9/12 graded**, worst graded error
  **2.52%** against an 8% tolerance, worst graded residual 1.45 dB. The three
  refusals are the policy working, not a hidden failure: 90 Hz (residual
  2.18 dB, the codec itself), 250 Hz (1.70 dB), and 300 Hz (F1 interval ±10.9%,
  too few degrees of freedom). A 20%-wrong expectation is rejected on every
  graded cell;
- **fixtures**: 12/12 `uncalibrated`, with the reasons counted in the summary.

The mode exits 1 if either control fails, and 2 if ffmpeg/libopus is missing,
because the codec control is mandatory: without it the floor is unknown and
nothing may be certified.

**What Q-01 may write into `web/data/voice-baselines.json`.**

- `f0Hz`: the median from the corpus tracker (60–400 Hz, 10 ms hop, 5-frame
  smoothing), as before. P-07e does not change it.
- `f1Hz`/`f2Hz`/`f3Hz`: **only** when that formant's calibration status is
  `graded`. The record must carry the 95% range (`f1RangeHz` etc.), the method
  (`harmonic-fit-calibration-v1`) and the profile (3.5 kHz band, 100 ms frames /
  50 ms hop).
- Everything else: `"status": "uncalibrated"` plus the reason string. Do **not**
  write LPC values, a point estimate inside a diagnostic range, or a range with
  `rangeIsAuthoritative: false` as if it were a baseline. A preset calibrated
  against an uncalibrated voice is labelled approximate (D20).
- `method` and `control`: record that the value came from
  `--formant-calibration` and that its codec control graded 9/12 synthetic
  cells; a future run that changes those numbers is a re-baseline, not a
  silent update.

The two shapes, exactly (the first is what every current voice gets; the second
is the only shape a future graded voice may use):

```json
{
  "voice": "af_heart",
  "f0Hz": 246.3,
  "formants": {
    "status": "uncalibrated",
    "method": "harmonic-fit-calibration-v1",
    "reason": "median harmonic residual 4.01 dB exceeds the 1.5 dB calibration gate",
    "f1Hz": null, "f1RangeHz": null,
    "f2Hz": null, "f2RangeHz": null,
    "f3Hz": null, "f3RangeHz": null
  }
}
```

```json
{
  "voice": "<graded voice>",
  "f0Hz": 150.0,
  "formants": {
    "status": "graded",
    "method": "harmonic-fit-calibration-v1",
    "profile": "3.5 kHz band, 100 ms frames / 50 ms hop",
    "f1Hz": 700.1, "f1RangeHz": [699.7, 700.5], "f1Status": "graded",
    "f2Hz": 1220.5, "f2RangeHz": [1220.0, 1221.0], "f2Status": "graded",
    "f3Hz": 2600.8, "f3RangeHz": [2600.3, 2601.2], "f3Status": "graded"
  }
}
```

On the current fixtures the Q-01 formant fields are **all `uncalibrated`**, so
the file carries F0 only. That is the correct result under D20, not a missing
deliverable.

**What remains unmeasurable.** Absolute F1/F2/F3 for real 32 kbps speech at this
model order. There is no F0 cohort where it works: low male voices (90–100 Hz)
have enough harmonics but the codec's own residual reaches 2.2 dB; high female
voices (250–300 Hz) fit the codec but have too few harmonics to identify six
parameters (3 residual degrees of freedom at 300 Hz). A shorter analysis window
does not help (75 ms and 50 ms change the residual by <1 dB), and the residual
is not frame smear. What would reopen it: a pitch-synchronous or cepstral
envelope estimator with its own ground truth *and* its own codec control, or a
source model that explains the glottal ripple. Until then, Q-01 publishes F0 and
marks formants `uncalibrated`.

