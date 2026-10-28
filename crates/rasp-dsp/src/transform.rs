//! Combined pitch + formant transformation (PLAN P-03, decisions D1/D3).
//!
//! [`VoiceTransformer`] composes the P-02 phase-vocoder pitch shifter with the
//! P-03 envelope correction:
//!
//! 1. Pitch-shift the input by `pitch_ratio` with
//!    [`crate::pitch::PitchShifter`]. That moves **both** the excitation and the
//!    formants by `pitch_ratio` (the resampled spectrum is scaled in
//!    frequency).
//! 2. Analyse the input and the pitch-shifted signal with the same STFT.
//! 3. Build the **target** envelope by warping the input's envelope by
//!    `formant_ratio` ([`SpectralEnvelope::warp`]).
//! 4. Multiply the pitch-shifted spectrum, frame by frame, by
//!    `exp(E_target(f) − E_observed(f))`, clamped to ±`max_gain_db` (30 dB by default).
//! 5. Overlap-add, then apply one global gain so the corrected signal has the
//!    **same RMS as the pitch-shifted signal**.
//!
//! The result keeps the pitch ratio's excitation and puts the formants at
//! `formant_ratio ×` their original positions. `pitch_ratio = 1` and
//! `formant_ratio = 1` is a bit-exact bypass (required by the P-08 null test).
//!
//! # Why post-correction (path b) and not integration into the vocoder (path a)
//!
//! Path (b) was chosen because it reuses P-02 **unchanged**, including its
//! per-frame pitch curve, and because it needs no change to the well-tested
//! stretch/resample loop. It also keeps one implementation of the formant rule
//! for both `formant_ratio` and pitch, rather than two code paths whose errors
//! would have to agree. The cost is one extra STFT/ISTFT pass (and two envelope
//! analyses) per render; the measured cost is reported in the P-03 report and
//! is small compared with the pitch shift itself. Path (a) would be faster and
//! would share the phase vocoder's frames, but it would have required
//! re-deriving the addition of excitation and envelope inside the phase-locking
//! loop.
//!
//! # The two windows (D19)
//!
//! There are **two independent analysis windows**, and they are chosen from
//! different signals:
//!
//! | Stage | Analyses | Window chosen from |
//! |---|---|---|
//! | [`crate::pitch::PitchShifter`] front end | the **input** | the input F0 — `N = 2048`, fixed ([`crate::pitch::WindowPolicy::Auto`]) |
//! | this correction pass | the **pitch-shifted output** | the shifted output F0 — 2048 normally, 4096 when it is low |
//!
//! The split is D19. The shifter realises its ratio in the read-out resampler,
//! so its analysis never sees the shifted-down signal and needs no widening
//! just because the requested output is low. This pass does analyse the shifted
//! output, so a low F0 here is exactly the case that needs the wider window.
//! Before D19 the correction pass inherited `PitchShifter::window_size()`,
//! which coupled it to the wrong signal in both directions.
//!
//! `N = 2048` resolves speech F0 down to roughly 90 Hz (3.8 bins per period);
//! below that the harmonic comb is too coarse for the 4 ms cepstral lifter to
//! separate cleanly. [`CorrectionWindowPolicy::Auto`] therefore measures the
//! shifted output's F0 and switches to `4096` when the measured median is below
//! [`WIDE_CORRECTION_F0_HZ`] (100 Hz — the 90 Hz resolution floor plus a margin
//! for the estimator). The estimate is a **proxy for window sizing only**; it
//! is never used to alter pitch, and a wrong window costs resolution or CPU, not
//! a sample of audio.
//!
//! The estimate is a **proxy**: a constant pitch ratio scales every frequency
//! exactly, so the shifted output's F0 is `ratio × input F0`. The proxy measures
//! the **input** F0 — always inside 2048's well-resolved 79–323 Hz band — and
//! multiplies by the ratio. That is far more reliable than trying to measure
//! 40–60 Hz with a 42.7 ms window, which is the very condition that needs the
//! wider window. Per frame it takes the normalised autocorrelation of the
//! frame's power spectrum (one inverse FFT per sampled frame, at most 64 frames
//! per render), corrects the periodic-Hann taper bias exactly, and picks the
//! peak in a 40–700 Hz lag band with parabolic interpolation. Frames below
//! −40 dB relative to the loudest analysed frame are skipped; the input-side
//! estimate is the **median** over the rest (D17/Q-01's medians-only rule), and
//! the reported proxy is that median times the ratio. On synthetic vowels with a
//! known F0 it is accurate to under 0.1%; on real speech it tracks the corpus
//! medians within a few Hz (`f0_proxy_matches_synthetic_and_fixture_medians`).
//! When the estimate is unavailable (silence, no periodic frames) the default
//! 2048 window is used. The proxy never alters audio: a wrong window costs
//! resolution or CPU, never a sample of gain.
//!
//! [`VoiceTransformer::correction_window_size`] reports the window of the last
//! render and [`VoiceTransformer::correction_f0_hz`] the proxy behind it;
//! [`VoiceTransformer::pitch_window_size`] reports the shifter's, which is
//! independent. Neither is [`crate::stft`] latency: the correction pass is
//! whole-buffer and has no streaming path.
//!
//! # Transients (P-04): shifter-only, and why
//!
//! The transient handling lives in [`crate::pitch::PitchShifter`] (onset
//! detection + region phase reset) and is therefore active in every render this
//! transformer makes, because step 1 is a pitch shift. The correction pass
//! (steps 2–4) deliberately adds **no** second reset: it applies a real,
//! non-negative gain to every bin and leaves each bin's phase untouched, so its
//! ISTFT is phase-coherent with its own analysis and cannot create a precursor.
//! Duplicating a reset there would reset phases that are already coherent and
//! would change the formant-corrected output for no measured gain. The measured
//! pre-echo therefore comes from the shifter alone: the table in
//! `pitch::tests::transients::transient_tables` renders both with the reset on
//! and off through this transformer, and the transformer moves in the same
//! direction and by the same amount as the shifter. The P-03 formant table is
//! unchanged by the reset (worst required cell still 7.3%).
//!
//! # Length, alignment and determinism
//!
//! [`VoiceTransformer::process`] and
//! [`VoiceTransformer::process_into`] return **exactly `input.len()` samples**
//! for any input length, including zero. The whole-buffer output is
//! sample-aligned with the input to within the analysis window's smear (the
//! same rule as P-02); there is no additional delay, because both the pitch
//! shifter and the correction operate on whole buffers. Everything is
//! deterministic — no RNG, clocks or threads.
//!
//! # Envelope for the robot chain (D10)
//!
//! [`VoiceTransformer::envelope`] exposes the target envelope used for the last
//! render. It is the object B-02 re-applies to a carrier: its frame indexing and
//! units are documented on [`SpectralEnvelope`]. It is empty after a bypass or
//! an empty input.
//!
//! # Known limits
//!
//! * **Constant pitch ratio only.** A per-frame pitch *curve* would move the
//!   input and output time axes relative to each other, so the envelope
//!   correction needs a time-mapped source envelope. B-04's monotone flattening
//!   drives the P-02 curve directly; combining a curve with formant correction
//!   is a follow-up, not part of P-03.
//! * **Extreme opposing ratios saturate.** Moving the formants *against* the
//!   pitch shift needs corrections beyond the ±30 dB clamp in the quiet
//!   valleys: measured on synthetic vowels, `pitch = +7 st, formant = 0.75`
//!   lands F1 at +46% and F2 at +104% of target, and `pitch = −7 st,
//!   formant = 1.33` lands F1 at −52% (and F2 at −37%). Two physical reasons,
//!   not a coding error: the target formant spacing becomes comparable to the
//!   shifted harmonic spacing (a 390 Hz formant gap at a 165 Hz F0), and the
//!   excitation carries no energy where a 2× upward formant move asks for it.
//!   The acceptance cells — formant-only, ±7 st with formant 1.0, and +7 st
//!   with formant 1.10 — all stay inside 8% on F1/F2. P-05's presets must stay
//!   out of the opposing corners. Raising `EnvelopeConfig::max_gain_db` to
//!   40 dB recovers the male `+7/0.75` corner (F1 −8.4%) but not the low-male
//!   one (the target formant spacing is below the harmonic spacing, so no
//!   envelope gain can resolve it), and it worsens the low-male `+7/1.0` cell
//!   from +6.9% to +8.1%; the default stays at 30 dB.
//! * **F3 is less accurate than F1/F2** at a large negative pitch shift
//!   (`−7 st, formant 1.0`: F3 −12%…−20%) because the target F3 is twice the
//!   observed one. The metric the product promises is F1/F2; F3 is reported for
//!   completeness.
//! * **Level is not the product's loudness policy.** The correction keeps the
//!   pitch-shifted signal's RMS; matching the *source* level and true-peak
//!   limiting are P-08's job ([`crate::loudness`], composed for natural mode by
//!   [`crate::params::NaturalRenderer`]). On the fixtures the worst crest
//!   increase is `af_heart` at `+7 st/1.10` (source peak 0.799 → 1.035).
//!
//! # Tilt (P-05)
//!
//! `tilt_db_per_oct` is a **per-bin gain on the target envelope**: bin `k` at
//! frequency `f_k` gets `tilt · log2(f_k / 1000 Hz)` dB added to the target
//! log-magnitude before the `exp(target − observed)` correction. That is the
//! same rule as [`SpectralEnvelope::warp`], one dimension simpler, and it means:
//!
//! * the curve is exactly `0 dB` at 1 kHz, `+tilt` an octave above and `−tilt`
//!   an octave below — a tilt, not a shelf;
//! * the pitch and formant axes are untouched, so `bright` (tilt only) moves no
//!   formant by construction;
//! * the exposed [`VoiceTransformer::envelope`] **includes** the tilt, because
//!   it is the target envelope of the render (B-02 re-applies whatever this
//!   reports);
//! * the global RMS normalisation at step 5 still matches the pitch-shifted
//!   level, so tilt changes spectral balance rather than loudness. P-08 owns
//!   the absolute level.
//!
//! The range is ±[`MAX_TILT_DB_PER_OCT`] dB/oct. At 48 kHz that is at most
//! ±22 dB between 60 Hz and 12 kHz, which is a strong but usable timbre trim.
//! A tilt-only point (`pitch = 1`, `formant = 1`, `tilt ≠ 0`) takes the
//! correction path rather than the bypass, so it costs one STFT/ISTFT; the
//! bit-exact bypass requires all three to be neutral.

