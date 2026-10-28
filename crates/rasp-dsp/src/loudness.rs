//! Loudness policy: RMS match to the source, then a true-peak limiter (P-08).
//!
//! A natural-mode render changes level in two ways: the phase vocoder's
//! stretch/resample changes the crest factor, and the formant correction moves
//! energy between bins. Neither is a loudness decision, so the chain ends with a
//! deliberate policy:
//!
//! ```text
//! processed ──► RMS match to source ──► true-peak limiter (≤ −1 dBTP) ──► out
//! ```
//!
//! The stage is [`LoudnessStage`]; [`crate::params::NaturalRenderer`] calls it
//! as its last step, and D8's robot chain will call the same stage after its own
//! processing (it is exposed, not wired, in this task).
//!
//! # Why whole-clip RMS, not short-term or LUFS
//!
//! The product renders **0.4–2.2 s one-shots**. BS.1770 (LUFS) needs 400 ms
//! blocks at a 100 ms hop and a relative gate: on a 0.4 s clip that is one or
//! two blocks, so the relative gate is ill-defined and the number is really a
//! 400 ms short-term loudness that moves with the phrase. A "short-term" window
//! of 3 s is longer than every clip in the corpus, so it degenerates to the
//! whole clip anyway.
//!
//! The policy is therefore **mean-square over the whole clip**:
//!
//! ```text
//! RMS = sqrt( (1/N) · Σ x[n]² )      gain = RMS_source / RMS_processed
//! ```
//!
//! This is fair for these clips because the transform **preserves length and
//! silence**: the source and the render contain the same number of samples and
//! the same leading/trailing silence, so the comparison is like-for-like and
//! the 11.7% of the corpus that is silence contributes the same zero energy on
//! both sides. It is also deterministic and trivially reproducible, which is
//! what the A/B gate (< 1 dB jump) needs. K-weighting was rejected because it
//! would make the measured level depend on the very spectral change the shift
//! is for — a preset that brightens the voice would be judged quieter for a
//! perceptual reason the player is not comparing.
//!
//! # The limiter
//!
//! * **True peak, not sample peak.** Inter-sample peaks are measured by
//!   band-limited interpolation to [`TRUE_PEAK_OVERSAMPLE`]× the sample rate
//!   with a windowed-sinc kernel ([`TruePeakMeter`]). A 12 kHz sine at 45° phase
//!   has samples at −3.01 dBFS and a true peak of 0 dBFS; a sample-peak limiter
//!   cannot see that, and the test suite pins it.
//! * **Ceiling −1 dBTP** ([`DEFAULT_CEILING_DBTP`]), with an internal margin of
//!   0.5 dB ([`DEFAULT_INTERNAL_MARGIN_DB`]) so the published gate has room for
//!   interpolation/estimator error. The limiter targets −1.5 dBTP internally and
//!   the report states both measurements.
//! * **Transparent below threshold.** If the post-RMS-match true peak is at or
//!   below the published ceiling the stage returns the signal **bit-exactly**
//!   and reports `limited = false`. No gain envelope is computed at all.
//! * **Attack/release, no pumping.** Gain reduction is pre-computed with a
//!   backward linear-attack pass (5 ms to traverse full scale) and a forward
//!   linear-release pass (100 ms). Because each sample's gain is the minimum of
//!   its own requirement and a slow rise, the envelope can never overshoot the
//!   requirement, and a single plosive cannot modulate the next syllable.
//! * **Look-ahead.** The backward pass starts the descent early by exactly the
//!   attack length, so the gain is already down when the peak arrives. There is
//!   no separate look-ahead delay: the whole stage is applied to a complete
//!   clip, and sample alignment is preserved (P-08 inherits P-02/P-03's
//!   alignment).
//!
//! # No panics, no dependencies
//!
//! Every configuration field is clamped or replaced at construction. An empty
//! signal, an all-zero signal, a zero sample rate, non-finite input and NaN
//! samples all produce documented finite behaviour. The interpolation kernel is
//! precomputed once per stage, so a render allocates nothing per sample.

use core::f64::consts::PI as PI_F64;
use std::fmt;

