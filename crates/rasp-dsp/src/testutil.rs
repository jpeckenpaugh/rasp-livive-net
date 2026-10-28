//! Test-only helpers shared by the DSP unit tests.
//!
//! Fixtures are generated, never committed (`docs/testing.md`), so every
//! fixture entry point returns `None` when `fixtures/manifest.json` is absent
//! and the calling test prints a skip line instead of failing. When the
//! manifest *is* present the tree is treated as a contract: byte count,
//! sample count and `sha256` must all match, and a stale tree fails loudly.
//!
//! The manifest is parsed with a tiny JSON reader instead of a dependency, so
//! the crate stays dependency-free even in its test build. Only the fields the
//! fixture contract names are read: `clips[].name`, `clips[].sampleCount` and
//! `clips[].files.f32.{path,bytes,sha256}`.

use std::fs;
use std::path::{Path, PathBuf};

/// One decoded fixture clip.
pub struct Fixture {
    /// `<voice>__<phrase>`, as recorded in the manifest.
    pub name: String,
    /// Raw 48 kHz mono f32 samples (FFmpeg's native Opus decoder).
    pub samples: Vec<f32>,
}

/// `fixtures/`, or `None` when the fixtures have not been generated.
pub fn fixtures_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    dir.join("manifest.json").is_file().then_some(dir)
}

/// The 12 fixture clips, or `None` when `fixtures/manifest.json` is absent.
///
/// Panics (with the offending path and field) if the manifest is present but
/// malformed or inconsistent with the files on disk. That is deliberate: a
/// half-deleted or stale fixture tree must not silently change what a test
/// measures.
pub fn load_fixtures() -> Option<Vec<Fixture>> {
    let dir = fixtures_dir()?;
    let manifest_path = dir.join("manifest.json");
    let text = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", manifest_path.display()));
    let json =
        Json::parse(&text).unwrap_or_else(|| panic!("cannot parse {}", manifest_path.display()));
    let clips = json
        .get("clips")
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("{} has no clips array", manifest_path.display()));

    let mut fixtures = Vec::with_capacity(clips.len());
    for clip in clips {
        let name = clip
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("{}: clip without a name", manifest_path.display()))
            .to_string();
        let sample_count =
            clip.get("sampleCount")
                .and_then(Json::as_f64)
                .unwrap_or_else(|| panic!("{name}: clip without sampleCount")) as usize;
        let file = clip
            .get("files")
            .and_then(|files| files.get("f32"))
            .unwrap_or_else(|| panic!("{name}: clip without files.f32"));
        let relative = file
            .get("path")
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("{name}: files.f32 without a path"));
        let bytes = file
            .get("bytes")
            .and_then(Json::as_f64)
            .unwrap_or_else(|| panic!("{name}: files.f32 without a byte count"))
            as usize;
        let sha256 = file
            .get("sha256")
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("{name}: files.f32 without sha256"));

        let path = dir.join(relative);
        let raw = fs::read(&path).unwrap_or_else(|err| {
            panic!(
                "fixture {name} is unreadable at {}: {err} — run \
                 `python3 scripts/make_fixtures.py`",
                path.display()
            )
        });
        assert_eq!(
            raw.len(),
            bytes,
            "fixture {name}: {} has {} bytes, manifest says {bytes} — regenerate with \
             `python3 scripts/make_fixtures.py --force`",
            path.display(),
            raw.len()
        );
        assert_eq!(
            raw.len(),
            sample_count * 4,
            "fixture {name}: byte count {} does not match sampleCount {sample_count}",
            raw.len()
        );
        assert_eq!(
            sha256_hex(&raw),
            sha256,
            "fixture {name}: sha256 mismatch — regenerate with \
             `python3 scripts/make_fixtures.py --force`"
        );

        let samples = raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect();
        fixtures.push(Fixture { name, samples });
    }
    Some(fixtures)
}

// ---------------------------------------------------------------------------
// Deterministic signals and measurements
// ---------------------------------------------------------------------------

/// Small deterministic PRNG, so tests never depend on a system RNG.
pub struct Lcg(u64);

impl Lcg {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
        Lcg(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    /// Next raw 32-bit word.
    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    /// Uniform sample in `[-1, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u32() as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32
    }
}