use std::fmt;

use crate::envelope::{EnvelopeAnalyzer, EnvelopeConfig, EnvelopeError, SpectralEnvelope};
use crate::pitch::{PitchShiftConfig, PitchShiftError, PitchShifter};
use crate::stft::{Complex, RealFft, Spectrogram, Stft, StftConfig, StftError};

/// Lowest formant ratio accepted (PLAN D1 / P-03).
pub const MIN_FORMANT_RATIO: f32 = 0.75;
/// Highest formant ratio accepted (PLAN D1 / P-03).
pub const MAX_FORMANT_RATIO: f32 = 1.33;

/// Lowest spectral tilt accepted, in dB per octave (P-05).
pub const MIN_TILT_DB_PER_OCT: f32 = -6.0;
/// Highest spectral tilt accepted, in dB per octave (P-05).
pub const MAX_TILT_DB_PER_OCT: f32 = 6.0;
/// Frequency at which the tilt curve is exactly 0 dB (P-05).
pub const TILT_REFERENCE_HZ: f32 = 1000.0;

/// Fraction of the analysed signal's pitch period that caps the cepstral
/// lifter (P-05 fix for the P-03 envelope).
///
/// The lifter must exclude the pitch pulse at quefrency `1/F0`, so its cutoff
/// is capped at `0.75 / F0`. The cap is inactive for output F0 up to 187 Hz
/// (0.75 / 187 Hz = 4 ms, the P-03 default), which is why the P-03 formant
/// table — measured with 90/110 Hz vowels, output F0 ≤ 165 Hz — is unchanged.
/// Above that it shortens with F0: 3 ms at 250 Hz, 2.3 ms at 330 Hz.
pub const LIFTER_PERIOD_FRACTION: f32 = 0.75;
/// Guard frequency for the tilt curve: bin 0 (DC) would otherwise be `log2(0)`.
const TILT_MIN_HZ: f32 = 1.0;
/// Absolute bound on the per-bin tilt gain, in dB. With the ±6 dB/oct range this
/// never binds inside the audible band (20 Hz is −33.7 dB, 12 kHz +21.5 dB); it
/// exists so a future range change cannot produce an absurd envelope value.
const MAX_TILT_GAIN_DB: f32 = 40.0;
/// Nepers per decibel.
const NEPERS_PER_DB: f32 = 1.0 / 8.685_889;

