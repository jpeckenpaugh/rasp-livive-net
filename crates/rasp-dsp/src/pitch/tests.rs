//! Tests for the phase-vocoder pitch shifter (PLAN P-02).
//!
//! Synthetic signals prove the pitch maths; fixture-based tests skip when
//! `fixtures/manifest.json` is absent, per `docs/testing.md`. The reported
//! cents-error table comes from `scripts/metrics.py`, not from these tests.

use super::*;
use crate::testutil::{self, estimate_f0, harmonic_vowel, max_abs, sine, white_noise};

const SR: u32 = 48_000;

fn build() -> PitchShifter {
    PitchShifter::new(SR, PitchShiftConfig::auto()).expect("valid configuration")
}

fn cents_error(expected: f64, measured: f64) -> f64 {
    1200.0 * (measured / expected).log2()
}

fn rms(signal: &[f32]) -> f64 {
    if signal.is_empty() {
        return 0.0;
    }
    let sum: f64 = signal.iter().map(|&x| (x as f64) * (x as f64)).sum();
    (sum / signal.len() as f64).sqrt()
}

/// Highest-energy analysis slice, so input and output are measured on the
/// same voiced region (the whole-buffer output is time-aligned).
const ANALYSIS_SLICE: usize = 4096;

fn voiced_slice_start(signal: &[f32]) -> usize {
    let window = ANALYSIS_SLICE.min(signal.len());
    let stride = (window / 4).max(1);
    let mut best = (0usize, f64::NEG_INFINITY);
    let mut start = 0usize;
    while start + window <= signal.len() {
        let energy: f64 = signal[start..start + window]
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum();
        if energy > best.1 {
            best = (start, energy);
        }
        start += stride;
    }
    best.0
}

/// Analysis slice whose own F0 is closest to `target`, used to reach the low
/// end of a voice that moves around (the low male anchor).
fn slice_near_f0(signal: &[f32], target: f64) -> usize {
    let window = ANALYSIS_SLICE.min(signal.len());
    let stride = (window / 2).max(1);
    let mut best = (0usize, f64::INFINITY);
    let mut start = 0usize;
    while start + window <= signal.len() {
        let energy: f64 = signal[start..start + window]
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum();
        if energy > 0.05 {
            if let Some(f0) = estimate_f0(&signal[start..start + window], SR, 60.0, 400.0) {
                let distance = (f0 - target).abs();
                if distance < best.1 {
                    best = (start, distance);
                }
            }
        }
        start += stride;
    }
    best.0
}

// ---------------------------------------------------------------------------
// Synthetic signals
// ---------------------------------------------------------------------------

#[test]
fn sine_f0_is_accurate_at_every_semitone() {
    let input = sine(110.0, SR, SR as usize);
    let mut shifter = build();
    let mut worst = 0.0f64;
    for step in -12..=12 {
        let ratio = 2.0f32.powf(step as f32 / 12.0);
        shifter.set_ratio(ratio);
        let output = shifter.process(&input).expect("render");
        let expected = 110.0 * ratio as f64;
        let measured =
            estimate_f0(&output, SR, expected * 0.45, expected * 2.2).expect("voiced output");
        let error = cents_error(expected, measured);
        worst = worst.max(error.abs());
        assert!(
            error.abs() <= 2.0,
            "{step:+} st: expected {expected:.3} Hz, measured {measured:.3} Hz ({error:+.2} cents)"
        );
    }
    println!("sine 110 Hz: worst {worst:.3} cents over -12..=+12 st");
}

#[test]
fn sine_110_220_440_have_no_octave_errors() {
    let input_len = SR as usize;
    for frequency in [110.0f64, 220.0, 440.0] {
        for step in [-12i32, -7, -1, 0, 3, 7] {
            let ratio = 2.0f32.powf(step as f32 / 12.0);
            let input = sine(frequency, SR, input_len);
            let mut shifter = build();
            shifter.set_ratio(ratio);
            let output = shifter.process(&input).expect("render");
            let expected = frequency * ratio as f64;
            let measured =
                estimate_f0(&output, SR, expected * 0.45, expected * 2.2).expect("voiced");
            let error = cents_error(expected, measured);
            assert!(
                error.abs() <= 2.0,
                "{frequency} Hz {step:+} st: {measured:.2} Hz vs {expected:.2} Hz ({error:+.2} c)"
            );
        }
    }
}

