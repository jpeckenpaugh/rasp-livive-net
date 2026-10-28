# D-01 — Opus-in-WebM decode spike

**Status:** done · **Date:** 2026-10-28 · **Timebox:** one session
**Question:** can we decode the corpus (Opus in WebM, 48 kHz mono, 20 ms frames, ~32 kbps) in pure Rust on `wasm32-unknown-unknown`, and which crate?

**Answer: yes.** `symphonia-format-mkv` + `opus-pure` decodes all 12 spike clips with a sample count identical to libopus, 82.55–87.40 dB SNR, and decode is not a budget item.

---

## Recommendation

**Winner: `symphonia` 0.6.1 (`mkv` feature only) for demux + `opus-pure` 0.2.2 for raw-packet decode.**

**Fallback: `symphonia-adapter-opus-rs` 0.1.1 (wraps `ruopus` 0.1.2).** Same decoded samples, ~14% slower in wasm, +133 KiB after `wasm-opt -Oz`, one more layer of new code. Take it if `opus-pure` breaks on a corpus clip that the adapter handles.

Do **not** take `oporus` / `symphonia-adapter-oporus` (wrong output, and LGPL-2.1), or `libopus` (toolchain not present).

### Why `opus-pure` and not the adapter

| | `opus-pure` | `symphonia-adapter-opus-rs` |
|---|---|---|
| SNR vs libopus, 12 clips | 82.55–87.40 dB | 82.55–87.40 dB |
| Sample-count delta vs libopus | 0 / 12 | 0 / 12 |
| Native median | **0.68 ms/s** | 0.76 ms/s |
| Wasm median (Node) | **0.91 ms/s** | 1.04 ms/s |
| Wasm after `wasm-opt -Oz` | **466.6 KiB** (199.8 KiB gzip) | 599.6 KiB (249.0 KiB gzip) |
| Dependency crates in tree | 14 | 42 |
| Code we own for framing | ~40 lines (pre-skip + discard padding) | ~20 lines (discard padding only) |

The winner is smaller, faster, has one third of the dependency surface, and its per-packet API makes the framing explicit instead of hiding it in an adapter that silently ignores the container's trailing padding.

---

## Critical finding: the default `ffmpeg` output is not a conformance reference

`ffmpeg -i in.weba ...` uses **ffmpeg's native Opus decoder**. It does not produce the same waveform as libopus:

| Reference pair (12 clips) | SNR | Best integer lag |
|---|---|---|
| `opus-pure` vs **libopus** (`ffmpeg -c:a libopus -i`) | 82.55–87.40 dB | 0 |
| ffmpeg native (`ffmpeg -i`) vs **libopus** | 14.49–31.88 dB | 0…2 |

Fractional (windowed-sinc) alignment of ffmpeg native against libopus peaks at **21.5 dB**, so it is not a constant delay — it is a different SILK resampler / filterbank. The two decoders are not comparable sample-for-sample.

**Consequence for D-02:** its acceptance criterion "SNR > 60 dB vs `ffmpeg` reference" must name the decoder explicitly:

```
ffmpeg -v error -c:a libopus -i in.weba -ac 1 -ar 48000 -f f32le ref.f32
```

With ffmpeg's native decoder no conformant implementation can pass 60 dB. This is a wording fix to D-02, not a blocker.

### Exact framing rule (verified on all 12 clips)

1. OpusHead `pre_skip` (312 samples here) is dropped from the front.
2. The Matroska `DiscardPadding` element (`0x75A2`, big-endian signed nanoseconds) is dropped from the back, converted as **round(ns × 48 000 / 1e9)** samples.

That reproduces ffmpeg/libopus sample counts exactly (delta 0 on 12/12). `durationMs` from the registry is **not** a sample-count oracle: `durationMs × 48 − decoded samples` ranged from **−342 to −1308** samples (−7.1 ms to −27.2 ms) across the 12 clips.

The MKV demuxer parses `DiscardPadding` but does not expose it (`segment.rs` stores it, `demuxer.rs` never sets `Packet::trim_end`). D-02 needs a 20-line EBML scan for the last `0x75A2`, or an upstream patch.

---

## Results

Environment: macOS arm64 (Apple M-series), rustc 1.99.1, Node v26.12.0, wasm-bindgen 0.2.129, wasm-opt 132, ffmpeg 9.0.2 with libopus 1.6.1. Native builds `--release` with `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`. 12 clips, 0.39–1.20 s decoded, 2.2–5.8 KB each. Native = 100 iterations, min; wasm = 100 iterations, min, in Node through `wasm-bindgen --target nodejs`. `ms/s` is milliseconds to decode one second of audio (lower is better).

