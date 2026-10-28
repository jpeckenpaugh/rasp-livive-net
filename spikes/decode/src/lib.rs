//! D-01 spike: decode Opus-in-WebM to 48 kHz mono f32.
//!
//! The point is measurement, not API design. Each backend is behind a feature
//! so that one can be compiled at a time; `decode::decode` returns raw decoded
//! samples plus the container facts needed to reproduce ffmpeg's framing.

use symphonia::core::formats::TrackType;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;

pub struct DecodeResult {
    pub samples: Vec<f32>,
    /// True when the backend already removed the OpusHead pre-skip.
    pub pre_skip_applied: bool,
    pub packets: usize,
    pub sample_rate: u32,
    pub channels: usize,
}

#[derive(Debug)]
pub enum Backend {
    OpusPure,
    Ruopus,
    AdapterOpusRs,
    Oporus,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::OpusPure => "opus-pure",
            Backend::Ruopus => "ruopus",
            Backend::AdapterOpusRs => "symphonia-adapter-opus-rs",
            Backend::Oporus => "oporus",
        }
    }
}

fn probe(bytes: &[u8]) -> Box<dyn symphonia::core::formats::FormatReader> {
    let mss = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(bytes.to_vec())),
        Default::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension("weba");
    symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .expect("probe failed")
}

/// OpusHead `pre_skip` from the track's CodecPrivate (Matroska stores the
/// full OpusHead, magic included).
fn pre_skip_from(extra: &[u8]) -> usize {
    if extra.len() >= 12 && &extra[0..8] == b"OpusHead" {
        u16::from_le_bytes([extra[10], extra[11]]) as usize
    } else {
        0
    }
}

#[cfg(feature = "backend-opus-pure")]
fn decode_raw(bytes: &[u8]) -> DecodeResult {
    use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder};

    let mut fmt = probe(bytes);
    let track = fmt.default_track(TrackType::Audio).expect("no audio track");
    let id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .expect("no codec params")
        .audio()
        .unwrap();
    let extra = params.extra_data.as_deref().unwrap_or(&[]);
    let _pre_skip = pre_skip_from(extra);
    let rate = 48_000u32;
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(1);

    let mut dec = OpusDecoder::new(rate as i32, channels).expect("decoder init");
    let mut samples = Vec::new();
    let mut buf = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut packets = 0usize;
    while let Some(packet) = fmt.next_packet().expect("demux") {
        if packet.track_id != id {
            continue;
        }
        let n = dec
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut buf)
            .expect("decode");
        samples.extend_from_slice(&buf[..n * channels]);
        packets += 1;
    }
    DecodeResult {
        samples,
        pre_skip_applied: false,
        packets,
        sample_rate: rate,
        channels,
    }
}

#[cfg(feature = "backend-ruopus")]
fn decode_raw(bytes: &[u8]) -> DecodeResult {
    use ruopus::OpusDecoder;

    let mut fmt = probe(bytes);
    let track = fmt.default_track(TrackType::Audio).expect("no audio track");
    let id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .expect("no codec params")
        .audio()
        .unwrap();
    let extra = params.extra_data.as_deref().unwrap_or(&[]);
    let _pre_skip = pre_skip_from(extra);
    let rate = 48_000u32;
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(1);

    let mut dec = OpusDecoder::with_rate(rate, channels);
    let mut samples = Vec::new();
    let mut packets = 0usize;
    while let Some(packet) = fmt.next_packet().expect("demux") {
        if packet.track_id != id {
            continue;
        }
        let pcm = dec.decode_packet(&packet.data).expect("decode");
        samples.extend_from_slice(&pcm);
        packets += 1;
    }
    DecodeResult {
        samples,
        pre_skip_applied: false,
        packets,
        sample_rate: rate,
        channels,
    }
}

#[cfg(feature = "backend-adapter-opus-rs")]
fn decode_raw(bytes: &[u8]) -> DecodeResult {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::codecs::registry::RegisterableAudioDecoder;

    let mut fmt = probe(bytes);
    let track = fmt.default_track(TrackType::Audio).expect("no audio track");
    let id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .expect("no codec params")
        .audio()
        .unwrap();
    let rate = params.sample_rate.unwrap_or(48_000);
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(1);
    let mut dec = symphonia_adapter_opus_rs::OpusAdapter::try_registry_new(
        params,
        &AudioDecoderOptions::default(),
    )
    .expect("adapter init");

    let mut samples = Vec::new();
    let mut packets = 0usize;
    while let Some(packet) = fmt.next_packet().expect("demux") {
        if packet.track_id != id {
            continue;
        }
        let buf = dec.decode(&packet).expect("decode");
        let n = buf.samples_interleaved();
        let start = samples.len();
        samples.resize(start + n, 0.0);
        buf.copy_to_slice_interleaved(&mut samples[start..]);
        packets += 1;
    }
    DecodeResult {
        samples,
        pre_skip_applied: true,
        packets,
        sample_rate: rate,
        channels,
    }
}

