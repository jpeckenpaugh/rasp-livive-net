//! Tests for the combined pitch/formant transformer (PLAN P-03).
//!
//! # What is being measured
//!
//! The acceptance criteria are formant *positions* after a transformation, so
//! the tests need a ruler. The ruler is [`testutil::fit_formants`], a fit of the
//! **generative model** to the harmonic amplitudes of the signal. It is run on:
//!
//! * the **source** vowel (synth values are known exactly by construction),
//! * an **analytic reference** — a vowel synthesized directly with the pitch and
//!   formant values the transformation should produce,
//! * the **processed** output.
//!
//! The reference is the control the P-02 lesson requires: if the ruler cannot
//! recover the formant ratio from an exact signal, the ruler is wrong and no
//! claim about the DSP is made. The table below is printed by
//! `synthetic_formant_table` (ignored by default; run with
//! `cargo test -p rasp-dsp --release synthetic_formant_table -- --ignored --nocapture`).
//!
//! Synthetic ground truth is primary. The fixtures are a sanity check on real
//! 32 kbps speech: they have no known formants, so they are only checked for
//! finiteness, level, pitch accuracy and gross envelope movement.
//!
//! # The two windows (D19)
//!
//! The correction pass's window is chosen from the **shifted output** F0, using
//! `ratio × input F0` as the proxy, and is independent of the pitch shifter's
//! window. `correction_window_follows_the_shifted_output_not_the_ratio` pins the
//! rule (including that the shifter stays at 2048 throughout) and
//! `f0_proxy_matches_synthetic_and_fixture_medians` certifies the proxy against
//! synthetic signals with known F0 and the corpus medians.

use super::*;
use crate::pitch::{PitchShiftConfig, PitchShifter};
use crate::stft::Complex;
use crate::stft::StftConfig;
use crate::testutil::{self, estimate_f0, max_abs, rms};
use std::time::Instant;

const SR: u32 = 48_000;
/// 0.6 s: long enough for a stable fit, short enough to keep the debug suite
/// under its 60 s budget.
const LEN: usize = 24_000;

