//! Tests for endpoint detection and trimming (PLAN P-04).
//!
//! The corpus assertions use the generated fixtures, which are FFmpeg's
//! native-decoded PCM — the same decoder `scripts/measure_corpus.py` used for
//! `docs/corpus.md`, so the expected head/tail numbers are directly
//! comparable. Those tests skip (print a line) when `fixtures/manifest.json` is
//! absent; the synthetic tests always run.
//!
//! The full-corpus report over all 1404 clips is `#[ignore]`d:
//!
//! ```text
//! cargo test -p rasp-dsp --release corpus_endpoint_report -- --ignored --nocapture
//! ```

use super::*;
use crate::transform::{VoiceTransformConfig, VoiceTransformer};

const SR: u32 = 48_000;

/// The four clips the P-04 packet names, with their F-06 measurements
/// (`docs/corpus.md` §3 and the reconstruction the packet quotes).
const MEASURED: [(&str, f32, f32); 4] = [
    // fixture name, measured head ms, measured tail ms
    ("bf_emma__nice-day", 60.0, 456.0),
    ("zf_xiaoni__greet", 47.0, 58.0),
    ("am_onyx__got-time", 82.0, 307.0),
    ("hf_alpha__good-to-see", 36.0, 66.0),
];

fn fixtures() -> Option<Vec<crate::testutil::Fixture>> {
    crate::testutil::load_fixtures()
}

fn fixture(name: &str) -> Option<Vec<f32>> {
    fixtures()?
        .into_iter()
        .find(|fixture| fixture.name == name)
        .map(|fixture| fixture.samples)
}

/// Deterministic tone: a single sine at 220 Hz, amplitude 0.5.
fn tone(len: usize) -> Vec<f32> {
    (0..len)
        .map(|index| {
            let phase = 2.0 * std::f64::consts::PI * 220.0 * index as f64 / SR as f64;
            (0.5 * phase.sin()) as f32
        })
        .collect()
}

/// Tone between two silent stretches, with a one-sample ramp to avoid a
/// discontinuous synthetic onset that no real clip has.
fn padded_tone(head: usize, active: usize, tail: usize) -> Vec<f32> {
    let mut samples = vec![0.0f32; head + active + tail];
    let body = tone(active);
    samples[head..head + active].copy_from_slice(&body);
    samples
}

fn ms(samples: usize) -> f32 {
    samples as f32 * 1000.0 / SR as f32
}

fn ms_samples(ms: usize) -> usize {
    ms * SR as usize / 1000
}

// ---------------------------------------------------------------------------
// Corpus agreement
// ---------------------------------------------------------------------------

#[test]
fn detection_matches_the_measured_corpus_clips() {
    let Some(clips) = fixtures() else {
        println!("skipping: fixtures/manifest.json is absent");
        return;
    };
    let config = EndpointConfig::corpus();
    for (name, head_ms, tail_ms) in MEASURED {
        let Some(clip) = clips.iter().find(|clip| clip.name == name) else {
            println!("skipping {name}: not in the fixture set");
            continue;
        };
        let endpoints = detect_endpoints(&clip.samples, SR, &config);
        assert!(
            (endpoints.head_ms - head_ms).abs() <= 3.0,
            "{name}: detected head {:.1} ms, measured {head_ms:.1} ms",
            endpoints.head_ms
        );
        assert!(
            (endpoints.tail_ms - tail_ms).abs() <= 3.0,
            "{name}: detected tail {:.1} ms, measured {tail_ms:.1} ms",
            endpoints.tail_ms
        );
        // Margins: exactly 10 ms of head and `min(20 ms, tail)` of tail kept
        // beyond the detection point.
        assert_eq!(
            endpoints.start,
            (endpoints.head_ms * SR as f32 / 1000.0).round() as usize
                - config.pre_onset_samples(SR)
        );
        let expected_end = clip.samples.len()
            - (endpoints.tail_ms * SR as f32 / 1000.0).round() as usize
            + config.post_offset_samples(SR);
        assert_eq!(endpoints.end, expected_end.min(clip.samples.len()));
        assert!(endpoints.start < endpoints.end, "{name}: empty trim");
        println!(
            "{}: n={} head={:.1}ms tail={:.1}ms keep=[{}, {}) {} samples",
            name,
            clip.samples.len(),
            endpoints.head_ms,
            endpoints.tail_ms,
            endpoints.start,
            endpoints.end,
            endpoints.len()
        );
    }
}

