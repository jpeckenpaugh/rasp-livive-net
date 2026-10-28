//! WebM/Opus demuxing and decoding for RASP, producing interleaved `f32`
//! samples at 48 kHz.
//!
//! The demuxer is Symphonia's Matroska/WebM reader (`mkv` feature only); the
//! decoder is `opus-pure`. Both are called through their safe APIs, this crate
//! compiles with `#![forbid(unsafe_code)]`, and every input-derived failure is
//! a typed error rather than an abort.
//!
//! # Framing
//!
//! A decoded Opus stream is longer than the audio it carries at both ends. The
//! two corrections are mandatory for a sample count that matches libopus (and
//! therefore ffmpeg's `-c:a libopus`), and both are applied here:
//!
//! 1. **Front:** the `pre_skip` field of the `OpusHead` stored in the Matroska
//!    `CodecPrivate` element is dropped. It is the encoder's algorithmic
//!    delay; the corpus in `web/audio/` uses 312 samples.
//! 2. **Back:** the Matroska `DiscardPadding` element (`0x75A2`, big-endian
//!    signed nanoseconds) is dropped. It is converted with
//!    `round(ns * 48000 / 1e9)` samples, which is what ffmpeg/libopus does.
//!
//! `DiscardPadding` is a property of the final packet, not of the stream, and
//! libopus/ffmpeg does **not** clamp an oversized value: it ignores the whole
//! discard. Measured on `af_heart/greet` (20 ms, 960-sample packets), a value
//! of 960 drops the final packet while 961 drops nothing. The decode loop
//! records the last packet's frame count and the discard is applied only when
//! it fits inside that packet, which is exactly libopus's rule (D-02b).
//!
//! Symphonia 0.6.1 parses `DiscardPadding` but does not expose it on
//! [`Packet`](symphonia::core::packet::Packet), so the last `0x75A2` element is
//! located by a small structural EBML scan (`discard_padding_ns`). The scan
//! walks only master elements (`Segment`, `Cluster`, `BlockGroup`), so packet
//! payload bytes are never mistaken for the element. It takes the last
//! `DiscardPadding` in the file; the corpus is single-track, and per-track
//! disambiguation is not attempted.
//!
//! # `durationMs` is not a sample-count oracle
//!
//! The registry (`web/registry.json`) carries a `durationMs` per clip. It is
//! **not** a usable sample count: measured across the D-01 spike clips,
//! `durationMs * 48 - decoded_samples` ranged from **-342 to -1308 samples**
//! (-7.1 to -27.2 ms). Use the decoded buffer length. The framing above — not
//! `durationMs` — is what reproduces libopus exactly.
//!
//! # Channels
//!
//! Output is **interleaved** (`samples[frame * channels + channel]`). Mono and
//! stereo (Opus channel mapping family 0, one or two channels) are supported;
//! the corpus is mono but nothing here assumes it. Streams with more than two
//! channels, or with a non-zero mapping family, are rejected with
//! [`DecodeError::UnsupportedCodec`] rather than downmixed silently. No
//! resampling is performed: the decode rate is always 48 kHz (PLAN D-03 owns
//! resampling to an `AudioContext` rate).
//!
//! # Scope
//!
//! Tested against the vendored corpus only: 32 kbps VBR mono speech, 20 ms
//! frames. CELT-only or hybrid music, DTX, packet loss, and non-20 ms frame
//! times are not covered by tests and should not be claimed as supported.
//!
//! # Accuracy
//!
//! `opus-pure` reproduces libopus to roughly one 16-bit LSB. D-02 measured
//! **79.4–89.5 dB** SNR against `ffmpeg -c:a libopus` on 66 corpus clips (the
//! 54 `hello` clips plus the six shortest and six longest), against a 60 dB
//! target, with an exact sample-count match on all 66. The result is not
//! bit-exact, so a null test against libopus will not be zero.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::Cursor;
use std::path::Path;

use opus_pure::{OpusDecoder, MAX_PACKET_SAMPLES};
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;