#[test]
fn harmonic_vowel_f0_is_accurate() {
    let input = harmonic_vowel(110.0, SR, SR as usize);
    let mut shifter = build();
    for step in [-12i32, -7, -1, 0, 3, 7, 12] {
        let ratio = 2.0f32.powf(step as f32 / 12.0);
        shifter.set_ratio(ratio);
        let output = shifter.process(&input).expect("render");
        let expected = 110.0 * ratio as f64;
        let measured = estimate_f0(&output, SR, expected * 0.45, expected * 2.2).expect("voiced");
        let error = cents_error(expected, measured);
        assert!(
            error.abs() <= 2.0,
            "vowel {step:+} st: {measured:.2} Hz vs {expected:.2} Hz ({error:+.2} c)"
        );
    }
}

#[test]
fn white_noise_stays_finite_and_bounded() {
    let input = white_noise(24_000, 7);
    let input_rms = rms(&input);
    for ratio in [0.5f32, 0.75, 1.5, 2.0] {
        let mut shifter = build();
        shifter.set_ratio(ratio);
        let output = shifter.process(&input).expect("render");
        assert_eq!(output.len(), input.len());
        assert!(
            output.iter().all(|x| x.is_finite()),
            "non-finite sample at ratio {ratio}"
        );
        let output_rms = rms(&output);
        assert!(
            output_rms < input_rms * 4.0 && output_rms > input_rms * 0.1,
            "ratio {ratio}: RMS {output_rms:.4} vs input {input_rms:.4}"
        );
        assert!(max_abs(&output) < 10.0, "ratio {ratio}: output exploded");
    }
}

#[test]
fn silence_stays_silent() {
    let input = vec![0.0f32; 12_345];
    for ratio in [0.5f32, 1.0, 1.7] {
        let mut shifter = build();
        shifter.set_ratio(ratio);
        let output = shifter.process(&input).expect("render");
        assert_eq!(output.len(), input.len());
        assert!(
            output.iter().all(|&x| x == 0.0),
            "ratio {ratio} produced sound"
        );
    }
}

#[test]
fn identity_ratio_is_a_bit_exact_bypass() {
    let input = white_noise(24_000, 11);
    let mut shifter = build();
    shifter.set_ratio(1.0);
    let output = shifter.process(&input).expect("render");
    assert_eq!(output.len(), input.len());
    assert_eq!(output, input, "ratio 1.0 must be a bit-exact bypass");

    let curve = shifter
        .process_with_curve(&input, &[1.0; 64])
        .expect("render");
    assert_eq!(curve, input, "a constant-1.0 curve must also bypass");
}

// ---------------------------------------------------------------------------
// Length, determinism, construction
// ---------------------------------------------------------------------------

#[test]
fn output_length_always_equals_input_length() {
    let lengths = [0usize, 1, 2, 17, 511, 512, 513, 2047, 2048, 2049, 10_000];
    let mut shifter = build();
    for ratio in [0.5f32, 0.9, 1.0, 1.7, 2.0] {
        shifter.set_ratio(ratio);
        for &length in &lengths {
            let input = white_noise(length, length as u64 + 1);
            let output = shifter.process(&input).expect("render");
            assert_eq!(
                output.len(),
                length,
                "ratio {ratio}, input length {length}: output {}",
                output.len()
            );
            let curved = shifter
                .process_with_curve(&input, &[ratio; 64])
                .expect("render");
            assert_eq!(
                curved.len(),
                length,
                "curve ratio {ratio}, input length {length}: output {}",
                curved.len()
            );
        }
    }
}