/// A steady synthetic vowel: F0 plus harmonics shaped by a cascade of
/// two-pole formant resonators (PLAN P-03 ground truth).
///
/// Harmonic `h` at `h·f0` has amplitude
/// `Π 1/|1 − 2r·cos(ω_j)·z + r²z²| · h^-1`, `r = exp(−π·B_j/sample_rate)`,
/// evaluated at `z = exp(−jω)`. The `1/h` glottal tilt is deliberate (real
/// voiced speech has one). The result is peak-normalised to 0.5.
///
/// **The generated signal's formants are exactly `formants` by construction**,
/// which is what makes it usable as primary ground truth for P-03 rather than
/// measuring one estimator with another.
pub fn vowel(
    f0: f64,
    formants: &[f64],
    bandwidths: &[f64],
    sample_rate: u32,
    len: usize,
) -> Vec<f32> {
    let mut signal = vec![0.0f64; len];
    if f0 <= 0.0 || sample_rate == 0 || len == 0 || formants.len() != bandwidths.len() {
        return signal.into_iter().map(|x| x as f32).collect();
    }
    let sample_rate_f = sample_rate as f64;
    let nyquist = sample_rate_f / 2.0;
    let mut harmonic = 1u32;
    while (harmonic as f64) * f0 < nyquist {
        let frequency = harmonic as f64 * f0;
        let mut envelope = 1.0f64;
        for (&formant, &bandwidth) in formants.iter().zip(bandwidths) {
            let radius = (-core::f64::consts::PI * bandwidth / sample_rate_f).exp();
            let (sin_w, cos_w) =
                (2.0 * core::f64::consts::PI * frequency / sample_rate_f).sin_cos();
            let z_re = cos_w;
            let z_im = -sin_w;
            // 1 - 2r·cos(ω_j)·z + r²·z²
            let (_, cos_f) = (2.0 * core::f64::consts::PI * formant / sample_rate_f).sin_cos();
            let a = 1.0;
            let b_re = -2.0 * radius * cos_f * z_re;
            let b_im = -2.0 * radius * cos_f * z_im;
            let z2_re = z_re * z_re - z_im * z_im;
            let z2_im = 2.0 * z_re * z_im;
            let c_re = radius * radius * z2_re;
            let c_im = radius * radius * z2_im;
            let re = a + b_re + c_re;
            let im = b_im + c_im;
            envelope *= 1.0 / (re * re + im * im).sqrt().max(1e-12);
        }
        let amplitude = envelope / harmonic as f64;
        for (i, slot) in signal.iter_mut().enumerate() {
            let phase = 2.0 * core::f64::consts::PI * frequency * i as f64 / sample_rate_f;
            *slot += amplitude * phase.sin();
        }
        harmonic += 1;
    }
    let peak = signal
        .iter()
        .fold(0.0f64, |acc, &x| acc.max(x.abs()))
        .max(1e-12);
    signal
        .into_iter()
        .map(|x| (0.5 * x / peak) as f32)
        .collect()
}

/// Root-mean-square of a signal.
pub fn rms(signal: &[f32]) -> f64 {
    if signal.is_empty() {
        return 0.0;
    }
    let sum: f64 = signal.iter().map(|&x| (x as f64) * (x as f64)).sum();
    (sum / signal.len() as f64).sqrt()
}

/// Hann-windowed magnitude spectrum, zero-padded to `fft_size` (a power of
/// two). Test-only: used by the P-03 formant ruler, not by the DSP.
pub fn magnitude_spectrum(signal: &[f32], fft_size: usize) -> Option<Vec<f32>> {
    if signal.is_empty() || fft_size < signal.len() || !fft_size.is_power_of_two() {
        return None;
    }
    let mut frame = vec![0.0f32; fft_size];
    let n = signal.len();
    for (i, slot) in frame.iter_mut().take(n).enumerate() {
        let window = 0.5 - 0.5 * (2.0 * core::f64::consts::PI * i as f64 / n as f64).cos();
        *slot = signal[i] * window as f32;
    }
    let mut fft = crate::stft::RealFft::new(fft_size);
    let mut spectrum = vec![crate::stft::Complex::ZERO; fft_size / 2 + 1];
    fft.forward(&frame, &mut spectrum);
    Some(spectrum.iter().map(|bin| bin.magnitude()).collect())
}

