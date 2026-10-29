//! `wasm-bindgen` surface for the RASP voice API (PLAN W-01, V-02).
//!
//! This crate is the thin boundary between JavaScript and the product code:
//! [`rasp_opus`] decodes a `.weba` clip, [`rasp_dsp::variant`] resolves the
//! game's seed to a corpus voice and a parameter set, and
//! [`rasp_dsp::NaturalRenderer`] applies the natural chain with the D22
//! headroom mapping and the P-08 loudness policy. No DSP, decode or policy
//! logic lives here.
//!
//! # `pp.livive.net` contract
//!
//! A Play Park NPC has one `recipe.seed` string. The host does:
//!
//! ```js
//! import init, { variant_schema, describe, render } from "./wasm/rasp.js";
//! await init();
//!
//! const spec = describe(npc.recipe.seed);
//! // { seed, voiceId, archetype, schema, pitchSt, formantRatio, tiltDbPerOct }
//!
//! const audio = render(webaBytes, npc.recipe.seed); // Float32Array @ 48 kHz
//! const buffer = ctx.createBuffer(1, audio.length, 48000);
//! buffer.copyToChannel(audio, 0);
//! ```
//!
//! * **`describe(seed)` is pure.** It hashes the string, picks one of the 54
//!   corpus voices and derives the parameters. It constructs no decoder, no
//!   renderer and no `AudioContext`, so the game can render a voice card before
//!   any audio exists.
//! * **`render(weba, seed)` takes the `.weba` bytes**, not PCM. The host never
//!   needs to know about Opus, the container or the preset chain: decode →
//!   derive → render happens in one call. Output is mono `f32` at 48 kHz (the
//!   corpus rate) and is exactly as long as the decoded clip, so it is
//!   sample-aligned with the source and safe for an A/B crossfade.
//! * **Determinism (D24).** The same seed string returns the same voice and
//!   parameters on every platform and every session. `describe(seed)` and
//!   `render(_, seed)` agree by construction: both call
//!   [`rasp_dsp::variant::derive_voice`].
//! * **Schema.** [`variant_schema`] is the version of the derivation contract.
//!   A host that needs permanence should store the returned `spec`; a schema
//!   bump may re-voice the cast and is a release note, not a silent change.
//!
//! # Memory ownership
//!
//! `describe` returns a plain JavaScript object owned by the JS garbage
//! collector. `render` returns `Vec<f32>`, which wasm-bindgen copies into a new
//! `Float32Array`; **the caller owns that array and there is no `free` handle
//! to release** (unlike the older [`Decoded`] object, whose Rust buffer is
//! freed explicitly). A caller may keep, transfer or cache the array without
//! touching the wasm allocator again.
//!
//! # Errors are typed values, never traps
//!
//! [`render`] returns `Result<Vec<f32>, JsValue>`. A failure throws a **string**
//! value with a stable code prefix, which the host can branch on without
//! parsing prose:
//!
//! | Prefix | Meaning |
//! |---|---|
//! | `DecodeError: …` | the `.weba` bytes are not a decodable WebM/Opus clip |
//! | `RenderError: …` | the DSP chain rejected a configuration or a render |
//!
//! Nothing in this crate panics on caller input and the crate is
//! `#![forbid(unsafe_code)]`. An empty seed string is valid (it is a seed like
//! any other); a missing seed argument is a JavaScript `TypeError` from the
//! generated glue, which is a host bug rather than audio input.
//!
//! # Legacy decode surface
//!
//! [`decode`] and [`Decoded`] are unchanged from D-04: the smoke page
//! (`web/smoke.html`) uses them to decode and play the unprocessed corpus. New
//! callers should prefer [`render`], which does not need the preset chain
//! exposed.

#![forbid(unsafe_code)]

use std::fmt;

use js_sys::{Error as JsError, Object, Reflect};
use rasp_dsp::variant::derive_voice;
use rasp_dsp::{NaturalRenderer, VARIANT_SCHEMA};
use rasp_opus::{DecodeError, DecodedClip};
use wasm_bindgen::prelude::*;