/// One synthetic vowel definition: `(name, f0, formants, bandwidths)`.
type Vowel = (&'static str, f64, [f64; 3], [f64; 3]);

const VOWELS: [Vowel; 2] = [
    (
        "male 110",
        110.0,
        [700.0, 1220.0, 2600.0],
        [80.0, 90.0, 120.0],
    ),
    (
        "low male 90",
        90.0,
        [500.0, 1500.0, 2500.0],
        [70.0, 90.0, 120.0],
    ),
];

fn transformer(config: VoiceTransformConfig) -> VoiceTransformer {
    VoiceTransformer::new(SR, config).expect("valid configuration")
}

fn build(pitch_semitones: f32, formant: f32) -> VoiceTransformer {
    let mut transformer = transformer(VoiceTransformConfig::d2());
    transformer.set_pitch_semitones(pitch_semitones);
    transformer.set_formant_ratio(formant);
    transformer
}

fn render(pitch_semitones: f32, formant: f32, input: &[f32]) -> Vec<f32> {
    build(pitch_semitones, formant)
        .process(input)
        .expect("valid render")
}

fn source_vowel(vowel: &Vowel) -> Vec<f32> {
    testutil::vowel(vowel.1, &vowel.2, &vowel.3, SR, LEN)
}

fn fit(signal: &[f32], f0: f64, init: [f64; 3], bandwidths: [f64; 3]) -> [f64; 3] {
    let (formants, _) = testutil::fit_formants(signal, SR, f0, init, bandwidths)
        .expect("enough harmonics for a formant fit");
    formants
}

fn assert_within(measured: &[f64], expected: [f64; 3], tolerance_percent: f64, context: &str) {
    let errors = testutil::relative_error_percent(measured, &expected);
    for (index, error) in errors.iter().enumerate().take(2) {
        assert!(
            error.abs() <= tolerance_percent,
            "{context}: F{} measured {:.0} Hz, expected {:.0} Hz ({error:+.1}%, tolerance \
             {tolerance_percent}%)",
            index + 1,
            measured[index],
            expected[index]
        );
    }
}

// ---------------------------------------------------------------------------
// Contract tests (always run)
// ---------------------------------------------------------------------------

#[test]
fn bypass_is_bit_exact() {
    let input = testutil::white_noise(2_400, 1);
    let output = render(0.0, 1.0, &input);
    assert_eq!(output, input);
    assert!(build(0.0, 1.0).envelope().is_empty());
}

#[test]
fn output_length_always_equals_input_length() {
    for length in [0usize, 1, 17, 512, 4_096] {
        let input = testutil::white_noise(length, length as u64 + 7);
        for (pitch, formant) in [(0.0f32, 1.10f32), (7.0, 1.0)] {
            let output = render(pitch, formant, &input);
            assert_eq!(
                output.len(),
                input.len(),
                "length {length}, pitch {pitch}, formant {formant}"
            );
            assert!(output.iter().all(|value| value.is_finite()));
        }
    }
}

#[test]
fn empty_input_is_an_empty_output() {
    let mut transformer = transformer(VoiceTransformConfig::d2());
    transformer.set_pitch_ratio(1.5);
    let mut output = vec![1.0f32; 8];
    transformer.process_into(&[], &mut output).expect("valid");
    assert!(output.is_empty());
    assert!(transformer.envelope().is_empty());
}

#[test]
fn formant_ratio_is_clamped_and_sanitised() {
    let mut transformer = transformer(VoiceTransformConfig::d2());
    transformer.set_formant_ratio(10.0);
    assert_eq!(transformer.formant_ratio(), MAX_FORMANT_RATIO);
    transformer.set_formant_ratio(0.01);
    assert_eq!(transformer.formant_ratio(), MIN_FORMANT_RATIO);
    transformer.set_formant_ratio(f32::NAN);
    assert_eq!(transformer.formant_ratio(), 1.0);
    assert_eq!(
        VoiceTransformer::formant_limits(),
        (MIN_FORMANT_RATIO, MAX_FORMANT_RATIO)
    );
}

#[test]
fn zero_sample_rate_is_an_error() {
    assert_eq!(
        VoiceTransformer::new(0, VoiceTransformConfig::d2()).unwrap_err(),
        VoiceTransformError::ZeroSampleRate
    );
}

/// D19: the correction window follows the **shifted output** F0, while the
/// shifter's window stays 2048 regardless of the ratio. The two must be able to
/// differ in both directions.
#[test]
fn correction_window_follows_the_shifted_output_not_the_ratio() {
    // 220 Hz input: a −7 st shift leaves 147 Hz, which 2048 resolves.
    let high = testutil::vowel(
        220.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        LEN,
    );
    let mut transformer = build(-7.0, 1.0);
    let _ = transformer.process(&high).expect("valid");
    assert_eq!(transformer.pitch_window_size(), 2048);
    assert_eq!(transformer.correction_window_size(), 2048);
    let f0 = transformer.correction_f0_hz().expect("voiced output");
    assert!(
        (f0 - 147.0).abs() < 7.0,
        "shifted-output proxy should be near 147 Hz, got {f0:.1}"
    );

    // 90 Hz input: a −7 st shift leaves 60 Hz, which needs 4096 even though the
    // shifter's own analysis is unchanged.
    let low = testutil::vowel(
        90.0,
        &[500.0, 1500.0, 2500.0],
        &[70.0, 90.0, 120.0],
        SR,
        LEN,
    );
    let mut transformer = build(-7.0, 1.0);
    let _ = transformer.process(&low).expect("valid");
    assert_eq!(transformer.pitch_window_size(), 2048);
    assert_eq!(
        transformer.correction_window_size(),
        4096,
        "a 60 Hz shifted output must use the wide correction window"
    );
    let f0 = transformer.correction_f0_hz().expect("voiced output");
    assert!(
        (f0 - 60.0).abs() < 3.0,
        "shifted-output proxy should be near 60 Hz, got {f0:.1}"
    );

    // Conversely, a low voice at ratio 1 still analyses a 90 Hz signal in the
    // correction pass; the pitch window remains 2048 in both cases.
    assert_eq!(build(0.0, 1.10).pitch_window_size(), 2048);

    // The decoupling is real in both directions: a deliberately wide pitch
    // window must NOT drag the correction window with it.
    let config = VoiceTransformConfig {
        pitch: PitchShiftConfig::fixed(4096),
        ..VoiceTransformConfig::d2()
    };
    let mut transformer = VoiceTransformer::new(SR, config).expect("valid");
    transformer.set_pitch_semitones(-7.0);
    transformer.set_formant_ratio(1.0);
    let _ = transformer.process(&high).expect("valid");
    assert_eq!(transformer.pitch_window_size(), 4096);
    assert_eq!(
        transformer.correction_window_size(),
        2048,
        "the correction window must be chosen from the shifted output, not inherited"
    );
}

#[test]
fn fixed_correction_window_is_honoured() {
    let low = testutil::vowel(
        90.0,
        &[500.0, 1500.0, 2500.0],
        &[70.0, 90.0, 120.0],
        SR,
        LEN,
    );
    let config = VoiceTransformConfig {
        correction_window: CorrectionWindowPolicy::Fixed(2048),
        ..VoiceTransformConfig::d2()
    };
    let mut transformer = transformer(config);
    transformer.set_formant_ratio(1.10);
    let _ = transformer.process(&low).expect("valid");
    assert_eq!(transformer.correction_window_size(), 2048);
    assert_eq!(transformer.correction_f0_hz(), None);

    let mut wide = VoiceTransformer::new(
        SR,
        VoiceTransformConfig {
            correction_window: CorrectionWindowPolicy::Fixed(4096),
            ..VoiceTransformConfig::d2()
        },
    )
    .expect("valid");
    wide.set_formant_ratio(1.10);
    let _ = wide.process(&low).expect("valid");
    assert_eq!(wide.correction_window_size(), 4096);

    // An unsupported fixed size is a typed constructor error, not a panic.
    let config = VoiceTransformConfig {
        correction_window: CorrectionWindowPolicy::Fixed(3000),
        ..VoiceTransformConfig::d2()
    };
    assert!(matches!(
        VoiceTransformer::new(SR, config),
        Err(VoiceTransformError::Stft(_))
    ));
}

/// The window-sizing proxy is documented as accurate enough to make a
/// 100 Hz decision; this measures it against synthetic signals with a known F0
/// and the corpus medians from `docs/corpus.md`.
#[test]
fn f0_proxy_matches_synthetic_and_fixture_medians() {
    for f0 in [90.0f64, 110.0, 150.0, 220.0] {
        let input = testutil::vowel(f0, &[700.0, 1220.0, 2600.0], &[80.0, 90.0, 120.0], SR, LEN);
        let mut transformer = build(0.0, 1.10);
        let _ = transformer.process(&input).expect("valid");
        let measured = transformer.correction_f0_hz().expect("voiced");
        let error_percent = 100.0 * (measured as f64 - f0) / f0;
        println!("proxy: synthetic {f0:.0} Hz -> {measured:.2} Hz ({error_percent:+.2}%)");
        assert!(
            error_percent.abs() <= 2.0,
            "synthetic {f0:.0} Hz: proxy {measured:.2} Hz ({error_percent:+.2}%)"
        );
    }

    let Some(fixtures) = testutil::load_fixtures() else {
        println!("skipping fixture half: fixtures/manifest.json is absent");
        return;
    };
    // F0 medians from docs/corpus.md §5.2 / F-06.
    for (name, expected) in [
        ("am_onyx__got-time", 88.4f64),
        ("af_aoede__you-got-me", 221.3),
        ("af_heart__friends-now", 228.8),
        ("bf_alice__maybe-next-time", 248.0),
    ] {
        let Some(fixture) = fixtures.iter().find(|f| f.name == name) else {
            continue;
        };
        let mut transformer = build(0.0, 1.10);
        let _ = transformer.process(&fixture.samples).expect("valid");
        let measured = transformer.correction_f0_hz().expect("voiced fixture");
        let error_percent = 100.0 * (measured as f64 - expected) / expected;
        println!("proxy: {name} {expected:.1} Hz -> {measured:.2} Hz ({error_percent:+.2}%)");
        assert!(
            error_percent.abs() <= 10.0,
            "{name}: proxy {measured:.2} Hz vs corpus median {expected:.1} Hz ({error_percent:+.2}%)"
        );
    }
}

#[test]
fn renders_are_deterministic_and_free_of_state_leaks() {
    let mut input = source_vowel(&VOWELS[0]);
    input.truncate(6_000);
    let mut transformer = build(7.0, 1.10);
    let first = transformer.process(&input).expect("valid render");
    // A different render in between must not perturb the next one.
    transformer.set_pitch_semitones(-3.0);
    let _ = transformer.process(&input).expect("valid render");
    transformer.set_pitch_semitones(7.0);
    let second = transformer.process(&input).expect("valid render");
    assert_eq!(first, second);

    // A second instance with the same parameters must agree bit for bit.
    let third = render(7.0, 1.10, &input);
    assert_eq!(first, third);
}

#[test]
fn pitch_is_accurate_with_formants_held() {
    let input = source_vowel(&VOWELS[0]);
    for semitones in [-7.0f32, 7.0] {
        let output = render(semitones, 1.0, &input);
        let ratio = 2.0f64.powf(semitones as f64 / 12.0);
        let expected = 110.0 * ratio;
        let measured =
            estimate_f0(&output, SR, expected * 0.8, expected * 1.25).expect("voiced output");
        let cents = 1200.0 * (measured / expected).log2();
        println!(
            "pitch {semitones:+.0} st with formant 1.00: {measured:.2} Hz vs {expected:.2} Hz \
             ({cents:+.2} cents)"
        );
        assert!(
            cents.abs() <= 10.0,
            "{semitones:+} st: {measured:.2} Hz vs {expected:.2} Hz ({cents:+.1} cents)"
        );
    }
}

#[test]
fn envelope_is_exposed_for_the_robot_chain() {
    let input = source_vowel(&VOWELS[0]);
    let mut transformer = build(7.0, 1.10);
    let output = transformer.process(&input).expect("valid");
    let envelope = transformer.envelope();
    assert!(!envelope.is_empty());
    assert_eq!(envelope.sample_rate(), SR);
    // The envelope belongs to the correction pass, so its geometry follows the
    // correction window (D19), which may differ from the shifter's.
    assert_eq!(envelope.window_size(), transformer.correction_window_size());
    assert_eq!(envelope.hop(), transformer.correction_window_size() / 4);
    let stft = crate::stft::Stft::new(StftConfig::new(
        transformer.correction_window_size(),
        transformer.correction_window_size() / 4,
    ))
    .expect("valid");
    assert_eq!(envelope.frames(), stft.frame_count(output.len()));
    assert_eq!(envelope.bins(), stft.bins());
    // The envelope is a usable multiplier: envelope × excitation reconstructs a
    // frame, which is exactly what B-02 will do with a carrier.
    let mut bins = vec![Complex::new(0.01, 0.0); envelope.bins()];
    envelope.apply_gain(0, &mut bins).expect("matching lengths");
    assert!(bins.iter().all(|bin| bin.magnitude().is_finite()));
}

#[test]
fn formant_only_shift_moves_f1_and_f2_by_the_requested_ratio() {
    for vowel in &VOWELS {
        let input = source_vowel(vowel);
        let output = render(0.0, 1.10, &input);
        let expected = [vowel.2[0] * 1.10, vowel.2[1] * 1.10, vowel.2[2] * 1.10];
        let measured = fit(&output, vowel.1, expected, vowel.3);
        assert_within(
            &measured,
            expected,
            8.0,
            &format!("{} formant 1.10", vowel.0),
        );
    }
}

#[test]
fn formants_are_preserved_at_plus_and_minus_seven_semitones() {
    for vowel in &VOWELS {
        let input = source_vowel(vowel);
        for semitones in [-7.0f32, 7.0] {
            let output = render(semitones, 1.0, &input);
            let ratio = 2.0f64.powf(semitones as f64 / 12.0);
            let measured = fit(&output, vowel.1 * ratio, vowel.2, vowel.3);
            assert_within(
                &measured,
                vowel.2,
                8.0,
                &format!("{} pitch {semitones:+} formant 1.00", vowel.0),
            );
        }
    }
}

#[test]
fn both_axes_compose_without_dragging_the_formants() {
    for vowel in &VOWELS {
        let input = source_vowel(vowel);
        let output = render(7.0, 1.10, &input);
        let ratio = 2.0f64.powf(7.0 / 12.0);
        let expected = [vowel.2[0] * 1.10, vowel.2[1] * 1.10, vowel.2[2] * 1.10];
        let measured = fit(&output, vowel.1 * ratio, expected, vowel.3);
        assert_within(
            &measured,
            expected,
            8.0,
            &format!("{} pitch +7 formant 1.10", vowel.0),
        );
    }
}

#[test]
fn level_is_matched_to_the_pitch_shifted_signal() {
    let input = source_vowel(&VOWELS[0]);
    let mut shifter = PitchShifter::new(SR, PitchShiftConfig::auto()).expect("valid");
    shifter.set_ratio(2.0f32.powf(7.0 / 12.0));
    let shifted = shifter.process(&input).expect("render");
    // The 1.33 corner applies the largest correction, so it is the strictest
    // level check; 1.0 is covered by `formants_are_preserved_*`.
    let output = render(7.0, 1.33, &input);
    let ratio = rms(&output) / rms(&shifted);
    assert!(
        (ratio - 1.0).abs() < 0.02,
        "formant 1.33: output/pitch RMS {ratio:.3}"
    );
    assert!(
        max_abs(&output) < 4.0,
        "formant 1.33: peak {}",
        max_abs(&output)
    );
}

#[test]
fn fixtures_render_without_nan_or_length_change() {
    let Some(fixtures) = testutil::load_fixtures() else {
        println!("skipping: fixtures/manifest.json is absent (run scripts/make_fixtures.py)");
        return;
    };
    for fixture in fixtures.iter().take(1) {
        for (pitch, formant) in [(0.0f32, 1.10f32), (7.0, 1.0)] {
            let output = render(pitch, formant, &fixture.samples);
            assert_eq!(output.len(), fixture.samples.len(), "{}", fixture.name);
            assert!(
                output.iter().all(|value| value.is_finite()),
                "{} pitch {pitch} formant {formant}: non-finite output",
                fixture.name
            );
            assert!(
                max_abs(&output) < 8.0,
                "{} pitch {pitch} formant {formant}: peak {}",
                fixture.name,
                max_abs(&output)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Measurement tables (ignored: run explicitly, see the module docs)
// ---------------------------------------------------------------------------

/// The P-03 acceptance table: synthetic vowel × (pitch, formant) → measured
/// F1/F2/F3 and % error, with the analytic-reference control.
///
/// ```text
/// cargo test -p rasp-dsp --release synthetic_formant_table -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement table: cargo test -p rasp-dsp --release synthetic_formant_table -- --ignored --nocapture"]
fn synthetic_formant_table() {
    let cells = [
        (0.0f32, 1.00f32),
        (0.0, 1.10),
        (0.0, 0.75),
        (0.0, 1.33),
        (7.0, 1.00),
        (7.0, 1.10),
        (7.0, 0.75),
        (-7.0, 1.00),
        (-7.0, 1.10),
        (-7.0, 1.33),
    ];
    println!(
        "\n| vowel | pitch | formant | F1 | F2 | F3 | err F1% | err F2% | err F3% | control max% | N |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    for vowel in &VOWELS {
        let input = source_vowel(vowel);
        let source = fit(&input, vowel.1, vowel.2, vowel.3);
        println!(
            "| {} (source) | 0 | 1.00 | {:.0} | {:.0} | {:.0} | — | — | — | — | — |",
            vowel.0, source[0], source[1], source[2]
        );
        for (pitch, formant) in cells {
            let mut transformer = build(pitch, formant);
            let output = transformer.process(&input).expect("valid render");
            let ratio = 2.0f64.powf(pitch as f64 / 12.0);
            let expected = [
                vowel.2[0] * formant as f64,
                vowel.2[1] * formant as f64,
                vowel.2[2] * formant as f64,
            ];
            let reference = testutil::vowel(
                vowel.1 * ratio,
                &expected,
                &[
                    vowel.3[0] * formant as f64,
                    vowel.3[1] * formant as f64,
                    vowel.3[2] * formant as f64,
                ],
                SR,
                LEN,
            );
            let control = fit(&reference, vowel.1 * ratio, expected, vowel.3);
            let measured = fit(&output, vowel.1 * ratio, expected, vowel.3);
            let errors = testutil::relative_error_percent(&measured, &expected);
            let control_errors = testutil::relative_error_percent(&control, &expected);
            let control_worst = control_errors
                .iter()
                .fold(0.0f64, |acc, value| acc.max(value.abs()));
            println!(
                "| {} | {:+.0} | {:.2} | {:.0} | {:.0} | {:.0} | {:+.1} | {:+.1} | {:+.1} | {:.1} | {} |",
                vowel.0,
                pitch,
                formant,
                measured[0],
                measured[1],
                measured[2],
                errors[0],
                errors[1],
                errors[2],
                control_worst,
                transformer.correction_window_size()
            );
        }
    }
}

/// Cepstral vs LPC stability on the 32 kbps fixtures.
///
/// ```text
/// cargo test -p rasp-dsp --release envelope_method_stability -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement table: cargo test -p rasp-dsp --release envelope_method_stability -- --ignored --nocapture"]
fn envelope_method_stability() {
    let Some(fixtures) = testutil::load_fixtures() else {
        println!("skipping: fixtures/manifest.json is absent");
        return;
    };
    let wanted = ["am_onyx", "af_aoede", "bf_alice", "af_heart"];
    let mut stft = crate::stft::Stft::new(StftConfig::default()).expect("valid");
    let bin_hz = stft.bin_width_hz(SR) as f64;
    println!(
        "\n| fixture | method | frame Δ (dB, median) | F1 peak jitter (Hz) | F2 peak jitter (Hz) | mean |gain| (dB) | source peak | render peak |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for fixture in &fixtures {
        if !wanted.iter().any(|name| fixture.name.starts_with(name)) {
            continue;
        }
        let spectrogram = stft.analyze(&fixture.samples);
        for (name, config) in [
            ("cepstral", EnvelopeConfig::default()),
            ("lpc", EnvelopeConfig::lpc()),
        ] {
            let mut analyzer = EnvelopeAnalyzer::new(SR, config).expect("valid");
            let envelope = analyzer.analyze(&spectrogram).expect("valid");
            let frames = envelope.frames();
            let bins = envelope.bins();
            // Skip the leading/trailing zero-pad frames.
            let first = (stft.window_size() / 2 / stft.hop()).max(1);
            let last = frames.saturating_sub(first + 1);

            let mut deltas = Vec::new();
            for frame in first + 1..last {
                if let (Some(a), Some(b)) = (envelope.frame(frame - 1), envelope.frame(frame)) {
                    for (x, y) in a.iter().zip(b) {
                        deltas.push(((x - y) as f64 * 8.685_889).abs());
                    }
                }
            }
            deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
            let median_delta = deltas.get(deltas.len() / 2).copied().unwrap_or(0.0);

            // Peak position with parabolic interpolation; frames whose maximum
            // sits on a band edge are skipped rather than reported as a jump.
            let jitter = |low: f64, high: f64| -> f64 {
                let low_bin = (low / bin_hz) as usize + 2;
                let high_bin = ((high / bin_hz) as usize).min(bins - 3);
                let mut peaks = Vec::new();
                for frame in first..last {
                    let Some(values) = envelope.frame(frame) else {
                        continue;
                    };
                    let mut best_bin = low_bin;
                    let mut best = f32::NEG_INFINITY;
                    for (bin, &value) in values.iter().enumerate().take(high_bin + 1).skip(low_bin)
                    {
                        if value > best {
                            best = value;
                            best_bin = bin;
                        }
                    }
                    if best_bin <= low_bin || best_bin >= high_bin {
                        continue;
                    }
                    let left = values[best_bin - 1] as f64;
                    let centre = values[best_bin] as f64;
                    let right = values[best_bin + 1] as f64;
                    let denominator = left - 2.0 * centre + right;
                    let offset = if denominator.abs() > 1e-12 {
                        0.5 * (left - right) / denominator
                    } else {
                        0.0
                    };
                    peaks.push((best_bin as f64 + offset.clamp(-0.5, 0.5)) * bin_hz);
                }
                if peaks.len() < 3 {
                    return 0.0;
                }
                let mean = peaks.iter().sum::<f64>() / peaks.len() as f64;
                let variance = peaks
                    .iter()
                    .map(|value| (value - mean).powi(2))
                    .sum::<f64>()
                    / peaks.len() as f64;
                variance.sqrt()
            };
            let f1_jitter = jitter(300.0, 1000.0);
            let f2_jitter = jitter(900.0, 2400.0);

            // How hard does the correction push? Mean |gain| over the clip.
            let shifted = {
                let mut shifter =
                    crate::pitch::PitchShifter::new(SR, PitchShiftConfig::auto()).expect("valid");
                shifter.set_ratio(2.0f32.powf(7.0 / 12.0));
                shifter.process(&fixture.samples).expect("render")
            };
            let shifted_spectrogram = stft.analyze(&shifted);
            let shifted_envelope = analyzer.analyze(&shifted_spectrogram).expect("valid");
            let mut gain_total = 0.0f64;
            let mut gain_count = 0usize;
            for frame in 0..frames.min(shifted_envelope.frames()) {
                if let (Some(target), Some(observed)) =
                    (envelope.frame(frame), shifted_envelope.frame(frame))
                {
                    for (t, o) in target.iter().zip(observed) {
                        gain_total += ((t - o) as f64 * 8.685_889).abs();
                        gain_count += 1;
                    }
                }
            }
            let mean_gain = if gain_count == 0 {
                0.0
            } else {
                gain_total / gain_count as f64
            };

            let mut transformer = VoiceTransformer::new(
                SR,
                VoiceTransformConfig {
                    pitch: PitchShiftConfig::auto(),
                    envelope: config,
                    correction_window: CorrectionWindowPolicy::Auto,
                },
            )
            .expect("valid");
            transformer.set_pitch_semitones(7.0);
            transformer.set_formant_ratio(1.10);
            let output = transformer.process(&fixture.samples).expect("valid");
            println!(
                "| {} | {name} | {median_delta:.2} | {f1_jitter:.0} | {f2_jitter:.0} | {mean_gain:.1} | {:.3} | {:.3} |",
                fixture.name,
                max_abs(&fixture.samples),
                max_abs(&output)
            );
        }
    }
}

/// Cepstral vs LPC vs the brief's 40 ms lifter, on the required acceptance
/// cells. Documents why the default cutoff is 4 ms and why LPC is the
/// documented alternative rather than the default.
///
/// ```text
/// cargo test -p rasp-dsp --release envelope_method_comparison -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement table: cargo test -p rasp-dsp --release envelope_method_comparison -- --ignored --nocapture"]
fn envelope_method_comparison() {
    let cells = [(0.0f32, 1.10f32), (7.0, 1.00), (-7.0, 1.00), (7.0, 1.10)];
    let variants: [(&str, EnvelopeConfig); 3] = [
        ("cepstral 4 ms", EnvelopeConfig::default()),
        (
            "cepstral 40 ms",
            EnvelopeConfig {
                lifter_seconds: 0.040,
                ..EnvelopeConfig::default()
            },
        ),
        ("lpc 18", EnvelopeConfig::lpc()),
    ];
    println!("\n| estimator | vowel | pitch | formant | F1 err% | F2 err% | F3 err% |");
    println!("|---|---|---|---|---|---|---|");
    for (name, envelope) in variants {
        for vowel in &VOWELS {
            let input = source_vowel(vowel);
            for (pitch, formant) in cells {
                let config = VoiceTransformConfig {
                    pitch: PitchShiftConfig::auto(),
                    envelope,
                    correction_window: CorrectionWindowPolicy::Auto,
                };
                let mut transformer = VoiceTransformer::new(SR, config).expect("valid");
                transformer.set_pitch_semitones(pitch);
                transformer.set_formant_ratio(formant);
                let output = transformer.process(&input).expect("valid");
                let ratio = 2.0f64.powf(pitch as f64 / 12.0);
                let expected = [
                    vowel.2[0] * formant as f64,
                    vowel.2[1] * formant as f64,
                    vowel.2[2] * formant as f64,
                ];
                let measured = fit(&output, vowel.1 * ratio, expected, vowel.3);
                let errors = testutil::relative_error_percent(&measured, &expected);
                println!(
                    "| {name} | {} | {pitch:+.0} | {formant:.2} | {:+.1} | {:+.1} | {:+.1} |",
                    vowel.0, errors[0], errors[1], errors[2]
                );
            }
        }
    }
}

/// Native CPU cost, in milliseconds per second of audio.
///
/// Reports the pitch shifter alone at ratios 0.5/1.0/2.0, the full transformer
/// at the same ratios, and the cost of an independently sized correction window
/// (Auto vs `Fixed(2048)` and `Fixed(4096)` on the same signal).
///
/// ```text
/// cargo test -p rasp-dsp --release cpu_cost -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measurement: cargo test -p rasp-dsp --release cpu_cost -- --ignored --nocapture"]
fn cpu_cost() {
    let input = testutil::vowel(
        110.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        SR as usize,
    );
    let seconds = input.len() as f64 / SR as f64;

    let report = |label: &str, elapsed: f64, window: Option<usize>| {
        println!(
            "{label}: {:.1} ms/s ({:.1}x realtime){}",
            elapsed * 1000.0 / seconds,
            seconds / elapsed,
            window.map_or(String::new(), |n| format!(", N={n}"))
        );
    };

    // The pitch shifter alone, at the three requested ratios.
    for ratio in [0.5f32, 1.0, 2.0] {
        let mut shifter = PitchShifter::new(SR, PitchShiftConfig::auto()).expect("valid");
        shifter.set_ratio(ratio);
        let _ = shifter.process(&input).expect("valid");
        let start = Instant::now();
        for _ in 0..3 {
            let _ = shifter.process(&input).expect("valid");
        }
        report(
            &format!("pitch-only ratio {ratio:.2}"),
            start.elapsed().as_secs_f64() / 3.0,
            Some(shifter.window_size()),
        );
    }

    // The full transformer with the formant axis engaged (formant 1.00 at
    // ratio 1.0 would be the bit-exact bypass and measure nothing).
    for ratio in [0.5f32, 1.0, 2.0] {
        let mut transformer = transformer(VoiceTransformConfig::d2());
        transformer.set_pitch_ratio(ratio);
        transformer.set_formant_ratio(1.10);
        let _ = transformer.process(&input).expect("valid");
        let start = Instant::now();
        for _ in 0..3 {
            let _ = transformer.process(&input).expect("valid");
        }
        report(
            &format!("transformer ratio {ratio:.2}, formant 1.10 (auto)"),
            start.elapsed().as_secs_f64() / 3.0,
            Some(transformer.correction_window_size()),
        );
    }

    // The correction window's own contribution, isolated on a signal that
    // forces the wide window (110 Hz at −7 st = 73 Hz). Fixed(2048) also drops
    // the F0 proxy, so the difference includes the estimator.
    let low = input.clone();
    let cells = [
        (
            "correction auto (proxy, N=F0-dependent)",
            VoiceTransformConfig::d2(),
        ),
        (
            "correction fixed 2048 (no proxy)",
            VoiceTransformConfig {
                correction_window: CorrectionWindowPolicy::Fixed(2048),
                ..VoiceTransformConfig::d2()
            },
        ),
        (
            "correction fixed 4096",
            VoiceTransformConfig {
                correction_window: CorrectionWindowPolicy::Fixed(4096),
                ..VoiceTransformConfig::d2()
            },
        ),
    ];
    for (label, config) in cells {
        let mut transformer = transformer(config);
        transformer.set_pitch_semitones(-7.0);
        transformer.set_formant_ratio(1.10);
        let _ = transformer.process(&low).expect("valid");
        let start = Instant::now();
        for _ in 0..3 {
            let _ = transformer.process(&low).expect("valid");
        }
        report(
            label,
            start.elapsed().as_secs_f64() / 3.0,
            Some(transformer.correction_window_size()),
        );
    }
}