/// Harmonic amplitudes of a harmonic signal, `(frequency_hz, magnitude)`.
///
/// For harmonic `h` this is the largest magnitude in a ±`tolerance_hz` window
/// around `h·f0`, from a zero-padded Hann spectrum. Using the maximum rather
/// than the exact-bin value keeps the estimate honest when the analysis length
/// is not an integer number of periods.
pub fn harmonic_amplitudes(
    signal: &[f32],
    sample_rate: u32,
    f0: f64,
    tolerance_hz: f64,
    max_hz: f64,
) -> Vec<(f64, f64)> {
    let fft_size = signal.len().next_power_of_two().max(1024);
    let Some(spectrum) = magnitude_spectrum(signal, fft_size) else {
        return Vec::new();
    };
    let bin_hz = sample_rate as f64 / fft_size as f64;
    let nyquist = sample_rate as f64 / 2.0;
    let mut out = Vec::new();
    let mut harmonic = 1u32;
    while (harmonic as f64) * f0 < max_hz.min(nyquist - 100.0) {
        let frequency = harmonic as f64 * f0;
        let low = (((frequency - tolerance_hz) / bin_hz).floor().max(0.0)) as usize;
        let high = (((frequency + tolerance_hz) / bin_hz).ceil() as usize).min(spectrum.len() - 1);
        if low <= high {
            let peak = spectrum[low..=high].iter().copied().fold(0.0f32, f32::max);
            out.push((frequency, peak as f64));
        }
        harmonic += 1;
    }
    out
}

/// Log amplitude of a cascade of two-pole resonators at `frequency`, i.e. the
/// generative model behind [`vowel`]. Test-only.
pub fn resonator_log_amplitude(
    frequency: f64,
    formants: &[f64],
    bandwidths: &[f64],
    sample_rate: f64,
) -> f64 {
    let mut log_amplitude = 0.0;
    for (&formant, &bandwidth) in formants.iter().zip(bandwidths) {
        let radius = (-core::f64::consts::PI * bandwidth / sample_rate).exp();
        let (sin_w, cos_w) = (2.0 * core::f64::consts::PI * frequency / sample_rate).sin_cos();
        let z_re = cos_w;
        let z_im = -sin_w;
        let (_, cos_f) = (2.0 * core::f64::consts::PI * formant / sample_rate).sin_cos();
        let b_re = -2.0 * radius * cos_f * z_re;
        let b_im = -2.0 * radius * cos_f * z_im;
        let z2_re = z_re * z_re - z_im * z_im;
        let z2_im = 2.0 * z_re * z_im;
        let c_re = radius * radius * z2_re;
        let c_im = radius * radius * z2_im;
        let re = 1.0 + b_re + c_re;
        let im = b_im + c_im;
        log_amplitude -= (re * re + im * im).sqrt().max(1e-12).ln();
    }
    log_amplitude
}

/// One model-fit evaluation: log-amplitude residual of a three-formant
/// cascade plus a free `C + α·log f` source tilt.
fn formant_fit_residual(
    frequencies: &[f64],
    log_amplitudes: &[f64],
    formants: &[f64],
    bandwidths: &[f64],
    sample_rate: f64,
) -> f64 {
    let count = frequencies.len();
    if count < 8 {
        return f64::INFINITY;
    }
    let logs: Vec<f64> = frequencies.iter().map(|f| f.ln()).collect();
    let shaped: Vec<f64> = frequencies
        .iter()
        .zip(log_amplitudes)
        .map(|(&f, &amplitude)| {
            amplitude - resonator_log_amplitude(f, formants, bandwidths, sample_rate)
        })
        .collect();
    // Regress the residual on [1, log f] to absorb the glottal tilt.
    let mean_x = logs.iter().sum::<f64>() / count as f64;
    let mean_y = shaped.iter().sum::<f64>() / count as f64;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (x, y) in logs.iter().zip(&shaped) {
        sxx += (x - mean_x) * (x - mean_x);
        sxy += (x - mean_x) * (y - mean_y);
    }
    let alpha = if sxx > 1e-12 { sxy / sxx } else { 0.0 };
    let intercept = mean_y - alpha * mean_x;
    logs.iter()
        .zip(&shaped)
        .map(|(x, y)| {
            let error = y - intercept - alpha * x;
            error * error
        })
        .sum()
}