#[test]
fn same_input_and_parameters_are_bit_identical() {
    let input = white_noise(20_000, 3);
    let mut a = build();
    let mut b = build();
    a.set_ratio(1.37);
    b.set_ratio(1.37);
    let first = a.process(&input).expect("render");
    let second = a.process(&input).expect("render");
    let fresh = b.process(&input).expect("render");
    assert_eq!(first, second, "same instance is not repeatable");
    assert_eq!(first, fresh, "two instances disagree");

    let curve: Vec<f32> = (0..64).map(|i| 0.75 + 0.5 * (i as f32 / 63.0)).collect();
    let curve_first = a.process_with_curve(&input, &curve).expect("render");
    let curve_second = a.process_with_curve(&input, &curve).expect("render");
    assert_eq!(curve_first, curve_second, "curve render is not repeatable");
}

#[test]
fn ratio_is_clamped_to_the_supported_range() {
    let mut shifter = build();
    shifter.set_ratio(0.1);
    assert_eq!(shifter.ratio(), MIN_RATIO);
    shifter.set_ratio(9.0);
    assert_eq!(shifter.ratio(), MAX_RATIO);
    shifter.set_ratio(f32::NAN);
    assert_eq!(shifter.ratio(), 1.0);
    shifter.set_ratio(f32::INFINITY);
    assert_eq!(shifter.ratio(), 1.0);
}

#[test]
fn zero_sample_rate_is_rejected() {
    assert_eq!(
        PitchShifter::new(0, PitchShiftConfig::auto()).unwrap_err(),
        PitchShiftError::ZeroSampleRate
    );
}

#[test]
fn invalid_fixed_window_is_rejected() {
    assert_eq!(
        PitchShifter::new(SR, PitchShiftConfig::fixed(3000)).unwrap_err(),
        PitchShiftError::UnsupportedWindowSize { window_size: 3000 }
    );
}

/// D19: the automatic window follows the *input* signal, which the requested
/// ratio does not change. The ratio must never move it.
#[test]
fn auto_window_is_independent_of_the_ratio() {
    let mut shifter = build();
    assert_eq!(shifter.window_size(), 2048);

    for ratio in [2.0f32.powf(-12.0 / 12.0), 0.5, 0.5946, 1.0, 2.0] {
        shifter.set_ratio(ratio);
        assert_eq!(
            shifter.window_size(),
            2048,
            "ratio {ratio} must not widen the analysis window"
        );
        assert_eq!(shifter.hop(), 512);
    }

    // A curve cannot move it either, not even a one-hop dip to the minimum
    // ratio: the window is fixed at construction.
    let input = white_noise(8_000, 5);
    let curve = [1.0f32, 1.0, 0.5, 1.0];
    let _ = shifter.process_with_curve(&input, &curve).expect("render");
    assert_eq!(shifter.window_size(), 2048);

    // An explicit override still works (input F0 below 2048's resolution).
    let mut wide = PitchShifter::new(SR, PitchShiftConfig::fixed(4096)).expect("valid");
    wide.set_ratio(0.5);
    assert_eq!(wide.window_size(), 4096);
    assert_eq!(wide.hop(), 1024);
}

#[test]
fn fixed_window_policy_is_honoured() {
    let mut shifter =
        PitchShifter::new(SR, PitchShiftConfig::fixed(1024)).expect("valid configuration");
    shifter.set_ratio(0.5);
    assert_eq!(shifter.window_size(), 1024);
    assert_eq!(shifter.hop(), 256);
}

// ---------------------------------------------------------------------------
// Ratio curve
// ---------------------------------------------------------------------------

#[test]
fn constant_curve_matches_the_scalar_path() {
    let input = white_noise(12_000, 9);
    for ratio in [0.5f32, 0.8, 1.0, 1.25, 2.0] {
        let mut shifter = build();
        shifter.set_ratio(ratio);
        let scalar = shifter.process(&input).expect("render");
        let curve = shifter
            .process_with_curve(&input, &[ratio; 128])
            .expect("render");
        assert_eq!(
            scalar, curve,
            "constant curve at ratio {ratio} diverged from the scalar path"
        );
    }
}