/// Native sample rate, in Hz, of every [`DecodedClip`] this crate produces.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// A decoded clip: interleaved `f32` samples at [`SAMPLE_RATE_HZ`].
#[derive(Debug, Clone)]
pub struct DecodedClip {
    /// Sample rate of `samples`, in Hz. Always [`SAMPLE_RATE_HZ`] today.
    pub sample_rate: u32,
    /// Number of interleaved channels (1 = mono, 2 = stereo).
    pub channels: u16,
    /// Interleaved samples in `[-1.0, 1.0]` for a conventionally mastered
    /// source. Length is a multiple of `channels`.
    pub samples: Vec<f32>,
}

impl DecodedClip {
    /// Number of frames (samples per channel) in the clip.
    pub fn frame_count(&self) -> usize {
        // `channels` is 1 or 2 for decoded clips; `max(1)` keeps a
        // hand-constructed zero from dividing by zero.
        self.samples.len() / usize::from(self.channels).max(1)
    }

    /// Duration in seconds, derived from the decoded sample count.
    pub fn duration_secs(&self) -> f64 {
        self.frame_count() as f64 / f64::from(self.sample_rate)
    }
}

/// Why a `.weba` clip could not be decoded.
///
/// The variants distinguish the cases a caller can act on: a wrong file, a
/// container without audio, an audio codec this crate does not handle, a
/// corrupt stream, and a valid stream that carries no audio samples.
///
/// [`DecodeError::ContainerNotWebm`] is reserved for inputs whose first bytes
/// are not the EBML magic; a truncated (or otherwise unreadable) input that
/// does start with the magic is [`DecodeError::MalformedContainer`].
#[derive(Debug)]
#[non_exhaustive]
pub enum DecodeError {
    /// The input is not an EBML/WebM (Matroska) container at all: it is
    /// non-empty and does not start with the EBML magic.
    ContainerNotWebm,
    /// The container has no audio track.
    NoAudioTrack,
    /// The audio track is not something this crate decodes (only 48 kHz Opus,
    /// one or two channels, mapping family 0).
    UnsupportedCodec(&'static str),
    /// The WebM container is structurally broken (truncated, bad element
    /// sizes, missing `OpusHead`, …).
    MalformedContainer(&'static str),
    /// An Opus packet in an otherwise readable container did not decode.
    MalformedPacket(&'static str),
    /// The input carried no decodable audio: no bytes at all, or a stream
    /// that decodes to zero samples after pre-skip and end trimming.
    EmptyStream,
    /// Reading the file failed. Only [`decode_weba_file`] produces this.
    Io(std::io::Error),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::ContainerNotWebm => {
                f.write_str("input is not a WebM (Matroska) container")
            }
            DecodeError::NoAudioTrack => f.write_str("container has no audio track"),
            DecodeError::UnsupportedCodec(detail) => {
                write!(f, "unsupported audio stream: {detail}")
            }
            DecodeError::MalformedContainer(detail) => {
                write!(f, "malformed WebM container: {detail}")
            }
            DecodeError::MalformedPacket(detail) => {
                write!(f, "malformed Opus packet: {detail}")
            }
            DecodeError::EmptyStream => {
                f.write_str("stream decoded to no audio samples after trimming")
            }
            DecodeError::Io(err) => write!(f, "could not read input: {err}"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DecodeError::Io(err) => Some(err),
            _ => None,
        }
    }
}

/// Decode a WebM/Opus (`.weba`) clip from memory.
///
/// Returns interleaved 48 kHz f32 samples with the Opus pre-skip and the
/// Matroska `DiscardPadding` already removed. See the crate documentation for
/// the exact framing rule and the channel policy.
pub fn decode_weba(bytes: &[u8]) -> Result<DecodedClip, DecodeError> {
    let mut format = open_container(bytes)?;

    // Copy the track facts out before taking `format` mutably for packet reads.
    let (track_id, channels, pre_skip) = {
        let track = format
            .first_track(TrackType::Audio)
            .ok_or(DecodeError::NoAudioTrack)?;

        let params = match track.codec_params.as_ref() {
            Some(CodecParameters::Audio(params)) => params,
            // `first_track(TrackType::Audio)` only returns audio tracks, so this
            // is defensive rather than reachable in practice.
            _ => {
                return Err(DecodeError::UnsupportedCodec(
                    "audio track has no audio codec parameters",
                ));
            }
        };

        if params.codec != CODEC_ID_OPUS {
            return Err(DecodeError::UnsupportedCodec("audio track is not Opus"));
        }

        if let Some(rate) = params.sample_rate {
            if rate != SAMPLE_RATE_HZ {
                return Err(DecodeError::UnsupportedCodec(
                    "only 48 kHz Opus is supported (resampling is PLAN D-03)",
                ));
            }
        }

        let extra = params
            .extra_data
            .as_deref()
            .ok_or(DecodeError::MalformedContainer(
                "Opus track has no CodecPrivate (OpusHead)",
            ))?;
        let head = OpusHead::parse(extra)?;

        // The container's channel count and the OpusHead's must agree; a
        // mismatch means the interleaving would be a guess.
        if let Some(container_channels) = params.channels.as_ref().map(|c| c.count()) {
            if container_channels != usize::from(head.channels) {
                return Err(DecodeError::MalformedContainer(
                    "container and OpusHead disagree on the channel count",
                ));
            }
        }

        (track.id, usize::from(head.channels), head.pre_skip)
    };

    let mut decoder = OpusDecoder::new(SAMPLE_RATE_HZ as i32, channels).map_err(|_| {
        DecodeError::UnsupportedCodec("opus-pure rejected the stream's channel layout")
    })?;

    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut samples: Vec<f32> = Vec::new();

    // Frames produced by the most recent packet. `DiscardPadding` is a
    // per-final-packet trim, and an oversized value is ignored rather than
    // clamped (D-02b), so this is what the tail trim is validated against.
    let mut last_packet_frames = 0usize;

    // Symphonia's Matroska reader does not set `Packet::trim_start`/`trim_end`
    // (it parses `DiscardPadding` and drops it), so both container trims are
    // applied once after this loop. If a future reader starts setting them,
    // they must be honoured there instead, or the audio would be trimmed twice.
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(err) => return Err(container_error(err)),
        };

        if packet.track_id != track_id {
            continue;
        }

        // `MAX_PACKET_SAMPLES` is 120 ms at 48 kHz, the longest Opus packet.
        let decoded = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .map_err(|_| DecodeError::MalformedPacket("Opus packet did not decode"))?;

        if decoded > MAX_PACKET_SAMPLES {
            return Err(DecodeError::MalformedPacket(
                "decoder returned more samples than an Opus packet can hold",
            ));
        }

        samples.extend_from_slice(&block[..decoded * channels]);
        last_packet_frames = decoded;
    }