/// Fit three formant frequencies and bandwidths to a harmonic signal, with a
/// free source tilt.
///
/// Test-only measurement ruler for P-03. It fits the **generative model** of
/// [`vowel`] (three two-pole resonators plus `h^-α`), so on a signal synthesized
/// with known formants it recovers them to well under 1% — that is the control
/// the P-02 lesson demands before any processed signal is judged. Bandwidths are
/// free because a spectral-envelope estimate broadens formants, and a ruler
/// that assumed the generator's bandwidths would misattribute that broadening
/// to a frequency shift.
///
/// `init` seeds the search (the expected answer); coordinate descent with
/// halving steps keeps it robust without a scipy-grade optimiser.
pub fn fit_formants(
    signal: &[f32],
    sample_rate: u32,
    f0: f64,
    init: [f64; 3],
    init_bandwidths: [f64; 3],
) -> Option<([f64; 3], [f64; 3])> {
    let amps = harmonic_amplitudes(signal, sample_rate, f0, 25.0, 6000.0);
    if amps.len() < 8 {
        return None;
    }
    let frequencies: Vec<f64> = amps.iter().map(|(f, _)| *f).collect();
    let log_amplitudes: Vec<f64> = amps.iter().map(|(_, a)| a.max(1e-12).ln()).collect();
    let sample_rate_f = sample_rate as f64;
    let nyquist = sample_rate_f / 2.0;

    let mut formants = init;
    let mut bandwidths = init_bandwidths;
    let mut steps = [
        init[0] * 0.20,
        init[1] * 0.20,
        init[2] * 0.20,
        init_bandwidths[0] * 0.50,
        init_bandwidths[1] * 0.50,
        init_bandwidths[2] * 0.50,
    ];
    let mut best = formant_fit_residual(
        &frequencies,
        &log_amplitudes,
        &formants,
        &bandwidths,
        sample_rate_f,
    );

    // Coarse multi-start grid first: coordinate descent alone can lock onto a
    // harmonic instead of the formant, which would make the ruler, not the DSP,
    // decide the result.
    let mut grid_best = (best, formants);
    for i1 in 0..9 {
        let f1 = init[0] * (0.72 + 0.07 * i1 as f64);
        for i2 in 0..9 {
            let f2 = init[1] * (0.72 + 0.07 * i2 as f64);
            if f2 <= f1 * 1.15 {
                continue;
            }
            for i3 in 0..9 {
                let f3 = init[2] * (0.72 + 0.07 * i3 as f64);
                if f3 <= f2 * 1.15 || f3 >= nyquist * 0.95 {
                    continue;
                }
                let candidate = [f1, f2, f3];
                let residual = formant_fit_residual(
                    &frequencies,
                    &log_amplitudes,
                    &candidate,
                    &bandwidths,
                    sample_rate_f,
                );
                if residual < grid_best.0 {
                    grid_best = (residual, candidate);
                }
            }
        }
    }
    best = grid_best.0;
    formants = grid_best.1;

    for _ in 0..7 {
        let mut improved = true;
        while improved {
            improved = false;
            for index in 0..3 {
                for direction in [-1.0f64, 1.0] {
                    let mut candidate = formants;
                    candidate[index] += direction * steps[index];
                    if !(120.0 < candidate[0]
                        && candidate[0] * 1.15 < candidate[1]
                        && candidate[1] * 1.15 < candidate[2]
                        && candidate[2] < nyquist * 0.95)
                    {
                        continue;
                    }
                    let residual = formant_fit_residual(
                        &frequencies,
                        &log_amplitudes,
                        &candidate,
                        &bandwidths,
                        sample_rate_f,
                    );
                    if residual < best {
                        best = residual;
                        formants = candidate;
                        improved = true;
                    }
                }
            }
            for index in 0..3 {
                for direction in [-1.0f64, 1.0] {
                    let mut candidate = bandwidths;
                    candidate[index] =
                        (candidate[index] + direction * steps[3 + index]).clamp(20.0, 900.0);
                    let residual = formant_fit_residual(
                        &frequencies,
                        &log_amplitudes,
                        &formants,
                        &candidate,
                        sample_rate_f,
                    );
                    if residual < best {
                        best = residual;
                        bandwidths = candidate;
                        improved = true;
                    }
                }
            }
        }
        for step in steps.iter_mut() {
            *step *= 0.5;
        }
    }

    Some((formants, bandwidths))
}