#[test]
fn empty_curve_falls_back_to_the_scalar_ratio() {
    let input = white_noise(9_000, 13);
    let mut shifter = build();
    shifter.set_ratio(1.4);
    let scalar = shifter.process(&input).expect("render");
    let empty = shifter.process_with_curve(&input, &[]).expect("render");
    assert_eq!(scalar, empty);
}

#[test]
fn non_finite_curve_entries_fall_back_to_the_scalar_ratio() {
    let input = white_noise(9_000, 14);
    let mut shifter = build();
    shifter.set_ratio(1.0);
    let with_nan = shifter
        .process_with_curve(&input, &[f32::NAN, f32::INFINITY])
        .expect("render");
    shifter.set_ratio(1.0);
    let scalar = shifter.process(&input).expect("render");
    assert_eq!(with_nan, scalar);

    // A mixed curve must resolve each entry exactly as the frame loop does:
    // the non-finite entry becomes the scalar ratio, the rest stay as asked.
    let input = sine(220.0, SR, 24_000);
    let mixed = shifter
        .process_with_curve(&input, &[f32::NAN, 1.5])
        .expect("render");
    let explicit = shifter
        .process_with_curve(&input, &[1.0, 1.5])
        .expect("render");
    assert_eq!(mixed, explicit);
}

#[test]
fn curve_changes_pitch_segment_by_segment() {
    // 220 Hz input. The first half asks for -12 st (110 Hz), the second +12 st
    // (440 Hz). The window is 2048 (D19), so the hop is 512.
    let input = sine(220.0, SR, SR as usize * 2);
    let mut shifter = build();
    let hop = 512;
    let curve_len = input.len() / hop + 2;
    let mut curve = vec![2.0f32; curve_len];
    for value in curve.iter_mut().take((input.len() / 2) / hop) {
        *value = 0.5;
    }
    shifter.set_ratio(1.0);
    let output = shifter.process_with_curve(&input, &curve).expect("render");
    assert_eq!(output.len(), input.len());

    let half = input.len() / 2;
    let low = estimate_f0(&output[..half], SR, 80.0, 160.0).expect("voiced first half");
    let high = estimate_f0(&output[half..], SR, 300.0, 600.0).expect("voiced second half");
    assert!(
        cents_error(110.0, low).abs() <= 5.0,
        "first half: {low:.2} Hz, expected 110 Hz"
    );
    assert!(
        cents_error(440.0, high).abs() <= 5.0,
        "second half: {high:.2} Hz, expected 440 Hz"
    );
}

/// B-04's monotone flattening needs a ratio curve that actually moves pitch
/// over time. A 2 s synthetic vowel gets a curve rising from 0.75 to 1.5 (one
/// octave); the measured F0 in eight 200 ms windows must rise with it.
#[test]
fn curve_ramp_moves_pitch_monotonically() {
    let input = harmonic_vowel(150.0, SR, SR as usize * 2);
    let mut shifter = build();
    // 0.75..1.5; the window stays at N = 2048 (D19), hop 512.
    let hop = 512;
    assert_eq!(shifter.hop(), hop);
    let curve_len = input.len() / hop + 2;
    let last = (curve_len - 1) as f32;
    let curve: Vec<f32> = (0..curve_len)
        .map(|i| 0.75 + 0.75 * i as f32 / last)
        .collect();

    let output = shifter.process_with_curve(&input, &curve).expect("render");
    assert_eq!(output.len(), input.len());

    let window = SR as usize / 5; // 200 ms
    let lead = SR as usize / 10; // skip the OLA ramp-in
    let span = input.len() - lead - window;
    let mut previous: Option<(f64, f64)> = None;
    let mut track = Vec::new();
    for index in 0..8 {
        let start = lead + index * span / 8;
        let centre_s = (start + window / 2) as f64 / SR as f64;
        let ratio = 0.75 + 0.75 * centre_s / 2.0;
        let expected = 150.0 * ratio;
        let measured = estimate_f0(
            &output[start..start + window],
            SR,
            expected * 0.65,
            expected * 1.55,
        )
        .expect("ramp output is voiced");
        track.push((centre_s, expected, measured));
        if let Some((previous_s, previous_hz)) = previous {
            assert!(
                measured > previous_hz,
                "F0 did not rise between {previous_s:.2} s ({previous_hz:.2} Hz) and \
                 {centre_s:.2} s ({measured:.2} Hz)"
            );
        }
        previous = Some((centre_s, measured));
    }

    // The window averages over a moving ratio, so allow ±200 cents; a stalled
    // or inverted curve fails by much more.
    for &(centre_s, expected, measured) in &track {
        let error = cents_error(expected, measured);
        println!("ramp {centre_s:.2} s: expected {expected:6.2} Hz, measured {measured:6.2} Hz ({error:+6.1} c)");
        assert!(
            error.abs() <= 200.0,
            "ramp at {centre_s:.2} s: {measured:.2} Hz vs expected {expected:.2} Hz ({error:+.1} cents)"
        );
    }
    let first = track.first().expect("track").2;
    let last = track.last().expect("track").2;
    let span_cents = cents_error(first, last);
    println!("ramp: {first:.2} Hz -> {last:.2} Hz ({span_cents:+.0} cents over 8 windows)");
    // The windows are centred at 0.20 s and 1.69 s, so the curve between them
    // spans 1200·log2(1.38/0.825) ≈ 890 cents. A flat or inverted curve fails
    // this by an order of magnitude.
    assert!(
        span_cents > 700.0,
        "the ramp only moved {span_cents:+.1} cents; expected roughly 890"
    );
}