/// Correction window for a shifted output in the normal F0 range (D19).
pub const DEFAULT_CORRECTION_WINDOW: usize = 2048;
/// Correction window used when the shifted output F0 is low (D19).
pub const WIDE_CORRECTION_WINDOW: usize = 4096;
/// Shifted-output F0 below which the correction pass widens its window.
///
/// D2 measures `N = 2048` as resolving F0 down to roughly 90 Hz; this sits one
/// estimator margin above that floor so a borderline estimate errs wide (more
/// resolution, never less).
pub const WIDE_CORRECTION_F0_HZ: f32 = 100.0;

/// Lag band of the window-sizing F0 proxy, in Hz.
const F0_PROXY_MIN_HZ: f32 = 40.0;
/// Upper lag-band edge. Well above the corpus's 323 Hz input and the ±12 st
/// output range's top, so the proxy is not band-limited by the product range.
const F0_PROXY_MAX_HZ: f32 = 700.0;
/// A frame must be within this power ratio of the loudest analysed frame.
const F0_PROXY_POWER_FLOOR: f32 = 1e-4;
/// At most this many frames are analysed (evenly spaced across the clip).
const F0_PROXY_MAX_FRAMES: usize = 64;
/// Minimum normalised autocorrelation peak for a frame to contribute.
const F0_PROXY_VOICING: f32 = 0.3;

/// How [`VoiceTransformer`] sizes the correction pass's analysis window (D19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CorrectionWindowPolicy {
    /// Estimate the **pitch-shifted output's** F0 and use
    /// [`WIDE_CORRECTION_WINDOW`] when the median is below
    /// [`WIDE_CORRECTION_F0_HZ`], else [`DEFAULT_CORRECTION_WINDOW`].
    ///
    /// The estimate is a window-sizing proxy only: it never changes audio.
    #[default]
    Auto,
    /// Always use this power-of-two window size (hop is always `N/4`).
    /// `2048` and `4096` are the intended choices; `1024` is allowed for
    /// experiments but resolves too little of the speech band.
    Fixed(usize),
}

/// Configuration for [`VoiceTransformer`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VoiceTransformConfig {
    /// Window policy for the pitch shifter (D19: 2048, chosen from the input).
    pub pitch: PitchShiftConfig,
    /// Envelope estimator and limits for the formant correction.
    pub envelope: EnvelopeConfig,
    /// Window policy for the correction pass (D19: chosen from the shifted
    /// output, independently of `pitch`).
    pub correction_window: CorrectionWindowPolicy,
}

impl VoiceTransformConfig {
    /// D2 defaults for pitch, the P-03 cepstral envelope defaults, and the
    /// D19 automatic correction window.
    pub const fn d2() -> Self {
        VoiceTransformConfig {
            pitch: PitchShiftConfig::auto(),
            envelope: EnvelopeConfig::cepstral(),
            correction_window: CorrectionWindowPolicy::Auto,
        }
    }
}

/// Errors from [`VoiceTransformer`]. Every caller-controlled failure is typed;
/// nothing here panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTransformError {
    /// A zero sample rate.
    ZeroSampleRate,
    /// The pitch shifter rejected its configuration.
    Pitch(PitchShiftError),
    /// The envelope stage rejected its configuration or a spectrogram.
    Envelope(EnvelopeError),
    /// The correction STFT rejected its geometry (only possible for an
    /// unsupported window size).
    Stft(StftError),
}

impl fmt::Display for VoiceTransformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VoiceTransformError::ZeroSampleRate => {
                f.write_str("sample rate must be non-zero for voice transformation")
            }
            VoiceTransformError::Pitch(err) => write!(f, "pitch shifter: {err}"),
            VoiceTransformError::Envelope(err) => write!(f, "envelope: {err}"),
            VoiceTransformError::Stft(err) => write!(f, "correction STFT: {err}"),
        }
    }
}

impl std::error::Error for VoiceTransformError {}

impl From<PitchShiftError> for VoiceTransformError {
    fn from(value: PitchShiftError) -> Self {
        VoiceTransformError::Pitch(value)
    }
}

impl From<EnvelopeError> for VoiceTransformError {
    fn from(value: EnvelopeError) -> Self {
        VoiceTransformError::Envelope(value)
    }
}

impl From<StftError> for VoiceTransformError {
    fn from(value: StftError) -> Self {
        VoiceTransformError::Stft(value)
    }
}