/// RMS log-amplitude residual, in dB, of a **given** formant/bandwidth model
/// against a harmonic signal.
///
/// This is the same goodness-of-fit number `scripts/audio_metrics.py` gates on
/// (`FORMANT_FIT_MAX_RESIDUAL_DB`): `sqrt(Σ error² / n_harmonics)`, converted
/// from nepers to dB. It answers a different question from
/// [`fit_formants`] — "does this signal look like the model at all?" — which is
/// what D20 requires before a fitted point may be published.
///
/// Returns `None` when the signal has fewer than eight harmonics in the
/// 6 kHz measurement band, i.e. the residual would not be defined.
pub fn formant_fit_residual_db(
    signal: &[f32],
    sample_rate: u32,
    f0: f64,
    formants: [f64; 3],
    bandwidths: [f64; 3],
) -> Option<f64> {
    let amps = harmonic_amplitudes(signal, sample_rate, f0, 25.0, 6000.0);
    if amps.len() < 8 {
        return None;
    }
    let frequencies: Vec<f64> = amps.iter().map(|(frequency, _)| *frequency).collect();
    let log_amplitudes: Vec<f64> = amps
        .iter()
        .map(|(_, amplitude)| amplitude.max(1e-12).ln())
        .collect();
    let sum = formant_fit_residual(
        &frequencies,
        &log_amplitudes,
        &formants,
        &bandwidths,
        sample_rate as f64,
    );
    let residual = (sum / amps.len() as f64).sqrt() * 8.685_889_638;
    residual.is_finite().then_some(residual)
}

/// Relative error of `measured` against `expected`, in percent.
pub fn relative_error_percent(measured: &[f64], expected: &[f64]) -> Vec<f64> {
    measured
        .iter()
        .zip(expected)
        .map(|(m, e)| if *e == 0.0 { 0.0 } else { 100.0 * (m - e) / e })
        .collect()
}

/// Deterministic white noise in `[-1, 1)`.
pub fn white_noise(len: usize, seed: u64) -> Vec<f32> {
    let mut rng = Lcg::new(seed);
    (0..len).map(|_| rng.next_f32()).collect()
}

/// A sinusoid of amplitude 0.5 at `freq` Hz.
pub fn sine(freq: f64, sample_rate: u32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| {
            let phase = 2.0 * core::f64::consts::PI * freq * i as f64 / sample_rate as f64;
            (0.5 * phase.sin()) as f32
        })
        .collect()
}

/// A steady synthetic vowel: F0 plus 30 harmonics shaped by three formants.
///
/// Formants at 700 / 1220 / 2600 Hz with 80 Hz bandwidths, each harmonic
/// weighted by the summed resonance response and `1/h`. Peak-normalised to
/// 0.5. Used to check that pitch tracking survives a realistic harmonic
/// complex, not just a pure tone.
pub fn harmonic_vowel(f0: f64, sample_rate: u32, len: usize) -> Vec<f32> {
    let mut signal = vec![0.0f64; len];
    for harmonic in 1..=30u32 {
        let freq = harmonic as f64 * f0;
        if freq >= sample_rate as f64 / 2.0 {
            break;
        }
        let mut envelope = 0.0f64;
        for formant in [700.0f64, 1220.0, 2600.0] {
            let detune = (freq - formant) / 80.0;
            envelope += 1.0 / (1.0 + detune * detune);
        }
        let amplitude = envelope / harmonic as f64;
        for (i, slot) in signal.iter_mut().enumerate() {
            let phase = 2.0 * core::f64::consts::PI * freq * i as f64 / sample_rate as f64;
            *slot += amplitude * phase.sin();
        }
    }
    let peak = signal
        .iter()
        .fold(0.0f64, |acc, &x| acc.max(x.abs()))
        .max(1e-12);
    signal
        .into_iter()
        .map(|x| (0.5 * x / peak) as f32)
        .collect()
}