#[test]
fn the_worst_tail_in_the_corpus_trims_to_its_measurement() {
    // bf_isabella/one-more is the 524 ms tail named in PLAN §1/§12. It is not
    // in the 12-clip fixture set, so this uses a synthetic clip with the same
    // silence profile; the ignored full-corpus report covers the real file.
    let mut samples = vec![0.0f32; ms_samples(38)];
    samples.extend(padded_tone(0, 48_000, ms_samples(524)));
    let endpoints = detect_endpoints(&samples, SR, &EndpointConfig::corpus());
    assert!((endpoints.tail_ms - 524.0).abs() <= 12.0, "{endpoints:?}");
    // The 20 ms post-offset margin means ~504 ms is removed, not 524 ms.
    assert!((ms(samples.len() - endpoints.end) - 504.0).abs() <= 12.0);
    assert_eq!(
        endpoints.len(),
        samples.len() - endpoints.start - (samples.len() - endpoints.end)
    );
}

// ---------------------------------------------------------------------------
// Detection semantics
// ---------------------------------------------------------------------------

#[test]
fn margins_are_kept_and_measured_durations_are_reported() {
    // 100 ms of head silence, 300 ms of tone, 200 ms of tail silence.
    let samples = padded_tone(4_800, 14_400, 9_600);
    let endpoints = detect_endpoints(&samples, SR, &EndpointConfig::corpus());
    // The measurement is F-06's window position, which leads a hard synthetic
    // onset by up to half a window (5 ms) and lags a gradual one.
    assert!((endpoints.head_ms - 100.0).abs() <= 8.0, "{endpoints:?}");
    assert!((endpoints.tail_ms - 200.0).abs() <= 8.0, "{endpoints:?}");
    // The margin is exact: `start` is the detected head minus 10 ms, and the
    // detected head cannot be later than the onset (that would cut audio).
    assert_eq!(
        endpoints.start,
        (endpoints.head_ms * SR as f32 / 1000.0).round() as usize
            - EndpointConfig::corpus().pre_onset_samples(SR)
    );
    assert!(endpoints.start <= 4_800 - 480 + 1);
    assert_eq!(
        endpoints.end,
        samples.len() - (endpoints.tail_ms * SR as f32 / 1000.0).round() as usize
            + EndpointConfig::corpus().post_offset_samples(SR)
    );
    assert!(endpoints.end >= samples.len() - 9_600 + 960 - 1);
    assert_eq!(endpoints.len(), endpoints.end - endpoints.start);
    assert!(!endpoints.is_empty());
}

#[test]
fn short_clicks_below_the_minimum_run_are_ignored() {
    // A 5 ms burst inside 400 ms of silence: shorter than the 20 ms run.
    let mut samples = vec![0.0f32; 19_200];
    let burst = tone(240);
    samples[9_600..9_840].copy_from_slice(&burst);
    let endpoints = detect_endpoints(&samples, SR, &EndpointConfig::corpus());
    assert_eq!(endpoints.start, 0, "an isolated click must not trim");
    assert_eq!(endpoints.end, samples.len());
    assert!((endpoints.head_ms - 400.0).abs() < 0.01);
    let trimmed = trim_clip(&samples, SR, &EndpointConfig::corpus());
    assert!(!trimmed.fade_applied);
    assert_eq!(trimmed.samples, samples);
}

#[test]
fn all_silence_is_never_trimmed_to_nothing() {
    let samples = vec![0.0f32; 48_000];
    let endpoints = detect_endpoints(&samples, SR, &EndpointConfig::corpus());
    assert_eq!(endpoints.start, 0);
    assert_eq!(endpoints.end, samples.len());
    // F-06's convention: the whole clip is both head and tail silence.
    assert!((endpoints.head_ms - 1000.0).abs() < 1e-3);
    assert!((endpoints.tail_ms - 1000.0).abs() < 1e-3);
    let trimmed = trim_clip(&samples, SR, &EndpointConfig::corpus());
    assert_eq!(trimmed.samples.len(), samples.len());
    assert!(!trimmed.fade_applied);
}