/// Version string for diagnostics. Contains the crate version and nothing that
/// depends on the registry or the build environment.
#[wasm_bindgen]
pub fn version() -> String {
    concat!("rasp-wasm ", env!("CARGO_PKG_VERSION")).to_string()
}

/// The derivation schema this build implements (D24).
///
/// Mix this value into any cache key. A different number means a different
/// `seed → voice + params` contract.
#[wasm_bindgen]
pub fn variant_schema() -> u16 {
    VARIANT_SCHEMA
}

/// Resolve a game seed to its rendered voice description (V-02).
///
/// Pure: no decoding, no DSP, no allocation beyond the returned object.
///
/// The result is `{ seed, voiceId, archetype, schema, pitchSt, formantRatio,
/// tiltDbPerOct }`:
///
/// * `seed` — the exact string passed in;
/// * `voiceId` — one of the 54 ids in `web/registry.json`;
/// * `archetype` — one of the twelve V-01 archetype ids (`masc-deep`,
///   `fem-airy`, `child`, …);
/// * `schema` — [`variant_schema`] at the time of the call;
/// * `pitchSt` / `formantRatio` / `tiltDbPerOct` — the **requested** point, on
///   the D24 quantisation grid. These are what the game shows and stores. The
///   renderer may scale them by the D22 headroom weight for a particular line;
///   that effective point is not part of the identity.
#[wasm_bindgen]
pub fn describe(seed: &str) -> Result<JsValue, JsValue> {
    let spec = derive_voice(seed);
    let object = Object::new();
    set(&object, "seed", &JsValue::from_str(seed));
    set(&object, "voiceId", &JsValue::from_str(spec.base_voice));
    set(
        &object,
        "archetype",
        &JsValue::from_str(spec.archetype.id()),
    );
    set(
        &object,
        "schema",
        &JsValue::from_f64(f64::from(spec.schema)),
    );
    set(
        &object,
        "pitchSt",
        &JsValue::from_f64(f64::from(spec.pitch_st)),
    );
    set(
        &object,
        "formantRatio",
        &JsValue::from_f64(f64::from(spec.formant_ratio)),
    );
    set(
        &object,
        "tiltDbPerOct",
        &JsValue::from_f64(f64::from(spec.tilt_db_per_oct)),
    );
    Ok(object.into())
}

/// Decode a `.weba` clip, derive its voice from `seed`, and render one line.
///
/// Returns mono `f32` samples at 48 kHz. The length equals the decoded clip's
/// frame count exactly (the natural chain neither trims nor pads), so the
/// output is sample-aligned with the source for A/B playback. A stereo input is
/// averaged to mono; the corpus is mono.
///
/// The chain is [`rasp_dsp::NaturalRenderer::d2`]: pitch/formant/tilt with D22
/// headroom scaling, then the P-08 RMS match and true-peak limiter. The caller
/// owns the returned array; see the crate docs.
#[wasm_bindgen]
pub fn render(weba: &[u8], seed: &str) -> Result<Vec<f32>, JsValue> {
    let clip = rasp_opus::decode_weba(weba).map_err(|err| failure("DecodeError", &err))?;
    let input = mono_samples(&clip);
    let spec = derive_voice(seed);

    let mut renderer =
        NaturalRenderer::d2(clip.sample_rate).map_err(|err| failure("RenderError", &err))?;
    renderer.set_params(spec.to_params());

    let mut output = Vec::with_capacity(input.len());
    renderer
        .process_into(&input, &mut output)
        .map_err(|err| failure("RenderError", &err))?;
    Ok(output)
}

/// A decoded clip: interleaved `f32` samples at 48 kHz.
#[wasm_bindgen]
pub struct Decoded {
    clip: DecodedClip,
}

#[wasm_bindgen]
impl Decoded {
    /// Sample rate of [`samples`](Self::samples), in Hz. Always 48000 today.
    pub fn sample_rate(&self) -> u32 {
        self.clip.sample_rate
    }

    /// Number of interleaved channels (1 = mono, 2 = stereo).
    pub fn channels(&self) -> u16 {
        self.clip.channels
    }