/// Fundamental frequency by normalised autocorrelation with parabolic
/// refinement, over the highest-energy 8192-sample window.
///
/// Tuned for the shifter's tests, not a replacement for the metrics harness:
/// it returns the *shortest* strong period so octave-down errors are visible,
/// and it searches the caller's `fmin..fmax` rather than a fixed 60–400 Hz
/// band (the −12 st low-male case is below 60 Hz).
pub fn estimate_f0(signal: &[f32], sample_rate: u32, fmin: f64, fmax: f64) -> Option<f64> {
    if signal.len() < 256
        || sample_rate == 0
        || !fmin.is_finite()
        || !fmax.is_finite()
        || fmin <= 0.0
        || fmax <= fmin
    {
        return None;
    }
    let max_window = 4096.min(signal.len());
    // Pick the highest-energy window so a fixture's head/tail silence does not
    // decide the estimate.
    let mut best_start = 0usize;
    let mut best_energy = f64::NEG_INFINITY;
    let stride = (max_window / 4).max(1);
    let mut start = 0usize;
    while start + max_window <= signal.len() {
        let energy: f64 = signal[start..start + max_window]
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum();
        if energy > best_energy {
            best_energy = energy;
            best_start = start;
        }
        start += stride;
    }
    let frame = &signal[best_start..best_start + max_window];
    let mean = frame.iter().map(|&x| x as f64).sum::<f64>() / frame.len() as f64;
    let centred: Vec<f64> = frame.iter().map(|&x| x as f64 - mean).collect();

    let low_lag = (sample_rate as f64 / fmax).floor().max(2.0) as usize;
    let high_lag = (sample_rate as f64 / fmin).ceil() as usize;
    if high_lag + 1 >= max_window || low_lag >= high_lag {
        return None;
    }

    // Prefix sums of squares make the normalisation O(1) per lag.
    let mut prefix = vec![0.0f64; centred.len() + 1];
    for (i, value) in centred.iter().enumerate() {
        prefix[i + 1] = prefix[i] + value * value;
    }
    let energy = |from: usize, to: usize| -> f64 { prefix[to] - prefix[from] };
    let correlation = |lag: usize| -> f64 {
        let count = max_window - lag;
        let mut dot = 0.0f64;
        for i in 0..count {
            dot += centred[i] * centred[i + lag];
        }
        let den = (energy(0, count) * energy(lag, lag + count)).sqrt();
        if den > 1e-20 {
            dot / den
        } else {
            0.0
        }
    };

    let mut values = Vec::with_capacity(high_lag - low_lag + 1);
    for lag in low_lag..=high_lag {
        values.push(correlation(lag));
    }
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if maximum < 0.3 {
        return None;
    }
    // Prefer the first local maximum that is close to the global one: a
    // subharmonic (double period) is then not chosen by accident.
    let threshold = maximum * 0.9;
    let mut chosen = None;
    for i in 1..values.len() - 1 {
        if values[i] >= values[i - 1] && values[i] >= values[i + 1] && values[i] >= threshold {
            chosen = Some(i);
            break;
        }
    }
    let index = chosen.or_else(|| {
        values
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal))
            .map(|(i, _)| i)
    })?;
    let lag = low_lag + index;
    if lag == 0 || lag + 2 > max_window {
        return None;
    }

    let left = correlation(lag - 1);
    let centre = values[index];
    let right = correlation(lag + 1);
    let denominator = left - 2.0 * centre + right;
    let offset = if denominator.abs() > 1e-12 {
        0.5 * (left - right) / denominator
    } else {
        0.0
    };
    let refined = lag as f64 + offset.clamp(-0.5, 0.5);
    (refined > 0.0).then_some(sample_rate as f64 / refined)
}

/// Ratio in dB, floored so an exact zero does not produce `-inf`.
pub fn db(num: f64, den: f64) -> f64 {
    20.0 * (num.max(1e-15) / den.max(1e-15)).log10()
}

/// Largest absolute sample.
pub fn max_abs(signal: &[f32]) -> f64 {
    signal
        .iter()
        .fold(0.0f64, |acc, &x| acc.max((x as f64).abs()))
}

