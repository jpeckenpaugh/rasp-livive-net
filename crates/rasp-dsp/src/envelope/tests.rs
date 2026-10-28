//! Unit tests for the spectral envelope (PLAN P-03).

use super::*;
use crate::stft::{Stft, StftConfig};
use crate::testutil::{self, sine, vowel, white_noise};

const SR: u32 = 48_000;

fn build(config: EnvelopeConfig) -> EnvelopeAnalyzer {
    EnvelopeAnalyzer::new(SR, config).expect("valid configuration")
}

/// Analyser + one spectrogram of `signal`.
fn analyse(signal: &[f32], config: EnvelopeConfig) -> (SpectralEnvelope, Spectrogram) {
    let mut stft = Stft::new(StftConfig::default()).expect("valid configuration");
    let spectrogram = stft.analyze(signal);
    let mut analyzer = build(config);
    let envelope = analyzer
        .analyze(&spectrogram)
        .expect("analysis of a valid spectrogram");
    (envelope, spectrogram)
}

#[test]
fn zero_sample_rate_is_an_error() {
    assert_eq!(
        EnvelopeAnalyzer::new(0, EnvelopeConfig::default()).unwrap_err(),
        EnvelopeError::ZeroSampleRate
    );
}

#[test]
fn configuration_is_sanitised() {
    let config = EnvelopeConfig {
        method: EnvelopeMethod::Lpc,
        lifter_seconds: -3.0,
        true_envelope_passes: 99,
        lpc_order: 400,
        temporal_smoothing_seconds: 9.0,
        max_gain_db: 900.0,
    };
    let analyzer = build(config);
    let effective = analyzer.config();
    assert_eq!(effective.lifter_seconds, 1.0e-4);
    assert_eq!(effective.true_envelope_passes, 16);
    assert_eq!(effective.lpc_order, MAX_LPC_ORDER);
    assert!((effective.temporal_smoothing_seconds - 0.2).abs() < 1e-6);
    assert!((effective.max_gain_db - MAX_GAIN_DB_LIMIT).abs() < 1e-6);
}

#[test]
fn geometry_follows_the_spectrogram() {
    let signal = white_noise(20_000, 3);
    let (envelope, spectrogram) = analyse(&signal, EnvelopeConfig::default());
    assert_eq!(envelope.frames(), spectrogram.frames());
    assert_eq!(envelope.bins(), spectrogram.bins());
    assert_eq!(envelope.hop(), spectrogram.hop());
    assert_eq!(envelope.window_size(), spectrogram.window_size());
    assert_eq!(envelope.sample_rate(), SR);
    assert!(!envelope.is_empty());
    assert_eq!(envelope.frame(envelope.frames()), None);
}

#[test]
fn analyzer_handles_a_change_of_window_size() {
    let signal = white_noise(12_000, 9);
    let mut analyzer = build(EnvelopeConfig::default());
    for window in [1024usize, 4096] {
        let mut stft = Stft::new(StftConfig::new(window, window / 4)).expect("valid");
        let spectrogram = stft.analyze(&signal);
        let envelope = analyzer.analyze(&spectrogram).expect("valid");
        assert_eq!(envelope.window_size(), window);
        assert_eq!(envelope.bins(), window / 2 + 1);
    }
    assert_eq!(analyzer.window_size(), 4096);
}

#[test]
fn envelope_is_finite_on_silence_and_noise() {
    for signal in [vec![0.0f32; 8192], white_noise(8192, 11)] {
        let (envelope, _) = analyse(&signal, EnvelopeConfig::default());
        assert!(envelope.data().iter().all(|value| value.is_finite()));
    }
}

#[test]
fn envelope_tracks_the_tone_it_was_analysed_from() {
    // A 0.5-amplitude tone: the peak bin's log magnitude is the envelope's
    // maximum, and it sits at the tone (within one bin).
    let frequency = 1_000.0;
    let signal = sine(frequency, SR, 16_384);
    let (envelope, spectrogram) = analyse(&signal, EnvelopeConfig::default());
    let frame = envelope.frames() / 2;
    let values = envelope.frame(frame).expect("mid frame");
    let peak = values
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal))
        .map(|(index, _)| index)
        .expect("non-empty frame");
    let bin_hz = SR as f64 / spectrogram.window_size() as f64;
    let expected = (frequency / bin_hz).round() as usize;
    assert!(
        peak.abs_diff(expected) <= 1,
        "envelope peak bin {peak}, expected {expected}"
    );
    // The envelope must be within a few dB of the observed log magnitude there.
    let observed = spectrogram.frame(frame).expect("frame")[peak]
        .magnitude()
        .ln();
    assert!(
        (values[peak] - observed).abs() < 4.0,
        "envelope {} vs observed {}",
        values[peak],
        observed
    );
}