    trim_framing(bytes, samples, channels, pre_skip, last_packet_frames)
}

/// Decode a WebM/Opus (`.weba`) clip from a file.
///
/// On `wasm32-unknown-unknown` this compiles but every call fails with
/// [`DecodeError::Io`]: there is no filesystem. Browser callers (W-01) must
/// fetch the bytes and use [`decode_weba`].
pub fn decode_weba_file(path: impl AsRef<Path>) -> Result<DecodedClip, DecodeError> {
    let bytes = std::fs::read(path).map_err(DecodeError::Io)?;
    decode_weba(&bytes)
}

/// Probe `bytes` as a container, mapping the failure modes the caller cares
/// about.
///
/// The EBML magic check is the only test for "not WebM". Once the magic is
/// present, any probe failure means a WebM header this build cannot read
/// (usually truncation), not a foreign container, so it is reported as
/// [`DecodeError::MalformedContainer`] (D-02c).
fn open_container(bytes: &[u8]) -> Result<Box<dyn FormatReader + '_>, DecodeError> {
    // No bytes at all: the magic cannot be present, and there is nothing to
    // decode. This is the empty-stream case, distinct from a wrong file.
    if bytes.is_empty() {
        return Err(DecodeError::EmptyStream);
    }

    if !bytes.starts_with(&EBML_MAGIC) {
        return Err(DecodeError::ContainerNotWebm);
    }

    let source = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("weba");

    symphonia::default::get_probe()
        .probe(&hint, source, Default::default(), Default::default())
        .map_err(|err| match err {
            SymphoniaError::DecodeError(detail) => DecodeError::MalformedContainer(detail),
            // An in-memory cursor cannot fail with a real I/O error; this is a
            // stream that ended before the header did.
            SymphoniaError::IoError(_) => {
                DecodeError::MalformedContainer("stream ended before the container header")
            }
            SymphoniaError::LimitError(detail) => DecodeError::MalformedContainer(detail),
            SymphoniaError::SeekError(_) | SymphoniaError::ResetRequired => {
                DecodeError::MalformedContainer("container header could not be read")
            }
            // `Unsupported` (and anything new) after a valid magic means the
            // probe gave up on a WebM document it could not parse. Claiming
            // "not WebM" here is what misreported 3,377 truncations (D-05).
            SymphoniaError::Unsupported(_) => {
                DecodeError::MalformedContainer("WebM header could not be parsed")
            }
            _ => DecodeError::MalformedContainer("WebM header could not be parsed"),
        })
}

