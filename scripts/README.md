# scripts/

Tooling that is not part of the Rust workspace. Each script documents its own
runtime; `vendor_corpus.py` is deliberately Python-standard-library-only (no
virtualenv, no `pip install`).

## `vendor_corpus.py` — vendor the `dg-pp` corpus

`web/registry.json` and `web/audio/<voice_id>/<phrase_id>.weba` are a pinned
snapshot of the upstream `dg-pp` bake (PLAN §2 D6): 54 voices × 26 phrases =
1404 clips, 7,005,583 bytes, 8 languages. They are committed, not hot-linked.
This script is the only supported way to build or repair that snapshot.

```sh
python3 scripts/vendor_corpus.py                 # vendor: reuse good clips, fetch missing/bad ones
python3 scripts/vendor_corpus.py --check         # CI: verify the committed snapshot offline
python3 scripts/vendor_corpus.py --refresh-registry  # human: pick up a new upstream bake
python3 scripts/vendor_corpus.py --compare-upstream  # see upstream drift, change nothing
```

Everything is relative to this repository; the script can be run from any
directory and writes to `<repo>/web` unless `--root` says otherwise.

### Default mode (vendor)

1. Reads the committed `web/registry.json` as the authority. If it does not
   exist, the registry is fetched from upstream and written after all clips
   have been verified.
2. Hashes every local clip and reuses only exact matches (size **and**
   `sha256`). Missing, truncated, oversized, or corrupted files are
   re-downloaded.
3. Downloads with at most **8** concurrent workers (hard cap; `--jobs` can
   lower it, never raise it). Transient network errors, HTTP 429, and 5xx are
   retried up to 3 attempts with 0.5 s / 1 s exponential backoff. A 404 or a
   size/hash mismatch fails immediately and loudly: the origin and the
   registry disagree, so retrying cannot help.
4. Writes to a temporary file in the destination directory, `fsync`s, then
   `os.replace()`s it into place. A failed or interrupted download can never
   leave a partial `.weba` under `web/audio/`.
5. Re-verifies the whole corpus, then asserts `build.totals` (`clips`,
   `voices`, `phrases`, `bytes`) against both the manifest and the bytes on
   disk. Any disagreement is a fatal error.
6. Prints a throttled progress summary (4 lines when piped, an updating line
   on a TTY) and a final tally: voices, phrases, clips, bytes, reused,
   downloaded, elapsed.

Running it twice is a no-op: the second run reports `reused 1404`,
`downloaded 0`, and exits 0.

### `--check` (alias `--verify-only`) — the CI entry point

Fully offline. Verifies, against `web/registry.json`:

- every registry path is the canonical local path
  (`audio/<voice_id>/<phrase_id>.weba`),
- every referenced file exists,
- every file's byte count matches `bytes`,
- every file's SHA-256 matches `sha256`,
- no unexpected `.weba` files exist under `web/audio/`,
- `build.totals` matches the registry and the disk.

On success it prints one line and exits 0. On any drift it prints a grouped,
file-level diff (capped at 40 entries per group, with a remainder count), exits
1, and changes nothing.

### `--refresh-registry` — human-driven re-vendor

Fetches `registry.json` from upstream, validates it, rewrites each clip
`path` to the local relative form, reports the diff against the committed
registry (build id, added/removed/changed clips), then downloads and verifies
the corpus. The new `web/registry.json` is written only after every clip it
references is present and verified, so the committed registry never points at
missing audio. Review the registry diff like any other data change.

### `--compare-upstream` — drift sentinel

Fetches the upstream registry and compares it with the committed snapshot
(build id, per-clip `sha256`/`bytes`/`durationMs`, additions and removals).
Exit 0 means the snapshots match; exit 1 means upstream was rebuilt. It never
touches local files. Useful as a scheduled job: it is the only mode that can
see the origin, since `--check` is offline by contract.

### `--prune`

Vendoring warns about `.weba` files under `web/audio/` that the registry does
not list. `--prune` deletes them after the rest of the corpus verifies. Only
use it during a deliberate re-vendor.

### Drift story

The corpus is a **pinned snapshot**, not a mirror.

- **Local damage** (bad merge, interrupted checkout, accidental edit, a tool
  rewriting a clip): `--check` fails and names each file. Re-run the default
  mode; it re-downloads only the broken clips.
- **Upstream bake rebuilt** (new hashes, new build id): `--check` cannot see
  this because it is offline. `--compare-upstream` reports it (read-only), or
  a human runs `--refresh-registry`. The refreshed registry is a reviewable
  diff: build id, added/removed clips, changed hashes. If a refresh is
  interrupted after the registry is updated but before every clip lands,
  `--check` fails and names the clips still on the old bake; re-run vendoring
  to finish.
