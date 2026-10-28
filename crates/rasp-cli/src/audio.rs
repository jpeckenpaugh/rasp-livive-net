//! Audio input/output: `.weba` decode, raw `.f32`, RIFF WAV read/write.
//!
//! The CLI's contract is **48 kHz mono**, matching the corpus (PLAN K4) and
//! every metric in the project. Inputs at another rate or channel count are
//! rejected with a diagnostic rather than silently resampled: resampling is
//! D-03's job and it belongs in the product path, not in a measurement tool.
//!
//! Output is deterministic by construction: raw little-endian `f32`, or a RIFF
//! header with no timestamp or vendor chunk followed by fixed-width samples.
//! The 16-bit path rounds half away from zero and clamps, so it cannot wrap.
//! There is no dither — dither needs a random source, and byte-identical
//! re-renders are a P-06 acceptance criterion.

use std::fs;
use std::path::Path;

use rasp_opus::{decode_weba, SAMPLE_RATE_HZ};

/// The only sample rate this CLI accepts or writes: the decoder's native rate,
/// which is also the corpus rate (PLAN K4).
pub const SAMPLE_RATE: u32 = SAMPLE_RATE_HZ;

/// A loaded 48 kHz mono input.
#[derive(Debug, Clone)]
pub struct Input {
    /// Mono samples.
    pub samples: Vec<f32>,
    /// Always [`SAMPLE_RATE`]; kept explicit so reports can state it.
    pub sample_rate: u32,
    /// Always 1.
    pub channels: u16,
    /// Where the samples came from: `"weba"`, `"f32"` or `"wav"`.
    pub kind: &'static str,
}

/// Output formats a render can be written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutFormat {
    /// 16-bit PCM WAV. The listening default.
    Wav16,
    /// 32-bit IEEE-float WAV.
    WavF32,
    /// Headerless little-endian `f32`. The metrics default.
    RawF32,
}

impl OutFormat {
    /// Every format, in help-text order.
    pub const ALL: [OutFormat; 3] = [OutFormat::Wav16, OutFormat::WavF32, OutFormat::RawF32];

    /// The id accepted by `--format`.
    pub const fn id(self) -> &'static str {
        match self {
            OutFormat::Wav16 => "wav16",
            OutFormat::WavF32 => "wavf32",
            OutFormat::RawF32 => "f32",
        }
    }

    /// The file extension used with `--out-dir`.
    pub const fn extension(self) -> &'static str {
        match self {
            OutFormat::Wav16 | OutFormat::WavF32 => "wav",
            OutFormat::RawF32 => "f32",
        }
    }

    /// Parse a `--format` value.
    pub fn parse(text: &str) -> Option<Self> {
        OutFormat::ALL
            .into_iter()
            .find(|format| format.id() == text)
    }

    /// All ids, for diagnostics.
    pub fn ids() -> String {
        OutFormat::ALL
            .iter()
            .map(|format| format.id())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Load a `.weba`, raw `.f32`, or WAV file as 48 kHz mono.
///
/// Format detection is by content first (RIFF magic, EBML magic), then by
/// suffix, so a misnamed file still loads correctly and an unrecognised one
/// fails with a clear message.
pub fn load(path: &Path) -> Result<Input, String> {
    if !path.exists() {
        return Err(format!("{}: no such file", path.display()));
    }
    if !path.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    let bytes = fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{}: empty file", path.display()));
    }

    if is_riff_wave(&bytes) {
        return parse_wav(&bytes, path);
    }

    let suffix = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let is_ebml = bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]);
    if is_ebml || suffix == "weba" || suffix == "webm" {
        let clip = decode_weba(&bytes).map_err(|err| format!("{}: {err}", path.display()))?;
        return mono_48k(clip.samples, clip.sample_rate, clip.channels, "weba", path);
    }

    if matches!(suffix.as_str(), "f32" | "f32le" | "raw" | "pcm") {
        return raw_f32(&bytes, path);
    }

    Err(format!(
        "{}: unrecognised input (expected .weba, .f32 or .wav)",
        path.display()
    ))
}

fn mono_48k(
    samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
    kind: &'static str,
    path: &Path,
) -> Result<Input, String> {
    if sample_rate != SAMPLE_RATE {
        return Err(format!(
            "{}: sample rate is {sample_rate} Hz; this CLI is {SAMPLE_RATE} Hz only",
            path.display()
        ));
    }
    if channels != 1 {
        return Err(format!(
            "{}: {channels} channels; this CLI is mono only (the corpus is 48 kHz mono, PLAN K4)",
            path.display()
        ));
    }
    if samples.is_empty() {
        return Err(format!("{}: no samples", path.display()));
    }
    Ok(Input {
        samples,
        sample_rate,
        channels,
        kind,
    })
}