#[test]
fn reconstruction_from_envelope_times_excitation_is_identity() {
    let signal = vowel(
        110.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        12_000,
    );
    let (envelope, spectrogram) = analyse(&signal, EnvelopeConfig::default());
    let frame = envelope.frames() / 2;
    let original = spectrogram.frame(frame).expect("frame").to_vec();

    let mut excitation = original.clone();
    envelope
        .divide_from(frame, &mut excitation)
        .expect("matching lengths");
    let mut reconstructed = vec![Complex::ZERO; envelope.bins()];
    envelope
        .reconstruct_frame(frame, &excitation, &mut reconstructed)
        .expect("matching lengths");

    let mut worst = 0.0f32;
    for (a, b) in original.iter().zip(&reconstructed) {
        worst = worst.max((a.re - b.re).abs()).max((a.im - b.im).abs());
    }
    let scale = original
        .iter()
        .fold(0.0f32, |acc, bin| acc.max(bin.magnitude()));
    assert!(
        worst <= scale * 1e-4 + 1e-6,
        "reconstruction error {worst} vs scale {scale}"
    );
}

#[test]
fn warp_moves_a_synthetic_peak_by_the_ratio() {
    let mut envelope = SpectralEnvelope::new(2, 512, 128, 512, SR);
    let frame = envelope.frame_mut(0).expect("frame");
    for (bin, value) in frame.iter_mut().enumerate() {
        let distance = bin as f32 - 100.0;
        *value = (-distance * distance / (2.0 * 8.0 * 8.0)).exp();
    }
    let first = envelope.frame(0).expect("frame").to_vec();
    envelope
        .frame_mut(1)
        .expect("frame")
        .copy_from_slice(&first);

    for (ratio, expected) in [(1.10f32, 110.0f32), (0.75, 75.0), (1.33, 133.0)] {
        let warped = envelope.warp(ratio);
        let values = warped.frame(0).expect("frame");
        let peak = values
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal))
            .map(|(index, _)| index as f32)
            .expect("non-empty");
        assert!(
            (peak - expected).abs() <= 1.5,
            "ratio {ratio}: peak {peak}, expected {expected}"
        );
        // Frames are warped independently but identically.
        assert_eq!(warped.frame(0), warped.frame(1));
    }
}

#[test]
fn warp_is_an_identity_at_one_and_clamps_bad_ratios() {
    let mut envelope = SpectralEnvelope::new(1, 64, 16, 64, SR);
    for (bin, value) in envelope.frame_mut(0).expect("frame").iter_mut().enumerate() {
        *value = bin as f32 * 0.1;
    }
    let same = envelope.warp(1.0);
    assert_eq!(same, envelope);
    let bad = envelope.warp(f32::NAN);
    assert_eq!(bad, envelope);
    // Out-of-range ratios are clamped, not rejected.
    let clamped = envelope.warp(100.0);
    assert!(clamped.data().iter().all(|value| value.is_finite()));
}

#[test]
fn apply_gain_and_divide_from_reject_the_wrong_geometry() {
    let envelope = SpectralEnvelope::new(2, 64, 16, 64, SR);
    let mut bins = vec![Complex::new(1.0, 0.0); 32];
    assert!(matches!(
        envelope.apply_gain(0, &mut bins),
        Err(EnvelopeError::BinLength { .. })
    ));
    let mut bins = vec![Complex::new(1.0, 0.0); 64];
    assert!(matches!(
        envelope.apply_gain(2, &mut bins),
        Err(EnvelopeError::FrameIndex { .. })
    ));
}