/// Map a mid-stream Symphonia error. All of them mean the container is broken
/// or truncated; none is a filesystem error (the source is a `Cursor`).
fn container_error(err: SymphoniaError) -> DecodeError {
    match err {
        SymphoniaError::DecodeError(detail) => DecodeError::MalformedContainer(detail),
        SymphoniaError::Unsupported(detail) => DecodeError::MalformedContainer(detail),
        SymphoniaError::LimitError(detail) => DecodeError::MalformedContainer(detail),
        SymphoniaError::IoError(_) => {
            DecodeError::MalformedContainer("stream ended before the last packet")
        }
        SymphoniaError::SeekError(_) | SymphoniaError::ResetRequired => {
            DecodeError::MalformedContainer("container could not be read to the end")
        }
        _ => DecodeError::MalformedContainer("container could not be read"),
    }
}

/// The `OpusHead` fields this crate needs.
#[derive(Debug, Clone, Copy)]
struct OpusHead {
    channels: u8,
    pre_skip: u16,
}

impl OpusHead {
    /// Length of the fixed part of an `OpusHead` (mapping family 0).
    const FIXED_LEN: usize = 19;

    fn parse(extra: &[u8]) -> Result<Self, DecodeError> {
        if extra.len() < Self::FIXED_LEN || !extra.starts_with(b"OpusHead") {
            return Err(DecodeError::MalformedContainer(
                "Opus CodecPrivate is not a valid OpusHead",
            ));
        }

        // RFC 7845 §5.1: the major version is the high nibble; a new major
        // version may change the layout, so refuse it rather than guess.
        if extra[8] >> 4 != 0 {
            return Err(DecodeError::UnsupportedCodec(
                "OpusHead major version is not 0",
            ));
        }

        let channels = extra[9];
        let pre_skip = u16::from_le_bytes([extra[10], extra[11]]);
        let mapping_family = extra[18];

        if mapping_family != 0 {
            return Err(DecodeError::UnsupportedCodec(
                "Opus channel mapping family other than 0",
            ));
        }

        match channels {
            1 | 2 => Ok(OpusHead { channels, pre_skip }),
            0 => Err(DecodeError::MalformedContainer(
                "OpusHead declares zero channels",
            )),
            _ => Err(DecodeError::UnsupportedCodec("more than two Opus channels")),
        }
    }
}

