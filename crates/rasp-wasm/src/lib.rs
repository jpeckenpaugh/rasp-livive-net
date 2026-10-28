//! Thin `wasm-bindgen` binding layer over [`rasp_opus`].
//!
//! This crate deliberately contains **no decoding, DSP, or policy logic**: it
//! converts a `&[u8]` into [`rasp_opus::decode_weba`] and exposes the result to
//! JavaScript. W-01 extends this surface (`init` / `render` / `free`) and owns
//! the render path; D-04 only needs decode-and-play.
//!
//! # JavaScript surface
//!
//! ```js
//! import init, { version, decode } from "./wasm/rasp.js";
//! await init();
//!
//! console.log(version()); // "rasp-wasm 0.1.0"
//! const clip = decode(bytes); // `bytes`: Uint8Array of a .weba file
//! clip.sample_rate();         // 48000
//! clip.channels();            // 1 or 2
//! clip.length();              // frames per channel (NOT interleaved samples)
//! clip.samples();             // Float32Array, interleaved, *a copy*
//! clip.free();                // release the Rust-side buffer
//! ```
//!
//! `samples()` returns a **copy** as a `Float32Array` (wasm-bindgen's default
//! for `Vec<f32>`), so the caller may keep it after `free()`. `length()` counts
//! frames — for mono it equals `samples().length`; for stereo it is half.
//!
//! # Errors
//!
//! `decode` returns `Result<Decoded, JsValue>`, which wasm-bindgen turns into a
//! thrown JavaScript value. Failures are never traps or aborts: the value is a
//! real `Error` object with
//!
//! - `name === "RaspDecodeError"`
//! - `code` set to a stable machine-readable variant name, one of
//!   `ContainerNotWebm`, `NoAudioTrack`, `UnsupportedCodec`,
//!   `MalformedContainer`, `MalformedPacket`, `EmptyStream`, `Io`
//!   (or `DecodeError` for a variant added later)
//! - `message` the same text as the Rust `Display` implementation.

use js_sys::{Error as JsError, Reflect};
use rasp_opus::{DecodeError, DecodedClip};
use wasm_bindgen::prelude::*;

/// Version string for diagnostics. Contains the crate version and nothing that
/// depends on the registry or the build environment.
#[wasm_bindgen]
pub fn version() -> String {
    concat!("rasp-wasm ", env!("CARGO_PKG_VERSION")).to_string()
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

    // The error mapping and `Decoded` accessors are exercised through the real
    // wasm module by `scripts/verify_wasm_decode.mjs`; on the host, `JsValue`
    // cannot be inspected (the JS glue does not exist), so only the pure parts
    // are unit-tested here.
}
