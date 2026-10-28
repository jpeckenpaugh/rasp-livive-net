//! Tests for the STFT primitives (PLAN P-01).
//!
//! The fixture-based tests skip when `fixtures/manifest.json` is absent, per
//! `docs/testing.md`; run `python3 scripts/make_fixtures.py` to exercise them.

use super::*;
use crate::testutil::{self, argmax_abs, db, max_abs, white_noise};

const SIZES: [usize; 3] = [1024, 2048, 4096];

fn build(n: usize) -> Stft {
    Stft::new(StftConfig::new(n, n / 4)).expect("valid configuration")
}

fn stream(n: usize) -> StftStream {
    StftStream::new(StftConfig::new(n, n / 4)).expect("valid configuration")
}

struct ReconError {
    rms_db: f64,
    peak_db: f64,
    max_abs: f64,
}

fn recon_error(expected: &[f32], actual: &[f32]) -> ReconError {
    assert_eq!(expected.len(), actual.len());
    let mut error_energy = 0.0f64;
    let mut signal_energy = 0.0f64;
    let mut worst = 0.0f64;
    for (&e, &a) in expected.iter().zip(actual) {
        let delta = a as f64 - e as f64;
        error_energy += delta * delta;
        signal_energy += (e as f64) * (e as f64);
        worst = worst.max(delta.abs());
    }
    let len = expected.len().max(1) as f64;
    let error_rms = (error_energy / len).sqrt();
    let signal_rms = (signal_energy / len).sqrt();
    ReconError {
        rms_db: db(error_rms, signal_rms),
        peak_db: db(worst, max_abs(expected)),
        max_abs: worst,
    }
}