/// Window-sizing F0 proxy for the correction pass (D19).
///
/// This is deliberately not a general-purpose pitch tracker: it answers one
/// question — is the shifted output's typical F0 below
/// [`WIDE_CORRECTION_F0_HZ`]? — and never touches the audio. Per analysed frame
/// it takes the normalised autocorrelation of the frame's power spectrum (one
/// inverse real FFT), picks the highest peak in the 40–700 Hz lag band with
/// parabolic interpolation, and reports the **median** across up to
/// [`F0_PROXY_MAX_FRAMES`] evenly spaced, sufficiently loud frames.
struct F0Estimator {
    fft: RealFft,
    window_size: usize,
    /// Frame power spectrum, `N/2 + 1` real-valued bins.
    power: Vec<Complex>,
    /// Autocorrelation of the current frame, `N` samples.
    autocorrelation: Vec<f32>,
    /// Power of every frame in the analysed spectrogram.
    frame_power: Vec<f32>,
}

impl F0Estimator {
    fn new(window_size: usize) -> Self {
        F0Estimator {
            fft: RealFft::new(window_size),
            window_size,
            power: vec![Complex::ZERO; window_size / 2 + 1],
            autocorrelation: vec![0.0; window_size],
            frame_power: Vec::new(),
        }
    }

    fn reset(&mut self, window_size: usize) {
        if window_size == self.window_size {
            return;
        }
        *self = F0Estimator::new(window_size);
    }

    /// Median F0 over the loud, periodic frames of `spectrogram`, or `None`.
    fn estimate_median(&mut self, spectrogram: &Spectrogram, sample_rate: u32) -> Option<f32> {
        let frames = spectrogram.frames();
        let bins = spectrogram.bins();
        if sample_rate == 0 || frames == 0 || bins == 0 {
            return None;
        }
        self.reset(spectrogram.window_size());
        if bins != self.power.len() {
            self.power.resize(bins, Complex::ZERO);
        }

        self.frame_power.clear();
        self.frame_power.reserve(frames);
        let mut loudest = 0.0f32;
        for frame in spectrogram.frames_iter() {
            let mut power = 0.0f32;
            for bin in frame {
                power += bin.magnitude_sq();
            }
            self.frame_power.push(power);
            if power > loudest {
                loudest = power;
            }
        }
        if !loudest.is_finite() || loudest <= 0.0 {
            return None;
        }
        let floor = loudest * F0_PROXY_POWER_FLOOR;

        let bin_hz = sample_rate as f32 / self.window_size as f32;
        let first_bin = ((F0_PROXY_MIN_HZ * 0.5) / bin_hz).ceil() as usize;
        let skip_bins = first_bin.min(bins);

        let low_lag = (sample_rate as f32 / F0_PROXY_MAX_HZ).floor().max(2.0) as usize;
        let high_lag =
            ((sample_rate as f32 / F0_PROXY_MIN_HZ).ceil() as usize).min(self.window_size * 3 / 5);
        if high_lag <= low_lag + 2 {
            return None;
        }

        let stride = frames.div_ceil(F0_PROXY_MAX_FRAMES).max(1);
        let mut estimates = Vec::with_capacity(F0_PROXY_MAX_FRAMES);
        for frame in (0..frames).step_by(stride) {
            let Some(power) = self.frame_power.get(frame).copied() else {
                continue;
            };
            if power < floor {
                continue;
            }
            let Some(bins_frame) = spectrogram.frame(frame) else {
                continue;
            };
            for (index, (slot, bin)) in self.power.iter_mut().zip(bins_frame).enumerate() {
                *slot = if index < skip_bins {
                    Complex::ZERO
                } else {
                    Complex::new(bin.magnitude_sq(), 0.0)
                };
            }
            self.fft.inverse(&self.power, &mut self.autocorrelation);
            let zero_lag = self.autocorrelation[0];
            if !zero_lag.is_finite() || zero_lag <= 1e-20 {
                continue;
            }
            let Some(lag) = best_lag(&self.autocorrelation, low_lag, high_lag, self.window_size)
            else {
                continue;
            };
            let peak = corrected_correlation(&self.autocorrelation, lag, self.window_size);
            if peak / zero_lag < F0_PROXY_VOICING {
                continue;
            }
            let offset = parabolic_offset(
                corrected_correlation(&self.autocorrelation, lag - 1, self.window_size),
                peak,
                corrected_correlation(&self.autocorrelation, lag + 1, self.window_size),
            );
            let refined = lag as f32 + offset;
            if refined <= 0.0 {
                continue;
            }
            let f0 = sample_rate as f32 / refined;
            if f0.is_finite() && f0 > 0.0 {
                estimates.push(f0);
            }
        }
        median(&mut estimates)
    }
}

/// Taper bias of the circular autocorrelation of a periodic-Hann window at
/// `lag`, relative to lag zero.
///
/// The STFT frame is multiplied by a periodic Hann window, so its power-spectrum
/// autocorrelation is `Σ w[n]w[n+τ]x[n]x[n+τ]`, whose envelope falls with
/// `τ` and would bias the peak towards short lags (high F0). For a periodic Hann
/// the discrete sum is exactly `(N/8)(2 + cos(2πτ/N))`, so dividing by this
/// ratio removes the bias. Floored so a lag near `N/2` cannot blow up.
fn window_bias(lag: usize, window_size: usize) -> f32 {
    if window_size == 0 {
        return 1.0;
    }
    let angle = 2.0 * core::f64::consts::PI * lag as f64 / window_size as f64;
    let bias = ((2.0 + angle.cos()) / 3.0) as f32;
    bias.max(0.1)
}

fn corrected_correlation(autocorrelation: &[f32], lag: usize, window_size: usize) -> f32 {
    if lag == 0 || lag >= autocorrelation.len() {
        return 0.0;
    }
    autocorrelation[lag] / window_bias(lag, window_size)
}