- **Never** fix drift by editing `web/registry.json` by hand. Change the
  snapshot by re-vendoring, then review and commit the diff.

### CI (F-07)

```sh
python3 scripts/vendor_corpus.py --check
```

No network, no dependencies, about 0.1 s for 7 MB. Exit 0 is green. This
guards the committed corpus against corruption and ensures the 1404 files /
7,005,583 bytes stay intact. `--compare-upstream` is deliberately **not** part
of required CI: a rebake upstream must not turn every pull request red. Run it
on a schedule or on demand instead.

### Notes

- Upstream paths are already `audio/<voice_id>/<phrase_id>.weba`, which is
  also the local layout relative to `web/`, so the vendored registry is
  currently byte-identical to upstream. The rewrite is still applied and
  enforced: the local path is derived from the voice/phrase ids, never copied
  blindly, so an upstream path change cannot smuggle a remote or absolute URL
  into the app.
- `web/audio/` must stay committed (D6). It is intentionally absent from
  `.gitignore`.
- `du -sh web/audio` reports ~9.7 MB on APFS because of per-file allocation
  (1404 × 4 KB blocks); the payload is 7,005,583 bytes and `du -sk` reports
  ~9,900 KB. Use the registry totals or `--check` for the authoritative size.
- The origin is third-party but static. Keep concurrency at 8; the cap is not
  a tuning knob.

## `make_fixtures.py` — generate the F-05 audio fixtures

`fixtures/` holds the twelve audio clips that the native and wasm audio tests
decode (PLAN F-05, DECISIONS D13 as amended by F-06). They are **derived**
audio, so they are generated on demand and gitignored: PLAN §8 forbids committed
fixtures over 1 MB, and this set is ~6.2 MiB. Only `fixtures/README.md` and the
`.gitkeep` placeholder are tracked.

```sh
python3 scripts/make_fixtures.py           # build missing/stale fixtures, reuse current ones
python3 scripts/make_fixtures.py --force   # rebuild all 36 files
python3 scripts/make_fixtures.py --out target/fixtures   # write somewhere else
```

Standard library only (Python 3.8+); the only external tool is `ffmpeg`, which
must be built with libopus. The script never touches the network.

### What it writes

| path | content |
|---|---|
| `fixtures/wav/<voice>__<phrase>.wav` | 48 kHz mono 16-bit PCM, for listening and WAV-only tools |
| `fixtures/f32/<voice>__<phrase>.f32` | 48 kHz mono raw little-endian f32, headerless |
| `fixtures/reference/<voice>__<phrase>.f32` | same, decoded by libopus (`ffmpeg -c:a libopus`) |
| `fixtures/manifest.json` | per clip: source URL, registry sha256/bytes, source hash, sample count, duration, phrase group, hashes of the three outputs, and why the clip was chosen |

`reference/` and `f32/` are **not** interchangeable. `-c:a libopus` selects the
libopus decoder; plain `ffmpeg -i` uses FFmpeg's native Opus decoder, which is a
different waveform and not sample-aligned (D-01, `docs/spikes/decode.md`). Only
`reference/` is a valid oracle for D-02.

Decoding is whole-clip and untrimmed: leading/trailing silence stays, because
P-04 has to trim it. The three outputs of a clip always have the same sample
count; the script fails if the decoders disagree.

### Integrity and idempotency

Before decoding, every chosen clip's `sha256` and byte count are checked against
`web/registry.json`. A missing file, size mismatch or hash mismatch exits 1 with
the exact repair command (`python3 scripts/vendor_corpus.py`, then `--check`)
and decodes nothing.

A run with nothing to do prints the table with every row `cached`, decodes
nothing, writes nothing and exits 0. The manifest stores the source hash,
the `ffmpeg <version> + libopus` signature and each output's hash, so a clip is
rebuilt when the source changes, when ffmpeg changes (the reference bytes are
decoder-specific), or when an output is missing/edited. `--force` ignores the
cache.

Selection is pinned in `SELECTION` next to the reasons, and the script enforces
the F-05 coverage contract on every run: exactly 12 clips, one per voice, every
phrase group represented, and short (~0.4 s), median (~1 s) and long (≥ 1.9 s)
material present.

`fixtures/README.md` documents the clip table and the manifest fields;
`docs/testing.md` documents the contract for Rust tests that consume them.

### CI (F-07)

```sh
python3 scripts/make_fixtures.py
python3 -c "import json;d=json.load(open('fixtures/manifest.json'));assert len(d['clips'])==12"
```

`.github/workflows/ci.yml`'s `fixtures-smoke` job runs this (with a tolerant
manifest check) on `ubuntu-latest`, whose ffmpeg has libopus, against the
vendored corpus.
