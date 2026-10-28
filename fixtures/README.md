# fixtures/

Decoded audio for native and wasm tests: twelve corpus clips, three ways, plus a
manifest. **Nothing here is committed except this README and the `.gitkeep`
placeholder.** The audio is derived from the vendored corpus, is regenerated on
demand, and would blow the "no committed fixtures over 1 MB" rule (PLAN §8).
`.gitignore` keeps `fixtures/*` ignored with explicit exceptions for those two
files.

## Generate

```sh
python3 scripts/make_fixtures.py           # make what is missing, skip what is current
python3 scripts/make_fixtures.py --force   # rebuild all 36 files
```

Requirements: Python 3.8+ (standard library only) and `ffmpeg` built with
libopus. The corpus must already be vendored (`python3 scripts/vendor_corpus.py`;
`--check` verifies it offline). A fully cached run decodes nothing and writes
nothing; it only re-hashes the generated files (and asks `ffmpeg --version`) to
prove the cache is still valid.

The script verifies the `sha256` and byte count of every source `.weba` against
`web/registry.json` *before* decoding. A missing or modified clip is a fatal
error naming the command that repairs it; it never decodes an unverified file.

## Layout

| path | format | use |
|---|---|---|
| `wav/<voice>__<phrase>.wav` | 48 kHz mono 16-bit PCM | listening, and tools that only accept WAV |
| `f32/<voice>__<phrase>.f32` | 48 kHz mono raw little-endian `f32`, headerless | numeric tests; size is a multiple of 4 |
| `reference/<voice>__<phrase>.f32` | same, decoded by **libopus** | the oracle for decode/shift comparisons |
| `manifest.json` | JSON | provenance: what, why, hashes, sample counts |

Clips are decoded whole and untouched: no trimming, no fades, no
normalisation, no resampling. Leading/trailing silence is part of the fixture
(pre-built `bf_emma__nice-day` carries the corpus's worst tail, 456 ms), so
trim and fade tests have real material. Source is already 48 kHz mono; the
`-ac 1 -ar 48000` flags only pin the contract.

`wav`, `f32` and `reference` for one clip always hold the same number of
samples. The script fails if the three decoders disagree, because a fixture set
that is not sample-aligned is worse than no fixture set.

### `reference/` is not `f32/`

`f32/` is FFmpeg's native Opus decoder; `reference/` is libopus
(`ffmpeg -c:a libopus -i`). The two produce **different waveforms** from the
same `.weba` — 14.5–31.9 dB SNR against each other, and not a constant delay, so
no alignment fixes it. Compare a Rust decoder against `reference/` only
(DECISIONS D5, `docs/spikes/decode.md`). A test that compares against
`f32/` and expects a high SNR is wrong.

## Manifest

`manifest.json` is the machine-readable contract. Paths in `clips[].files[]`
are relative to this directory; `clips[].source.path` is relative to `web/`.
Besides the obvious fields it records:

- `registry.buildId` / `sourceHash` / `clipVersion` — which bake the fixtures
  came from;
- `decoder.signature` — `ffmpeg <version> + libopus`; a different decoder
  rebuilds everything, because the reference output is decoder-specific;
- per clip: `source.sha256`, `source.bytes`, `source.url`,
  `registryDurationMs` (approximate, ±1 Opus frame), `sampleCount`,
  `durationSeconds`, the hash of each generated file, the phrase group, and
  `reason` — why that clip earned its slot.

Tests should treat the manifest's `sampleCount` and `files[].sha256` as the
authority, not directory listings: the fixture set is pinned, so a mismatch
means someone regenerated with a different corpus or ffmpeg and the numeric
expectations need re-baselining.

## The twelve clips

One clip per voice from the D13 set (as amended by F-06): six English, two
Mandarin, two Japanese, one Hindi, one Spanish. Together they cover every
phrase group, the shortest and second-longest clip in the corpus, the English
pitch extremes, the D7 language-mismatch case and the weakest-HNR voice.

| voice | lang | phrase | group | seconds | why |
|---|---|---|---|---|---|
| `af_heart` | en | `friends-now` | friendship | 0.990 | default voice, median length |
| `af_aoede` | en | `you-got-me` | win | 0.799 | high F0 anchor, 221 Hz |
| `bf_alice` | en | `maybe-next-time` | close | 1.430 | 248 Hz stress case |
| `am_onyx` | en | `got-time` | play | 1.747 | low F0 anchor, 88 Hz, longest curated |
| `am_eric` | en | `good-game` | loss | 0.803 | mid male, unbiased A/B |
| `bf_emma` | en | `nice-day` | ambient | 1.278 | 456 ms tail silence (P-04) |
| `zf_xiaoni` | zh | `greet` | greeting | 0.394 | shortest in corpus, zh gate |
| `zm_yunxi` | zh | `no-worries` | decline | 0.728 | zh male, zh gate |
| `jf_nezumi` | ja | `be-friends` | friendship | 1.335 | cleanest voice (15.5 dB HNR) |
| `jm_kumo` | ja | `watch-this` | play | 0.816 | ja male, 86 Hz |
| `hf_alpha` | hi | `good-to-see` | greeting | 2.058 | longest curated, D7 mismatch |
| `ef_dora` | es | `see-you` | close | 1.101 | Spanish slot, 7.9 dB HNR |

`SELECTION` in `scripts/make_fixtures.py` is the source of truth for this table
and for the `reason` strings; the script re-checks every group and the
short/median/long coverage on each run. Totals: 646,948 samples, 13.48 s,
6,470,008 bytes (≈ 1.2 MiB WAV, 2.5 MiB each f32 tree).

## Ownership

- The set is **fixed** by PLAN F-05 / DECISIONS D13. Changing it is a PM call,
  not a local convenience: update `SELECTION`, this table, and re-baseline
  anything that hard-coded a sample count.
- The audio derives from the vendored `dg-pp` corpus (Kokoro-82M voices) and
  inherits its attribution obligations (PLAN K5, D15). Fixtures must never be
  published as part of the site.
- There is no network access anywhere in this pipeline. If the corpus is
  damaged, repair it with `scripts/vendor_corpus.py`, not by hand.