/// True-peak measurement oversampling factor (BS.1770 uses 4×).
pub const TRUE_PEAK_OVERSAMPLE: usize = 4;
/// Published true-peak ceiling, in dBTP.
pub const DEFAULT_CEILING_DBTP: f32 = -1.0;
/// Internal safety margin below the published ceiling, in dB.
pub const DEFAULT_INTERNAL_MARGIN_DB: f32 = 0.5;
/// Gain-reduction attack: samples needed to traverse full scale (0 → 1 gain).
pub const DEFAULT_ATTACK_SECONDS: f32 = 0.005;
/// Gain-reduction release time constant for the linear rise back to unity.
pub const DEFAULT_RELEASE_SECONDS: f32 = 0.100;
/// Lowest accepted published ceiling, in dBTP. Below this a "no overs" claim
/// would be about inaudibly quiet files, not a master.
pub const MIN_CEILING_DBTP: f32 = -12.0;
/// Highest accepted published ceiling, in dBTP (0 dBTP = full scale).
pub const MAX_CEILING_DBTP: f32 = 0.0;
/// Absolute floor used by the dB helpers, so silence reports a finite number
/// rather than `-inf`.
pub const DB_FLOOR: f32 = 1e-12;

/// Half-length of the true-peak interpolation kernel, in input samples.
///
/// 16 either side (33 taps) at 4× gives image rejection far below the 0.5 dB
/// margin the limiter keeps; the control tests measure a pure sine to within
/// ±0.05 dB from 50 Hz to 18 kHz.
const TP_HALF_TAPS: usize = 16;
/// Taps per interpolation phase.
const TP_TAPS: usize = 2 * TP_HALF_TAPS + 1;

/// A 4× band-limited true-peak meter (P-08).
///
/// The interpolation kernel is a Blackman-windowed sinc evaluated at the three
/// fractional phases (`¼`, `½`, `¾`) between input samples; phase 0 is the
/// sample itself. Each phase's coefficients are DC-normalised so a constant
/// signal interpolates to itself exactly.
///
/// Accuracy is a **controlled** claim: `sine_true_peak_control` measures a pure
/// sine (whose true peak is exactly its amplitude) from 50 Hz to 18 kHz and the
/// `quarter_rate_sine_overs` test pins the classic inter-sample overload.
#[derive(Debug, Clone)]
pub struct TruePeakMeter {
    phases: [[f32; TP_TAPS]; TRUE_PEAK_OVERSAMPLE],
}

impl Default for TruePeakMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl TruePeakMeter {
    /// Build the interpolation kernel.
    pub fn new() -> Self {
        let mut phases = [[0.0f32; TP_TAPS]; TRUE_PEAK_OVERSAMPLE];
        for (phase, coeffs) in phases.iter_mut().enumerate() {
            *coeffs = phase_coefficients(phase);
        }
        TruePeakMeter { phases }
    }

    /// True peak of `signal`, in linear amplitude.
    ///
    /// Non-finite samples are ignored (treated as zero) and an empty signal
    /// reports `0.0`. The result is never below the sample peak.
    pub fn measure(&self, signal: &[f32]) -> f32 {
        if signal.is_empty() {
            return 0.0;
        }
        let mut peak = 0.0f32;
        for &sample in signal {
            let value = sample.abs();
            if value.is_finite() && value > peak {
                peak = value;
            }
        }
        for index in 0..signal.len() {
            for coeffs in self.phases.iter().skip(1) {
                let value = self.interpolate(signal, index, coeffs).abs();
                if value.is_finite() && value > peak {
                    peak = value;
                }
            }
        }
        peak
    }

