//! Tests for the loudness policy and the true-peak limiter (PLAN P-08).
//!
//! # Controls before claims
//!
//! Every number here is a claim about a signal whose answer is known:
//!
//! * **True-peak meter** — a pure sine's true peak is exactly its amplitude.
//!   The control sweeps 50 Hz…18 kHz at four phases and must read within
//!   0.1 dB. The falsifiable case is a quarter-rate sine at 45°: every sample is
//!   −3.01 dBFS, the true peak is 0 dBFS, and a sample-peak meter provably
//!   misses it.
//! * **Level** — a scaled copy has the requested RMS by construction.
//! * **Limiter** — a signal driven above the ceiling must come out at or below
//!   it *as measured by the same 4× meter*, and a signal already below it must
//!   come out bit-identical.
//!
//! The fixture-wide preset evidence lives in `params::tests::preset_fixture_table`;
//! this module pins the stage's contract.

use super::*;
use crate::testutil::{self, Lcg};

const SR: u32 = 48_000;

fn sine(freq: f64, amplitude: f32, phase: f64, len: usize) -> Vec<f32> {
    (0..len)
        .map(|index| {
            let value = amplitude as f64
                * (2.0 * core::f64::consts::PI * freq * index as f64 / SR as f64 + phase).sin();
            value as f32
        })
        .collect()
}

/// Cosine fade at both ends so the measurement is not dominated by the
/// boundary discontinuity (the true peak of a faded sine is still its flat-part
/// amplitude).
fn fade_edges(signal: &mut [f32], seconds: f32) {
    let length = ((seconds * SR as f32) as usize).min(signal.len() / 2);
    if length == 0 {
        return;
    }
    let len = signal.len();
    for index in 0..length {
        let gain =
            (0.5 - 0.5 * (core::f64::consts::PI * index as f64 / length as f64).cos()) as f32;
        signal[index] *= gain;
        signal[len - 1 - index] *= gain;
    }
}

#[test]
fn level_helpers_are_total() {
    assert_eq!(rms(&[]), 0.0);
    assert_eq!(sample_peak(&[]), 0.0);
    assert_eq!(true_peak(&[]), 0.0);
    let values = rms_dbfs(&[]);
    assert!(values.is_finite());

    let dirty = [f32::NAN, f32::INFINITY, -0.5, 0.25, f32::NEG_INFINITY];
    assert!(rms(&dirty).is_finite());
    assert!(true_peak(&dirty).is_finite());
    assert_eq!(sample_peak(&dirty), 0.5);

    let mut quiet = vec![0.0f32; 64];
    let source = vec![0.1f32; 64];
    let gain = match_rms_to(&mut quiet, &source);
    assert_eq!(gain, 0.0, "a silent signal must not be amplified from zero");
    assert!(quiet.iter().all(|&value| value == 0.0));
}

/// Control: a sine's true peak is its amplitude, so the meter can be judged
/// against an exact answer rather than against another estimator.
#[test]
fn true_peak_meter_recovers_a_sine() {
    let meter = TruePeakMeter::new();
    let amplitude = 0.5f32;
    let expected = 20.0 * amplitude.log10();
    let mut worst = 0.0f64;
    for freq in [50.0f64, 500.0, 2_000.0, 5_000.0, 10_000.0, 18_000.0] {
        for phase in [0.0f64, 0.3, 1.0, 2.0] {
            let mut signal = sine(freq, amplitude, phase, SR as usize / 4);
            fade_edges(&mut signal, 0.005);
            let measured = dbtp(meter.measure(&signal));
            worst = worst.max((measured - expected).abs() as f64);
            assert!(
                (measured - expected).abs() < 0.1,
                "sine {freq} Hz phase {phase}: {measured:.3} dBTP vs {expected:.3} dBFS"
            );
        }
    }
    println!("true-peak sine control: worst error {worst:.3} dB over 50 Hz…18 kHz, 4 phases");
}

/// The classic inter-sample overload: at a quarter of the sample rate, a 45°
/// phase puts every sample at −3.01 dBFS while the waveform peaks at full
/// scale. A sample-peak meter cannot see this; the 4× meter must.
#[test]
fn true_peak_meter_sees_quarter_rate_inter_sample_overs() {
    let amplitude = 0.5f32;
    let signal = sine(
        SR as f64 / 4.0,
        amplitude,
        core::f64::consts::PI / 4.0,
        4_800,
    );
    let sample = sample_peak(&signal);
    let peak = true_peak(&signal);
    println!(
        "quarter-rate sine: sample peak {:.3} dBFS, true peak {:.3} dBFS",
        dbfs(sample),
        dbfs(peak)
    );
    assert!(
        (dbfs(sample) - (dbfs(amplitude) - 3.0103)).abs() < 0.05,
        "the sample peak of this sine must be 3.01 dB below its amplitude, got {:.3}",
        dbfs(sample)
    );
    assert!(
        (dbfs(peak) - dbfs(amplitude)).abs() < 0.1,
        "the true peak must be {:.3} dBFS, got {:.3}",
        dbfs(amplitude),
        dbfs(peak)
    );
    assert!(
        peak > sample * 1.3,
        "true peak {peak:.4} is not meaningfully above sample peak {sample:.4}"
    );
}