/// Every ratio in the supported range must terminate and keep the length.
///
/// The frame loop advances the analysis position by `H/ratio` per frame, so it
/// is bounded by `2·len/H + 2` frames for any ratio in `[0.5, 2.0]`; this
/// sweep is the regression that the bound actually holds at every boundary. It
/// also exercises the shared resampler across 31 different rate pairs.
#[test]
fn ratio_sweep_terminates_and_keeps_length() {
    let input = sine(220.0, SR, 7_200);
    let mut shifter = build();
    let mut step = 0;
    while step <= 30 {
        let ratio = 0.5 + 0.05 * step as f32;
        shifter.set_ratio(ratio);
        let output = shifter.process(&input).expect("render");
        assert_eq!(output.len(), input.len(), "ratio {ratio:.2}");
        assert!(
            output.iter().all(|x| x.is_finite()),
            "ratio {ratio:.2} produced a non-finite sample"
        );
        let level = rms(&output);
        assert!(
            level > 1e-3 && level < 1.0,
            "ratio {ratio:.2}: RMS {level:.5} is not in a sane range"
        );
        step += 1;
    }
}

#[test]
fn short_curve_holds_its_last_value() {
    // A one-entry curve must behave like a constant ratio over the whole input.
    let input = sine(220.0, SR, 20_000);
    let mut shifter = build();
    shifter.set_ratio(1.0);
    let short = shifter.process_with_curve(&input, &[1.5]).expect("render");
    shifter.set_ratio(1.5);
    let scalar = shifter.process(&input).expect("render");
    assert_eq!(short, scalar, "a one-entry curve did not hold");
}

#[test]
fn long_curve_is_ignored_past_the_end_of_the_input() {
    let input = sine(220.0, SR, 8_000);
    // The hop is 512, so the input spans indices 0..~16. Entries past that
    // must not influence the render.
    let mut a = vec![1.2f32; 32];
    let mut b = vec![1.2f32; 64];
    for value in a.iter_mut().skip(24) {
        *value = 1.4;
    }
    for value in b.iter_mut().skip(24) {
        *value = 1.9;
    }
    let mut shifter = build();
    let first = shifter.process_with_curve(&input, &a).expect("render");
    let second = shifter.process_with_curve(&input, &b).expect("render");
    assert!(
        first == second,
        "curve entries past the input changed the render"
    );
}

// ---------------------------------------------------------------------------
// Alignment and latency
// ---------------------------------------------------------------------------