fn raw_f32(bytes: &[u8], path: &Path) -> Result<Input, String> {
    if !bytes.len().is_multiple_of(4) {
        return Err(format!(
            "{}: {} bytes is not a whole number of f32 samples",
            path.display(),
            bytes.len()
        ));
    }
    let samples = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect();
    mono_48k(samples, SAMPLE_RATE, 1, "f32", path)
}

fn is_riff_wave(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

/// A parsed `fmt ` chunk.
#[derive(Debug, Clone, Copy)]
struct WavFormat {
    kind: u16,
    channels: u16,
    sample_rate: u32,
    bits: u16,
}

fn parse_wav(bytes: &[u8], path: &Path) -> Result<Input, String> {
    if bytes.len() < 12 {
        return Err(format!("{}: truncated RIFF header", path.display()));
    }
    let mut position = 12usize;
    let mut format: Option<WavFormat> = None;
    let mut data: Option<&[u8]> = None;

    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let declared = u32::from_le_bytes([
            bytes[position + 4],
            bytes[position + 5],
            bytes[position + 6],
            bytes[position + 7],
        ]) as usize;
        let body_start = position + 8;
        let body_end = body_start.saturating_add(declared);
        if body_end > bytes.len() {
            return Err(format!(
                "{}: truncated '{}' chunk (declares {declared} bytes, {} remain)",
                path.display(),
                String::from_utf8_lossy(id),
                bytes.len().saturating_sub(body_start)
            ));
        }
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err(format!("{}: 'fmt ' chunk is too short", path.display()));
            }
            format = Some(WavFormat {
                kind: u16::from_le_bytes([body[0], body[1]]),
                channels: u16::from_le_bytes([body[2], body[3]]),
                sample_rate: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                bits: u16::from_le_bytes([body[14], body[15]]),
            });
        } else if id == b"data" {
            data = Some(body);
        }
        // Chunks are word-aligned: an odd-sized chunk has one pad byte.
        position = body_end + (declared & 1);
    }

    let format = format.ok_or_else(|| format!("{}: no 'fmt ' chunk", path.display()))?;
    let data = data.ok_or_else(|| format!("{}: no 'data' chunk", path.display()))?;
    if format.channels != 1 {
        return Err(format!(
            "{}: {} channels; this CLI is mono only",
            path.display(),
            format.channels
        ));
    }
    if format.sample_rate != SAMPLE_RATE {
        return Err(format!(
            "{}: sample rate is {} Hz; this CLI is {SAMPLE_RATE} Hz only",
            path.display(),
            format.sample_rate
        ));
    }

    let samples = match (format.kind, format.bits) {
        (1, 16) => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| f32::from(i16::from_le_bytes(*chunk)) / 32768.0)
            .collect(),
        (1, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| {
                let value = i32::from_le_bytes(*chunk);
                value as f32 / 2_147_483_648.0
            })
            .collect(),
        (3, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect(),
        (1, 24) => {
            return Err(format!(
                "{}: 24-bit PCM is not supported; convert to wav16, wavf32 or f32",
                path.display()
            ))
        }
        (kind, bits) => {
            return Err(format!(
                "{}: unsupported WAV encoding (format {kind}, {bits} bits); \
                 expected PCM 16/32-bit or IEEE float 32-bit",
                path.display()
            ))
        }
    };

    mono_48k(samples, format.sample_rate, format.channels, "wav", path)
}