/// Apply the two container trims and return the finished clip.
///
/// `pre_skip` is clamped to the decoded length. `DiscardPadding` is a trim of
/// the **final packet** only, so it is applied only when it fits inside
/// `last_packet_frames`; a larger value is ignored, as libopus does (D-02b).
/// If nothing survives, the stream is empty (or so damaged that it carries no
/// audio) and this returns [`DecodeError::EmptyStream`] rather than an empty
/// clip.
fn trim_framing(
    bytes: &[u8],
    mut samples: Vec<f32>,
    channels: usize,
    pre_skip: u16,
    last_packet_frames: usize,
) -> Result<DecodedClip, DecodeError> {
    // Defensive: the caller validates 1 or 2 channels, but never divide by 0.
    let channels = channels.max(1);

    let total_frames = samples.len() / channels;
    let start = usize::from(pre_skip).min(total_frames);
    let discard = discard_samples(discard_padding_ns(bytes));
    // libopus/ffmpeg do not clamp an oversized `DiscardPadding`: the discard
    // is dropped entirely, so the final packet survives intact. Measured
    // against `ffmpeg -c:a libopus` on `af_heart/greet`: 960 samples trims the
    // final 960-sample packet, 961 samples trims nothing.
    let discard = if discard <= last_packet_frames {
        discard
    } else {
        0
    };
    let end = total_frames.saturating_sub(discard);

    if start >= end {
        return Err(DecodeError::EmptyStream);
    }

    samples.truncate(end * channels);
    samples.drain(..start * channels);

    Ok(DecodedClip {
        sample_rate: SAMPLE_RATE_HZ,
        channels: channels as u16,
        samples,
    })
}

/// Convert a `DiscardPadding` value in nanoseconds to samples, rounding to
/// nearest as libopus/ffmpeg do. Negative values (allowed by Matroska for
/// overlapping blocks) are treated as zero.
fn discard_samples(ns: i64) -> usize {
    if ns <= 0 {
        return 0;
    }
    let num = u128::from(ns as u64) * u128::from(SAMPLE_RATE_HZ) + 500_000_000;
    let samples = num / 1_000_000_000;
    match usize::try_from(samples) {
        Ok(value) => value,
        Err(_) => usize::MAX,
    }
}

/// Magic bytes of an EBML document (Matroska and WebM both start with it).
const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];

const ID_SEGMENT: u32 = 0x1853_8067;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_BLOCK_GROUP: u32 = 0x00A0;
const ID_DISCARD_PADDING: u32 = 0x75A2;

/// Depth bound for the EBML walk. A real file needs three levels
/// (`Segment` → `Cluster` → `BlockGroup`); the bound keeps hostile nesting
/// from exhausting the stack.
const MAX_EBML_DEPTH: u8 = 8;

/// Find the last `DiscardPadding` element (ID `0x75A2`) in the file and return
/// its signed big-endian value in nanoseconds, or 0 if there is none.
///
/// Symphonia 0.6.1 parses this element into an internal `BlockGroupElement`
/// field and then drops it; there is no accessor and `Packet::trim_end` is
/// never set by the Matroska demuxer. Walking the master elements ourselves is
/// the smallest correct workaround: unlike a raw byte scan it cannot match
/// `0x75A2` inside compressed packet payload.
fn discard_padding_ns(bytes: &[u8]) -> i64 {
    let mut found = 0i64;
    walk_ebml(bytes, 0, bytes.len(), 0, &mut found);
    found
}

fn walk_ebml(bytes: &[u8], start: usize, end: usize, depth: u8, found: &mut i64) {
    if depth > MAX_EBML_DEPTH {
        return;
    }

    let limit = end.min(bytes.len());
    let mut pos = start;

    while pos < limit {
        let Some((id, id_len)) = read_element_id(bytes, pos) else {
            return;
        };
        let Some((size, size_len, unknown_size)) = read_element_size(bytes, pos + id_len) else {
            return;
        };

        let data_start = pos + id_len + size_len;
        if data_start > limit {
            return;
        }

        let data_end = if unknown_size {
            limit
        } else {
            match usize::try_from(size)
                .ok()
                .and_then(|size| data_start.checked_add(size))
            {
                Some(data_end) if data_end <= limit => data_end,
                _ => return,
            }
        };

        match id {
            // Descend into the masters that can contain `DiscardPadding`.
            ID_SEGMENT | ID_CLUSTER | ID_BLOCK_GROUP => {
                walk_ebml(bytes, data_start, data_end, depth + 1, found);
            }
            ID_DISCARD_PADDING => {
                if let Some(value) = read_signed_be(&bytes[data_start..data_end]) {
                    *found = value;
                }
            }
            _ => {}
        }

        if unknown_size {
            // An unknown-size element runs to the end of its parent; nothing
            // can follow it at this level.
            return;
        }
        pos = data_end;
    }
}

