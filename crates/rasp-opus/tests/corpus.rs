//! Corpus tests for `rasp-opus`.
//!
//! These are fixture-free: they walk the vendored corpus in `web/audio/`, the
//! same 1,404 clips D-04 will ship. The default run covers a fixed, fast
//! subset — `hello.weba` for all 54 voices plus the six shortest and six
//! longest clips by registry duration. `--ignored` adds the full 1,404-clip
//! pass (`full_corpus_decodes`), which is still only a few seconds but is not
//! needed on every push.
//!
//! # The reference is libopus, not `ffmpeg -i`
//!
//! Plain `ffmpeg -i in.weba ...` decodes with FFmpeg's native Opus
//! implementation, which is a *different* decoder: 14.5–31.9 dB against
//! libopus, and not sample-aligned (best integer lag 0–2, and even fractional
//! sinc alignment peaks at 21.5 dB). No conformant implementation can reach
//! 60 dB against it. The valid oracle names the decoder:
//!
//! ```text
//! ffmpeg -v error -c:a libopus -i <file> -ac 1 -ar 48000 -f f32le -
//! ```
//!
//! If `ffmpeg` (or its `libopus` decoder) is missing, the reference tests print
//! a notice and return instead of failing: the structural checks still run.
//!
//! # What is asserted
//!
//! - decoded sample count equals the libopus reference exactly;
//! - SNR against the reference is > 60 dB;
//! - no NaN or infinity, and peak <= 1.0 + 1e-6;
//! - duration is within 1 ms of the reference;
//! - malformed input (empty, random, truncated, non-WebM) returns a typed
//!   error and never panics;
//! - an out-of-range `DiscardPadding` is ignored exactly as libopus does
//!   (D-02b), and every truncation of a real clip that still carries the EBML
//!   magic is reported as a malformed container, never as a foreign file
//!   (D-02c).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rasp_opus::{decode_weba, decode_weba_file, DecodeError, DecodedClip, SAMPLE_RATE_HZ};

/// Clips selected by `durationMs` from `web/registry.json`: the six shortest
/// and the six longest in the corpus. Paths are relative to `web/audio/`.
/// Kept as a literal so the test never depends on a JSON parser.
const SPREAD: [&str; 12] = [
    // Shortest (421–509 ms): zh x3, af_bella, pm_santa, af_heart.
    "zf_xiaoni/greet.weba",
    "af_bella/yay.weba",
    "zm_yunxi/greet.weba",
    "zm_yunjian/greet.weba",
    "pm_santa/greet.weba",
    "af_heart/yay.weba",
    // Longest (2025–2187 ms): im_nicola x3, hf_alpha (Hindi reading English),
    // hm_psi, hm_omega.
    "im_nicola/see-you.weba",
    "hf_alpha/good-to-see.weba",
    "hm_psi/be-friends.weba",
    "im_nicola/maybe-next-time.weba",
    "hm_omega/good-to-see.weba",
    "im_nicola/got-time.weba",
];

/// `<repo>/web/audio`, resolved from this crate's manifest directory.
fn corpus_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/audio");
    if root.join("af_heart/hello.weba").is_file() {
        Some(root)
    } else {
        eprintln!(
            "notice: vendored corpus not found at {}; skipping corpus tests",
            root.display()
        );
        None
    }
}

fn decode_path(path: &Path) -> DecodedClip {
    match decode_weba_file(path) {
        Ok(clip) => clip,
        Err(err) => panic!("decode {} failed: {err}", path.display()),
    }
}

/// Structural invariants every decoded clip must satisfy.
fn check_clip(label: &str, clip: &DecodedClip) {
    assert_eq!(
        clip.sample_rate, SAMPLE_RATE_HZ,
        "{label}: unexpected sample rate"
    );
    assert_eq!(clip.channels, 1, "{label}: corpus is mono");
    assert!(!clip.samples.is_empty(), "{label}: decoded to no samples");
    assert_eq!(
        clip.samples.len() % usize::from(clip.channels),
        0,
        "{label}: interleaved buffer is not a whole number of frames"
    );
    assert_eq!(
        clip.frame_count(),
        clip.samples.len(),
        "{label}: mono frame count disagrees with buffer length"
    );

    let mut peak = 0.0f32;
    for &sample in &clip.samples {
        assert!(sample.is_finite(), "{label}: non-finite sample {sample}");
        peak = peak.max(sample.abs());
    }
    assert!(
        peak <= 1.0 + 1e-6,
        "{label}: peak {peak} exceeds full scale"
    );
}