#[test]
fn output_is_time_aligned_with_the_input() {
    for ratio in [0.5f32, 1.0, 1.5, 2.0] {
        let mut input = vec![0.0f32; 48_000];
        let position = 24_000;
        input[position] = 1.0;
        let mut shifter = build();
        shifter.set_ratio(ratio);
        let output = shifter.process(&input).expect("render");

        // The impulse is smeared over the analysis window, so use the energy
        // centroid of the burst rather than its peak.
        let radius = 4 * shifter.window_size();
        let low = position.saturating_sub(radius);
        let high = (position + radius).min(output.len());
        let mut weighted = 0.0f64;
        let mut total = 0.0f64;
        for (index, &value) in output.iter().enumerate().take(high).skip(low) {
            let energy = (value as f64) * (value as f64);
            weighted += index as f64 * energy;
            total += energy;
        }
        assert!(total > 1e-9, "ratio {ratio}: impulse disappeared");
        let centroid = weighted / total;
        let drift = centroid - position as f64;
        println!("ratio {ratio}: impulse centroid drift {drift:+.1} samples");
        assert!(
            drift.abs() < 512.0,
            "ratio {ratio}: impulse centroid drifted {drift:+.1} samples"
        );
    }
}

#[test]
fn latency_is_one_window() {
    let mut shifter = build();
    assert_eq!(shifter.latency_samples(), 2048);
    assert!((shifter.latency_seconds() - 2048.0 / SR as f32).abs() < 1e-6);
    // D19: a −12 st ratio does not change the window or the latency.
    shifter.set_ratio(0.5);
    assert_eq!(shifter.window_size(), 2048);
    assert_eq!(shifter.latency_samples(), 2048);
    // An explicit wide window is still reported.
    let wide = PitchShifter::new(SR, PitchShiftConfig::fixed(4096)).expect("valid");
    assert_eq!(wide.latency_samples(), 4096);
}

// ---------------------------------------------------------------------------
// Fixtures (skipped when the generated fixture tree is absent)
// ---------------------------------------------------------------------------

/// The lowest-frequency fixture and the two named high anchors.
const PITCH_FIXTURES: [&str; 3] = [
    "am_onyx__got-time",
    "af_heart__friends-now",
    "af_aoede__you-got-me",
];

fn fixture_reference(name: &str) -> Option<(Vec<f32>, f64, usize)> {
    let fixtures = testutil::load_fixtures()?;
    let fixture = fixtures.into_iter().find(|f| f.name == name)?;
    let start = voiced_slice_start(&fixture.samples);
    let end = (start + ANALYSIS_SLICE).min(fixture.samples.len());
    let base = estimate_f0(&fixture.samples[start..end], SR, 60.0, 400.0)?;
    Some((fixture.samples, base, start))
}

#[test]
fn fixtures_track_pitch_across_the_range() {
    let Some(first) = testutil::fixtures_dir() else {
        println!("fixtures absent; skipping fixture pitch test");
        return;
    };
    let _ = first;

    for name in PITCH_FIXTURES {
        let Some((samples, base, start)) = fixture_reference(name) else {
            println!("{name}: no voiced segment; skipping");
            continue;
        };
        let end = (start + ANALYSIS_SLICE).min(samples.len());
        let mut shifter = build();
        let mut determinism_checked = false;
        for step in [-12i32, -7, -1, 0, 3, 7, 12] {
            let ratio = 2.0f32.powf(step as f32 / 12.0);
            shifter.set_ratio(ratio);
            let output = shifter.process(&samples).expect("render");
            assert_eq!(
                output.len(),
                samples.len(),
                "{name} {step:+} st: output length changed"
            );
            // Determinism on real speech, not just on the synthetic noise case.
            if !determinism_checked {
                let again = shifter.process(&samples).expect("render");
                assert_eq!(output, again, "{name} {step:+} st: not repeatable");
                determinism_checked = true;
            }
            let expected = base * ratio as f64;
            let measured = estimate_f0(&output[start..end], SR, expected * 0.45, expected * 2.2)
                .expect("voiced output");
            let error = cents_error(expected, measured);
            println!("{name} {step:+3} st: {measured:8.2} Hz, {error:+7.2} cents");
            // This autocorrelation estimate is biased by ~10 cents on real
            // speech (non-stationary F0 inside the analysis window); the
            // authoritative number is scripts/metrics.py. The bound here is
            // for octave errors and gross failure, not for the 5-cent target.
            assert!(
                error.abs() <= 15.0,
                "{name} {step:+} st: expected {expected:.2} Hz, measured {measured:.2} Hz \
                 ({error:+.2} cents)"
            );
        }
    }
}