/// Index of the largest absolute sample, or `None` when empty.
pub fn argmax_abs(signal: &[f32]) -> Option<usize> {
    signal
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            a.abs()
                .partial_cmp(&b.abs())
                .unwrap_or(core::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
}

// ---------------------------------------------------------------------------
// Minimal JSON reader (test-only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Json {
    Null,
    Bool,
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn parse(text: &str) -> Option<Json> {
        let mut parser = JsonParser {
            bytes: text.as_bytes(),
            pos: 0,
        };
        let value = parser.value()?;
        parser.skip_ws();
        (parser.pos == parser.bytes.len()).then_some(value)
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(values) => Some(values),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(value) => Some(*value),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl JsonParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        (self.bump()? == byte).then_some(())
    }

    fn literal(&mut self, text: &[u8]) -> Option<()> {
        let end = self.pos.checked_add(text.len())?;
        if self.bytes.get(self.pos..end)? == text {
            self.pos = end;
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::String),
            b't' => {
                self.literal(b"true")?;
                Some(Json::Bool)
            }
            b'f' => {
                self.literal(b"false")?;
                Some(Json::Bool)
            }
            b'n' => {
                self.literal(b"null")?;
                Some(Json::Null)
            }
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            match byte {
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' => self.pos += 1,
                _ => break,
            }
        }
        if self.pos == start {
            return None;
        }
        let text = core::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        text.parse().ok().map(Json::Number)
    }

    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let byte = self.bump()?;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let escape = self.bump()?;
                    match escape {
                        b'"' | b'\\' | b'/' => out.push(escape),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            // The generated manifest uses raw UTF-8, so this is
                            // only a structural fallback (BMP, no surrogates).
                            let hex = self.bytes.get(self.pos..self.pos + 4)?;
                            self.pos += 4;
                            let code =
                                u16::from_str_radix(core::str::from_utf8(hex).ok()?, 16).ok()?;
                            let ch = char::from_u32(code as u32)?;
                            let mut buffer = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                        }
                        _ => return None,
                    }
                }
                _ => out.push(byte),
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.expect(b'[')?;
        let mut values = Vec::new();
        self.skip_ws();
        if self.peek()? == b']' {
            self.pos += 1;
            return Some(Json::Array(values));
        }
        loop {
            values.push(self.value()?);
            self.skip_ws();
            match self.bump()? {
                b',' => self.skip_ws(),
                b']' => return Some(Json::Array(values)),
                _ => return None,
            }
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek()? == b'}' {
            self.pos += 1;
            return Some(Json::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.bump()? {
                b',' => self.skip_ws(),
                b'}' => return Some(Json::Object(entries)),
                _ => return None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SHA-256 (test-only fixture integrity check)
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// Lowercase hex SHA-256 of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(data.len() + 72);
    message.extend_from_slice(data);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.as_chunks::<64>().0 {
        sha256_compress(&mut state, block);
    }

    let mut hex = String::with_capacity(64);
    for word in state {
        hex.push_str(&format!("{word:08x}"));
    }
    hex
}

fn sha256_compress(state: &mut [u32; 8], block: &[u8]) {
    let mut schedule = [0u32; 64];
    for (index, word) in block.as_chunks::<4>().0.iter().enumerate() {
        schedule[index] = u32::from_be_bytes(*word);
    }
    for i in 16..64 {
        let s0 = schedule[i - 15].rotate_right(7)
            ^ schedule[i - 15].rotate_right(18)
            ^ (schedule[i - 15] >> 3);
        let s1 = schedule[i - 2].rotate_right(17)
            ^ schedule[i - 2].rotate_right(19)
            ^ (schedule[i - 2] >> 10);
        schedule[i] = schedule[i - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choice = (e & f) ^ (!e & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(choice)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(schedule[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn json_reader_handles_the_manifest_subset() {
        let json =
            Json::parse(r#"{"a":[1,-2.5,true,null],"b":"x\"y","c":{"d":3}}"#).expect("valid JSON");
        assert_eq!(json.get("a").and_then(Json::as_array).unwrap().len(), 4);
        assert_eq!(json.get("b").and_then(Json::as_str), Some("x\"y"));
        assert_eq!(
            json.get("c")
                .and_then(|c| c.get("d"))
                .and_then(Json::as_f64),
            Some(3.0)
        );
    }
}