/// Encode samples in `format`. Returns the exact bytes to write.
pub fn encode(samples: &[f32], format: OutFormat) -> Result<Vec<u8>, String> {
    let bytes_per_sample = match format {
        OutFormat::Wav16 => 2usize,
        OutFormat::WavF32 | OutFormat::RawF32 => 4,
    };
    let data_len = samples
        .len()
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| "output is too large to encode".to_string())?;
    if data_len > u32::MAX as usize {
        return Err("output exceeds the 4 GiB WAV/raw limit".to_string());
    }

    match format {
        OutFormat::RawF32 => {
            let mut out = Vec::with_capacity(data_len);
            for sample in samples {
                out.extend_from_slice(&sample.to_le_bytes());
            }
            Ok(out)
        }
        OutFormat::Wav16 | OutFormat::WavF32 => {
            // The RIFF size field counts this chunk plus 36 bytes of header.
            if data_len > u32::MAX as usize - 36 {
                return Err("output exceeds the 4 GiB WAV limit".to_string());
            }
            let mut out = Vec::with_capacity(44 + data_len);
            let (encoding, bits) = match format {
                OutFormat::Wav16 => (1u16, 16u16),
                _ => (3u16, 32u16),
            };
            let block_align = bits / 8; // mono
            let byte_rate = SAMPLE_RATE * u32::from(block_align);

            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
            out.extend_from_slice(b"WAVE");
            out.extend_from_slice(b"fmt ");
            out.extend_from_slice(&16u32.to_le_bytes());
            out.extend_from_slice(&encoding.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes()); // channels
            out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
            out.extend_from_slice(&byte_rate.to_le_bytes());
            out.extend_from_slice(&block_align.to_le_bytes());
            out.extend_from_slice(&bits.to_le_bytes());
            out.extend_from_slice(b"data");
            out.extend_from_slice(&(data_len as u32).to_le_bytes());

            match format {
                OutFormat::Wav16 => {
                    for sample in samples {
                        out.extend_from_slice(&quantise_16(*sample).to_le_bytes());
                    }
                }
                _ => {
                    for sample in samples {
                        out.extend_from_slice(&sample.to_le_bytes());
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Convert one sample to 16-bit PCM: round half away from zero, clamp, and map
/// NaN/infinity to silence.
fn quantise_16(sample: f32) -> i16 {
    let scaled = sample * 32768.0;
    if !scaled.is_finite() {
        return 0;
    }
    scaled.round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav16_round_trips_within_one_lsb() {
        let samples = [
            0.0f32,
            0.5,
            -0.5,
            0.999_969_5,
            -1.0,
            f32::NAN,
            f32::INFINITY,
        ];
        let bytes = encode(&samples, OutFormat::Wav16).expect("encode");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let data = &bytes[44..];
        assert_eq!(data.len(), samples.len() * 2);
        let decoded: Vec<f32> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| f32::from(i16::from_le_bytes(*chunk)) / 32768.0)
            .collect();
        assert!((decoded[1] - 0.5).abs() < 1.0 / 32768.0);
        assert!((decoded[2] + 0.5).abs() < 1.0 / 32768.0);
        assert_eq!(decoded[5], 0.0);
        assert_eq!(decoded[6], 0.0);
    }

    #[test]
    fn encoding_is_deterministic() {
        let samples: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.01).sin()).collect();
        for format in OutFormat::ALL {
            let first = encode(&samples, format).expect("encode");
            let second = encode(&samples, format).expect("encode");
            assert_eq!(first, second, "{} must be deterministic", format.id());
        }
    }

    #[test]
    fn quantisation_clamps_instead_of_wrapping() {
        assert_eq!(quantise_16(2.0), 32767);
        assert_eq!(quantise_16(-2.0), -32768);
        assert_eq!(quantise_16(0.0), 0);
    }

    #[test]
    fn parses_a_wav_we_wrote() {
        let samples = [0.25f32, -0.25, 0.0, 0.5];
        for format in [OutFormat::Wav16, OutFormat::WavF32] {
            let dir = std::env::temp_dir().join(format!("rasp-cli-wav-{}", format.id()));
            let _ = fs::create_dir_all(&dir);
            let path = dir.join("x.wav");
            fs::write(&path, encode(&samples, format).expect("encode")).expect("write");
            let loaded = load(&path).expect("load");
            assert_eq!(loaded.samples.len(), samples.len());
            assert_eq!(loaded.kind, "wav");
            let tolerance = if format == OutFormat::Wav16 {
                1.0 / 32768.0
            } else {
                0.0
            };
            for (got, want) in loaded.samples.iter().zip(samples.iter()) {
                assert!((got - want).abs() <= tolerance);
            }
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir(&dir);
        }
    }

    #[test]
    fn rejects_bad_inputs_without_panicking() {
        let dir = std::env::temp_dir().join("rasp-cli-bad-inputs");
        fs::create_dir_all(&dir).expect("temp dir");
        let cases: [(&str, &[u8]); 4] = [
            ("empty.f32", b""),
            ("odd.f32", &[0u8; 6]),
            ("junk.xyz", b"not audio at all"),
            ("trunc.wav", b"RIFF\x00\x00\x00\x00WAVEfmt "),
        ];
        for (name, bytes) in cases {
            let path = dir.join(name);
            fs::write(&path, bytes).expect("write");
            assert!(load(&path).is_err(), "{name} should fail");
            let _ = fs::remove_file(&path);
        }
        let _ = fs::remove_dir(&dir);
    }
}