    /// Per-sample true-peak envelope into `out`, i.e. for each sample the
    /// largest absolute interpolated value in the following quarter-sample
    /// positions. `out.len() == signal.len()` afterwards.
    pub fn envelope_into(&self, signal: &[f32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(signal.len());
        for index in 0..signal.len() {
            let sample = signal[index];
            let mut peak = if sample.is_finite() {
                sample.abs()
            } else {
                0.0
            };
            for coeffs in self.phases.iter().skip(1) {
                let value = self.interpolate(signal, index, coeffs).abs();
                if value.is_finite() && value > peak {
                    peak = value;
                }
            }
            out.push(peak);
        }
    }

    /// Interpolate between `index` and `index + 1` with one phase's kernel.
    fn interpolate(&self, signal: &[f32], index: usize, coeffs: &[f32; TP_TAPS]) -> f32 {
        let mut acc = 0.0f64;
        let base = index as isize - TP_HALF_TAPS as isize;
        for (tap, &coefficient) in coeffs.iter().enumerate() {
            let position = base + tap as isize;
            if position >= 0 {
                let position = position as usize;
                if position < signal.len() {
                    let sample = signal[position];
                    if sample.is_finite() {
                        acc += sample as f64 * coefficient as f64;
                    }
                }
            }
        }
        acc as f32
    }
}

/// Coefficients for the fractional phase `phase / TRUE_PEAK_OVERSAMPLE`.
fn phase_coefficients(phase: usize) -> [f32; TP_TAPS] {
    let fraction = phase as f64 / TRUE_PEAK_OVERSAMPLE as f64;
    let mut coeffs = [0.0f32; TP_TAPS];
    let mut sum = 0.0f64;
    let half = TP_HALF_TAPS as f64;
    for (tap, slot) in coeffs.iter_mut().enumerate() {
        let offset = tap as f64 - half - fraction;
        let distance = offset.abs();
        if distance <= half + 1.0 {
            let angle = PI_F64 * offset / (half + 1.0);
            let window = 0.42 + 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos();
            let value = sinc(offset) * window;
            *slot = value as f32;
            sum += value;
        }
    }
    if sum.abs() > 1e-9 {
        for slot in &mut coeffs {
            *slot = (*slot as f64 / sum) as f32;
        }
    }
    coeffs
}

/// Normalised sinc: `sin(πx)/(πx)`, with the removable singularity at 0 = 1.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let pix = PI_F64 * x;
        pix.sin() / pix
    }
}

/// Root-mean-square of `signal` over every sample, including silence.
///
/// This is the documented P-08 level measure: the whole clip, because a clip is
/// one utterance and the transform preserves its length and silence.
pub fn rms(signal: &[f32]) -> f32 {
    if signal.is_empty() {
        return 0.0;
    }
    let sum: f64 = signal
        .iter()
        .map(|&sample| {
            let value = if sample.is_finite() {
                sample as f64
            } else {
                0.0
            };
            value * value
        })
        .sum();
    ((sum / signal.len() as f64).sqrt() as f32).max(0.0)
}

/// [`rms`] in dBFS, floored at [`DB_FLOOR`].
pub fn rms_dbfs(signal: &[f32]) -> f32 {
    dbfs(rms(signal))
}

/// Largest absolute sample, ignoring non-finite values.
pub fn sample_peak(signal: &[f32]) -> f32 {
    let mut peak = 0.0f32;
    for &sample in signal {
        let value = sample.abs();
        if value.is_finite() && value > peak {
            peak = value;
        }
    }
    peak
}

/// True peak of `signal` in linear amplitude (4× oversampling).
pub fn true_peak(signal: &[f32]) -> f32 {
    TruePeakMeter::new().measure(signal)
}

/// True peak of `signal` in dBTP, floored at [`DB_FLOOR`].
pub fn true_peak_dbtp(signal: &[f32]) -> f32 {
    dbtp(true_peak(signal))
}

/// `20 log10(value)`, floored so a zero — or a non-finite value — reports a
/// finite number rather than `-inf`.
pub fn dbfs(value: f32) -> f32 {
    if value.is_finite() {
        20.0 * value.max(DB_FLOOR).log10()
    } else {
        20.0 * DB_FLOOR.log10()
    }
}

/// Alias of [`dbfs`] with the dBTP name, so reports read correctly.
pub fn dbtp(value: f32) -> f32 {
    dbfs(value)
}

/// Scale `signal` so its whole-clip RMS equals `source`'s, returning the
/// applied gain in dB.
///
/// Does nothing (and returns `0.0`) when either RMS is negligible or
/// non-finite, or when the required gain would be non-finite. This is the exact
/// rule [`LoudnessConfig::match_rms`] applies; it is public so the robot chain
/// can use it without constructing a whole stage.
pub fn match_rms_to(signal: &mut [f32], source: &[f32]) -> f32 {
    let target = rms(source);
    let current = rms(signal);
    if !target.is_finite() || !current.is_finite() || target < DB_FLOOR || current < DB_FLOOR {
        return 0.0;
    }
    let gain = target / current;
    if !gain.is_finite() || gain <= 0.0 {
        return 0.0;
    }
    for sample in signal.iter_mut() {
        *sample *= gain;
    }
    20.0 * gain.log10()
}

