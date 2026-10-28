# `rasp-opus`

WebM/Opus (`.weba`) decode for RASP: Symphonia's Matroska demuxer plus
`opus-pure`, producing interleaved `f32` at 48 kHz with libopus-compatible
framing.

```rust
use rasp_opus::decode_weba;

let clip = decode_weba(&bytes)?;          // bytes = the .weba file
assert_eq!(clip.sample_rate, 48_000);
assert_eq!(clip.channels, 1);
// clip.samples is interleaved: [frame0_ch0, frame0_ch1, frame1_ch0, ...]
```

`decode_weba_file(path)` is the same thing for native callers. It compiles for
`wasm32-unknown-unknown` but always returns `DecodeError::Io` there; browser code
(W-01) must fetch the bytes and call `decode_weba`.

## Framing (the point of this crate)

Two trims reproduce libopus/`ffmpeg -c:a libopus` exactly:

1. OpusHead `pre_skip` samples from the front (312 for this corpus).
2. Matroska `DiscardPadding` (`0x75A2`, signed big-endian nanoseconds) from the
   back, converted with `round(ns * 48_000 / 1e9)`.

`DiscardPadding` is a trim of the **final packet**, not of the whole stream.
libopus/ffmpeg do not clamp an oversized value — they ignore the discard
entirely. Measured on `af_heart/greet` (20 ms, 960-sample packets): a value of
960 samples drops the final packet, while 961 drops nothing. The decode loop
records the last packet's frame count and applies the discard only when it fits
inside that packet (D-02b). Values above the packet size therefore reproduce
libopus rather than diverging by the difference.

Symphonia 0.6.1 parses `DiscardPadding` into an internal field and then drops
it (`Packet::trim_end` is never set by the MKV demuxer), so the crate walks the
EBML master elements itself (`Segment` → `Cluster` → `BlockGroup`). That is a
structural walk, not a byte scan, so packet payload bytes cannot be mistaken for
the element. `pre_skip` is clamped to the decoded length; a stream that trims
to nothing is `DecodeError::EmptyStream`.

Registry `durationMs * 48` is **not** a sample-count oracle:
`durationMs * 48 - decoded_samples` ranged from −342 to −1308 samples on the
D-01 clips (7.1–27.2 ms). Use the decoded buffer length.

## Error surface

`DecodeError` distinguishes `ContainerNotWebm`, `NoAudioTrack`,
`UnsupportedCodec` (with a static reason: not Opus, not 48 kHz, >2 channels,
mapping family ≠ 0, …), `MalformedContainer`, `MalformedPacket`, `EmptyStream`,
and `Io` (file reads only). No `unwrap`/`expect`/`panic!` on input-derived
values, and `#![forbid(unsafe_code)]`; `opus-pure`'s internals stay behind safe
calls.

`ContainerNotWebm` means one thing: the input is non-empty and its first four
bytes are not the EBML magic (`1A 45 DF A3`). A truncated or otherwise
unreadable input that does carry the magic is `MalformedContainer`, never
"not WebM" (D-02c). Empty input is `EmptyStream`. The 3,421-length truncation
sweep of `af_heart/greet.weba` yields 1 success (the full clip), 1
`EmptyStream`, 3 `ContainerNotWebm` (partial magic only) and 3,416
`MalformedContainer`.

Channels: mono and stereo (mapping family 0) are supported and interleaved as
documented; more than two channels are rejected rather than silently downmixed.
The corpus is mono, but nothing assumes it. No resampling: output is always
48 kHz (D-03 owns arbitrary rates).

## Measured (D-02, 2026-10-28)

Machine: macOS arm64 (Apple M-series), rustc 1.99.1, ffmpeg 9.0.2 / libopus
1.6.1. Decode is compared against `ffmpeg -v error -c:a libopus -i <file> -ac 1
-ar 48000 -f f32le -` (plain `ffmpeg -i` is a different decoder and is **not** a
valid oracle).

| Check | Result |
|---|---|
| Reference clips (54 `hello` + 6 shortest + 6 longest) | **66/66** exact sample-count match, delta **+0…+0** |
| SNR vs libopus | **79.39–89.54 dB** (target > 60 dB) |
| Duration vs reference | 0.000 ms difference (sample-identical length) |
| NaN/Inf | none |
| Peak | 0.892 max on the 66-clip set; 0.952 max over all 1,404 |
| Full corpus (`--ignored`) | **1404/1404** decode, 1,433.0 s of audio, 18,900–104,606 frames/clip |
| Throughput, 1.075 s clip, 100 iters × 4 runs | **0.64–0.69 ms/s** best (0.71–0.76 mean) |
| Throughput, 2.179 s clip, 100 iters × 4 runs | **0.65 ms/s** best (0.69–0.77 mean) |
| `wasm32-unknown-unknown` release rlib | 57,624 B |
| Minimal cdylib consumer, wasm | 466,750 B raw / 180,054 B gzip; after `wasm-opt -Oz` 365,442 B / 165,926 B gzip |

`ms/s` is wall time to decode one second of audio, in-memory, native release.
The wasm figures come from a temporary `cdylib` shim calling `decode_weba` (no
`wasm-bindgen`; W-01 owns the real bindings and W-02 the budget). The crate uses
`std::fs` only in `decode_weba_file`, no `std::time`, and no threads; the wasm
build of all targets is clean.

## Tests

```text
cargo test -p rasp-opus                        # unit + 2.6 s corpus subset
cargo test -p rasp-opus -- --ignored           # full 1,404-clip pass (~25 s debug)
```

The corpus tests walk `web/audio/` (no committed fixtures) and skip with a
printed notice if the corpus or `ffmpeg`/`libopus` is missing. They include the
patched-`DiscardPadding` oracle (960/961/1200/5000 samples against
`ffmpeg -c:a libopus`, D-02b) and the full truncation sweep (every prefix of a
real clip, D-02c). Malformed-input cases (empty, random, RIFF, truncated at
every length, corrupted tail) must return a typed error, never a panic.

## Known limits

Tested only on this corpus: 32 kbps VBR mono speech, 20 ms frames. CELT-only or
hybrid music, DTX, packet loss, and non-20 ms frame durations are untested. The
decoder is accurate to ~1 i16 LSB, not bit-exact; a null test against libopus
will not be zero. The `DiscardPadding` scan takes the last element in the file
and is not track-aware; that is correct for this single-track corpus and would
need revisiting for multi-track WebM. Only that final element is considered, so
a file with per-block (mid-stream) padding would need per-packet association;
the corpus has none, and the full 1,404-clip digest is unchanged by the D-02b
rule (max corpus padding 956 samples < 960).

## Follow-ups

- **D-04:** fetch `web/registry.json`, decode each clip through this crate; the
  full-corpus test above is the native counterpart. Use the decoded length, not
  `durationMs`, for timing/display.
- **W-01:** call `decode_weba(&[u8])`; do not expose `decode_weba_file`. Add the
  `wasm-bindgen` surface around `DecodedClip` (channels/samples) and keep the
  typed `DecodeError` instead of trapping. A streaming `Decoder` (packet-at-a-
  time) is not provided yet; add it only when a task needs it.
- If a future Symphonia exposes `DiscardPadding` on `Packet::trim_end`, delete
  the EBML walk and use it — the corpus tests are the guard.