/// Index of the highest taper-corrected autocorrelation in `[low, high]`.
fn best_lag(autocorrelation: &[f32], low: usize, high: usize, window_size: usize) -> Option<usize> {
    let high = high.min(autocorrelation.len().saturating_sub(2));
    if high <= low || low == 0 {
        return None;
    }
    let mut best = low;
    let mut best_value = f32::NEG_INFINITY;
    for lag in low..=high {
        let value = corrected_correlation(autocorrelation, lag, window_size);
        if value > best_value {
            best_value = value;
            best = lag;
        }
    }
    Some(best)
}

/// Parabolic peak offset in samples, clamped to half a sample.
fn parabolic_offset(left: f32, centre: f32, right: f32) -> f32 {
    let denominator = left - 2.0 * centre + right;
    if denominator.abs() <= 1e-12 || !denominator.is_finite() {
        return 0.0;
    }
    (0.5 * (left - right) / denominator).clamp(-0.5, 0.5)
}

/// Median of a finite-value list, averaging the two middle values when even.
fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let middle = values.len() / 2;
    let value = if values.len().is_multiple_of(2) {
        0.5 * (values[middle - 1] + values[middle])
    } else {
        values[middle]
    };
    value.is_finite().then_some(value)
}

/// Independent pitch and formant control (D1).
///
/// ```
/// use rasp_dsp::transform::{VoiceTransformConfig, VoiceTransformer};
///
/// let mut transformer = VoiceTransformer::new(48_000, VoiceTransformConfig::d2()).unwrap();
/// transformer.set_pitch_semitones(7.0);
/// transformer.set_formant_ratio(1.10);
/// let input = vec![0.0f32; 4800];
/// let output = transformer.process(&input).unwrap();
/// assert_eq!(output.len(), input.len());
/// ```
pub struct VoiceTransformer {
    sample_rate: u32,
    config: VoiceTransformConfig,
    pitch: PitchShifter,
    formant_ratio: f32,
    /// Spectral tilt in dB per octave, applied to the target envelope (P-05).
    tilt_db_per_oct: f32,
    analyzer: EnvelopeAnalyzer,
    /// STFT used for the correction pass. Its window is chosen from the
    /// **pitch-shifted output** via [`CorrectionWindowPolicy`] (D19) and is
    /// independent of the pitch shifter's window.
    stft: Stft,
    /// Window-sizing F0 proxy for the shifted output (D19).
    f0: F0Estimator,
    /// F0 proxy used for the most recent render, for reporting and tests.
    correction_f0: Option<f32>,
    /// Analysis of the input (target-envelope source).
    input_spectrogram: Spectrogram,
    /// Analysis of the pitch-shifted signal (what is corrected in place).
    shifted_spectrogram: Spectrogram,
    /// Scratch envelope of the input (later reused for the shifted/observed
    /// envelope, which is why it holds one analysis at a time).
    input_envelope: SpectralEnvelope,
    /// The target envelope of the last render; exposed for D10 / B-02.
    output_envelope: SpectralEnvelope,
    /// Per-bin tilt gains in nepers for the current correction geometry, rebuilt
    /// once per render (one `log2` per bin) and reused across frames.
    tilt_nepers: Vec<f32>,
    /// Pitch-shifted signal.
    pitch_output: Vec<f32>,
    /// Corrected signal.
    corrected: Vec<f32>,
}

impl fmt::Debug for VoiceTransformer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceTransformer")
            .field("sample_rate", &self.sample_rate)
            .field("config", &self.config)
            .field("pitch_ratio", &self.pitch.ratio())
            .field("formant_ratio", &self.formant_ratio)
            .field("tilt_db_per_oct", &self.tilt_db_per_oct)
            .field("pitch_window_size", &self.pitch.window_size())
            .field("correction_window_size", &self.stft.window_size())
            .field("correction_f0_hz", &self.correction_f0)
            .finish_non_exhaustive()
    }
}

impl VoiceTransformer {
    /// Build a transformer at `sample_rate`, bypassed (`pitch = formant = 1`).
    ///
    /// With [`CorrectionWindowPolicy::Auto`] the correction STFT starts at
    /// [`DEFAULT_CORRECTION_WINDOW`] and is resized per render from the shifted
    /// output's F0. A [`CorrectionWindowPolicy::Fixed`] size is validated here;
    /// an unsupported size returns [`VoiceTransformError::Stft`].
    pub fn new(
        sample_rate: u32,
        config: VoiceTransformConfig,
    ) -> Result<Self, VoiceTransformError> {
        if sample_rate == 0 {
            return Err(VoiceTransformError::ZeroSampleRate);
        }
        let pitch = PitchShifter::new(sample_rate, config.pitch)?;
        let analyzer = EnvelopeAnalyzer::new(sample_rate, config.envelope)?;
        // The two windows are independent (D19): this one is never derived from
        // `pitch.window_size()`, only from the correction policy.
        let window_size = match config.correction_window {
            CorrectionWindowPolicy::Auto => DEFAULT_CORRECTION_WINDOW,
            CorrectionWindowPolicy::Fixed(size) => size,
        };
        let stft = Stft::new(StftConfig::new(window_size, window_size / 4))?;
        let bins = stft.bins();
        Ok(VoiceTransformer {
            sample_rate,
            config,
            pitch,
            formant_ratio: 1.0,
            tilt_db_per_oct: 0.0,
            analyzer,
            stft,
            f0: F0Estimator::new(window_size),
            correction_f0: None,
            input_spectrogram: Spectrogram::new(0, bins, window_size / 4, window_size),
            shifted_spectrogram: Spectrogram::new(0, bins, window_size / 4, window_size),
            input_envelope: SpectralEnvelope::new(
                0,
                bins,
                window_size / 4,
                window_size,
                sample_rate,
            ),
            output_envelope: SpectralEnvelope::new(
                0,
                bins,
                window_size / 4,
                window_size,
                sample_rate,
            ),
            tilt_nepers: Vec::new(),
            pitch_output: Vec::new(),
            corrected: Vec::new(),
        })
    }