/// Configuration for [`LoudnessStage`].
///
/// Every field is sanitised by [`sanitised`](LoudnessConfig::sanitised) at
/// construction; out-of-range values are clamped and non-finite values fall
/// back to the default for that field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessConfig {
    /// Match the processed clip's whole-clip RMS to the source's. On by default.
    pub match_rms: bool,
    /// Published true-peak ceiling in dBTP, clamped to
    /// [`MIN_CEILING_DBTP`]..=[`MAX_CEILING_DBTP`]. Default −1.0.
    pub ceiling_dbtp: f32,
    /// Extra margin below `ceiling_dbtp` that the limiter targets, in dB,
    /// clamped to `0..=6`. Default 0.5.
    pub internal_margin_db: f32,
    /// Attack length in seconds (time to traverse full-scale gain reduction),
    /// clamped to `0..=0.1`. Default 5 ms.
    pub attack_seconds: f32,
    /// Release time in seconds for a full-scale gain recovery, clamped to
    /// `0..=2`. Default 100 ms.
    pub release_seconds: f32,
}

impl Default for LoudnessConfig {
    fn default() -> Self {
        LoudnessConfig {
            match_rms: true,
            ceiling_dbtp: DEFAULT_CEILING_DBTP,
            internal_margin_db: DEFAULT_INTERNAL_MARGIN_DB,
            attack_seconds: DEFAULT_ATTACK_SECONDS,
            release_seconds: DEFAULT_RELEASE_SECONDS,
        }
    }
}

impl LoudnessConfig {
    /// The P-08 defaults: RMS match on, −1 dBTP ceiling, 0.5 dB margin,
    /// 5 ms attack, 100 ms release.
    pub const fn standard() -> Self {
        LoudnessConfig {
            match_rms: true,
            ceiling_dbtp: DEFAULT_CEILING_DBTP,
            internal_margin_db: DEFAULT_INTERNAL_MARGIN_DB,
            attack_seconds: DEFAULT_ATTACK_SECONDS,
            release_seconds: DEFAULT_RELEASE_SECONDS,
        }
    }

    /// Clamp every field into its supported range.
    pub fn sanitised(self) -> Self {
        LoudnessConfig {
            match_rms: self.match_rms,
            ceiling_dbtp: clamp_or(
                self.ceiling_dbtp,
                MIN_CEILING_DBTP,
                MAX_CEILING_DBTP,
                DEFAULT_CEILING_DBTP,
            ),
            internal_margin_db: clamp_or(
                self.internal_margin_db,
                0.0,
                6.0,
                DEFAULT_INTERNAL_MARGIN_DB,
            ),
            attack_seconds: clamp_or(self.attack_seconds, 0.0, 0.1, DEFAULT_ATTACK_SECONDS),
            release_seconds: clamp_or(self.release_seconds, 0.0, 2.0, DEFAULT_RELEASE_SECONDS),
        }
    }
}

/// Clamp a finite value, substituting `fallback` for a non-finite one.
fn clamp_or(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

/// Errors from [`LoudnessStage`]. The only caller-controlled failure is a zero
/// sample rate; nothing panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoudnessError {
    /// A zero sample rate cannot define attack/release lengths.
    ZeroSampleRate,
}

impl fmt::Display for LoudnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoudnessError::ZeroSampleRate => {
                f.write_str("sample rate must be non-zero for the loudness stage")
            }
        }
    }
}

impl std::error::Error for LoudnessError {}

/// What the loudness stage did to one clip.
///
/// All dB values are finite; silence reports [`DB_FLOOR`]-derived numbers
/// rather than `-inf`. `pre_limit_true_peak_dbtp` is measured **after** the RMS
/// match and **before** limiting, which is the number that decides whether the
/// limiter engages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessReport {
    /// Whole-clip RMS of the source clip, dBFS.
    pub source_rms_dbfs: f32,
    /// Whole-clip RMS of the processed clip before the RMS match, dBFS.
    pub processed_rms_dbfs: f32,
    /// Whole-clip RMS of the final output, dBFS.
    pub output_rms_dbfs: f32,
    /// Gain the RMS match applied, dB (0 when it did not run).
    pub rms_gain_db: f32,
    /// `output_rms_dbfs − source_rms_dbfs`: the A/B level jump. Negative means
    /// the limiter had to remove level. The product gate is `< 1 dB` in
    /// magnitude.
    pub level_delta_db: f32,
    /// True peak after the RMS match and before limiting, dBTP.
    pub pre_limit_true_peak_dbtp: f32,
    /// True peak of the final output, dBTP (4× oversampling).
    pub output_true_peak_dbtp: f32,
    /// Sample peak of the final output, dBFS (for comparison with dBTP).
    pub output_sample_peak_dbfs: f32,
    /// Smallest linear gain the limiter applied (1.0 = no reduction).
    pub limiter_min_gain: f32,
    /// Smallest limiter gain in dB (0.0 = no reduction).
    pub limiter_min_gain_db: f32,
    /// `true` when the limiter altered the signal.
    pub limited: bool,
}