#[cfg(feature = "backend-oporus")]
fn decode_raw(bytes: &[u8]) -> DecodeResult {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::codecs::registry::RegisterableAudioDecoder;

    let mut fmt = probe(bytes);
    let track = fmt.default_track(TrackType::Audio).expect("no audio track");
    let id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .expect("no codec params")
        .audio()
        .unwrap();
    let rate = params.sample_rate.unwrap_or(48_000);
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(1);
    let mut dec = symphonia_adapter_oporus::OpusDecoder::try_registry_new(
        params,
        &AudioDecoderOptions::default(),
    )
    .expect("adapter init");

    let mut samples = Vec::new();
    let mut packets = 0usize;
    while let Some(packet) = fmt.next_packet().expect("demux") {
        if packet.track_id != id {
            continue;
        }
        let buf = dec.decode(&packet).expect("decode");
        let n = buf.samples_interleaved();
        let start = samples.len();
        samples.resize(start + n, 0.0);
        buf.copy_to_slice_interleaved(&mut samples[start..]);
        packets += 1;
    }
    DecodeResult {
        samples,
        pre_skip_applied: true,
        packets,
        sample_rate: rate,
        channels,
    }
}

/// `DiscardPadding` (0x75A2), nanoseconds, from the WebM BlockGroup. The MKV
/// demuxer parses it but does not expose it, so the spike scans for it. Always
/// the last such element in these files.
fn discard_padding_ns(bytes: &[u8]) -> u64 {
    let mut out = 0u64;
    let mut i = 0usize;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0x75 && bytes[i + 1] == 0xA2 {
            // EBML element size: a vint whose leading zeros give the length.
            let first = bytes[i + 2];
            let mut mask = 0x80u8;
            let mut n = 1usize;
            while n <= 8 && first & mask == 0 {
                mask >>= 1;
                n += 1;
            }
            if n <= 8 && i + 2 + n <= bytes.len() {
                let mut len = (first & (mask - 1)) as usize;
                for k in 1..n {
                    len = (len << 8) | bytes[i + 2 + k] as usize;
                }
                let start = i + 2 + n;
                if len >= 1 && len <= 8 && start + len <= bytes.len() {
                    // EBML integers are big-endian two's complement.
                    let mut v = 0u64;
                    for k in 0..len {
                        v = (v << 8) | bytes[start + k] as u64;
                    }
                    out = v;
                    i = start + len;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// ffmpeg-compatible framing: drop `pre_skip` from the front and the
/// container's `DiscardPadding` from the back.
pub fn conform(bytes: &[u8], mut r: DecodeResult, pre_skip: usize) -> Vec<f32> {
    // ffmpeg rounds ns to the nearest sample (checked against all 12 clips).
    let ns = discard_padding_ns(bytes) as u128;
    let discard = (ns * 48_000 + 500_000_000) / 1_000_000_000;
    let mut start = 0usize;
    if !r.pre_skip_applied {
        start = pre_skip * r.channels;
    }
    let end = r
        .samples
        .len()
        .saturating_sub(discard as usize * r.channels);
    if start > end {
        start = end;
    }
    r.samples.drain(end..);
    r.samples.drain(..start);
    r.samples
}

pub fn decode_with(bytes: &[u8], backend: Backend) -> Vec<f32> {
    let pre_skip = {
        let fmt = probe(bytes);
        let track = fmt.default_track(TrackType::Audio).expect("no audio track");
        let params = track
            .codec_params
            .as_ref()
            .expect("no codec params")
            .audio()
            .unwrap();
        pre_skip_from(params.extra_data.as_deref().unwrap_or(&[]))
    };
    let r = match backend {
        #[cfg(feature = "backend-opus-pure")]
        Backend::OpusPure => decode_raw(bytes),
        #[cfg(feature = "backend-ruopus")]
        Backend::Ruopus => decode_raw(bytes),
        #[cfg(feature = "backend-adapter-opus-rs")]
        Backend::AdapterOpusRs => decode_raw(bytes),
        #[cfg(feature = "backend-oporus")]
        Backend::Oporus => decode_raw(bytes),
        #[allow(unreachable_patterns)]
        _ => panic!("backend not compiled in"),
    };
    conform(bytes, r, pre_skip)
}

/// Decode with the feature-selected backend.
pub fn decode(bytes: &[u8]) -> Vec<f32> {
    #[cfg(feature = "backend-opus-pure")]
    let b = Backend::OpusPure;
    #[cfg(all(feature = "backend-ruopus", not(feature = "backend-opus-pure")))]
    let b = Backend::Ruopus;
    #[cfg(all(
        feature = "backend-adapter-opus-rs",
        not(any(feature = "backend-opus-pure", feature = "backend-ruopus"))
    ))]
    let b = Backend::AdapterOpusRs;
    #[cfg(all(
        feature = "backend-oporus",
        not(any(
            feature = "backend-opus-pure",
            feature = "backend-ruopus",
            feature = "backend-adapter-opus-rs"
        ))
    ))]
    let b = Backend::Oporus;
    decode_with(bytes, b)
}

pub fn backend_name() -> &'static str {
    #[cfg(feature = "backend-opus-pure")]
    return "opus-pure";
    #[cfg(all(feature = "backend-ruopus", not(feature = "backend-opus-pure")))]
    return "ruopus";
    #[cfg(all(
        feature = "backend-adapter-opus-rs",
        not(any(feature = "backend-opus-pure", feature = "backend-ruopus"))
    ))]
    return "symphonia-adapter-opus-rs";
    #[cfg(all(
        feature = "backend-oporus",
        not(any(
            feature = "backend-opus-pure",
            feature = "backend-ruopus",
            feature = "backend-adapter-opus-rs"
        ))
    ))]
    return "oporus";
}

#[cfg(feature = "wasm")]
mod wasm_api {
    use super::*;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    pub fn backend() -> String {
        backend_name().to_string()
    }

    /// Decode `.weba` bytes to interleaved f32 at 48 kHz.
    #[wasm_bindgen]
    pub fn decode_weba(bytes: &[u8]) -> Vec<f32> {
        decode(bytes)
    }
}