/// The 54 `hello.weba` clips, one per voice directory.
fn hello_clips(root: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => panic!("cannot list {}: {err}", root.display()),
    };

    let mut voices: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    voices.sort();

    assert_eq!(
        voices.len(),
        54,
        "expected 54 voice directories in {}",
        root.display()
    );

    voices
        .into_iter()
        .map(|voice| {
            let path = voice.join("hello.weba");
            assert!(path.is_file(), "missing {}", path.display());
            path
        })
        .collect()
}

fn spread_clips(root: &Path) -> Vec<PathBuf> {
    SPREAD
        .iter()
        .map(|relative| {
            let path = root.join(relative);
            assert!(
                path.is_file(),
                "missing {} (corpus changed?)",
                path.display()
            );
            path
        })
        .collect()
}

/// True when `ffmpeg` is installed and exposes the `libopus` decoder. FFmpeg's
/// native Opus decoder is not an acceptable substitute (see module docs).
fn libopus_available() -> bool {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-decoders"])
        .stdin(Stdio::null())
        .output();

    match output {
        Ok(output) => {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("libopus")
        }
        Err(_) => false,
    }
}

/// Decode a clip with libopus through ffmpeg, as interleaved f32 at 48 kHz.
fn reference_pcm(path: &Path) -> Result<Vec<f32>, String> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-c:a", "libopus", "-i"])
        .arg(path)
        .args(["-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("could not run ffmpeg: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg failed: {}", stderr.trim()));
    }
    if output.stdout.len() % 4 != 0 {
        return Err("ffmpeg produced a partial f32 sample".to_string());
    }

    Ok(output
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect())
}

/// Signal-to-noise ratio of `actual` against `reference`, in dB. Both are
/// sample-aligned; an all-zero error reports infinity.
fn snr_db(reference: &[f32], actual: &[f32]) -> f64 {
    assert_eq!(
        reference.len(),
        actual.len(),
        "SNR needs equal-length inputs"
    );

    let mut signal = 0.0f64;
    let mut error = 0.0f64;
    for (&reference, &actual) in reference.iter().zip(actual) {
        let reference = f64::from(reference);
        let difference = reference - f64::from(actual);
        signal += reference * reference;
        error += difference * difference;
    }

    if error == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (signal / error).log10()
    }
}

fn weba_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            weba_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("weba") {
            out.push(path);
        }
    }
}

/// Error category as text, so the sweep can count variants without
/// implementing `PartialEq` on the public, `#[non_exhaustive]` enum.
fn variant(err: &DecodeError) -> &'static str {
    match err {
        DecodeError::ContainerNotWebm => "ContainerNotWebm",
        DecodeError::NoAudioTrack => "NoAudioTrack",
        DecodeError::UnsupportedCodec(_) => "UnsupportedCodec",
        DecodeError::MalformedContainer(_) => "MalformedContainer",
        DecodeError::MalformedPacket(_) => "MalformedPacket",
        DecodeError::EmptyStream => "EmptyStream",
        DecodeError::Io(_) => "Io",
        _ => "unknown",
    }
}

/// WebM files start with the EBML magic; this is the crate's only
/// "not WebM" test.
const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];

/// Nanoseconds for a whole number of 48 kHz samples, rounded to nearest — the
/// inverse of the crate's `discard_samples`.
fn discarding_ns(samples: u32) -> u32 {
    let ns = (u64::from(samples) * 1_000_000_000 + 24_000) / 48_000;
    u32::try_from(ns).expect("padding value fits a 4-byte DiscardPadding")
}