| Candidate | WebM demux | Correctness vs libopus | Native ms/s (median, range, 100 iters) | Wasm ms/s (median, range) | Wasm KiB, bindgen → `-Oz` (gzip) | Crate health |
|---|---|---|---|---|---|---|
| **`opus-pure` 0.2.2** + `symphonia-format-mkv` 0.6.1 | Symphonia | **82.55–87.40 dB**, Δ0 samples, max abs err 1.5e-05 | **0.68** (0.66–0.72) | **0.91** (0.88–0.95) | **575.4 → 466.6** (213.7 → 199.8) | BSD-3-Clause · 2026-09-28 · 47,941 downloads · 134 `unsafe` sites · 14 deps |
| `ruopus` 0.1.2 + `symphonia-format-mkv` (direct) | Symphonia | 82.55–87.40 dB, Δ0 | 0.90 (0.86–0.98) | not built (same decoder as the adapter row) | — | MIT · 2026-07-01 · 9,523 downloads · 45 `unsafe` sites · 41 deps |
| `symphonia-adapter-opus-rs` 0.1.1 (→ `ruopus` 0.1.2) | Symphonia | 82.55–87.40 dB, Δ0 | 0.76 (0.73–0.80) | 1.04 (1.00–1.08) | 748.6 → 599.6 (276.1 → 249.0) | MIT OR Apache-2.0 · 2026-08-02 · 497 downloads · 0 `unsafe` · 42 deps |
| `symphonia-adapter-oporus` 0.1.3 (→ `oporus` 0.2.4) | Symphonia | **FAIL: −9.19…+11.15 dB**, output clips at ±1.0 | 0.88 (0.86–0.94) | not attempted | — | **LGPL-2.1** · 2026-06-07 · 2,147 downloads · 0 `unsafe` · 17 deps |
| `libopus` via `opus` 0.4.0 / `opusic-sys` 0.7.6 | n/a | not reached | **build fails** | **build fails** | — | MIT/Apache-2.0 · 2026-08-23 · 3.4M downloads · bundled build needs cmake |

### Notes on the numbers

- **`opus-pure` correctness.** All 12 clips are sample-aligned at lag 0, with the same length as libopus. The residual is one i16 LSB (`max_abs_err` 1.5e-05–1.6e-05), i.e. the decoder's 16-bit output path, not a framing error. This clears D-02's 60 dB target by 22 dB.
- **`ruopus` = `opus-pure` output** to within the same LSB, but it needs the misleadingly named `spectrograms` feature: without it `celt/mdct.rs` falls back to an O(n²) DFT and native decode drops to **23.6–48.2 ms/s** (a 24–56× regression against the same crate with `spectrograms` on). With `spectrograms` it pulls in `spectrograms` + `rustfft`.
- **`wasm-opt -Oz` does not change decode speed measurably**: on the same clip, 0.99–1.05 ms/s optimised vs 0.96–1.00 ms/s unoptimised across repeated 100-iteration runs.
- **Timing noise.** These are min-of-100 on a shared machine. Repeated identical runs varied by up to ±10%; a single 20-iteration run was once 3× slower (CPU contention). Treat differences below 15% as noise.
- **Native vs wasm output is not byte-identical.** `max_abs_err` 5.96e-08, SNR 149–165 dB, 6,640/27,148 samples differ in low bits. This is float math (sin/cos and rounding), not a decoder difference.
- **`oporus` output is wrong, not just quiet.** The first 8 samples match, then the waveform diverges and clips. Its adapter converts i16 to f32 correctly (`/32768`), so the decoder itself is producing non-conformant PCM on these streams. It also fails on license (LGPL-2.1 static-linked into a wasm blob for an MIT project), so it was not debugged further.
- **Wasm size** is the `wasm-bindgen`-processed module (what ships), before and after `wasm-opt -Oz`; gzip is deterministic gzip level 9. Neither includes the JS shim (2.9 KB).

### Why `libopus` was not pursued

`opus` 0.4.0 → `opusic-sys` 0.7.6 builds the vendored libopus through CMake. On this machine:

```
thread 'main' panicked at cmake-0.1.58/src/lib.rs:1132:
failed to execute command: No such file or directory (os error 2)
is `cmake` not installed?
```

This fails for the **native** target too, before wasm is relevant. `cmake`, `emcc`, and `wasm-ld` are not on `PATH`; Apple clang 21.0.0 does have the wasm32 backend, so a hand-rolled `cc` build is conceivable, but there is no installed wasm linker/archiver workflow and it is outside the timebox. `audiopus` 0.2.0/0.3.0-rc.0 is the same CMake story and was not attempted. Rejected on toolchain, not on merit.

---

## Winner: exact dependencies and call sequence

### `Cargo.toml`

```toml
[dependencies]
symphonia = { version = "0.6.1", default-features = false, features = ["mkv"] }
opus-pure = "0.2.2"

# wasm surface only; keep the crate version exactly equal to the CLI version.
wasm-bindgen = "=0.2.129"
```

Leave `opus-pure` at its defaults (it enables no features); the encoder, Ogg reader, and DRED/PLC code are not needed. `default-features = false` on Symphonia keeps FLAC/Vorbis/WAV/ADPCM out of the wasm.

### Decode sequence