/// Number of bytes in an EBML variable-length integer whose first byte is
/// `first`, or `None` if the first byte is invalid (zero) or too long.
fn vint_len(first: u8) -> Option<usize> {
    if first == 0 {
        None
    } else {
        Some(first.leading_zeros() as usize + 1)
    }
}

/// Read an EBML element ID. IDs keep their marker bits and are at most four
/// bytes wide.
fn read_element_id(bytes: &[u8], pos: usize) -> Option<(u32, usize)> {
    let first = *bytes.get(pos)?;
    let len = vint_len(first)?;
    if len > 4 {
        return None;
    }
    let raw = bytes.get(pos..pos.checked_add(len)?)?;
    let mut id = 0u32;
    for &byte in raw {
        id = (id << 8) | u32::from(byte);
    }
    Some((id, len))
}

/// Read an EBML element size: `(value, bytes_consumed, is_unknown)`.
fn read_element_size(bytes: &[u8], pos: usize) -> Option<(u64, usize, bool)> {
    let first = *bytes.get(pos)?;
    let len = vint_len(first)?;
    let raw = bytes.get(pos..pos.checked_add(len)?)?;

    // Strip the length marker bit(s).
    let mut value = u64::from(first) & ((1u64 << (8 - len)) - 1);
    for &byte in &raw[1..] {
        value = (value << 8) | u64::from(byte);
    }

    // All value bits set is the reserved "unknown size" encoding.
    let unknown_size = value == (1u64 << (7 * len as u32)) - 1;
    Some((value, len, unknown_size))
}