#[test]
fn empty_and_degenerate_inputs_are_finite_and_safe() {
    let config = EndpointConfig::corpus();
    let endpoints = detect_endpoints(&[], SR, &config);
    assert_eq!(endpoints, Endpoints::full(0));
    let trimmed = trim_clip(&[], SR, &config);
    assert!(trimmed.is_empty());
    assert!(!trimmed.fade_applied);

    // Zero sample rate: no envelope geometry, no panic.
    let endpoints = detect_endpoints(&[0.1; 100], 0, &config);
    assert_eq!(endpoints.start, 0);
    assert_eq!(endpoints.end, 100);

    // Hostile configuration: non-finite and negative fields fall back.
    let hostile = EndpointConfig {
        threshold_dbfs: f32::NAN,
        frame_seconds: f32::INFINITY,
        min_run_seconds: -3.0,
        pre_onset_seconds: f32::NAN,
        post_offset_seconds: -1.0,
        fade_seconds: f32::NAN,
    };
    let samples = padded_tone(2_400, 9_600, 2_400);
    let endpoints = detect_endpoints(&samples, SR, &hostile);
    assert!(endpoints.start <= endpoints.end && endpoints.end <= samples.len());
    assert!(endpoints.head_ms.is_finite() && endpoints.tail_ms.is_finite());

    // Non-finite samples are treated as energy the detector can read, but the
    // result must stay finite and bounded.
    let mut noisy = padded_tone(2_400, 9_600, 2_400);
    noisy[5_000] = f32::NAN;
    let endpoints = detect_endpoints(&noisy, SR, &config);
    assert!(endpoints.start <= endpoints.end && endpoints.end <= noisy.len());
}

#[test]
fn threshold_and_margins_are_configurable() {
    let samples = padded_tone(4_800, 14_400, 9_600);
    // A margin wider than the measured silence keeps the whole clip.
    let generous = EndpointConfig {
        pre_onset_seconds: 0.2,
        post_offset_seconds: 0.3,
        ..EndpointConfig::corpus()
    };
    let endpoints = detect_endpoints(&samples, SR, &generous);
    assert_eq!(endpoints.start, 0);
    assert_eq!(endpoints.end, samples.len());

    // A very high threshold treats the tone itself as silence, so nothing is
    // trimmed (there is no qualifying run) rather than everything.
    let deaf = EndpointConfig {
        threshold_dbfs: 6.0,
        ..EndpointConfig::corpus()
    };
    let endpoints = detect_endpoints(&samples, SR, &deaf);
    assert_eq!(endpoints.start, 0);
    assert_eq!(endpoints.end, samples.len());
}

// ---------------------------------------------------------------------------
// Fades and boundaries
// ---------------------------------------------------------------------------

#[test]
fn fades_start_at_exactly_zero_and_finish_inside_five_ms() {
    let samples = padded_tone(4_800, 14_400, 9_600);
    let trimmed = trim_clip(&samples, SR, &EndpointConfig::corpus());
    assert!(trimmed.fade_applied);
    assert_eq!(trimmed.samples.len(), trimmed.end - trimmed.start);
    assert_eq!(trimmed.samples[0], 0.0, "the first sample must be silent");
    let last = trimmed.samples.len() - 1;
    assert_eq!(trimmed.samples[last], 0.0, "the last sample must be silent");

    // 5 ms at 48 kHz. Gains follow 0.5 - 0.5·cos(π·i/(L-1)).
    let fade = 240usize;
    let gain = |i: usize| 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / (fade - 1) as f32).cos();
    for i in [0usize, 1, 60, 120, fade - 1] {
        let source = samples[trimmed.start + i];
        let expected = source * gain(i);
        assert!(
            (trimmed.samples[i] - expected).abs() <= 1e-6,
            "fade-in sample {i}: {} vs {expected}",
            trimmed.samples[i]
        );
    }
    // The interior is untouched: the sample just after the ramp is the source.
    assert_eq!(
        trimmed.samples[fade],
        samples[trimmed.start + fade],
        "the fade must not reach past its 5 ms envelope"
    );

    // The 20 ms kept after the offset is longer than the fade, so the tail is
    // faded inside audio that is already near-silent.
    let source = samples[trimmed.start + last];
    assert!((trimmed.samples[last] - source * gain(0)).abs() <= 1e-9);
}