/// One-sided naive DFT in f64, for the radix-2 FFT correctness check.
fn naive_dft(x: &[f32]) -> Vec<Complex> {
    let n = x.len();
    (0..=n / 2)
        .map(|k| {
            let mut re = 0.0f64;
            let mut im = 0.0f64;
            for (index, &sample) in x.iter().enumerate() {
                let angle = -2.0 * PI_F64 * (k * index) as f64 / n as f64;
                re += sample as f64 * angle.cos();
                im += sample as f64 * angle.sin();
            }
            Complex::new(re as f32, im as f32)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

#[test]
fn window_is_periodic_hann() {
    for &n in &SIZES {
        let transform = build(n);
        let window = transform.window();
        assert_eq!(window.len(), n);
        assert_eq!(window[0], 0.0, "N={n}: w[0] must be exactly 0");
        assert_eq!(window[n / 2], 1.0, "N={n}: w[N/2] must be exactly 1");
        // Periodic, not symmetric: the last sample is w[-1] = w[1], not 0.
        assert!(window[n - 1] > 0.0, "N={n}: w[N-1] must be non-zero");
        assert!(
            (window[n - 1] - window[1]).abs() < 1e-6,
            "N={n}: w[N-1] should equal w[1] for the periodic window"
        );
        for (i, &value) in window.iter().enumerate() {
            let expected = 0.5 - 0.5 * (2.0 * PI_F64 * i as f64 / n as f64).cos();
            assert!(
                (value as f64 - expected).abs() < 1e-7,
                "N={n}: w[{i}] = {value}, expected {expected}"
            );
        }
    }
}

#[test]
fn hann_squared_cola_is_flat() {
    for &n in &SIZES {
        let transform = build(n);
        let window = transform.window();
        let hop = n / 4;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for offset in 0..n {
            let mut sum = 0.0f64;
            for m in 0..4 {
                let value = window[(offset + m * hop) % n] as f64;
                sum += value * value;
            }
            min = min.min(sum);
            max = max.max(sum);
        }
        let spread = max - min;
        assert!(
            spread < 1e-6,
            "N={n}: squared-Hann COLA varies by {spread:e} (min {min}, max {max})"
        );
        assert!(
            (min - 1.5).abs() < 1e-5,
            "N={n}: COLA sum is {min}, expected 1.5"
        );
        assert!(
            (transform.cola_sum as f64 - 1.5).abs() < 1e-5,
            "N={n}: stored normalization is {}",
            transform.cola_sum
        );
    }
}

// ---------------------------------------------------------------------------
// FFT
// ---------------------------------------------------------------------------

#[test]
fn real_fft_matches_naive_dft() {
    for &n in &[1024, 2048] {
        let mut fft = Fft::new(n);
        // Strong DC and Nyquist components guarantee both edge bins are well
        // conditioned, not near-zero cancellations.
        let noise = white_noise(n, 0xF00D_u64 + n as u64);
        let x: Vec<f32> = noise
            .iter()
            .enumerate()
            .map(|(i, &v)| 0.4 + if i % 2 == 0 { 0.3 } else { -0.3 } + 0.2 * v)
            .collect();
        let mut bins = vec![Complex::ZERO; n / 2 + 1];
        fft.forward(&x, &mut bins);
        let reference = naive_dft(&x);

        let mut max_error = 0.0f64;
        let mut max_magnitude = 0.0f64;
        for (a, b) in bins.iter().zip(&reference) {
            let delta = ((a.re - b.re) as f64).hypot((a.im - b.im) as f64);
            max_error = max_error.max(delta);
            max_magnitude = max_magnitude.max(b.magnitude() as f64);
        }
        let relative = max_error / max_magnitude;
        assert!(
            relative < 1e-5,
            "N={n}: FFT vs naive DFT relative error {relative:e} (max {max_error:e})"
        );

        // DC and Nyquist are real, and individually accurate.
        assert!(bins[0].im.abs() < 1e-6, "N={n}: DC bin has imaginary part");
        assert!(
            bins[n / 2].im.abs() < 1e-6,
            "N={n}: Nyquist bin has imaginary part"
        );
        let dc = reference[0].re.abs() as f64;
        let nyquist = reference[n / 2].re.abs() as f64;
        assert!(
            ((bins[0].re - reference[0].re) as f64).abs() / dc < 1e-5,
            "N={n}: DC bin error"
        );
        assert!(
            ((bins[n / 2].re - reference[n / 2].re) as f64).abs() / nyquist < 1e-5,
            "N={n}: Nyquist bin error"
        );
        eprintln!(
            "P-01 FFT N={n}: max relative error vs naive DFT = {relative:e}, \
             max |Δ| = {max_error:e}"
        );
    }
}

#[test]
fn real_fft_roundtrips() {
    for &n in &SIZES {
        let mut fft = Fft::new(n);
        let x = white_noise(n, 0x1234 + n as u64);
        let mut bins = vec![Complex::ZERO; n / 2 + 1];
        let mut y = vec![0.0f32; n];
        fft.forward(&x, &mut bins);
        fft.inverse(&bins, &mut y);
        let error = recon_error(&x, &y);
        assert!(
            error.rms_db < -100.0,
            "N={n}: FFT round trip is {} dB",
            error.rms_db
        );
        eprintln!(
            "P-01 FFT roundtrip N={n}: {} dB RMS, max |Δ| = {:.3e}",
            error.rms_db, error.max_abs
        );
    }
}

#[test]
fn fft_is_linear() {
    let n = 1024;
    let mut fft = Fft::new(n);
    let x = white_noise(n, 11);
    let y = white_noise(n, 22);
    let (a, b) = (0.7f32, -1.3f32);
    let combined: Vec<f32> = x.iter().zip(&y).map(|(&u, &v)| a * u + b * v).collect();

    let mut fx = vec![Complex::ZERO; n / 2 + 1];
    let mut fy = vec![Complex::ZERO; n / 2 + 1];
    let mut fcombined = vec![Complex::ZERO; n / 2 + 1];
    fft.forward(&x, &mut fx);
    fft.forward(&y, &mut fy);
    fft.forward(&combined, &mut fcombined);

    let mut max_error = 0.0f64;
    let mut max_magnitude = 0.0f64;
    for ((combined_bin, &x_bin), &y_bin) in fcombined.iter().zip(&fx).zip(&fy) {
        let expected = x_bin * a + y_bin * b;
        max_error = max_error.max((*combined_bin - expected).magnitude() as f64);
        max_magnitude = max_magnitude.max(expected.magnitude() as f64);
    }
    let relative = max_error / max_magnitude;
    assert!(relative < 1e-5, "linearity error {relative:e}");
}

#[test]
fn parseval_holds() {
    for &n in &SIZES {
        let mut fft = Fft::new(n);
        let x = white_noise(n, 0xBEEF + n as u64);
        let mut bins = vec![Complex::ZERO; n / 2 + 1];
        fft.forward(&x, &mut bins);

        let time_energy: f64 = x.iter().map(|&v| (v as f64) * (v as f64)).sum();
        let mut spectral_energy = bins[0].magnitude_sq() as f64 + bins[n / 2].magnitude_sq() as f64;
        for bin in &bins[1..n / 2] {
            spectral_energy += 2.0 * bin.magnitude_sq() as f64;
        }
        let expected = n as f64 * time_energy;
        let relative = (spectral_energy - expected).abs() / expected;
        assert!(
            relative < 1e-5,
            "N={n}: Parseval relative error {relative:e} (spectral {spectral_energy:e}, \
             time {expected:e})"
        );
    }
}

#[test]
fn fft_throughput_is_reported() {
    // Reported for PLAN P-01; no timing assertion (CI machines vary).
    let n = 2048;
    let mut fft = Fft::new(n);
    let x = white_noise(n, 7);
    let mut bins = vec![Complex::ZERO; n / 2 + 1];
    let iterations = 2000u32;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        fft.forward(&x, &mut bins);
        std::hint::black_box(&bins);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let per_transform = elapsed / iterations as f64;
    eprintln!(
        "P-01 FFT timing N={n}: {:.2} µs/transform forward, {:.1} Msamples/s",
        per_transform * 1e6,
        n as f64 / per_transform / 1e6
    );
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_white_noise() {
    for &n in &SIZES {
        let mut transform = build(n);
        let input = white_noise(3 * n + 123, 0x51A7 + n as u64);
        let output = transform.roundtrip(&input).expect("round trip");
        assert_eq!(output.len(), input.len());
        let error = recon_error(&input, &output);
        assert!(
            error.rms_db < -100.0,
            "N={n}: reconstruction is {} dB RMS (peak {} dB, max |Δ| {:.3e})",
            error.rms_db,
            error.peak_db,
            error.max_abs
        );
        eprintln!(
            "P-01 roundtrip noise N={n}: {:.1} dB RMS, {:.1} dB peak, max |Δ| = {:.3e}",
            error.rms_db, error.peak_db, error.max_abs
        );
    }
}

#[test]
fn roundtrip_fixtures() {
    let Some(fixtures) = testutil::load_fixtures() else {
        eprintln!(
            "skipping: fixtures/manifest.json is missing - \
             run `python3 scripts/make_fixtures.py` to generate it"
        );
        return;
    };

    for &n in &SIZES {
        let mut transform = build(n);
        let mut worst_db = f64::NEG_INFINITY;
        let mut worst_peak = f64::NEG_INFINITY;
        let mut worst_abs = 0.0f64;
        let mut worst_name = String::new();
        for fixture in &fixtures {
            let output = transform
                .roundtrip(&fixture.samples)
                .expect("round trip on a fixture");
            let error = recon_error(&fixture.samples, &output);
            if error.rms_db > worst_db {
                worst_db = error.rms_db;
                worst_peak = error.peak_db;
                worst_abs = error.max_abs;
                worst_name = fixture.name.clone();
            }
            assert!(
                error.rms_db < -100.0,
                "N={n}, {}: reconstruction is {} dB RMS (max |Δ| {:.3e})",
                fixture.name,
                error.rms_db,
                error.max_abs
            );
        }
        eprintln!(
            "P-01 fixtures N={n}: worst {worst_db:.1} dB RMS ({worst_peak:.1} dB peak, \
             max |Δ| {worst_abs:.3e}) on {worst_name}"
        );
    }
}

#[test]
fn impulse_lands_at_expected_index() {
    for &n in &SIZES {
        let mut transform = build(n);
        let len = 3 * n;
        for &position in &[0usize, 1, n / 4, n / 2, len - 1] {
            let mut input = vec![0.0f32; len];
            input[position] = 1.0;
            let output = transform.roundtrip(&input).expect("round trip");
            assert_eq!(output.len(), len);
            assert_eq!(
                argmax_abs(&output),
                Some(position),
                "N={n}: impulse at {position} moved"
            );
            assert!(
                (output[position] - 1.0).abs() < 1e-5,
                "N={n}: impulse at {position} reconstructed as {}",
                output[position]
            );
            let neighbours = [
                output.get(position.wrapping_sub(1)).copied().unwrap_or(0.0),
                output.get(position + 1).copied().unwrap_or(0.0),
            ];
            for value in neighbours {
                assert!(
                    value.abs() < 1e-5,
                    "N={n}: impulse at {position} leaked {value} into a neighbour"
                );
            }
        }
    }
}

#[test]
fn empty_and_short_inputs_round_trip() {
    for &n in &SIZES {
        let mut transform = build(n);
        assert!(transform.roundtrip(&[]).expect("empty").is_empty());
        assert_eq!(transform.analyze(&[]).frames(), 0);

        for len in [1usize, 2, 17, n - 1, n, n + 1] {
            let input = white_noise(len, 99 + len as u64);
            let output = transform.roundtrip(&input).expect("short round trip");
            assert_eq!(output.len(), len, "N={n}, len={len}");
            assert!(
                recon_error(&input, &output).rms_db < -100.0,
                "N={n}, len={len}: short input did not round trip"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[test]
fn streaming_latency_and_impulse_alignment() {
    for &n in &SIZES {
        let cfg = StftConfig::new(n, n / 4);
        let mut stream = StftStream::new(cfg).expect("valid stream");
        assert_eq!(stream.latency_samples(), n);
        assert!(
            (stream.latency_seconds(48_000) - n as f32 / 48_000.0).abs() < 1e-9,
            "N={n}: latency_seconds"
        );
        assert_eq!(stream.latency_seconds(0), 0.0);

        let len = 3 * n;
        let mut input = vec![0.0f32; len];
        let position = n + 37;
        input[position] = 1.0;

        // Nothing can be emitted before N samples have arrived.
        let mut output = Vec::new();
        stream.process_chunk_identity(&input[..n - 1], &mut output);
        assert!(
            output.is_empty(),
            "N={n}: {} samples emitted before the reported latency",
            output.len()
        );
        stream.process_chunk_identity(&input[n - 1..n], &mut output);
        assert_eq!(
            output.len(),
            n / 4,
            "N={n}: after N input samples exactly one hop should be ready"
        );
        stream.process_chunk_identity(&input[n..], &mut output);
        stream.flush_identity_into(&mut output);

        assert_eq!(output.len(), len);
        assert_eq!(argmax_abs(&output), Some(position), "N={n}: impulse moved");
        assert!((output[position] - 1.0).abs() < 1e-5);
    }
}

#[test]
fn chunked_output_matches_whole_buffer() {
    let n = 2048;
    let hop = n / 4;
    let mut transform = build(n);
    let input = white_noise(5 * n + 13, 0xC0FFEE);
    let whole = transform.roundtrip(&input).expect("round trip");

    let patterns: [&[usize]; 3] = [
        &[1, 7, hop - 1, hop, hop + 1, 4096, 12_345],
        &[hop; 1],
        &[3, 1000, 999, 1, 4096, 17, 31],
    ];
    for pattern in patterns {
        let mut stream = stream(n);
        let mut output = Vec::new();
        let mut offset = 0usize;
        let mut index = 0usize;
        while offset < input.len() {
            let want = pattern[index % pattern.len()].max(1);
            let end = (offset + want).min(input.len());
            stream.process_chunk_identity(&input[offset..end], &mut output);
            offset = end;
            index += 1;
        }
        stream.flush_identity_into(&mut output);
        assert_eq!(output.len(), whole.len());
        if output != whole {
            let worst = output
                .iter()
                .zip(&whole)
                .enumerate()
                .map(|(i, (&a, &b))| (i, (a - b).abs()))
                .fold((0usize, 0.0f32), |acc, x| if x.1 > acc.1 { x } else { acc });
            let mismatches = output.iter().zip(&whole).filter(|(a, b)| a != b).count();
            panic!(
                "chunked output is not bit-identical: {mismatches} samples differ, \
                 worst {} at {worst:?}",
                worst.1
            );
        }
    }

    // The closure path must match analyze -> modify -> synthesize exactly,
    // including frame ordering.
    let mut stream = stream(n);
    let mut streamed = Vec::new();
    let mut seen = Vec::new();
    let mut offset = 0;
    for chunk in input.chunks(hop) {
        stream.process_chunk(chunk, &mut streamed, |frame, bins| {
            seen.push(frame);
            for bin in bins.iter_mut() {
                *bin = *bin * 0.5;
            }
        });
        offset += chunk.len();
    }
    stream.flush_into(&mut streamed, |frame, bins| {
        seen.push(frame);
        for bin in bins.iter_mut() {
            *bin = *bin * 0.5;
        }
    });
    assert_eq!(offset, input.len());

    let mut analysed = transform.analyze(&input);
    let frames = analysed.frames();
    for frame in analysed.frames_iter_mut() {
        for bin in frame.iter_mut() {
            *bin = *bin * 0.5;
        }
    }
    let modified = transform
        .synthesize(&analysed, input.len())
        .expect("synthesize");
    assert_eq!(seen, (0..frames).collect::<Vec<_>>());
    assert!(
        streamed == modified,
        "streamed spectral modification differs from the whole-buffer path"
    );
    let expected: Vec<f32> = whole.iter().map(|v| v * 0.5).collect();
    assert!(recon_error(&expected, &modified).rms_db < -100.0);
}

#[test]
fn streaming_empty_input_is_empty() {
    let mut stream = stream(2048);
    let mut output = Vec::new();
    stream.process_chunk_identity(&[], &mut output);
    stream.flush_identity_into(&mut output);
    assert!(output.is_empty());
    // The stream is reusable after flush.
    stream.process_chunk_identity(&[0.25; 4], &mut output);
    stream.flush_identity_into(&mut output);
    assert_eq!(output.len(), 4);
}

// ---------------------------------------------------------------------------
// Validation: typed errors, never panics
// ---------------------------------------------------------------------------

#[test]
fn rejects_unsupported_window_sizes() {
    for &n in &[
        0usize,
        1,
        2,
        8,
        32,
        1000,
        1023,
        1025,
        3000,
        MAX_WINDOW_SIZE * 2,
    ] {
        let hop = n / 4;
        let error = Stft::new(StftConfig::new(n, hop)).unwrap_err();
        assert!(
            matches!(error, StftError::UnsupportedWindowSize { window_size } if window_size == n),
            "N={n}: got {error:?}"
        );
    }
}

#[test]
fn rejects_non_quarter_hops() {
    for &(n, hop) in &[
        (1024usize, 0usize),
        (1024, 128),
        (1024, 341),
        (1024, 512),
        (1024, 1024),
        (1024, 2048),
        (2048, 256),
        (4096, 512),
    ] {
        let error = Stft::new(StftConfig::new(n, hop)).unwrap_err();
        assert!(
            matches!(
                error,
                StftError::UnsupportedHop {
                    hop: actual_hop,
                    window_size,
                    expected,
                } if actual_hop == hop && window_size == n && expected == n / 4
            ),
            "N={n}, hop={hop}: got {error:?}"
        );
    }
}

#[test]
fn rejects_wrong_slice_lengths() {
    let mut transform = build(1024);
    let mut bins = vec![Complex::ZERO; transform.bins()];
    let mut frame = vec![0.0f32; transform.window_size()];

    let error = transform
        .analyze_frame(&frame[..10], &mut bins)
        .unwrap_err();
    assert!(matches!(
        error,
        StftError::FrameLength {
            expected: 1024,
            actual: 10
        }
    ));

    let error = transform.analyze_frame(&frame, &mut bins[..4]).unwrap_err();
    assert!(matches!(
        error,
        StftError::BinLength {
            expected: 513,
            actual: 4
        }
    ));

    let error = transform
        .synthesize_frame(&bins[..2], &mut frame)
        .unwrap_err();
    assert!(matches!(
        error,
        StftError::BinLength {
            expected: 513,
            actual: 2
        }
    ));

    let error = transform
        .synthesize_frame(&bins, &mut frame[..5])
        .unwrap_err();
    assert!(matches!(
        error,
        StftError::FrameLength {
            expected: 1024,
            actual: 5
        }
    ));
}

#[test]
fn rejects_mismatched_spectrograms() {
    let mut transform = build(1024);
    let input = white_noise(4096, 3);
    let analysed = transform.analyze(&input);

    let mut wrong_bins = analysed.clone();
    wrong_bins.reset(analysed.frames(), analysed.bins() + 1, analysed.hop(), 1024);
    let error = transform.synthesize(&wrong_bins, input.len()).unwrap_err();
    assert!(matches!(
        error,
        StftError::Geometry {
            field: "bins",
            expected: 513,
            ..
        }
    ));

    let mut wrong_hop = analysed.clone();
    wrong_hop.reset(analysed.frames(), analysed.bins(), 128, 1024);
    let error = transform.synthesize(&wrong_hop, input.len()).unwrap_err();
    assert!(matches!(error, StftError::Geometry { field: "hop", .. }));

    let mut wrong_window = analysed.clone();
    wrong_window.reset(analysed.frames(), analysed.bins(), analysed.hop(), 2048);
    let error = transform
        .synthesize(&wrong_window, input.len())
        .unwrap_err();
    assert!(matches!(
        error,
        StftError::Geometry {
            field: "window_size",
            ..
        }
    ));

    let error = transform
        .synthesize(&analysed, input.len() / 2)
        .unwrap_err();
    assert!(matches!(
        error,
        StftError::Geometry {
            field: "frames",
            ..
        }
    ));
}

#[test]
fn spectrogram_accessors_are_bounds_safe() {
    let mut spectrogram = Spectrogram::new(2, 3, 1, 4);
    assert_eq!(spectrogram.frames(), 2);
    assert_eq!(spectrogram.bins(), 3);
    assert_eq!(spectrogram.len(), 6);
    assert!(!spectrogram.is_empty());
    assert!(spectrogram.frame(2).is_none());
    assert!(spectrogram.frame_mut(2).is_none());
    assert_eq!(spectrogram.frame(1).map(<[Complex]>::len), Some(3));
    assert_eq!(spectrogram.frames_iter().count(), 2);
    assert_eq!(spectrogram.frames_iter_mut().count(), 2);

    spectrogram.reset(0, 3, 1, 4);
    assert!(spectrogram.is_empty());
    assert!(Spectrogram::new(0, 0, 1, 4).is_empty());
}

#[test]
fn configuration_and_helpers_are_reported_correctly() {
    let transform = build(2048);
    assert_eq!(transform.config(), StftConfig::new(2048, 512));
    assert_eq!(StftConfig::default(), StftConfig::d2());
    assert_eq!(transform.window_size(), 2048);
    assert_eq!(transform.hop(), 512);
    assert_eq!(transform.bins(), 1025);
    assert_eq!(transform.frame_count(0), 0);
    assert_eq!(transform.frame_count(1), (2048 + 1usize).div_ceil(512));
    assert_eq!(transform.frame_count(2048), 8);
    assert_eq!(transform.frame_center(0), -1024);
    assert_eq!(transform.frame_center(2), 0);
    assert_eq!(transform.frame_center(3) - transform.frame_center(2), 512);
    assert_eq!(transform.latency_samples(), 2048);
    assert!((transform.latency_seconds(48_000) - 42.666_668e-3).abs() < 1e-9);
    assert_eq!(transform.latency_seconds(0), 0.0);
    assert!((transform.bin_width_hz(48_000) - 23.4375).abs() < 1e-6);
    assert_eq!(transform.bin_hz(0, 48_000), 0.0);
    assert!((transform.bin_hz(1024, 48_000) - 24_000.0).abs() < 1e-3);
    assert_eq!(transform.bin_hz(99_999, 48_000), 24_000.0);
    assert_eq!(transform.bin_width_hz(0), 0.0);
    assert_eq!(transform.bin_hz(3, 0), 0.0);
}

#[test]
fn complex_type_ops_are_consistent() {
    let value = Complex::from_polar(2.0, 0.75);
    assert!((value.magnitude() - 2.0).abs() < 1e-6);
    assert!((value.magnitude_sq() - 4.0).abs() < 1e-5);
    assert!((value.phase() - 0.75).abs() < 1e-6);
    assert_eq!(value.conj().im, -value.im);
    assert_eq!(Complex::ZERO, Complex::new(0.0, 0.0));

    let a = Complex::new(1.0, 2.0);
    let b = Complex::new(-3.0, 0.5);
    let product = a * b;
    assert!((product.re - (-4.0)).abs() < 1e-6);
    assert!((product.im - (-5.5)).abs() < 1e-6);
    assert_eq!(a + b, Complex::new(-2.0, 2.5));
    assert_eq!(a - b, Complex::new(4.0, 1.5));
    assert_eq!(-a, Complex::new(-1.0, -2.0));
    assert_eq!(a.scale(2.0), Complex::new(2.0, 4.0));
    assert_eq!(a * 2.0, Complex::new(2.0, 4.0));

    let mut acc = a;
    acc += b;
    acc -= Complex::new(1.0, 1.0);
    acc *= Complex::new(1.0, 0.0);
    assert_eq!(acc, Complex::new(-3.0, 1.5));
}

#[test]
fn analyze_into_and_synthesize_into_reuse_buffers() {
    let mut transform = build(2048);
    let input = white_noise(12_345, 0x4444);
    let analysed = transform.analyze(&input);

    let mut reused = Spectrogram::new(0, 0, 0, 0);
    transform.analyze_into(&input, &mut reused);
    assert_eq!(reused, analysed);

    let mut output_a = Vec::new();
    transform
        .synthesize_into(&analysed, input.len(), &mut output_a)
        .expect("synthesize");
    let expected = transform.roundtrip(&input).expect("round trip");
    assert!(output_a == expected);

    // A second call reuses the accumulator and output allocation and produces
    // the same bytes.
    transform
        .synthesize_into(&analysed, input.len(), &mut output_a)
        .expect("synthesize");
    assert!(output_a == expected);
}

#[test]
fn streaming_buffers_stay_bounded() {
    for &n in &SIZES {
        let hop = n / 4;
        let mut stream = stream(n);
        let mut output = Vec::new();
        let input = white_noise(10 * n, 0x9999);
        for chunk in input.chunks(hop) {
            stream.process_chunk_identity(chunk, &mut output);
            assert!(
                stream.input.len() <= n + STREAM_BLOCK,
                "N={n}: input history grew to {}",
                stream.input.len()
            );
            assert!(
                stream.ola.len() <= n + STREAM_BLOCK + hop,
                "N={n}: overlap-add buffer grew to {}",
                stream.ola.len()
            );
        }
        stream.flush_identity_into(&mut output);
        assert_eq!(output.len(), input.len());
        assert!(recon_error(&input, &output).rms_db < -100.0);
        assert!(stream.ola.is_empty());
        assert!(stream.input.is_empty());
    }
}

#[test]
fn reset_discards_unflushed_state() {
    let mut transform = build(2048);
    let input = white_noise(10_000, 0x7777);
    let mut stream = stream(2048);
    let mut discarded = Vec::new();
    stream.process_chunk_identity(&input[..4000], &mut discarded);
    stream.reset();

    let mut output = Vec::new();
    stream.process_chunk_identity(&input, &mut output);
    stream.flush_identity_into(&mut output);
    let whole = transform.roundtrip(&input).expect("round trip");
    assert!(output == whole);
}