    /// The effective configuration in use (envelope fields as sanitised by the
    /// analyzer).
    pub fn config(&self) -> VoiceTransformConfig {
        VoiceTransformConfig {
            pitch: self.config.pitch,
            envelope: self.analyzer.config(),
            correction_window: self.config.correction_window,
        }
    }

    /// The sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The current pitch ratio.
    pub fn pitch_ratio(&self) -> f32 {
        self.pitch.ratio()
    }

    /// The current formant ratio.
    pub fn formant_ratio(&self) -> f32 {
        self.formant_ratio
    }

    /// Set the pitch ratio, clamped by the pitch shifter to
    /// [`crate::pitch::MIN_RATIO`]..=[`crate::pitch::MAX_RATIO`].
    pub fn set_pitch_ratio(&mut self, ratio: f32) {
        self.pitch.set_ratio(ratio);
    }

    /// Set the pitch shift in semitones: `ratio = 2^(semitones / 12)`, clamped
    /// by the pitch shifter to its supported ratio range.
    pub fn set_pitch_semitones(&mut self, semitones: f32) {
        let ratio = if semitones.is_finite() {
            2.0f32.powf(semitones / 12.0)
        } else {
            1.0
        };
        self.pitch.set_ratio(ratio);
    }

    /// Set the formant ratio, clamped to
    /// [`MIN_FORMANT_RATIO`]..=[`MAX_FORMANT_RATIO`]. Non-finite values become
    /// `1.0` (a bypass for that axis).
    pub fn set_formant_ratio(&mut self, ratio: f32) {
        self.formant_ratio = if ratio.is_finite() {
            ratio.clamp(MIN_FORMANT_RATIO, MAX_FORMANT_RATIO)
        } else {
            1.0
        };
    }

    /// The supported formant ratio range, inclusive.
    pub fn formant_limits() -> (f32, f32) {
        (MIN_FORMANT_RATIO, MAX_FORMANT_RATIO)
    }

    /// The current spectral tilt in dB per octave (P-05).
    pub fn tilt_db_per_oct(&self) -> f32 {
        self.tilt_db_per_oct
    }

    /// Set the spectral tilt in dB per octave, clamped to
    /// [`MIN_TILT_DB_PER_OCT`]..=[`MAX_TILT_DB_PER_OCT`]. Non-finite values
    /// become `0.0`.
    ///
    /// The gain is `tilt · log2(f / 1000 Hz)` dB per bin, added to the **target
    /// envelope**. `0.0` restores the exact pre-P-05 behaviour; with pitch,
    /// formant and tilt all neutral the render is a bit-exact bypass.
    pub fn set_tilt_db_per_oct(&mut self, tilt_db_per_oct: f32) {
        self.tilt_db_per_oct = if tilt_db_per_oct.is_finite() {
            tilt_db_per_oct.clamp(MIN_TILT_DB_PER_OCT, MAX_TILT_DB_PER_OCT)
        } else {
            0.0
        };
    }

    /// The supported tilt range, inclusive, in dB per octave.
    pub fn tilt_limits() -> (f32, f32) {
        (MIN_TILT_DB_PER_OCT, MAX_TILT_DB_PER_OCT)
    }

    /// Window size `N` the pitch shifter uses.
    ///
    /// This is chosen from the **input** signal (D19) and is therefore 2048
    /// for [`PitchShiftConfig::auto`] at every ratio. It does **not** describe
    /// the correction pass; use
    /// [`correction_window_size`](VoiceTransformer::correction_window_size).
    pub fn pitch_window_size(&self) -> usize {
        self.pitch.window_size()
    }

    /// Window size `N` the correction pass used in the most recent render.
    ///
    /// Chosen from the **pitch-shifted output's** F0 (D19): 2048 normally,
    /// 4096 when [`correction_f0_hz`](VoiceTransformer::correction_f0_hz) is
    /// below [`WIDE_CORRECTION_F0_HZ`]. Before the first render this reports
    /// the configured default or [`CorrectionWindowPolicy::Fixed`] size.
    pub fn correction_window_size(&self) -> usize {
        self.stft.window_size()
    }

    /// The F0 proxy (Hz) that sized the correction window in the most recent
    /// render: the measured input F0 times the current pitch ratio, i.e. the
    /// shifted output's expected F0.
    ///
    /// `None` for [`CorrectionWindowPolicy::Fixed`], a bypass, an empty input,
    /// or a signal with no loud periodic frames. The value is a window-sizing
    /// proxy — a median of per-frame autocorrelation estimates — not a
    /// measurement product, and it never alters audio (the adaptive lifter uses
    /// it to choose a cutoff, never to change pitch or gain).
    pub fn correction_f0_hz(&self) -> Option<f32> {
        self.correction_f0
    }

    /// The cepstral lifter cutoff the last analysis used, in seconds.
    ///
    /// The configured value, shortened when the shifted output's F0 is high
    /// enough that the pitch pulse would otherwise enter the envelope
    /// ([`LIFTER_PERIOD_FRACTION`]). Reporting is diagnostic; the value changes
    /// nothing about pitch or level.
    pub fn effective_lifter_seconds(&self) -> f32 {
        self.analyzer.config().lifter_seconds
    }

    /// Streaming lookahead in samples: exactly the pitch shifter's `N`.
    ///
    /// The whole-buffer API is sample-aligned and needs no compensation; this is
    /// the lookahead a streaming producer would need. The correction pass is
    /// whole-buffer (path b) and contributes no additional streaming latency;
    /// its window is independent of the shifter's (D19).
    pub fn latency_samples(&self) -> usize {
        self.pitch.latency_samples()
    }

