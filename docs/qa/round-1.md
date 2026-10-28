# QA round 1 — decode & playback parity

**Date:** 2026-10-28 · **Tester:** Jarad Peckenpaugh · **Gate:** M1 (D-04) · **Verdict:** PASS

**What was tested:** `web/smoke.html` — vendored `.weba` → `rasp-opus` in WebAssembly → `AudioBufferSourceNode`. No DSP, no worklet, no resampler. The point of the round was to prove the transport before any processing exists, so that later artifacts are unambiguously DSP.

## Result

| Check | Result |
|---|---|
| Clips sound correct (voice, language, phrase) | PASS |
| No clicks, dropouts or truncation at boundaries | PASS |
| Level parity vs `dg-pp` for the same voice/phrase | PASS |
| `Decode all` — full corpus | **1404/1404, 0 failed** |
| Wall time for full decode + fetch | 2.53 s (~3 s observed) |
| Median decode | 1.10 ms/clip (1.08 ms per second of audio, 928× realtime) |
| `AudioContext.sampleRate` | 48000 Hz |
| Chrome | PASS |
| Safari | PASS |

Reference clip exercised in the round: `af_aoede/lets-play` — 49,444 frames, 1.030 s, 3.60 ms decode, peak 0.8506.

## Notes

- The browser's `AudioContext` ran at 48 kHz, i.e. the resampler path (D-03) was a short-circuit in this round. The 44.1 kHz path is still unverified in a real browser — it will be exercised in the worklet round (W-03/W-08) and again in the cross-browser matrix (X-04). Not a defect, just an unclosed loop.
- Firefox was not tested in this round. It is in the X-04 matrix.
- Headless Chrome (`node scripts/verify_wasm_decode.mjs`, 1404/1404) and the human round agree, which is the outcome we wanted from having two independent paths.

## Defects

None.

## Follow-ups

- None for this stage. The 44.1 kHz and Firefox gaps are already covered by X-04.