#[test]
fn low_male_minus_12_has_no_octave_error() {
    let Some(fixtures) = testutil::load_fixtures() else {
        println!("fixtures absent; skipping low-male octave test");
        return;
    };
    let Some(sample) = fixtures.into_iter().find(|f| f.name == "am_onyx__got-time") else {
        println!("am_onyx fixture absent; skipping low-male octave test");
        return;
    };
    let samples = sample.samples;
    // The fixture moves around; use a slice close to the voice's 88 Hz median
    // so the −12 st output really is near 44 Hz.
    let start = slice_near_f0(&samples, 88.0);
    let end = (start + ANALYSIS_SLICE).min(samples.len());
    let base = estimate_f0(&samples[start..end], SR, 45.0, 200.0).expect("voiced input");
    let mut shifter = build();
    shifter.set_ratio(0.5);
    // D19: this is the *input* at 88 Hz, which 2048 resolves (4.1 bins); the
    // 44 Hz output is produced by the read-out resampler and is not analysed.
    assert_eq!(
        shifter.window_size(),
        2048,
        "the -12 st low-male case analyses the input and needs no widening"
    );
    let output = shifter.process(&samples).expect("render");
    let expected = base * 0.5;
    let measured = estimate_f0(&output[start..end], SR, 25.0, 140.0).expect("voiced low output");
    let error = cents_error(expected, measured);
    println!(
        "am_onyx -12 st: base {base:.2} Hz -> {measured:.2} Hz, expected {expected:.2} Hz \
         ({error:+.2} cents), N={}",
        shifter.window_size()
    );
    assert!(
        error.abs() <= 15.0,
        "-12 st low male: {measured:.2} Hz vs {expected:.2} Hz ({error:+.2} cents)"
    );
    assert!(
        (measured / expected - 1.0).abs() < 0.2,
        "octave error: {measured:.2} Hz vs expected {expected:.2} Hz"
    );
}

// ---------------------------------------------------------------------------
// Measurement reports (not part of the default test run)
// ---------------------------------------------------------------------------

/// The acceptance semitones for PLAN P-02 (`±5 cents`, no octave errors).
const METRIC_STEPS: [i32; 9] = [-12, -7, -4, -1, 0, 1, 4, 7, 12];

/// The D13 pitch anchors used for the authoritative acceptance table: the low
/// male (`am_onyx`, 88 Hz) plus three female voices (221–248 Hz).
const METRIC_FIXTURES: [&str; 4] = [
    "am_onyx__got-time",
    "af_heart__friends-now",
    "af_aoede__you-got-me",
    "bf_alice__maybe-next-time",
];

fn write_f32(path: &std::path::Path, samples: &[f32]) {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write render");
}