    /// [`latency_samples`](VoiceTransformer::latency_samples) in seconds.
    pub fn latency_seconds(&self) -> f32 {
        self.pitch.latency_seconds()
    }

    /// The target envelope of the last non-bypass render, in `ln|E|` per bin,
    /// frame-aligned with the output. Empty after a bypass or empty input.
    pub fn envelope(&self) -> &SpectralEnvelope {
        &self.output_envelope
    }

    /// Render a whole buffer, returning exactly `input.len()` samples.
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>, VoiceTransformError> {
        let mut output = Vec::with_capacity(input.len());
        self.process_into(input, &mut output)?;
        Ok(output)
    }

    /// Render a whole buffer into an existing vector, reusing its allocation.
    ///
    /// Clears `output` first. The result is exactly `input.len()` samples.
    pub fn process_into(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<(), VoiceTransformError> {
        output.clear();
        self.output_envelope.clear();
        self.correction_f0 = None;
        if input.is_empty() {
            return Ok(());
        }

        let pitch_bypassed = (self.pitch.ratio() - 1.0).abs() <= 1e-9;
        let formant_bypassed = (self.formant_ratio - 1.0).abs() <= 1e-9;
        let tilt_bypassed = self.tilt_db_per_oct == 0.0;
        if pitch_bypassed && formant_bypassed && tilt_bypassed {
            output.extend_from_slice(input);
            return Ok(());
        }

        // 1. Pitch shift (or a plain copy when the pitch axis is bypassed).
        //    The shifter sizes its own window from the input (D19).
        if pitch_bypassed {
            self.pitch_output.clear();
            self.pitch_output.extend_from_slice(input);
        } else {
            // P-02b: the pitch render path is fallible and its error is typed,
            // so it propagates instead of panicking across this boundary.
            self.pitch.process_into(input, &mut self.pitch_output)?;
        }

        // 2. Size the correction window from the signal this pass analyses: the
        //    shifted output, NOT the shifter's window (D19). Under a constant
        //    ratio that output's F0 is exactly `ratio × input F0`, so the input
        //    — always inside 2048's well-resolved 79–323 Hz band — is measured
        //    once and scaled. Only a low shifted F0 pays for a re-analysis.
        let window_size = self.choose_correction_window(input)?;
        self.ensure_stft(window_size)?;
        // 2b. Keep the cepstral lifter below the analysed signal's pitch period.
        //     A 4 ms lifter at 300 Hz F0 passes the pitch pulse into the
        //     "envelope", and the correction then chases harmonics: measured
        //     −4.7…−8.9 dB HNR on high female fixtures.
        self.apply_adaptive_lifter();
        let frames = self.stft.frame_count(input.len());

        // Step 2 left the input analysed at the final size in the Auto case.
        if matches!(
            self.config.correction_window,
            CorrectionWindowPolicy::Fixed(_)
        ) {
            self.stft.analyze_into(input, &mut self.input_spectrogram);
        }
        if !pitch_bypassed {
            self.stft
                .analyze_into(&self.pitch_output, &mut self.shifted_spectrogram);
        }

        // 3. Analyse the input envelope and build the target.
        self.analyzer
            .analyze_into(&self.input_spectrogram, &mut self.input_envelope)?;
        self.input_envelope
            .warp_into(self.formant_ratio, &mut self.output_envelope);
        // 3b. Tilt is one more per-bin gain on the same target envelope (P-05).
        self.apply_tilt();

        if pitch_bypassed {
            // The "shifted" signal is the input itself: one analysis serves both
            // roles, and the observed envelope is the input envelope in hand.
            apply_correction(
                frames,
                &mut self.input_spectrogram,
                &self.output_envelope,
                &self.input_envelope,
                self.analyzer.config().max_gain_neper(),
            );
            self.stft
                .synthesize_into(&self.input_spectrogram, input.len(), &mut self.corrected)?;
        } else {
            // The shifted output was analysed at the chosen window above; this
            // pass only adds its envelope.
            self.analyzer
                .analyze_into(&self.shifted_spectrogram, &mut self.input_envelope)?;
            apply_correction(
                frames,
                &mut self.shifted_spectrogram,
                &self.output_envelope,
                &self.input_envelope,
                self.analyzer.config().max_gain_neper(),
            );
            self.stft.synthesize_into(
                &self.shifted_spectrogram,
                input.len(),
                &mut self.corrected,
            )?;
        }

        // 5. Keep the pitch-shifted level: the correction redistributes energy
        //    between bins and would otherwise change loudness with the formant
        //    ratio. One global gain, so the formant pattern is untouched.
        if self.corrected.len() == self.pitch_output.len() {
            normalise_rms(&mut self.corrected, &self.pitch_output);
        }

        output.extend_from_slice(&self.corrected);
        if output.len() < input.len() {
            output.resize(input.len(), 0.0);
        }
        output.truncate(input.len());
        Ok(())
    }

    /// Choose the correction window from the **pitch-shifted output** (D19).
    ///
    /// A constant pitch ratio scales every frequency exactly, so the shifted
    /// output's F0 is `ratio × input F0`. The input is measured (2048 resolves
    /// its 79–323 Hz band), the product is the proxy for the output, and the
    /// wide window is chosen when that proxy is below
    /// [`WIDE_CORRECTION_F0_HZ`]. This avoids trying to measure 40–60 Hz with a
    /// 42.7 ms window — the very condition that needs the wide window.
    ///
    /// In the Auto case this leaves `input_spectrogram` analysed at the chosen
    /// window; a [`CorrectionWindowPolicy::Fixed`] policy performs no analysis.
    fn choose_correction_window(&mut self, input: &[f32]) -> Result<usize, VoiceTransformError> {
        match self.config.correction_window {
            CorrectionWindowPolicy::Fixed(size) => {
                self.correction_f0 = None;
                Ok(size)
            }
            CorrectionWindowPolicy::Auto => {
                self.ensure_stft(DEFAULT_CORRECTION_WINDOW)?;
                self.stft.analyze_into(input, &mut self.input_spectrogram);
                let input_f0 = self
                    .f0
                    .estimate_median(&self.input_spectrogram, self.sample_rate);
                let shifted_f0 = input_f0.map(|f0| f0 * self.pitch.ratio());
                self.correction_f0 = shifted_f0;
                if shifted_f0.is_some_and(|value| value < WIDE_CORRECTION_F0_HZ) {
                    self.ensure_stft(WIDE_CORRECTION_WINDOW)?;
                    self.stft.analyze_into(input, &mut self.input_spectrogram);
                    Ok(WIDE_CORRECTION_WINDOW)
                } else {
                    Ok(DEFAULT_CORRECTION_WINDOW)
                }
            }
        }
    }

    /// Resize the correction STFT and its matching F0 proxy, keeping both in
    /// step. Allocation happens only when the window changes, not per frame.
    fn ensure_stft(&mut self, window_size: usize) -> Result<(), VoiceTransformError> {
        if self.stft.window_size() == window_size {
            return Ok(());
        }
        self.stft = Stft::new(StftConfig::new(window_size, window_size / 4))?;
        self.f0 = F0Estimator::new(window_size);
        Ok(())
    }

    /// Keep the cepstral lifter below the analysed signal's pitch period.
    ///
    /// The correction pass analyses the **shifted output**, whose F0 is already
    /// known from the D19 window proxy. The lifter is capped at
    /// [`LIFTER_PERIOD_FRACTION`] / F0 so the cepstral peak at `1/F0` never
    /// enters the envelope. For output F0 ≤ 187 Hz the cap is above the
    /// configured 4 ms and nothing changes, which is why P-03's table is
    /// unaffected; above that the envelope gets smoother as F0 rises, which is
    /// the only resolution that exists there anyway.
    ///
    /// When the F0 proxy is unavailable (silence, or a fixed window policy that
    /// skips the estimator) the configured lifter is kept.
    fn apply_adaptive_lifter(&mut self) {
        let configured = self.config.envelope.lifter_seconds;
        let lifter = match self.correction_f0 {
            Some(f0) if f0.is_finite() && f0 > 0.0 => configured.min(LIFTER_PERIOD_FRACTION / f0),
            _ => configured,
        };
        self.analyzer.set_lifter_seconds(lifter);
    }

    /// Add the P-05 tilt curve to the target envelope, one value per bin.
    ///
    /// The curve is `tilt · log2(f / 1 kHz)` dB, evaluated at each **output**
    /// bin centre, so the result is independent of the pitch ratio: the
    /// envelope has already been warped into the output's frequency frame.
    /// This is a documented per-bin gain on the envelope, not an extra filter
    /// stage, so it adds no phase shift and no second STFT.
    fn apply_tilt(&mut self) {
        if self.tilt_db_per_oct == 0.0 {
            return;
        }
        let bins = self.output_envelope.bins();
        if bins == 0 {
            return;
        }
        let window_size = self.output_envelope.window_size();
        let sample_rate = self.output_envelope.sample_rate();
        if window_size == 0 || sample_rate == 0 {
            return;
        }
        self.tilt_nepers.clear();
        self.tilt_nepers.reserve(bins);
        let bin_hz = sample_rate as f32 / window_size as f32;
        for bin in 0..bins {
            let hz = (bin as f32 * bin_hz).max(TILT_MIN_HZ);
            let db = (self.tilt_db_per_oct * (hz / TILT_REFERENCE_HZ).log2())
                .clamp(-MAX_TILT_GAIN_DB, MAX_TILT_GAIN_DB);
            self.tilt_nepers.push(db * NEPERS_PER_DB);
        }
        for frame in 0..self.output_envelope.frames() {
            let Some(values) = self.output_envelope.frame_mut(frame) else {
                break;
            };
            for (value, &gain) in values.iter_mut().zip(&self.tilt_nepers) {
                *value += gain;
            }
        }
    }
}

/// Multiply every frame by `exp(target − observed)`, clamped to
/// `±limit` nepers. Frames present in only one analysis stop the loop, which
/// cannot happen when both signals have the same length.
fn apply_correction(
    frames: usize,
    spectrogram: &mut Spectrogram,
    target: &SpectralEnvelope,
    observed: &SpectralEnvelope,
    limit: f32,
) {
    for frame in 0..frames {
        let Some(target_frame) = target.frame(frame) else {
            break;
        };
        let Some(observed_frame) = observed.frame(frame) else {
            break;
        };
        let Some(bins) = spectrogram.frame_mut(frame) else {
            break;
        };
        for ((bin, &target_value), &observed_value) in
            bins.iter_mut().zip(target_frame).zip(observed_frame)
        {
            let correction = (target_value - observed_value).clamp(-limit, limit);
            *bin = bin.scale((correction as f64).exp() as f32);
        }
    }
}

/// Scale `signal` so its RMS equals `reference`'s, leaving it untouched when
/// either RMS is negligible or non-finite.
fn normalise_rms(signal: &mut [f32], reference: &[f32]) {
    let rms = |values: &[f32]| -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let sum: f64 = values.iter().map(|&x| (x as f64) * (x as f64)).sum();
        (sum / values.len() as f64).sqrt()
    };
    let reference_rms = rms(reference);
    let signal_rms = rms(signal);
    if !reference_rms.is_finite() || !signal_rms.is_finite() || signal_rms < 1e-12 {
        return;
    }
    let gain = (reference_rms / signal_rms) as f32;
    if !gain.is_finite() {
        return;
    }
    for value in signal.iter_mut() {
        *value *= gain;
    }
}

#[cfg(test)]
mod tests;