#[test]
fn trimmed_boundaries_have_no_click() {
    let Some(clip) = fixture("af_heart__friends-now") else {
        println!("skipping: fixtures/manifest.json is absent");
        return;
    };
    let trimmed = trim_clip(&clip, SR, &EndpointConfig::corpus());
    assert!(trimmed.fade_applied);
    let peak = trimmed
        .samples
        .iter()
        .fold(0.0f32, |acc, value| acc.max(value.abs())) as f64;
    let fade = 240usize;
    let gain = |i: usize| 0.5 - 0.5 * (std::f64::consts::PI * i as f64 / (fade - 1) as f64).cos();

    // First/last five samples: below what the fade itself predicts, with a
    // small allowance for f32 rounding.
    for i in 0..5 {
        let bound = peak * gain(i) + 1e-9;
        assert!(
            trimmed.samples[i].abs() as f64 <= bound,
            "head sample {i}: {} > fade expectation {bound}",
            trimmed.samples[i]
        );
        let j = trimmed.samples.len() - 1 - i;
        assert!(
            trimmed.samples[j].abs() as f64 <= bound,
            "tail sample {j}: {} > fade expectation {bound}",
            trimmed.samples[j]
        );
    }

    // Derivative inside the fade envelope: |y[i+1] - y[i]| cannot exceed the
    // peak times the sum of the two gains.
    for i in 0..fade - 1 {
        let slope = (trimmed.samples[i + 1] - trimmed.samples[i]).abs() as f64;
        let bound = peak * (gain(i) + gain(i + 1)) + 1e-9;
        assert!(
            slope <= bound,
            "head derivative {i}: {slope} > envelope {bound}"
        );
        let j = trimmed.samples.len() - 1 - i;
        let slope = (trimmed.samples[j] - trimmed.samples[j - 1]).abs() as f64;
        let bound = peak * (gain(i) + gain(i + 1)) + 1e-9;
        assert!(
            slope <= bound,
            "tail derivative {i}: {slope} > envelope {bound}"
        );
    }
}

#[test]
fn bypass_render_of_a_trimmed_clip_is_the_faded_input() {
    // P-04 boundary acceptance: pitch 0 / formant 1.0 must be a bit-exact
    // bypass, so rendering the trimmed clip reproduces the trim exactly —
    // no second fade, no length change.
    let Some(clip) = fixture("bf_emma__nice-day") else {
        println!("skipping: fixtures/manifest.json is absent");
        return;
    };
    let trimmed = trim_clip(&clip, SR, &EndpointConfig::corpus());
    let mut transformer = VoiceTransformer::new(SR, VoiceTransformConfig::d2()).expect("valid");
    let rendered = transformer.process(&trimmed.samples).expect("valid render");
    assert_eq!(rendered.len(), trimmed.len());
    assert_eq!(rendered, trimmed.samples);

    // And the trimmed signal is the faded source slice, not a faded copy of
    // the whole clip: the fade starts at `trimmed.start`.
    assert_eq!(trimmed.samples[0], 0.0);
    assert_ne!(trimmed.samples[240], 0.0);
    assert_eq!(trimmed.samples[240], clip[trimmed.start + 240]);
}