/// Rewrite the last `DiscardPadding` element of `bytes` to `samples`. The
/// corpus stores it as a 4-byte payload, which every value used here fits.
fn patch_last_discard_padding(bytes: &[u8], samples: u32) -> Vec<u8> {
    const ID: [u8; 2] = [0x75, 0xA2];

    let at = bytes
        .windows(ID.len())
        .rposition(|window| window == ID)
        .expect("clip has no DiscardPadding element");
    // The element ID is 2 bytes and the size vint follows; make sure the
    // match has room for a 4-byte payload.
    assert!(at + 7 <= bytes.len(), "DiscardPadding is cut short");

    let first = bytes[at + 2];
    let len = first.leading_zeros() as usize + 1;
    assert!((1..=8).contains(&len), "invalid EBML size vint");
    let payload_len = usize::from(first) & ((1usize << (8 - len)) - 1);
    assert_eq!(payload_len, 4, "expected a 4-byte DiscardPadding payload");

    let value_start = at + 2 + len;
    let mut patched = bytes.to_vec();
    patched[value_start..value_start + 4].copy_from_slice(&discarding_ns(samples).to_be_bytes());
    patched
}

/// D-02b: a `DiscardPadding` larger than the final packet must be ignored, as
/// libopus does, not clamped. 960 samples is one 20 ms packet at 48 kHz.
#[test]
fn patched_discard_padding_matches_libopus() {
    let Some(root) = corpus_root() else {
        return;
    };

    if !libopus_available() {
        eprintln!("notice: ffmpeg with libopus is not available; skipping the patched-pad oracle");
        return;
    }

    let path = root.join("af_heart/greet.weba");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => panic!("cannot read {}: {err}", path.display()),
    };

    let dir = std::env::temp_dir().join(format!("rasp-opus-d02b-{}", std::process::id()));
    if let Err(err) = fs::create_dir_all(&dir) {
        panic!("cannot create {}: {err}", dir.display());
    }

    let mut rows = Vec::new();
    for samples in [960u32, 961, 1200, 5000] {
        let patched = patch_last_discard_padding(&bytes, samples);
        let file = dir.join(format!("af_heart-greet-pad-{samples}.weba"));
        if let Err(err) = fs::write(&file, &patched) {
            panic!("cannot write {}: {err}", file.display());
        }

        let ours = match decode_weba(&patched) {
            Ok(clip) => clip,
            Err(err) => panic!("padded clip ({samples} samples) failed to decode: {err}"),
        };
        let reference = match reference_pcm(&file) {
            Ok(reference) => reference,
            Err(err) => panic!("reference decode failed for {samples} samples: {err}"),
        };

        assert_eq!(
            ours.frame_count(),
            reference.len(),
            "padding {samples}: {} decoded frames vs {} libopus frames",
            ours.frame_count(),
            reference.len()
        );
        let snr = snr_db(&reference, &ours.samples);
        assert!(
            snr > 60.0,
            "padding {samples}: SNR {snr:.2} dB is below the 60 dB target"
        );
        rows.push((samples, ours.frame_count(), reference.len(), snr));
    }

    // 960 fits the final packet and trims it; 961 and above are out of range
    // and are ignored entire — libopus gives back the whole packet, not 959 of
    // its 960 samples.
    assert_eq!(
        rows[1].1,
        rows[0].1 + 960,
        "an oversized discard must be ignored, not clamped"
    );
    assert_eq!(rows[1].1, rows[2].1);
    assert_eq!(rows[1].1, rows[3].1);

    for (samples, ours, reference, snr) in &rows {
        eprintln!(
            "discard padding {samples:>4} samples: ours {ours} frames, libopus {reference} frames, \
             SNR {snr:.2} dB"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// D-02c: every truncation of a real clip must be typed, and once the EBML
/// magic is present it must never be reported as "not a WebM container".
#[test]
fn truncation_sweep_reports_malformed_not_foreign() {
    let Some(root) = corpus_root() else {
        return;
    };

    let path = root.join("af_heart/greet.weba");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => panic!("cannot read {}: {err}", path.display()),
    };
    assert!(bytes.starts_with(&EBML_MAGIC));

    let mut ok = 0usize;
    let mut empty = 0usize;
    let mut not_webm = 0usize;
    let mut malformed = 0usize;

    // 0 through the full size, inclusive.
    for keep in 0..=bytes.len() {
        let prefix = &bytes[..keep];
        let result = decode_weba(prefix);

        let Err(err) = result else {
            ok += 1;
            continue;
        };

        match variant(&err) {
            "EmptyStream" => {
                assert_eq!(keep, 0, "only the empty input may be EmptyStream");
                empty += 1;
            }
            "ContainerNotWebm" => {
                // Allowed only while the magic itself is incomplete/absent.
                assert!(
                    !prefix.starts_with(&EBML_MAGIC),
                    "truncation to {keep} bytes starts with the EBML magic but was reported \
                     as ContainerNotWebm"
                );
                not_webm += 1;
            }
            "MalformedContainer" => {
                assert!(
                    prefix.starts_with(&EBML_MAGIC),
                    "truncation to {keep} bytes has no EBML magic but was reported as \
                     MalformedContainer"
                );
                malformed += 1;
            }
            other => panic!("truncation to {keep} bytes produced unexpected {other}: {err}"),
        }
    }

    // The first three prefixes are a partial magic (`1A`, `1A 45`, `1A 45 DF`);
    // only the full clip decodes. Everything from 4 bytes onward is a broken
    // WebM document.
    assert_eq!(empty, 1);
    assert_eq!(not_webm, 3);
    assert_eq!(ok, 1);
    assert_eq!(malformed, bytes.len() + 1 - empty - not_webm - ok);

    eprintln!(
        "truncation sweep: {} lengths -> ok {ok}, EmptyStream {empty}, \
         ContainerNotWebm {not_webm}, MalformedContainer {malformed}",
        bytes.len() + 1
    );
}

#[test]
fn hello_decodes_for_every_voice() {
    let Some(root) = corpus_root() else {
        return;
    };

    let mut total_frames = 0usize;
    let mut peak = 0.0f32;
    for path in hello_clips(&root) {
        let label = path.display().to_string();
        let clip = decode_path(&path);
        check_clip(&label, &clip);
        total_frames += clip.frame_count();
        peak = peak.max(clip.samples.iter().fold(0.0f32, |max, s| max.max(s.abs())));
    }

    eprintln!(
        "hello: 54/54 voices decoded, {:.3} s of audio total, peak {peak:.4}",
        total_frames as f64 / f64::from(SAMPLE_RATE_HZ)
    );
}

#[test]
fn fixed_spread_decodes() {
    let Some(root) = corpus_root() else {
        return;
    };

    let clips = spread_clips(&root);
    assert_eq!(clips.len(), SPREAD.len());

    for path in clips {
        let label = path.display().to_string();
        let clip = decode_path(&path);
        check_clip(&label, &clip);
    }

    eprintln!(
        "spread: {}/{} clips decoded (6 shortest, 6 longest)",
        SPREAD.len(),
        SPREAD.len()
    );
}

#[test]
fn matches_libopus_reference() {
    let Some(root) = corpus_root() else {
        return;
    };

    if !libopus_available() {
        eprintln!(
            "notice: ffmpeg with libopus is not available; skipping reference comparisons \
             (ffmpeg's native Opus decoder is not a valid oracle)"
        );
        return;
    }

    let mut checked: Vec<PathBuf> = hello_clips(&root);
    checked.extend(spread_clips(&root));

    let mut min_snr = f64::INFINITY;
    let mut max_snr = f64::NEG_INFINITY;
    let mut min_delta = i64::MAX;
    let mut max_delta = i64::MIN;

    for path in &checked {
        let label = path.display().to_string();
        let clip = decode_path(path);
        check_clip(&label, &clip);

        let reference = match reference_pcm(path) {
            Ok(reference) => reference,
            Err(err) => panic!("reference decode failed for {label}: {err}"),
        };

        let delta = clip.frame_count() as i64 - reference.len() as i64;
        min_delta = min_delta.min(delta);
        max_delta = max_delta.max(delta);
        assert_eq!(
            delta,
            0,
            "{label}: {} decoded frames vs {} reference frames",
            clip.frame_count(),
            reference.len()
        );

        let snr = snr_db(&reference, &clip.samples);
        assert!(
            snr > 60.0,
            "{label}: SNR {snr:.2} dB is below the 60 dB target"
        );
        min_snr = min_snr.min(snr);
        max_snr = max_snr.max(snr);

        let reference_secs = reference.len() as f64 / f64::from(SAMPLE_RATE_HZ);
        let delta_ms = (clip.duration_secs() - reference_secs).abs() * 1000.0;
        assert!(
            delta_ms <= 1.0,
            "{label}: duration differs from the reference by {delta_ms:.3} ms"
        );
    }

    eprintln!(
        "reference: {}/{} clips match libopus exactly; \
         SNR {min_snr:.2}..{max_snr:.2} dB; sample-count delta {min_delta:+}..{max_delta:+}",
        checked.len(),
        checked.len()
    );
}

#[test]
fn malformed_input_returns_typed_errors() {
    let Some(root) = corpus_root() else {
        return;
    };

    // Empty input has nothing to decode; it is not a foreign container.
    assert!(matches!(decode_weba(&[]), Err(DecodeError::EmptyStream)));

    // Deterministic pseudo-random bytes: no EBML magic, no panic.
    let mut state = 0x9E37_79B9u32;
    let mut junk = vec![0u8; 8192];
    for byte in &mut junk {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *byte = (state >> 24) as u8;
    }
    assert!(matches!(
        decode_weba(&junk),
        Err(DecodeError::ContainerNotWebm)
    ));

    // A RIFF/WAVE header is a container this crate does not read.
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&0u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    assert!(matches!(
        decode_weba(&wav),
        Err(DecodeError::ContainerNotWebm)
    ));

    // Truncations of a real clip must fail with a typed error, at every length
    // tried — including the last byte, which removes part of the final element.
    let bytes = match fs::read(root.join("af_heart/hello.weba")) {
        Ok(bytes) => bytes,
        Err(err) => panic!("cannot read the hello fixture: {err}"),
    };
    assert!(!bytes.is_empty());
    for keep in [4, 32, 100, bytes.len() / 2, bytes.len() - 1] {
        let result = decode_weba(&bytes[..keep.min(bytes.len())]);
        assert!(
            result.is_err(),
            "a clip truncated to {keep} bytes decoded successfully"
        );
        if let Err(err) = result {
            // Every failure must name a category, not panic or return a
            // default/empty clip.
            let message = err.to_string();
            assert!(
                !message.is_empty(),
                "truncation to {keep} bytes produced an empty error message"
            );
        }
    }

    // A valid container with the second half of its bytes replaced does not
    // have a well-defined decode: Opus packets carry no checksum, so the
    // decoder may conceal or produce noise. The guarantee under test is only
    // that the result is typed and the call does not panic.
    let mut corrupt = bytes.clone();
    let halfway = corrupt.len() / 2;
    for byte in corrupt.iter_mut().skip(halfway) {
        *byte = 0xFF;
    }
    let result = decode_weba(&corrupt);
    if let Err(err) = result {
        eprintln!("corrupt tail: typed error: {err}");
    }
}

#[test]
#[ignore = "decodes all 1,404 clips; run with cargo test -p rasp-opus -- --ignored"]
fn full_corpus_decodes() {
    let Some(root) = corpus_root() else {
        return;
    };

    let mut paths = Vec::new();
    weba_files(&root, &mut paths);
    paths.sort();
    assert_eq!(paths.len(), 1404, "expected the full vendored corpus");

    let mut total_frames = 0u64;
    let mut peak = 0.0f32;
    let mut shortest = usize::MAX;
    let mut longest = 0usize;

    for path in &paths {
        let label = path.display().to_string();
        let clip = decode_path(path);
        check_clip(&label, &clip);
        total_frames += clip.frame_count() as u64;
        shortest = shortest.min(clip.frame_count());
        longest = longest.max(clip.frame_count());
        peak = peak.max(clip.samples.iter().fold(0.0f32, |max, s| max.max(s.abs())));
    }

    eprintln!(
        "full corpus: {}/1404 decoded, {:.1} s of audio, frames {shortest}..{longest}, peak {peak:.4}",
        paths.len(),
        total_frames as f64 / f64::from(SAMPLE_RATE_HZ)
    );
}