    /// Number of **frames** (samples per channel). Use this as `AudioBuffer`'s
    /// length; do not use the interleaved sample count.
    pub fn length(&self) -> usize {
        self.clip.frame_count()
    }

    /// Interleaved samples, `length() * channels()` long, as a `Float32Array`
    /// **copy**. The copy stays valid after [`free`](Self::free).
    pub fn samples(&self) -> Vec<f32> {
        self.clip.samples.clone()
    }
}

/// Decode a WebM/Opus (`.weba`) clip.
///
/// `bytes` must be the whole file. See the crate docs for the error surface.
#[wasm_bindgen]
pub fn decode(bytes: &[u8]) -> Result<Decoded, JsValue> {
    rasp_opus::decode_weba(bytes)
        .map(|clip| Decoded { clip })
        .map_err(decode_error)
}

/// Set a property on a fresh JS object. A freshly created object is extensible,
/// so this cannot fail; ignoring the result keeps the wasm boundary panic-free.
fn set(object: &Object, key: &str, value: &JsValue) {
    let _ = Reflect::set(object.as_ref(), &JsValue::from_str(key), value);
}

/// A coded error value: `"<Code>: <message>"`, never a trap.
fn failure(code: &str, message: &dyn fmt::Display) -> JsValue {
    JsValue::from_str(&format!("{code}: {message}"))
}

/// Downmix a decoded clip to the mono sample stream the natural chain takes.
fn mono_samples(clip: &DecodedClip) -> Vec<f32> {
    if clip.channels <= 1 {
        return clip.samples.clone();
    }
    let channels = usize::from(clip.channels);
    clip.samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Convert a typed `DecodeError` into a JS `Error` (never a trap).
fn decode_error(err: DecodeError) -> JsValue {
    let code = match &err {
        DecodeError::ContainerNotWebm => "ContainerNotWebm",
        DecodeError::NoAudioTrack => "NoAudioTrack",
        DecodeError::UnsupportedCodec(_) => "UnsupportedCodec",
        DecodeError::MalformedContainer(_) => "MalformedContainer",
        DecodeError::MalformedPacket(_) => "MalformedPacket",
        DecodeError::EmptyStream => "EmptyStream",
        DecodeError::Io(_) => "Io",
        // `DecodeError` is `#[non_exhaustive]`: a future variant surfaces as a
        // typed error with a generic code rather than a compile failure here.
        _ => "DecodeError",
    };

    let error = JsError::new(&err.to_string());
    error.set_name("RaspDecodeError");
    // `set` on a fresh Error cannot fail (the object is extensible); ignore the
    // result rather than panicking across the wasm boundary.
    let _ = Reflect::set(
        error.as_ref(),
        &JsValue::from_str("code"),
        &JsValue::from_str(code),
    );
    error.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_compiled_in() {
        assert_eq!(version(), concat!("rasp-wasm ", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn schema_is_the_dsp_schema() {
        assert_eq!(variant_schema(), VARIANT_SCHEMA);
    }

    #[test]
    fn render_keeps_length_and_is_finite() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../web/audio/af_heart/hello.weba");
        let Ok(bytes) = std::fs::read(&path) else {
            // The corpus is vendored in the repo; a shallow checkout may not
            // have it, so this is a printed skip rather than a false failure.
            println!("SKIP: {} is not present", path.display());
            return;
        };
        let clip = rasp_opus::decode_weba(&bytes).expect("corpus clip decodes");
        let rendered = render(&bytes, "npc-0").expect("render succeeds");
        assert_eq!(rendered.len(), clip.frame_count());
        assert!(rendered.iter().all(|sample| sample.is_finite()));
        assert!(rendered.iter().any(|sample| *sample != 0.0));
    }

    // The error path and `describe` are exercised through the real wasm module
    // by `crates/rasp-wasm/verify.mjs`. On the host, constructing a `JsValue`
    // panics by design ("function not implemented on non-wasm32 targets"), so a
    // native test cannot observe the typed error value; only the pure success
    // path is unit-tested here.
}