impl LoudnessReport {
    /// A report for a clip the stage left untouched.
    fn untouched(source_rms_dbfs: f32, processed_rms_dbfs: f32, peak_dbtp: f32) -> Self {
        LoudnessReport {
            source_rms_dbfs,
            processed_rms_dbfs,
            output_rms_dbfs: processed_rms_dbfs,
            rms_gain_db: 0.0,
            level_delta_db: processed_rms_dbfs - source_rms_dbfs,
            pre_limit_true_peak_dbtp: peak_dbtp,
            output_true_peak_dbtp: peak_dbtp,
            output_sample_peak_dbfs: dbfs(0.0),
            limiter_min_gain: 1.0,
            limiter_min_gain_db: 0.0,
            limited: false,
        }
    }
}

/// The P-08 loudness stage: optional RMS match, then a true-peak limiter.
///
/// Reusable as-is by the robot chain (D8): give it the **original** clip as
/// `source` and the robot-processed clip as `signal`, and it lands at the same
/// level policy.
pub struct LoudnessStage {
    sample_rate: u32,
    config: LoudnessConfig,
    meter: TruePeakMeter,
    /// Scratch: per-sample true-peak envelope, then the applied gain envelope.
    envelope: Vec<f32>,
    /// Scratch: required/target gain, then the delayed requirement.
    gains: Vec<f32>,
    ceiling_linear: f32,
    internal_ceiling_linear: f32,
    attack_step: f32,
    release_step: f32,
}

impl fmt::Debug for LoudnessStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoudnessStage")
            .field("sample_rate", &self.sample_rate)
            .field("config", &self.config)
            .field("internal_ceiling_dbtp", &self.internal_ceiling_dbtp())
            .finish_non_exhaustive()
    }
}

impl LoudnessStage {
    /// Build a stage at `sample_rate`.
    ///
    /// `sample_rate == 0` is [`LoudnessError::ZeroSampleRate`]; every
    /// configuration field is clamped by [`LoudnessConfig::sanitised`].
    pub fn new(sample_rate: u32, config: LoudnessConfig) -> Result<Self, LoudnessError> {
        if sample_rate == 0 {
            return Err(LoudnessError::ZeroSampleRate);
        }
        let config = config.sanitised();
        let internal_dbtp = config.ceiling_dbtp - config.internal_margin_db;
        let ceiling_linear = 10.0f32.powf(config.ceiling_dbtp / 20.0);
        let internal_ceiling_linear = 10.0f32.powf(internal_dbtp / 20.0);
        let attack_samples = (config.attack_seconds * sample_rate as f32).max(1.0);
        let release_samples = (config.release_seconds * sample_rate as f32).max(1.0);
        Ok(LoudnessStage {
            sample_rate,
            config,
            meter: TruePeakMeter::new(),
            envelope: Vec::new(),
            gains: Vec::new(),
            ceiling_linear,
            internal_ceiling_linear,
            attack_step: 1.0 / attack_samples,
            release_step: 1.0 / release_samples,
        })
    }

    /// The effective (sanitised) configuration.
    pub fn config(&self) -> LoudnessConfig {
        self.config
    }

    /// The sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The published ceiling in dBTP.
    pub fn ceiling_dbtp(&self) -> f32 {
        self.config.ceiling_dbtp
    }

    /// The ceiling the limiter actually targets, in dBTP.
    pub fn internal_ceiling_dbtp(&self) -> f32 {
        self.config.ceiling_dbtp - self.config.internal_margin_db
    }