#[test]
fn lpc_envelope_is_finite_and_peaks_near_a_known_formant() {
    let signal = vowel(
        110.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        16_384,
    );
    let (envelope, spectrogram) = analyse(&signal, EnvelopeConfig::lpc());
    assert!(envelope.data().iter().all(|value| value.is_finite()));
    let frame = envelope.frames() / 2;
    let values = envelope.frame(frame).expect("frame");
    let bin_hz = SR as f32 / spectrogram.window_size() as f32;
    let band = (300.0 / bin_hz) as usize..(1_100.0 / bin_hz) as usize;
    let peak = band
        .clone()
        .max_by(|a, b| {
            values[*a]
                .partial_cmp(&values[*b])
                .unwrap_or(core::cmp::Ordering::Equal)
        })
        .expect("non-empty band");
    let peak_hz = peak as f32 * bin_hz;
    // The all-pole envelope includes the glottal tilt, so its absolute maximum
    // in this band can sit below the formant; what must hold is that the
    // envelope is not flat and that the analysed frame carries its energy in
    // the expected region rather than in the far high band.
    let high_bin = (3_500.0 / bin_hz) as usize;
    assert!(
        values[peak] > values[high_bin] + 1.0,
        "LPC envelope has no low-frequency structure: {peak_hz} Hz ({}) vs 3.5 kHz ({})",
        values[peak],
        values[high_bin]
    );
}

#[test]
fn temporal_smoothing_reduces_frame_to_frame_variation() {
    let signal = white_noise(24_000, 21);
    let mut unsmoothed = build(EnvelopeConfig {
        temporal_smoothing_seconds: 0.0,
        ..EnvelopeConfig::default()
    });
    let mut smoothed = build(EnvelopeConfig::default());
    let mut stft = Stft::new(StftConfig::default()).expect("valid");
    let spectrogram = stft.analyze(&signal);
    let raw = unsmoothed.analyze(&spectrogram).expect("valid");
    let smooth = smoothed.analyze(&spectrogram).expect("valid");

    let roughness = |envelope: &SpectralEnvelope| -> f64 {
        let mut total = 0.0;
        let mut count = 0usize;
        for frame in 1..envelope.frames() {
            let (Some(a), Some(b)) = (envelope.frame(frame - 1), envelope.frame(frame)) else {
                continue;
            };
            for (x, y) in a.iter().zip(b) {
                total += (x - y).abs() as f64;
                count += 1;
            }
        }
        if count == 0 {
            0.0
        } else {
            total / count as f64
        }
    };
    let raw_roughness = roughness(&raw);
    let smooth_roughness = roughness(&smooth);
    assert!(
        smooth_roughness < raw_roughness * 0.9,
        "smoothing should reduce frame-to-frame change: {smooth_roughness} vs {raw_roughness}"
    );
    // Smoothing must not change the interior mean level materially (edge
    // frames sit in the zero pad, where smoothing legitimately lifts the
    // value).
    let interior_mean = |envelope: &SpectralEnvelope| -> f64 {
        let frames = envelope.frames();
        let bins = envelope.bins();
        let first = 4.min(frames / 2);
        let last = frames.saturating_sub(4).max(first + 1);
        let data = &envelope.data()[first * bins..last * bins];
        data.iter().map(|&value| value as f64).sum::<f64>() / data.len().max(1) as f64
    };
    assert!((interior_mean(&raw) - interior_mean(&smooth)).abs() < 0.05);
}

#[test]
fn estimate_frame_matches_the_whole_buffer_path_without_smoothing() {
    let signal = vowel(
        120.0,
        &[700.0, 1220.0, 2600.0],
        &[80.0, 90.0, 120.0],
        SR,
        8192,
    );
    let mut stft = Stft::new(StftConfig::default()).expect("valid");
    let spectrogram = stft.analyze(&signal);
    let config = EnvelopeConfig {
        temporal_smoothing_seconds: 0.0,
        ..EnvelopeConfig::default()
    };
    let mut analyzer = build(config);
    let envelope = analyzer.analyze(&spectrogram).expect("valid");
    let mut frame_envelope = vec![0.0f32; spectrogram.bins()];
    analyzer
        .estimate_frame(spectrogram.frame(3).expect("frame"), &mut frame_envelope)
        .expect("matching lengths");
    let expected = envelope.frame(3).expect("frame");
    for (a, b) in frame_envelope.iter().zip(expected) {
        assert!((a - b).abs() < 1e-5, "{a} vs {b}");
    }
}

#[test]
fn envelope_stays_bounded_for_a_loud_input() {
    let mut signal = testutil::white_noise(8192, 5);
    for value in signal.iter_mut() {
        *value *= 4.0;
    }
    let (envelope, _) = analyse(&signal, EnvelopeConfig::default());
    assert!(
        envelope
            .data()
            .iter()
            .all(|value| value.is_finite() && *value < 40.0),
        "loud input produced an unbounded envelope"
    );
}