#[test]
fn match_rms_makes_the_whole_clip_rms_equal() {
    let mut signal: Vec<f32> = (0..4_800).map(|_| 0.01 * Lcg::new(5).next_f32()).collect();
    let source: Vec<f32> = (0..4_800)
        .map(|index| 0.2 * (index as f32 * 0.01).sin())
        .collect();
    let gain = match_rms_to(&mut signal, &source);
    let ratio = rms(&signal) / rms(&source);
    assert!(
        (ratio - 1.0).abs() < 1e-4,
        "RMS ratio {ratio} after a {gain:+.2} dB match"
    );
}

#[test]
fn stage_is_transparent_below_the_ceiling() {
    let mut stage = LoudnessStage::new(SR, LoudnessConfig::default()).expect("valid stage");
    let mut signal = sine(440.0, 0.5, 0.0, 4_800);
    fade_edges(&mut signal, 0.005);
    let source = signal.clone();
    let mut output = signal.clone();
    let report = stage.process_into(&source, &mut output);
    assert_eq!(
        output, signal,
        "a signal below the ceiling must be untouched"
    );
    assert!(!report.limited);
    assert_eq!(report.limiter_min_gain, 1.0);
    assert_eq!(report.level_delta_db, 0.0);
    assert!(report.output_true_peak_dbtp <= DEFAULT_CEILING_DBTP);
}

#[test]
fn stage_limits_a_hot_sine_below_minus_one_dbtp() {
    let mut stage = LoudnessStage::new(SR, LoudnessConfig::default()).expect("valid stage");
    let mut signal = sine(997.0, 0.9, 0.0, SR as usize);
    fade_edges(&mut signal, 0.005);
    let source = signal.clone();
    let mut output = signal.clone();
    let report = stage.process_into(&source, &mut output);
    assert!(report.limited, "a −0.9 dBTP sine must engage the limiter");
    assert!(
        report.output_true_peak_dbtp <= DEFAULT_CEILING_DBTP + 1e-3,
        "output true peak {:.3} dBTP exceeds the ceiling",
        report.output_true_peak_dbtp
    );
    assert!(
        report.output_true_peak_dbtp > DEFAULT_CEILING_DBTP - 1.0,
        "the limiter must not be much more conservative than asked: {:.3} dBTP",
        report.output_true_peak_dbtp
    );
    assert!(sample_peak(&output) <= 1.0);
    assert!(report.limiter_min_gain < 1.0);
    assert!(report.limiter_min_gain_db < 0.0);
    // The RMS match ran first, so the level report is the limiter's loss only.
    assert!(
        report.level_delta_db <= 0.0 && report.level_delta_db > -1.0,
        "level delta {:+.2} dB",
        report.level_delta_db
    );
}

#[test]
fn stage_has_no_overs_on_a_hot_random_signal() {
    let mut rng = Lcg::new(17);
    let mut signal: Vec<f32> = (0..SR as usize).map(|_| rng.next_f32()).collect();
    // Drive it well above the ceiling.
    let peak = true_peak(&signal);
    let boost = 10.0f32.powf(3.0 / 20.0) / peak;
    for sample in &mut signal {
        *sample *= boost;
    }
    assert!(true_peak_dbtp(&signal) > 2.0);
    let source = signal.clone();
    let mut stage = LoudnessStage::new(SR, LoudnessConfig::default()).expect("valid stage");
    let mut output = signal.clone();
    let report = stage.process_into(&source, &mut output);
    assert!(report.limited);
    assert!(
        true_peak_dbtp(&output) <= DEFAULT_CEILING_DBTP + 1e-3,
        "noise output true peak {:.3} dBTP",
        true_peak_dbtp(&output)
    );
    assert!(output.iter().all(|value| value.is_finite()));
    assert_eq!(output.len(), signal.len());
}