    /// Apply the policy in place: RMS match (optional) then true-peak limiting.
    ///
    /// `source` is the clip before processing (used only for its RMS);
    /// `signal` is processed in place. Lengths may differ; the RMS match uses
    /// each signal's own length, which is the documented whole-clip rule.
    pub fn process_into(&mut self, source: &[f32], signal: &mut [f32]) -> LoudnessReport {
        let source_rms_dbfs = rms_dbfs(source);
        let processed_rms_dbfs = rms_dbfs(signal);

        let mut rms_gain_db = 0.0;
        if self.config.match_rms {
            rms_gain_db = match_rms_to(signal, source);
        }

        // One interpolated envelope gives both the peak estimate and the
        // per-sample requirement; no second pass unless the limiter engages.
        self.meter.envelope_into(signal, &mut self.envelope);
        let pre_limit_peak = if signal.is_empty() {
            0.0
        } else {
            self.envelope.iter().copied().fold(0.0f32, f32::max)
        };
        let pre_limit_dbtp = dbtp(pre_limit_peak);

        let mut report =
            LoudnessReport::untouched(source_rms_dbfs, processed_rms_dbfs, pre_limit_dbtp);
        report.rms_gain_db = rms_gain_db;
        report.output_sample_peak_dbfs = dbfs(sample_peak(signal));

        if pre_limit_peak <= self.ceiling_linear || signal.is_empty() {
            // Transparent: no gain envelope is computed and no sample changes.
            report.output_rms_dbfs = rms_dbfs(signal);
            report.level_delta_db = report.output_rms_dbfs - report.source_rms_dbfs;
            return report;
        }

        // Local max over ±1 sample so an inter-sample peak cannot sit between
        // two independently computed requirements.
        let length = signal.len();
        self.gains.clear();
        self.gains.reserve(length);
        for index in 0..length {
            let mut value = self.envelope[index];
            if index > 0 {
                value = value.max(self.envelope[index - 1]);
            }
            if index + 1 < length {
                value = value.max(self.envelope[index + 1]);
            }
            self.gains.push(value);
        }

        // Backward pass: each sample's target is the smaller of its own
        // requirement and one attack-step above the sample that follows. Going
        // backwards is what makes the gain descend *before* the peak, and
        // taking the minimum with the requirement is what guarantees no overs.
        let mut next = 1.0f32;
        for index in (0..length).rev() {
            let required = if self.gains[index] > DB_FLOOR {
                (self.internal_ceiling_linear / self.gains[index]).min(1.0)
            } else {
                1.0
            };
            let target = required.min(next + self.attack_step).clamp(0.0, 1.0);
            self.gains[index] = target;
            next = target;
        }

        // Forward release pass, written over the envelope buffer.
        let mut gain = 1.0f32;
        let mut minimum = 1.0f32;
        for index in 0..length {
            gain = self.gains[index].min(gain + self.release_step);
            gain = gain.clamp(0.0, 1.0);
            self.envelope[index] = gain;
            if gain < minimum {
                minimum = gain;
            }
        }
        for (sample, &gain) in signal.iter_mut().zip(&self.envelope) {
            *sample *= gain;
        }

        // A gain envelope is applied per sample, while the true peak is an
        // *interpolated* value between samples: where the gain moves quickly
        // (a sharp attack) the attenuated inter-sample peak can land a few
        // tenths of a dB above `internal_ceiling`. A single global trim removes
        // that residual exactly — a scalar cannot introduce distortion — and
        // guarantees the published ceiling with the full margin intact.
        let mut trim = 1.0f32;
        let measured = self.meter.measure(signal);
        let measured_dbtp = dbtp(measured);
        if measured_dbtp > self.internal_ceiling_dbtp() {
            let target = 10.0f32.powf(self.internal_ceiling_dbtp() / 20.0);
            if measured > DB_FLOOR && target.is_finite() {
                trim = (target / measured).clamp(0.0, 1.0);
                for sample in signal.iter_mut() {
                    *sample *= trim;
                }
                minimum *= trim;
            }
        }

        report.output_rms_dbfs = rms_dbfs(signal);
        report.level_delta_db = report.output_rms_dbfs - report.source_rms_dbfs;
        report.output_true_peak_dbtp = if trim < 1.0 {
            self.internal_ceiling_dbtp()
        } else {
            measured_dbtp
        };
        report.output_sample_peak_dbfs = dbfs(sample_peak(signal));
        report.limiter_min_gain = minimum;
        report.limiter_min_gain_db = if minimum > 0.0 {
            20.0 * minimum.log10()
        } else {
            f32::NEG_INFINITY
        };
        report.limited = true;
        report
    }

    /// [`process_into`](LoudnessStage::process_into) on a copy.
    pub fn process(&mut self, source: &[f32], signal: &[f32]) -> (Vec<f32>, LoudnessReport) {
        let mut output = signal.to_vec();
        let report = self.process_into(source, &mut output);
        (output, report)
    }
}

#[cfg(test)]
mod tests;