/// Render the P-02 acceptance matrix as raw f32 for `scripts/metrics.py`.
///
/// Writes `<temp>/rasp-p02-renders/`, **never** into the repository:
/// `<fixture>.<±NN>.f32` for the four D13 pitch anchors, the same nine steps
/// for a sustained synthetic vowel, and a `manifest.tsv` recording the ratio,
/// the auto-selected window and the hop for every render. `metrics.py` is the
/// official arbiter (`PLAN.md` wave 3 amendment), so these numbers are what
/// P-02 is judged on, not `estimate_f0`.
///
/// Run with:
/// `cargo test -p rasp-dsp --release pitch::tests::write_metric_render_matrix -- --ignored --nocapture`
///
/// then, for each row, `scripts/.venv/bin/python scripts/metrics.py --ref
/// fixtures/f32/<name>.f32 --test <render> --pitch-st <n> --check`.
#[test]
#[ignore = "writes the metrics render matrix to the temp dir; run explicitly with --release --ignored --nocapture"]
fn write_metric_render_matrix() {
    let Some(fixtures) = testutil::load_fixtures() else {
        println!("fixtures absent; nothing to render");
        return;
    };
    let directory = std::env::temp_dir().join("rasp-p02-renders");
    std::fs::create_dir_all(&directory).expect("create render directory");
    let mut manifest = String::from("fixture\tstep\tcents\tratio\twindow\thop\tsamples\n");
    let mut shifter = build();

    for fixture in fixtures
        .iter()
        .filter(|f| METRIC_FIXTURES.contains(&f.name.as_str()))
    {
        for step in METRIC_STEPS {
            let ratio = 2.0f32.powf(step as f32 / 12.0);
            shifter.set_ratio(ratio);
            let output = shifter.process(&fixture.samples).expect("render");
            assert_eq!(output.len(), fixture.samples.len());
            let file = directory.join(format!("{}.{step:+03}.f32", fixture.name));
            write_f32(&file, &output);
            manifest.push_str(&format!(
                "{}\t{step:+}\t{}\t{ratio:.6}\t{}\t{}\t{}\n",
                fixture.name,
                step * 100,
                shifter.window_size(),
                shifter.hop(),
                output.len()
            ));
        }
        println!(
            "rendered {} at {} ({})",
            fixture.name,
            METRIC_STEPS.len(),
            directory.display()
        );
    }

    // Synthetic vowels keep every shifted median inside the metrics harness's
    // 60–400 Hz tracker band (25–350 Hz at ±12 st for the 88 Hz anchor is
    // *not* inside it: that is the point of the low anchor), so they measure
    // the shifter with no source-intonation or octave-error noise. 88 / 150 /
    // 228 Hz bracket the corpus pitch range.
    for f0 in [88.0f64, 150.0, 228.0] {
        let name = format!("synthetic_vowel{f0:.0}");
        let synthetic = harmonic_vowel(f0, SR, SR as usize * 2);
        write_f32(&directory.join(format!("{name}.ref.f32")), &synthetic);
        for step in METRIC_STEPS {
            let ratio = 2.0f32.powf(step as f32 / 12.0);
            shifter.set_ratio(ratio);
            let output = shifter.process(&synthetic).expect("render");
            assert_eq!(output.len(), synthetic.len());
            write_f32(&directory.join(format!("{name}.{step:+03}.f32")), &output);
            manifest.push_str(&format!(
                "{name}\t{step:+}\t{}\t{ratio:.6}\t{}\t{}\t{}\n",
                step * 100,
                shifter.window_size(),
                shifter.hop(),
                output.len()
            ));
        }
        println!("rendered {name}");
    }
    std::fs::write(directory.join("manifest.tsv"), manifest).expect("write manifest");
    println!("renders and manifest.tsv in {}", directory.display());
}

/// Release-build CPU report: milliseconds per second of audio.
#[test]
#[ignore = "timing report; run explicitly with --release --ignored --nocapture"]
fn report_cpu_per_second_of_audio() {
    let seconds = 5usize;
    let input = harmonic_vowel(120.0, SR, SR as usize * seconds);
    for ratio in [0.5f32, 0.99, 1.0, 2.0] {
        let mut shifter = build();
        shifter.set_ratio(ratio);
        // Warm the allocations and the resampler table.
        let _ = shifter.process(&input[..16_384]).expect("render");
        let start = std::time::Instant::now();
        let output = shifter.process(&input).expect("render");
        let elapsed = start.elapsed();
        std::hint::black_box(&output);
        println!(
            "ratio {ratio:.2}: {:.2} ms per second of audio ({:.1}x realtime, N={}, {})",
            elapsed.as_secs_f64() * 1e3 / seconds as f64,
            seconds as f64 / elapsed.as_secs_f64(),
            shifter.window_size(),
            if cfg!(debug_assertions) {
                "debug build"
            } else {
                "release build"
            }
        );
    }
}

#[path = "tests/transients.rs"]
mod transients;