/// Read a 1–8 byte big-endian two's-complement integer.
fn read_signed_be(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let mut value: i64 = 0;
    for &byte in bytes {
        value = (value << 8) | i64::from(byte);
    }
    let shift = 64 - 8 * bytes.len() as u32;
    Some((value << shift) >> shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a one-byte-size EBML element (payload must be < 127 bytes).
    fn element(id: &[u8], payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 127);
        let mut out = Vec::with_capacity(id.len() + 1 + payload.len());
        out.extend_from_slice(id);
        out.push(0x80 | payload.len() as u8);
        out.extend_from_slice(payload);
        out
    }

    fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.iter().flat_map(|part| part.iter().copied()).collect()
    }

    #[test]
    fn opus_head_reads_pre_skip_and_channels() {
        let mut head = [0u8; 19];
        head[..8].copy_from_slice(b"OpusHead");
        head[8] = 1;
        head[9] = 1;
        head[10..12].copy_from_slice(&312u16.to_le_bytes());
        head[12..16].copy_from_slice(&48_000u32.to_le_bytes());
        head[18] = 0;

        let parsed = OpusHead::parse(&head);
        assert!(matches!(
            parsed,
            Ok(OpusHead {
                channels: 1,
                pre_skip: 312
            })
        ));
    }

    #[test]
    fn opus_head_rejects_multichannel_and_bad_family() {
        let mut stereo = [0u8; 19];
        stereo[..8].copy_from_slice(b"OpusHead");
        stereo[8] = 1;
        stereo[9] = 2;
        assert!(OpusHead::parse(&stereo).is_ok());

        let mut surround = [0u8; 19];
        surround[..8].copy_from_slice(b"OpusHead");
        surround[8] = 1;
        surround[9] = 6;
        assert!(matches!(
            OpusHead::parse(&surround),
            Err(DecodeError::UnsupportedCodec(_))
        ));

        let mut mapped = [0u8; 19];
        mapped[..8].copy_from_slice(b"OpusHead");
        mapped[8] = 1;
        mapped[9] = 1;
        mapped[18] = 1;
        assert!(matches!(
            OpusHead::parse(&mapped),
            Err(DecodeError::UnsupportedCodec(_))
        ));
    }

    #[test]
    fn opus_head_rejects_short_or_foreign_extra_data() {
        for extra in [b"OpusHead".as_slice(), b"NotAnOpusHeadAtAll!!".as_slice()] {
            assert!(matches!(
                OpusHead::parse(extra),
                Err(DecodeError::MalformedContainer(_))
            ));
        }
    }

    #[test]
    fn discard_padding_is_found_inside_block_group() {
        let padding = element(&[0x75, 0xA2], &9_000_000i64.to_be_bytes()[3..]);
        let group = element(
            &[0xA0],
            &concat(&[element(&[0xA1], &[0x81, 0x00, 0x00, 0x80]), padding]),
        );
        let cluster = element(&[0x1F, 0x43, 0xB6, 0x75], &group);
        let segment = element(&[0x18, 0x53, 0x80, 0x67], &cluster);

        assert_eq!(discard_padding_ns(&segment), 9_000_000);
        assert_eq!(discard_samples(9_000_000), 432);
    }

    #[test]
    fn discard_padding_keeps_the_last_value() {
        let first = element(&[0x75, 0xA2], &[0x00, 0x00, 0x64]); // 100 ns
        let last = element(&[0x75, 0xA2], &500_000_000i64.to_be_bytes()); // 24000 samples
        let group = element(&[0xA0], &concat(&[first, last]));
        let cluster = element(&[0x1F, 0x43, 0xB6, 0x75], &group);

        assert_eq!(discard_padding_ns(&cluster), 500_000_000);
        assert_eq!(discard_samples(500_000_000), 24_000);
    }

    #[test]
    fn discard_padding_ignores_payload_that_looks_like_the_element() {
        // A Block whose payload contains the literal bytes `75 A2 00` must not
        // be mistaken for the element: the scanner skips non-master payloads.
        let block = element(&[0xA1], &[0x75, 0xA2, 0x00, 0x00, 0x00, 0x00]);
        let group = element(&[0xA0], &block);
        let cluster = element(&[0x1F, 0x43, 0xB6, 0x75], &group);

        assert_eq!(discard_padding_ns(&cluster), 0);
    }

    #[test]
    fn discard_padding_scanner_survives_truncation() {
        let padding = element(&[0x75, 0xA2], &[0x3D, 0x09, 0x00]);
        let group = element(&[0xA0], &padding);
        let cluster = element(&[0x1F, 0x43, 0xB6, 0x75], &group);

        // Every prefix must terminate with a value, never a panic.
        for end in 0..cluster.len() {
            let _ = discard_padding_ns(&cluster[..end]);
        }
        assert_eq!(discard_padding_ns(&cluster[..cluster.len() - 1]), 0);
    }

    #[test]
    fn discard_samples_rounds_to_nearest_and_ignores_negatives() {
        assert_eq!(discard_samples(0), 0);
        assert_eq!(discard_samples(-1), 0);
        assert_eq!(discard_samples(10_417), 1); // 0.5 sample rounds up
        assert_eq!(discard_samples(10_416), 0);
        assert_eq!(discard_samples(20_833), 1);
        assert_eq!(discard_samples(1_000_000_000), 48_000);
    }

    #[test]
    fn signed_big_endian_supports_every_width() {
        assert_eq!(read_signed_be(&[0x7F]), Some(127));
        assert_eq!(read_signed_be(&[0xFF]), Some(-1));
        assert_eq!(read_signed_be(&[0x80, 0x00]), Some(-32_768));
        assert_eq!(read_signed_be(&[0x00]), Some(0));
        assert_eq!(read_signed_be(&[]), None);
        assert_eq!(read_signed_be(&[0; 9]), None);
    }

    #[test]
    fn trim_framing_drops_pre_skip_and_discard_padding() {
        // 10_416_666 ns rounds to 500 samples at 48 kHz.
        let padding = element(&[0x75, 0xA2], &10_416_666i64.to_be_bytes());
        let samples: Vec<f32> = (0..1000).map(|frame| frame as f32).collect();

        let clip = trim_framing(&padding, samples, 1, 312, 1_000).ok();
        assert!(clip.is_some(), "expected a trimmed clip");
        let Some(clip) = clip else {
            return;
        };

        assert_eq!(clip.sample_rate, SAMPLE_RATE_HZ);
        assert_eq!(clip.channels, 1);
        assert_eq!(clip.frame_count(), 1000 - 312 - 500);
        assert_eq!(clip.samples.first().copied(), Some(312.0));
        assert_eq!(clip.samples.last().copied(), Some(499.0));
    }

    #[test]
    fn trim_framing_handles_stereo_interleaving() {
        let samples: Vec<f32> = (0..10).map(|sample| sample as f32).collect();
        let clip = trim_framing(&[], samples, 2, 1, 5).ok();
        assert!(clip.is_some(), "expected a trimmed clip");
        let Some(clip) = clip else {
            return;
        };

        assert_eq!(clip.channels, 2);
        assert_eq!(clip.frame_count(), 4);
        assert_eq!(clip.samples, vec![2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    }

    #[test]
    fn trim_framing_clamps_and_reports_an_empty_stream() {
        // Nothing decoded.
        assert!(matches!(
            trim_framing(&[], Vec::new(), 1, 0, 0),
            Err(DecodeError::EmptyStream)
        ));

        // Pre-skip longer than the stream.
        assert!(matches!(
            trim_framing(&[], vec![0.0; 10], 1, 20, 10),
            Err(DecodeError::EmptyStream)
        ));

        // A discard that fits the final packet can consume the whole stream
        // (0.5 s of padding on a 0.5 s sample buffer).
        let padding = element(&[0x75, 0xA2], &500_000_000i64.to_be_bytes());
        assert!(matches!(
            trim_framing(&padding, vec![0.0; 24_000], 1, 0, 24_000),
            Err(DecodeError::EmptyStream)
        ));

        // Pre-skip 0 and no padding is not "empty": it is the whole clip.
        assert!(matches!(
            trim_framing(&[], vec![0.0; 10], 1, 0, 10),
            Ok(ref clip) if clip.frame_count() == 10
        ));
    }

    #[test]
    fn trim_framing_applies_discard_only_inside_the_last_packet() {
        // 960 samples is 20 ms at 48 kHz — exactly one corpus packet.
        let padding = element(&[0x75, 0xA2], &20_000_000i64.to_be_bytes());
        let samples = || (0..1000).map(|frame| frame as f32).collect::<Vec<f32>>();

        // Fits: the tail of the last packet is dropped.
        let fits = trim_framing(&padding, samples(), 1, 0, 960).ok();
        assert!(matches!(fits, Some(ref clip) if clip.frame_count() == 40));

        // One sample larger than the last packet: libopus ignores the discard
        // entirely rather than clamping to the packet size (D-02b).
        let oversized = trim_framing(&padding, samples(), 1, 0, 959).ok();
        assert!(matches!(
            oversized,
            Some(ref clip) if clip.frame_count() == 1000
        ));

        // Same rule for a wildly out-of-range value.
        let huge = element(&[0x75, 0xA2], &1_000_000_000i64.to_be_bytes());
        let ignored = trim_framing(&huge, samples(), 1, 0, 960).ok();
        assert!(matches!(
            ignored,
            Some(ref clip) if clip.frame_count() == 1000
        ));
    }

    #[test]
    fn empty_and_random_input_is_typed_not_a_panic() {
        // No bytes: nothing to decode, but not evidence of a foreign container.
        assert!(matches!(decode_weba(&[]), Err(DecodeError::EmptyStream)));

        // Deterministic pseudo-random bytes: not EBML, no valid header.
        let mut state = 0x1234_5678u32;
        let mut junk = vec![0u8; 4096];
        for byte in &mut junk {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        assert!(matches!(
            decode_weba(&junk),
            Err(DecodeError::ContainerNotWebm)
        ));

        // A valid EBML magic followed by unusable bytes is a broken WebM
        // document, not a foreign container (D-02c).
        let mut truncated = EBML_MAGIC.to_vec();
        truncated.extend_from_slice(&junk[..64]);
        assert!(matches!(
            decode_weba(&truncated),
            Err(DecodeError::MalformedContainer(_))
        ));
    }
}