/// Release must recover during a quiet gap, not stay ducked: that is the
/// difference between a limiter and a compressor that pumps.
#[test]
fn stage_releases_between_bursts_without_pumping() {
    let mut stage = LoudnessStage::new(SR, LoudnessConfig::default()).expect("valid stage");
    let loud = |amplitude: f32| {
        let mut signal = testutil::vowel(
            120.0,
            &[700.0, 1220.0, 2600.0],
            &[80.0, 90.0, 120.0],
            SR,
            24_000,
        );
        for sample in &mut signal {
            *sample *= amplitude;
        }
        signal
    };
    let burst = loud(3.0);
    let quiet = loud(0.04);
    let mut signal = burst.clone();
    signal.extend_from_slice(&quiet);
    signal.extend_from_slice(&burst);
    let source = signal.clone();
    let mut output = signal.clone();
    stage.process_into(&source, &mut output);

    // Window RMS ratio (out / in) tells us the gain envelope.
    let ratio_db = |from: usize, to: usize| -> f64 {
        let mut out = 0.0f64;
        let mut input = 0.0f64;
        for index in from..to {
            out += (output[index] as f64).powi(2);
            input += (source[index] as f64).powi(2);
        }
        let out = (out.max(1e-20) / (to - from) as f64).sqrt();
        let input = (input.max(1e-20) / (to - from) as f64).sqrt();
        20.0 * (out / input).log10()
    };
    let loud_gain = ratio_db(10_000, 18_000);
    let quiet_gain = ratio_db(28_000, 34_000);
    let recovery_gain = ratio_db(52_000, 60_000);
    println!(
        "limiter gain: loud burst {loud_gain:+.2} dB, quiet gap {quiet_gain:+.2} dB, second burst {recovery_gain:+.2} dB"
    );
    assert!(loud_gain < -2.0, "the loud burst was not limited");
    assert!(
        quiet_gain - loud_gain > 3.0,
        "the limiter did not release in the gap ({quiet_gain:+.2} vs {loud_gain:+.2} dB)"
    );
    assert!(
        (recovery_gain - loud_gain).abs() < 2.0,
        "the second burst must be limited by the same amount ({recovery_gain:+.2} vs {loud_gain:+.2} dB)"
    );
    // Within the burst the gain must be steady: no low-frequency pumping.
    let early = ratio_db(10_000, 12_000);
    let late = ratio_db(16_000, 18_000);
    assert!(
        (early - late).abs() < 1.0,
        "gain moved {:.2} dB inside one burst ({early:+.2} → {late:+.2})",
        (early - late).abs()
    );
}

#[test]
fn config_is_clamped_and_zero_rate_is_an_error() {
    let stage = LoudnessStage::new(
        SR,
        LoudnessConfig {
            match_rms: false,
            ceiling_dbtp: -100.0,
            internal_margin_db: 99.0,
            attack_seconds: f32::NAN,
            release_seconds: -3.0,
        },
    )
    .expect("valid");
    let config = stage.config();
    assert_eq!(config.ceiling_dbtp, MIN_CEILING_DBTP);
    assert_eq!(config.internal_margin_db, 6.0);
    assert_eq!(config.attack_seconds, DEFAULT_ATTACK_SECONDS);
    assert_eq!(config.release_seconds, 0.0);
    assert_eq!(stage.internal_ceiling_dbtp(), MIN_CEILING_DBTP - 6.0);

    assert_eq!(
        LoudnessStage::new(0, LoudnessConfig::default()).unwrap_err(),
        LoudnessError::ZeroSampleRate
    );

    // With the RMS match off, the stage only limits.
    let mut only_limiter = LoudnessStage::new(
        SR,
        LoudnessConfig {
            match_rms: false,
            ..LoudnessConfig::default()
        },
    )
    .expect("valid");
    let mut quiet = sine(440.0, 0.1, 0.0, 4_800);
    let source: Vec<f32> = vec![0.0; 4_800];
    let before = quiet.clone();
    only_limiter.process_into(&source, &mut quiet);
    assert_eq!(quiet, before);
}

#[test]
fn empty_and_degenerate_inputs_are_safe() {
    let mut stage = LoudnessStage::new(SR, LoudnessConfig::default()).expect("valid");
    let mut empty: Vec<f32> = Vec::new();
    let report = stage.process_into(&[], &mut empty);
    assert!(empty.is_empty());
    assert!(!report.limited);
    assert!(report.output_rms_dbfs.is_finite());

    let mut zeros = vec![0.0f32; 512];
    let source = zeros.clone();
    let report = stage.process_into(&source, &mut zeros);
    assert!(!report.limited);
    assert_eq!(report.level_delta_db, 0.0);
    assert!(zeros.iter().all(|&value| value == 0.0));
}

#[test]
fn report_matches_the_measured_output() {
    let mut stage = LoudnessStage::new(SR, LoudnessConfig::default()).expect("valid");
    let source = testutil::white_noise(9_600, 3);
    let processed = testutil::white_noise(9_600, 4);
    let mut output = processed.clone();
    let report = stage.process_into(&source, &mut output);
    assert!((report.source_rms_dbfs - rms_dbfs(&source)).abs() < 1e-5);
    assert!((report.output_rms_dbfs - rms_dbfs(&output)).abs() < 1e-5);
    assert!((report.output_true_peak_dbtp - true_peak_dbtp(&output)).abs() < 1e-5);
    assert!(
        (report.level_delta_db - (report.output_rms_dbfs - report.source_rms_dbfs)).abs() < 1e-4
    );
}
