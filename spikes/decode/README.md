# D-01 decode spike (disposable)

Minimal harness for the Opus-in-WebM decode decision. One crate, four backends
behind mutually exclusive Cargo features, plus scripts that produce every number
in [`docs/spikes/decode.md`](../../docs/spikes/decode.md).

**This directory is a spike.** It is excluded from the root workspace
(`spikes/*`) and will be deleted once D-02 exists. Do not import it.

```
src/lib.rs        demux + per-backend decode + container framing
src/main.rs       native CLI: decode one clip, time it, write f32
compare.py        SNR / lag / sample-count comparison against a reference
fetch.sh          fetch registry + 12 clips, verify sha256, make references
run_native.sh     decode the 12 clips with the current build + compare
run_wasm.js       load a wasm-bindgen nodejs package, time, dump f32
```

## Prerequisites

- rustc/cargo 1.99.1, target `wasm32-unknown-unknown`
- `wasm-bindgen` CLI 0.2.129 (keep it equal to the crate pin in `Cargo.toml`)
- `ffmpeg`/`ffprobe` built with `--enable-libopus` (checked with `ffmpeg -decoders | grep libopus`)
- Node (tested on v26.12.0)
- `wasm-opt` from binaryen 132 (tested via `npm install binaryen@132`; the binary is at `node_modules/.bin/wasm-opt`)

The scripts default to `${RASP_SPIKE_DIR:-/tmp/rasp-spike}` for downloads and
outputs. Override with `RASP_SPIKE_DIR` or the positional argument.

## 1. Clips and references

```sh
cd spikes/decode
./fetch.sh
```

Fetches `registry.json` and these 12 clips, verifies every `sha256` and byte
count against the registry, then writes **two** references per clip:

- `<voice>/<phrase>.libopus.f32` — `ffmpeg -c:a libopus -i` (the reference)
- `<voice>/<phrase>.ffmpeg.f32` — `ffmpeg -i` (ffmpeg's native Opus decoder)

They are not the same waveform; see "Which reference" below.

Clip set: `af_heart/hello`, `af_heart/see-you`, `am_eric/good-luck`,
`am_fenrir/lets-play`, `bf_emma/catch-you-later`, `af_sarah/you-got-me`,
`pm_alex/nice-day`, `zf_xiaoni/greet`, `zm_yunxi/good-game`,
`jf_nezumi/see-you`, `if_sara/hello`, `hf_alpha/greet`.

## 2. Native measurements

Build one backend at a time; the features are meant to be exclusive
(`decode_with` is only reachable for the compiled one).

```sh
# winner
cargo build --release --no-default-features --features backend-opus-pure
./run_native.sh opus-pure

# ruopus directly (needs `spectrograms` for the FFT backend — see Cargo.toml)
cargo build --release --no-default-features --features backend-ruopus
./run_native.sh ruopus

# Symphonia adapter over ruopus
cargo build --release --no-default-features --features backend-adapter-opus-rs
./run_native.sh adapter-opus-rs

# LGPL, known-bad output; kept only so the table is reproducible
cargo build --release --no-default-features --features backend-oporus
./run_native.sh oporus
```

`run_native.sh` checks that `target/release/decode` was built with the tag you
passed (exits non-zero on mismatch), then writes f32, timing lines, and comparison JSON to
`$RASP_SPIKE_DIR/out/<tag>/`. `decode <clip> [out.f32] [--iters N]` can be run by
hand; it reports min and mean decode time over `N` iterations (default 20),
including demux.

Single comparison:

```sh
python3 compare.py "$RASP_SPIKE_DIR/af_heart/hello.libopus.f32" out.f32 --lag 3
```

`compare.py` searches integer lags in `[-3, +3]`, reports `snr_db` over the
overlapping region, the sample-count delta, and `max_abs_err`. Pure Python, no
numpy.

## 3. Wasm measurements

Build the library for wasm, run it through `wasm-bindgen --target nodejs`, then
timing is done in Node.

```sh
cargo build --release --target wasm32-unknown-unknown \
  --no-default-features --features backend-opus-pure,wasm --lib

wasm-bindgen --target nodejs --out-dir pkg-opus-pure \
  target/wasm32-unknown-unknown/release/rasp_decode_spike.wasm

node run_wasm.js pkg-opus-pure "$RASP_SPIKE_DIR/af_heart/hello.weba" 100 out.f32
```

The same works for `backend-adapter-opus-rs,wasm` with a different `--out-dir`.
`run_wasm.js` warms up once, times `N` calls with `process.hrtime.bigint()`,
reports min/mean and `ms_per_s`, and optionally writes the decoded f32.

### Size

```sh
# after wasm-bindgen
gzip -9 -c pkg-opus-pure/rasp_decode_spike_bg.wasm | wc -c

# after wasm-opt
node_modules/.bin/wasm-opt -Oz pkg-opus-pure/rasp_decode_spike_bg.wasm \
  -o pkg-opus-pure-opt.wasm
gzip -9 -c pkg-opus-pure-opt.wasm | wc -c
```

Copy `pkg-opus-pure-opt.wasm` over a copy of the package's `_bg.wasm` to confirm
the optimised module still decodes; `run_wasm.js` needs the JS shim next to it.

## 4. How framing works

The spike implements the two container trims that libopus/ffmpeg apply:

1. `pre_skip` (312 samples here): `u16` little-endian at offset 10 of the
   `OpusHead` in Matroska `CodecPrivate`.
2. `DiscardPadding` (`0x75A2`, big-endian signed nanoseconds, last occurrence in
   the file): `round(ns * 48_000 / 1e9)` samples.

`src/lib.rs::conform` applies them. The adapter backends already apply `pre_skip`
(via `Packet::trim_start`), so `pre_skip_applied` suppresses a second trim; the
trailing trim is applied in both cases because Symphonia's MKV demuxer parses but
does not expose `DiscardPadding`.

## Which reference

`ffmpeg -i` uses ffmpeg's **native** Opus decoder. It is not sample-aligned with
libopus (14.5–31.9 dB against libopus, and fractional alignment only reaches
21.5 dB). It is useless as a conformance reference. Use:

```sh
ffmpeg -v error -y -c:a libopus -i in.weba -ac 1 -ar 48000 -f f32le ref.f32
```

`compare.py` against libopus is what the results table reports.

## Notes

- `ruopus` without `spectrograms` is 23.6–48.2 ms/s native instead of
  0.86–0.98 ms/s: that feature selects the `rustfft`-backed MDCT, not just
  optional spectrogram output.
- `oporus` / `symphonia-adapter-oporus` are LGPL-2.1 and produce non-conformant
  PCM on these clips; they are in the tree only to document the rejection.
- The `libopus` route is not in this crate: `opus`/`opusic-sys` build libopus via
  CMake, and CMake is not installed. See the failure text in the results doc.