#[test]
fn trimming_is_deterministic() {
    let samples = padded_tone(4_003, 13_337, 9_101);
    let config = EndpointConfig::corpus();
    let first = trim_clip(&samples, SR, &config);
    let second = trim_clip(&samples, SR, &config);
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// Full-corpus report (ignored: decodes all 1404 clips)
// ---------------------------------------------------------------------------

/// Detect endpoints on the whole corpus and print the distributions the P-04
/// packet asks for: head/tail percentiles, the number of clips where trimming
/// would remove energy (voiced content), and any clip whose trim is empty.
///
/// Decodes `web/audio/` with the workspace's own decoder (`rasp-opus`), so the
/// numbers are the ones the product will see rather than FFmpeg's.
///
/// ```text
/// cargo test -p rasp-dsp --release corpus_endpoint_report -- --ignored --nocapture
/// ```
#[test]
#[ignore = "full-corpus report: decodes 1404 clips; cargo test -p rasp-dsp --release corpus_endpoint_report -- --ignored --nocapture"]
fn corpus_endpoint_report() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/audio");
    if !root.is_dir() {
        println!("skipping: {} is absent", root.display());
        return;
    }
    let config = EndpointConfig::corpus();
    let mut files = Vec::new();
    if let Ok(voices) = std::fs::read_dir(&root) {
        for voice in voices.flatten() {
            let path = voice.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    let file = entry.path();
                    if file.extension().is_some_and(|ext| ext == "weba") {
                        files.push(file);
                    }
                }
            }
        }
    }
    if files.is_empty() {
        println!("skipping: no .weba files under {}", root.display());
        return;
    }
    files.sort();

    let mut heads = Vec::new();
    let mut tails = Vec::new();
    let mut empty = Vec::new();
    let mut removed_ratio = Vec::new();
    let mut max_removed: (f64, String) = (0.0, String::new());
    let mut failures = 0usize;
    let mut total_seconds = 0.0f64;
    let mut total_removed = 0.0f64;

    for (index, file) in files.iter().enumerate() {
        let decoded = match rasp_opus::decode_weba_file(file) {
            Ok(clip) => clip,
            Err(error) => {
                failures += 1;
                println!("{}: decode failed: {error}", file.display());
                continue;
            }
        };
        let samples = decoded.samples;
        let endpoints = detect_endpoints(&samples, SR, &config);
        let trimmed = trim_clip(&samples, SR, &config);
        heads.push(endpoints.head_ms);
        tails.push(endpoints.tail_ms);
        let name = file
            .strip_prefix(&root)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| file.display().to_string());
        if endpoints.start >= endpoints.end {
            empty.push(name.clone());
        }

        // "Removes voiced content": how much of the clip's energy lives in the
        // samples the trim discards. A silence-only trim is a tiny fraction;
        // a wrong endpoint that cuts speech shows up as a real percentage.
        let total: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
        let head_removed: f64 = samples[..endpoints.start]
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum();
        let tail_removed: f64 = samples[endpoints.end..]
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum();
        let removed = if total > 0.0 {
            (head_removed + tail_removed) / total
        } else {
            0.0
        };
        removed_ratio.push(removed);
        total_seconds += samples.len() as f64 / SR as f64;
        total_removed += trimmed.samples.len() as f64 / SR as f64;
        if removed > max_removed.0 {
            max_removed = (removed, name);
        }
        if index % 200 == 0 {
            println!("  {} / {}", index + 1, files.len());
        }
    }

    let stats = |values: &mut Vec<f32>| {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        let at = |q: f32| {
            let index = ((values.len() - 1) as f32 * q).round() as usize;
            values[index]
        };
        (at(0.5), at(0.9), at(0.99), values[values.len() - 1])
    };
    let (head_med, head_p90, head_p99, head_max) = stats(&mut heads);
    let (tail_med, tail_p90, tail_p99, tail_max) = stats(&mut tails);
    removed_ratio.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let removed_1 = removed_ratio.iter().filter(|&&value| value > 0.01).count();
    let removed_01 = removed_ratio.iter().filter(|&&value| value > 0.001).count();
    let head_100 = heads.iter().filter(|&&value| value > 100.0).count();
    let tail_100 = tails.iter().filter(|&&value| value > 100.0).count();

    println!("\n## P-04 endpoint detection, full corpus\n");
    println!("clips: {} ({} decode failures)", files.len(), failures);
    println!(
        "| statistic | head ms | tail ms |\n|---|---|---|\n| median | {head_med:.1} | {tail_med:.1} |\n| p90 | {head_p90:.1} | {tail_p90:.1} |\n| p99 | {head_p99:.1} | {tail_p99:.1} |\n| max | {head_max:.1} | {tail_max:.1} |"
    );
    println!("\nclips > 100 ms silence: head {head_100}, tail {tail_100}");
    println!(
        "clips where the trim would remove > 1% of clip energy: {removed_1}; > 0.1%: {removed_01}; worst {:.3}% ({})",
        max_removed.0 * 100.0,
        max_removed.1
    );
    println!(
        "clips with an empty trim (start >= end): {} {}",
        empty.len(),
        empty.first().map(String::as_str).unwrap_or("")
    );
    println!(
        "measured duration {total_seconds:.1} s, trimmed duration {total_removed:.1} s, removed {:.1}%",
        100.0 * (total_seconds - total_removed) / total_seconds
    );
    assert_eq!(failures, 0, "every corpus clip must decode");
    assert!(empty.is_empty(), "a trim must never be empty");
}