```rust
use opus_pure::{OpusDecoder, MAX_PACKET_SAMPLES};   // MAX_PACKET_SAMPLES = 5760
use symphonia::core::formats::TrackType;

// 1. Probe the .weba. codecs registry is not needed: we decode packets ourselves.
let mut format = symphonia::default::get_probe()
    .probe(&hint, mss, Default::default(), Default::default())?;

// 2. Audio track + codec private data.
let track  = format.default_track(TrackType::Audio).ok_or(Error::NoTrack)?;
let params = track.codec_params.as_ref().unwrap().audio().unwrap();
let channels = params.channels.as_ref().unwrap().count();       // 1 for the corpus
let pre_skip = opus_head_pre_skip(params.extra_data.as_deref().unwrap_or(&[]));

// 3. One decoder for the whole clip.
let mut dec = OpusDecoder::new(48_000, channels)?;
let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
let mut pcm = Vec::new();

while let Some(packet) = format.next_packet()? {
    if packet.track_id != track.id { continue; }
    let n = dec.decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)?; // samples per channel
    pcm.extend_from_slice(&block[..n * channels]);
}

// 4. Container framing. Both trims are mandatory for a sample count that
//    matches libopus/ffmpeg.
let discard = (discard_padding_ns(&bytes) as u128 * 48_000 + 500_000_000) / 1_000_000_000;
let start = pre_skip * channels;
let end = pcm.len() - discard as usize * channels;
let out = &pcm[start..end];                                         // 48 kHz f32
```

`opus_head_pre_skip` reads the little-endian `u16` at byte offset 10 of the 19-byte `OpusHead` stored in Matroska `CodecPrivate`. `discard_padding_ns` is the last `0x75A2` element in the file, big-endian signed, in nanoseconds.

For stereo, the same code works with `channels = 2`; the corpus is mono.

---

## Known risks

1. **`opus-pure` is new.** First release 2026-08-27, two releases, 47,941 downloads, one maintainer. It is the youngest crate in the tree. Mitigation: the fallback adapter produces the same samples; D-02's tests run all 1,404 clips.
2. **134 `unsafe` sites in `opus-pure`.** Concentrated in `celt/mdct.rs`, `celt/bands.rs`, `celt/kiss_fft.rs`, `celt/pvq.rs`, plus SILK resampler/NSQ code; x86_64 AVX2 and aarch64 NEON kernels are `cfg`-gated. On `wasm32` the scalar paths compile and the observed cost is only 1.5× the native SIMD path. There is no `forbid(unsafe_code)`.
3. **16-bit output resolution.** `opus-pure` matches libopus to one i16 LSB, not bit-exactly. 82–87 dB is far above the 60 dB target; the 1-LSB dither is inaudible at 32 kbps source quality, but a "null test vs libopus" would not be zero.
4. **`DiscardPadding` is not exposed by Symphonia 0.6.1.** The byte scan is a spike-grade workaround with a real (small) parsing risk if a future encoder writes the element differently. The 1,404-clip count check in D-02 is the guard.
5. **Only 32 kbps mono speech was tested.** CELT-only/music, higher bitrates, DTX, packet loss, and 10/40/60 ms frames are untested. The corpus does not contain them, but `rasp-opus` should not claim general Opus support.
6. **wasm-bindgen version coupling.** The crate pin must equal the installed CLI (0.2.129 here) or the generated glue will not match.
7. **Fallback is not free.** `symphonia-adapter-opus-rs` 0.1.1 has 497 downloads and one release; it hard-codes a 20 ms default buffer and resizes on mismatch. It is a fallback, not a safer primary.

---

## What D-02 (`rasp-opus`) should implement

1. **Crate surface:** `decode_weba(&[u8]) -> Result<Vec<f32>, DecodeError>` returning interleaved 48 kHz f32, plus a lower-level `Decoder` that accepts packets for future streaming. No panics: every `probe`, `next_packet`, and `decode` error becomes a typed `DecodeError`.
2. **Demux:** Symphonia `mkv` only. Reject non-Opus tracks and non-48 kHz OpusHead rates explicitly.
3. **Framing:** pre-skip from `CodecPrivate`, then trailing `DiscardPadding` with `round(ns × 48 000 / 1e9)`. Do **not** use `durationMs` from the registry for this.
4. **Tests:** the 12 fixtures, asserting (a) sample count equals the libopus reference exactly, (b) SNR ≥ 60 dB vs the libopus reference, (c) no panic on truncated/corrupt packets. Generate references with `ffmpeg -c:a libopus -i`, never plain `ffmpeg -i`.
5. **Corpus pass:** decode all 1,404 clips; assert every output is finite, 48 kHz, and within ±1 sample of the container duration implied by packets − pre-skip − discard. This is the guard for risk 4.
6. **Do not vendor `opus-pure`'s encoder, Ogg reader, or DRED/PLC features.** Keep the dependency default-featured off.

---

## Reproduction

See [`spikes/decode/README.md`](../../spikes/decode/README.md). Every number above is produced by `spikes/decode/fetch.sh`, `run_native.sh`, `run_wasm.js`, and `compare.py`.
