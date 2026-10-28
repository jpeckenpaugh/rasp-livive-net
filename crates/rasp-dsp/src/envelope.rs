//! Spectral-envelope analysis and application (PLAN P-03, decision D3).
//!
//! This module splits a short-time spectrum into a **smooth spectral
//! envelope** `E(f)` and an **excitation** `X(f) / E(f)`, and it moves the
//! envelope on the frequency axis without touching the excitation. That is the
//! mechanism behind the product's central claim: pitch and formants are
//! independent axes (D1).
//!
//! # Units and frame indexing
//!
//! [`SpectralEnvelope`] stores, for every STFT frame, **one log-magnitude value
//! per one-sided bin**: `ln|X[k]|` in **nepers** (natural log of the magnitude
//! of the windowed STFT of the signal, i.e. the same normalisation as
//! [`crate::stft::Stft`] — no `1/N`, no sqrt). One neper is `20/ln(10) ≈ 8.686`
//! dB. Frame `t` is the frame that [`crate::stft::Stft::analyze`] produced for
//! that index, centred at `t·hop − window_size/2` **input** samples (see the
//! [`crate::stft`] module docs). Bins are linearly spaced at
//! `sample_rate / window_size` Hz, so bin `k` is `k · sample_rate /
//! window_size` Hz and the stored value describes `ln|X(f_k)|`.
//!
//! The envelope is deliberately a **low-order description of the spectral
//! shape**, not the spectrum: harmonics, noise and transients live in the
//! excitation. Multiplying `E(f)` by the excitation reconstructs a frame
//! exactly (up to the envelope floor), which is what
//! [`SpectralEnvelope::reconstruct_frame`] does and what the channel vocoder
//! (B-02, D10) needs.
//!
//! # Estimators
//!
//! Two estimators sit behind the same [`EnvelopeAnalyzer`] API, selected by
//! [`EnvelopeMethod`]:
//!
//! * [`EnvelopeMethod::Cepstral`] (default) — real-cepstral **liftering** of
//!   the log magnitude. The cepstrum is computed with the crate's own FFT,
//!   multiplied by a symmetric lifter, and transformed back. A quefrency
//!   cutoff of `lifter_seconds` keeps spectral detail down to roughly
//!   `1/lifter_seconds` Hz. The default is **4 ms** (≈ 250 Hz of detail); the
//!   design brief suggested ≈ 40 ms, which was measured and rejected — see
//!   the [cutoff](#why-4-ms-and-not-40-ms) section.
//! * [`EnvelopeMethod::Lpc`] — order-18 all-pole (Levinson–Durbin) envelope,
//!   the documented fallback from D3. It is exposed for comparison and for
//!   callers that want a parametric envelope; warping it is measurably less
//!   stable than warping the cepstral envelope (see the crate tests and the
//!   P-03 report).
//!
//! Optionally the cepstral estimate is refined by **true-envelope iteration**
//! (Röbel–Rodet): replace the log spectrum by `max(log|X|, E)` and re-lifter,
//! `true_envelope_passes` times. On a harmonic signal the plain cepstral
//! envelope sits below the harmonic peaks, which biases the apparent formant
//! position; the refinement lifts the envelope onto the peaks and removes most
//! of that bias. Three passes are the default and the measured sweet spot.
//!
//! # Why 4 ms and not 40 ms
//!
//! A lifter cutoff of `T` seconds removes spectral detail finer than `1/T` Hz.
//! To separate formants from a harmonic source, the cutoff must be **shorter
//! than the pitch period**: keeping detail down to 25 Hz (a 40 ms cutoff)
//! retains the harmonic comb for every voice in the corpus (79–323 Hz) as if it
//! were envelope, so the "envelope" starts cancelling harmonics when it is
//! re-applied. Measured on synthetic vowels with known formants, a 40 ms
//! cutoff moves F1 by **+12.3% (male, 700 Hz) and +16.9% (low male, 500 Hz)**
//! when 10% was requested — both outside the 8% acceptance tolerance — while
//! 4 ms gives +1.2% and +0.9%. It also fails in the opposite direction at
//! other cells, so 40 ms is not a viable "smoother" envelope. The full table
//! is printed by `envelope_method_comparison`; the constant is
//! [`DEFAULT_LIFTER_SECONDS`].
//!
//! # Temporal smoothing
//!
//! Envelopes are smoothed **across frames** (a centred rectangular average of
//! `temporal_smoothing_seconds`, 40 ms by default, snapped to a whole number
//! of hops) before they are used. This is what the P-03 acceptance criterion
//! "envelope temporally smoothed" asks for: it keeps frame-to-frame estimate
//! noise out of the formant path without smearing the formant *positions*
//! (the estimator is stationary over tens of milliseconds of speech).
//! [`EnvelopeAnalyzer::analyze`] applies it;
//! [`EnvelopeAnalyzer::estimate_frame`] (the per-frame path) does not.
//!
//! # Errors
//!
//! Nothing here panics on a caller-controlled path. Bad geometry, a zero
//! sample rate, or a wrong-length bin slice returns a typed
//! [`EnvelopeError`]. Estimator failures (an unstable LPC solution) fall back
//! to a flat envelope rather than propagating an error, because a single
//! degenerate frame must not fail a render.

use std::fmt;

use crate::stft::{Complex, RealFft, Spectrogram};

/// Default cepstral lifter cutoff, in seconds of quefrency (PLAN P-03).
pub const DEFAULT_LIFTER_SECONDS: f32 = 0.004;
/// Default number of true-envelope refinement passes.
pub const DEFAULT_TRUE_ENVELOPE_PASSES: usize = 3;
/// Default temporal smoothing across frames, in seconds.
pub const DEFAULT_TEMPORAL_SMOOTHING_SECONDS: f32 = 0.04;
/// Default LPC order for [`EnvelopeMethod::Lpc`].
pub const DEFAULT_LPC_ORDER: usize = 18;
/// Largest LPC order accepted (above this the autocorrelation solve is not
/// numerically trustworthy in `f32` analysis).
pub const MAX_LPC_ORDER: usize = 64;
/// Default per-bin correction limit used by [`SpectralEnvelope::apply_gain`]
/// callers: ±30 dB.
pub const DEFAULT_MAX_GAIN_DB: f32 = 30.0;
/// Largest correction limit accepted.
pub const MAX_GAIN_DB_LIMIT: f32 = 60.0;

/// Lowest accepted envelope warp ratio (a formant ratio of 0.5 is far outside
/// the product range, but the analysis object stays usable for experiments).
pub const MIN_WARP_RATIO: f32 = 0.25;
/// Highest accepted envelope warp ratio.
pub const MAX_WARP_RATIO: f32 = 4.0;

/// Floor applied to magnitudes before taking the log, `1e-9` (−180 dB). It
/// keeps silent bins finite without affecting audible content.
const MAGNITUDE_FLOOR: f32 = 1e-9;
/// Floor applied to an envelope gain when extracting an excitation, so a
/// near-silent envelope cannot produce an unbounded excitation.
const EXCITATION_GAIN_CEILING: f32 = 1e3;
/// Largest log-magnitude excursion accepted from an LPC solve, in nepers
/// (about ±260 dB); degenerate frames are clamped to this.
const LPC_LOG_LIMIT: f64 = 30.0;

/// Which envelope estimator [`EnvelopeAnalyzer`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnvelopeMethod {
    /// Cepstral liftering of the log magnitude (D3's primary method).
    #[default]
    Cepstral,
    /// All-pole (LPC) envelope of the given order (D3's documented fallback).
    Lpc,
}

/// Configuration for [`EnvelopeAnalyzer`].
///
/// All fields are sanitised by [`EnvelopeAnalyzer::new`]; out-of-range values
/// are clamped rather than rejected, because these are the values a UI slider
/// or WASM parameter struct will hand over. The effective values are reported
/// back by [`EnvelopeAnalyzer::config`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvelopeConfig {
    /// Estimator to use.
    pub method: EnvelopeMethod,
    /// Cepstral lifter cutoff in seconds (ignored by [`EnvelopeMethod::Lpc`]).
    pub lifter_seconds: f32,
    /// True-envelope refinement passes (ignored by [`EnvelopeMethod::Lpc`]).
    pub true_envelope_passes: usize,
    /// LPC order (ignored by [`EnvelopeMethod::Cepstral`]).
    pub lpc_order: usize,
    /// Temporal smoothing across frames in seconds; `0.0` disables it.
    pub temporal_smoothing_seconds: f32,
    /// Per-bin correction limit in dB, applied symmetrically when an envelope
    /// gain is derived. Stored here so every consumer of the envelope agrees on
    /// the bound.
    pub max_gain_db: f32,
}

impl Default for EnvelopeConfig {
    fn default() -> Self {
        EnvelopeConfig {
            method: EnvelopeMethod::Cepstral,
            lifter_seconds: DEFAULT_LIFTER_SECONDS,
            true_envelope_passes: DEFAULT_TRUE_ENVELOPE_PASSES,
            lpc_order: DEFAULT_LPC_ORDER,
            temporal_smoothing_seconds: DEFAULT_TEMPORAL_SMOOTHING_SECONDS,
            max_gain_db: DEFAULT_MAX_GAIN_DB,
        }
    }
}

impl EnvelopeConfig {
    /// The default cepstral configuration (4 ms lifter, 3 refinement passes,
    /// 40 ms temporal smoothing, ±30 dB).
    pub const fn cepstral() -> Self {
        EnvelopeConfig {
            method: EnvelopeMethod::Cepstral,
            lifter_seconds: DEFAULT_LIFTER_SECONDS,
            true_envelope_passes: DEFAULT_TRUE_ENVELOPE_PASSES,
            lpc_order: DEFAULT_LPC_ORDER,
            temporal_smoothing_seconds: DEFAULT_TEMPORAL_SMOOTHING_SECONDS,
            max_gain_db: DEFAULT_MAX_GAIN_DB,
        }
    }

    /// Order-18 LPC envelope with the same temporal smoothing.
    pub const fn lpc() -> Self {
        EnvelopeConfig {
            method: EnvelopeMethod::Lpc,
            lifter_seconds: DEFAULT_LIFTER_SECONDS,
            true_envelope_passes: 0,
            lpc_order: DEFAULT_LPC_ORDER,
            temporal_smoothing_seconds: DEFAULT_TEMPORAL_SMOOTHING_SECONDS,
            max_gain_db: DEFAULT_MAX_GAIN_DB,
        }
    }

    /// Clamp every field into its supported range.
    pub fn sanitised(mut self) -> Self {
        self.lifter_seconds = if self.lifter_seconds.is_finite() {
            self.lifter_seconds.clamp(1.0e-4, 0.05)
        } else {
            DEFAULT_LIFTER_SECONDS
        };
        self.true_envelope_passes = self.true_envelope_passes.min(16);
        self.lpc_order = self.lpc_order.clamp(4, MAX_LPC_ORDER);
        self.temporal_smoothing_seconds = if self.temporal_smoothing_seconds.is_finite() {
            self.temporal_smoothing_seconds.clamp(0.0, 0.2)
        } else {
            DEFAULT_TEMPORAL_SMOOTHING_SECONDS
        };
        self.max_gain_db = if self.max_gain_db.is_finite() {
            self.max_gain_db.clamp(0.0, MAX_GAIN_DB_LIMIT)
        } else {
            DEFAULT_MAX_GAIN_DB
        };
        self
    }

    /// The correction limit expressed as a natural-log ratio.
    pub(crate) fn max_gain_neper(&self) -> f32 {
        self.max_gain_db * core::f32::consts::LN_10 / 20.0
    }
}

/// Construction and arithmetic errors from this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The sample rate must be non-zero: the lifter cutoff and the temporal
    /// smoothing window are expressed in seconds.
    ZeroSampleRate,
    /// A spectrogram had zero bins, or geometry inconsistent with its own
    /// window size.
    EmptySpectrum,
    /// A bin slice had the wrong length for the envelope.
    BinLength {
        /// Number of bins the envelope was built for.
        expected: usize,
        /// Number of bins supplied.
        actual: usize,
    },
    /// A frame index was out of range.
    FrameIndex {
        /// Frame index requested.
        frame: usize,
        /// Number of frames in the envelope.
        frames: usize,
    },
    /// A spectrogram's geometry did not match another object it is combined
    /// with (frame count, bins or hop).
    Geometry {
        /// Field that disagreed (`"frames"`, `"bins"` or `"hop"`).
        field: &'static str,
        /// Value required.
        expected: usize,
        /// Value supplied.
        actual: usize,
    },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvelopeError::ZeroSampleRate => {
                f.write_str("sample rate must be non-zero for envelope analysis")
            }
            EnvelopeError::EmptySpectrum => {
                f.write_str("spectrogram has no bins (window size is too small)")
            }
            EnvelopeError::BinLength { expected, actual } => {
                write!(f, "spectrum has {actual} bins, expected {expected}")
            }
            EnvelopeError::FrameIndex { frame, frames } => {
                write!(
                    f,
                    "frame {frame} is out of range (envelope has {frames} frames)"
                )
            }
            EnvelopeError::Geometry {
                field,
                expected,
                actual,
            } => write!(
                f,
                "envelope/spectrogram {field} is {actual}, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for EnvelopeError {}

// ---------------------------------------------------------------------------
// SpectralEnvelope
// ---------------------------------------------------------------------------

/// A per-frame log-magnitude spectral envelope.
///
/// See the [module docs](self) for units and frame indexing. A frame is
/// `window_size / 2 + 1` values of `ln|X(f_k)|`, stored frame-major. The
/// envelope owns no signal and no phase: it is a reusable analysis product,
/// which is what D10's vocoder consumes.
#[derive(Clone, Debug, PartialEq)]
pub struct SpectralEnvelope {
    frames: usize,
    bins: usize,
    hop: usize,
    window_size: usize,
    sample_rate: u32,
    data: Vec<f32>,
}

impl SpectralEnvelope {
    /// Allocate a zeroed envelope with the given geometry. A zero sample rate
    /// is stored as zero and only affects frequency-reporting helpers.
    pub fn new(
        frames: usize,
        bins: usize,
        hop: usize,
        window_size: usize,
        sample_rate: u32,
    ) -> Self {
        SpectralEnvelope {
            frames,
            bins,
            hop,
            window_size,
            sample_rate,
            data: vec![0.0; frames.saturating_mul(bins)],
        }
    }

    /// Number of frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Bins per frame (`window_size / 2 + 1`).
    pub fn bins(&self) -> usize {
        self.bins
    }

    /// Hop in samples the envelope was analysed with.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Window size the envelope was analysed with.
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Sample rate the envelope assumes.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Whether the envelope has no frames.
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// All values, frame-major.
    pub fn data(&self) -> &[f32] {
        &self.data
    }

    /// One frame's log magnitudes, or `None` when `frame >= frames()`.
    pub fn frame(&self, frame: usize) -> Option<&[f32]> {
        if frame >= self.frames {
            return None;
        }
        Some(&self.data[frame * self.bins..(frame + 1) * self.bins])
    }

    /// One frame's log magnitudes, mutable, or `None` when out of range.
    pub fn frame_mut(&mut self, frame: usize) -> Option<&mut [f32]> {
        if frame >= self.frames {
            return None;
        }
        Some(&mut self.data[frame * self.bins..(frame + 1) * self.bins])
    }

    /// Frame period in seconds (`hop / sample_rate`), or `0.0` for a zero
    /// sample rate.
    pub fn frame_period_seconds(&self) -> f32 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.hop as f32 / self.sample_rate as f32
        }
    }

    /// Frequency of `bin` in Hz, or `0.0` for a zero sample rate or an
    /// out-of-range bin.
    pub fn bin_hz(&self, bin: usize) -> f32 {
        if self.sample_rate == 0 || bin >= self.bins || self.window_size == 0 {
            return 0.0;
        }
        bin as f32 * self.sample_rate as f32 / self.window_size as f32
    }

    /// Linear magnitude gain of `frame`/`bin`: `exp(log_magnitude)`.
    ///
    /// Returns `0.0` for an out-of-range index (an absent envelope must not
    /// silently amplify a bin) and clamps the result to `[0, 1e30]` so a
    /// degenerate envelope cannot produce an infinity.
    pub fn gain(&self, frame: usize, bin: usize) -> f32 {
        if frame >= self.frames || bin >= self.bins {
            return 0.0;
        }
        (self.data[frame * self.bins + bin] as f64).exp().min(1e30) as f32
    }

    /// Resize and zero-fill, reusing the allocation.
    pub fn reset(
        &mut self,
        frames: usize,
        bins: usize,
        hop: usize,
        window_size: usize,
        sample_rate: u32,
    ) {
        self.frames = frames;
        self.bins = bins;
        self.hop = hop;
        self.window_size = window_size;
        self.sample_rate = sample_rate;
        self.data.clear();
        self.data.resize(frames.saturating_mul(bins), 0.0);
    }

    /// Truncate to zero frames, keeping the allocation.
    pub fn clear(&mut self) {
        self.frames = 0;
        self.data.clear();
    }

    /// Warp the envelope on the frequency axis by `ratio`: a feature at
    /// frequency `f` ends up at `ratio·f`.
    ///
    /// The target envelope is `E_target(f) = E(f / ratio)` evaluated by linear
    /// interpolation in the bin index (bins are linearly spaced, so this is
    /// exactly the log-frequency scaling the product calls a formant shift).
    /// A formant at 700 Hz moves to 770 Hz for `ratio = 1.10`. Values beyond
    /// the analysed band are clamped to the first/last bin.
    ///
    /// A non-finite or out-of-range ratio is clamped to
    /// [`MIN_WARP_RATIO`]..=[`MAX_WARP_RATIO`]; `ratio == 1.0` copies.
    pub fn warp(&self, ratio: f32) -> SpectralEnvelope {
        let mut out = SpectralEnvelope::new(
            self.frames,
            self.bins,
            self.hop,
            self.window_size,
            self.sample_rate,
        );
        self.warp_into(ratio, &mut out);
        out
    }

    /// [`warp`](SpectralEnvelope::warp) into an existing envelope, reusing its
    /// allocation. The output geometry is set to this envelope's geometry.
    pub fn warp_into(&self, ratio: f32, out: &mut SpectralEnvelope) {
        let ratio = sanitise_ratio(ratio);
        let frames = self.frames;
        let bins = self.bins;
        if out.frames != frames || out.bins != bins {
            out.reset(frames, bins, self.hop, self.window_size, self.sample_rate);
        } else {
            out.hop = self.hop;
            out.window_size = self.window_size;
            out.sample_rate = self.sample_rate;
        }

        if (ratio - 1.0).abs() <= 1e-6 || bins == 0 {
            out.data.copy_from_slice(&self.data);
            return;
        }

        let last = (bins - 1) as f32;
        for frame in 0..frames {
            let src = &self.data[frame * bins..(frame + 1) * bins];
            let dst = &mut out.data[frame * bins..(frame + 1) * bins];
            for (k, slot) in dst.iter_mut().enumerate() {
                let position = (k as f32 / ratio).clamp(0.0, last);
                let low = position.floor() as usize;
                let high = (low + 1).min(bins - 1);
                let fraction = position - low as f32;
                *slot = src[low] * (1.0 - fraction) + src[high] * fraction;
            }
        }
    }

    /// Multiply one frame's complex bins by the envelope's linear magnitude.
    ///
    /// This is frame reconstruction from `envelope × excitation`: pass the
    /// excitation bins and get the shaped spectrum. Phase is untouched.
    ///
    /// # Errors
    ///
    /// [`EnvelopeError::BinLength`] when `bins.len() != self.bins()`, and
    /// [`EnvelopeError::FrameIndex`] when the frame is out of range.
    pub fn apply_gain(&self, frame: usize, bins: &mut [Complex]) -> Result<(), EnvelopeError> {
        self.check_frame(frame, bins.len())?;
        let envelope = &self.data[frame * self.bins..(frame + 1) * self.bins];
        for (bin, &log_magnitude) in bins.iter_mut().zip(envelope) {
            let gain = (log_magnitude as f64).exp().min(1e30) as f32;
            *bin = bin.scale(gain);
        }
        Ok(())
    }

    /// Divide one frame's complex bins by the envelope, extracting the
    /// excitation `X(f)/E(f)`.
    ///
    /// The divisor is floored at `exp(−ln(1e30))` worth of gain, i.e. the
    /// excitation is amplified by at most +260 dB relative to a vanishing
    /// envelope, so a silent frame stays finite. Phase is untouched.
    ///
    /// Use this for B-02's analysis side: apply the envelope to a carrier with
    /// [`apply_gain`](SpectralEnvelope::apply_gain) afterwards.
    pub fn divide_from(&self, frame: usize, bins: &mut [Complex]) -> Result<(), EnvelopeError> {
        self.check_frame(frame, bins.len())?;
        let envelope = &self.data[frame * self.bins..(frame + 1) * self.bins];
        for (bin, &log_magnitude) in bins.iter_mut().zip(envelope) {
            let gain = (log_magnitude as f64)
                .exp()
                .clamp(1.0 / EXCITATION_GAIN_CEILING as f64, 1e30);
            *bin = bin.scale((1.0 / gain) as f32);
        }
        Ok(())
    }

    /// Write `envelope × excitation` for one frame into `out`.
    ///
    /// `out` must have the same length as `excitation` and as the envelope's
    /// bin count.
    ///
    /// # Errors
    ///
    /// [`EnvelopeError::BinLength`] for either slice, or
    /// [`EnvelopeError::FrameIndex`] for an out-of-range frame.
    pub fn reconstruct_frame(
        &self,
        frame: usize,
        excitation: &[Complex],
        out: &mut [Complex],
    ) -> Result<(), EnvelopeError> {
        self.check_frame(frame, excitation.len())?;
        if out.len() != self.bins {
            return Err(EnvelopeError::BinLength {
                expected: self.bins,
                actual: out.len(),
            });
        }
        let envelope = &self.data[frame * self.bins..(frame + 1) * self.bins];
        for ((slot, &source), &log_magnitude) in out.iter_mut().zip(excitation).zip(envelope) {
            let gain = (log_magnitude as f64).exp().min(1e30) as f32;
            *slot = source.scale(gain);
        }
        Ok(())
    }

    fn check_frame(&self, frame: usize, bins: usize) -> Result<(), EnvelopeError> {
        if frame >= self.frames {
            return Err(EnvelopeError::FrameIndex {
                frame,
                frames: self.frames,
            });
        }
        if bins != self.bins {
            return Err(EnvelopeError::BinLength {
                expected: self.bins,
                actual: bins,
            });
        }
        Ok(())
    }
}

/// Clamp a warp ratio; non-finite values become `1.0` (a bypass).
fn sanitise_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_WARP_RATIO, MAX_WARP_RATIO)
    } else {
        1.0
    }
}

// ---------------------------------------------------------------------------
// Estimators
// ---------------------------------------------------------------------------

/// Common surface of the two frame estimators, so the analyzer's public API and
/// its temporal smoothing are shared (D3 asks for LPC "behind the same
/// trait/API").
trait FrameEnvelope {
    /// Geometry changed: rebuild plans and scratch.
    fn reset(&mut self, window_size: usize, bins: usize);
    /// Estimate `ln|E(f_k)|` for one frame's one-sided bins.
    fn estimate(&mut self, bins: &[Complex], out: &mut [f32]);
}

/// Cepstral-liftering estimator with optional true-envelope refinement.
struct CepstralEnvelope {
    sample_rate: u32,
    lifter_seconds: f32,
    passes: usize,
    /// Real FFT plan of length `window_size`.
    fft: RealFft,
    window_size: usize,
    bins: usize,
    /// Symmetric lifter weights, length `window_size`.
    lifter: Vec<f32>,
    /// `ln|X[k]|` of the current frame, length `bins`.
    log_magnitude: Vec<f32>,
    /// Working spectrum for the current iteration.
    work: Vec<f32>,
    /// Real cepstrum, length `window_size`.
    cepstrum: Vec<f32>,
    /// Complex scratch for the inverse/forward transforms.
    complex: Vec<Complex>,
}

impl CepstralEnvelope {
    fn new(
        sample_rate: u32,
        lifter_seconds: f32,
        passes: usize,
        window_size: usize,
        bins: usize,
    ) -> Self {
        let mut estimator = CepstralEnvelope {
            sample_rate,
            lifter_seconds,
            passes,
            fft: RealFft::new(window_size),
            window_size,
            bins,
            lifter: vec![0.0; window_size],
            log_magnitude: vec![0.0; bins],
            work: vec![0.0; bins],
            cepstrum: vec![0.0; window_size],
            complex: vec![Complex::ZERO; bins],
        };
        estimator.build_lifter();
        estimator
    }

    fn set_lifter_seconds(&mut self, lifter_seconds: f32) {
        if (self.lifter_seconds - lifter_seconds).abs() <= 1e-9 {
            return;
        }
        self.lifter_seconds = lifter_seconds;
        self.build_lifter();
    }

    fn build_lifter(&mut self) {
        let n = self.window_size;
        let cutoff = if self.sample_rate == 0 {
            1
        } else {
            (self.lifter_seconds * self.sample_rate as f32).round() as usize
        };
        // A cutoff at or beyond Nyquist would keep the whole cepstrum, which is
        // the identity (including the harmonic comb). Keep at least one tap and
        // never reach the symmetric half.
        let cutoff = cutoff.clamp(1, (n / 2).saturating_sub(1).max(1));
        self.lifter.fill(0.0);
        self.lifter[0] = 1.0;
        let taper_start = (cutoff as f32 * 0.8) as usize;
        let taper_span = (cutoff - taper_start).max(1) as f32;
        for n_pos in 1..=cutoff {
            let weight = if n_pos <= taper_start {
                1.0
            } else {
                let x = (n_pos - taper_start) as f32 / taper_span;
                0.5 * (1.0 + (core::f32::consts::PI * x).cos())
            };
            self.lifter[n_pos] = weight;
            let mirrored = self.window_size - n_pos;
            if mirrored < self.window_size && mirrored > cutoff {
                self.lifter[mirrored] = weight;
            }
        }
    }

    /// One liftering pass from `work` into `out`.
    fn lifter_pass(&mut self, out: &mut [f32]) {
        for (slot, &value) in self.complex.iter_mut().zip(&self.work) {
            *slot = Complex::new(value, 0.0);
        }
        let mut cepstrum = core::mem::take(&mut self.cepstrum);
        self.fft.inverse(&self.complex, &mut cepstrum);
        for (value, &weight) in cepstrum.iter_mut().zip(&self.lifter) {
            *value *= weight;
        }
        let mut complex = core::mem::take(&mut self.complex);
        self.fft.forward(&cepstrum, &mut complex);
        for (slot, value) in out.iter_mut().zip(&complex) {
            *slot = value.re;
        }
        self.cepstrum = cepstrum;
        self.complex = complex;
    }
}

impl FrameEnvelope for CepstralEnvelope {
    fn reset(&mut self, window_size: usize, bins: usize) {
        self.fft = RealFft::new(window_size);
        self.window_size = window_size;
        self.bins = bins;
        self.lifter.resize(window_size, 0.0);
        self.log_magnitude.resize(bins, 0.0);
        self.work.resize(bins, 0.0);
        self.cepstrum.resize(window_size, 0.0);
        self.complex.resize(bins, Complex::ZERO);
        self.build_lifter();
    }

    fn estimate(&mut self, bins: &[Complex], out: &mut [f32]) {
        debug_assert_eq!(bins.len(), self.bins);
        debug_assert_eq!(out.len(), self.bins);
        for (slot, bin) in self.log_magnitude.iter_mut().zip(bins) {
            *slot = bin.magnitude().max(MAGNITUDE_FLOOR).ln();
        }
        self.work.copy_from_slice(&self.log_magnitude);

        for pass in 0..=self.passes {
            self.lifter_pass(out);
            if pass == self.passes {
                break;
            }
            // True-envelope refinement: the envelope may not fall below the
            // observed log spectrum; re-lifter onto the harmonic peaks.
            for ((work, &observed), &smoothed) in self
                .work
                .iter_mut()
                .zip(&self.log_magnitude)
                .zip(out.iter())
            {
                *work = observed.max(smoothed);
            }
        }
    }
}

/// All-pole estimator (Levinson–Durbin on the frame autocorrelation).
struct LpcEnvelope {
    order: usize,
    fft: RealFft,
    window_size: usize,
    bins: usize,
    /// Autocorrelation, length `order + 1`.
    autocorrelation: Vec<f64>,
    /// Prediction coefficients `a[0] = 1`, length `order + 1`.
    coefficients: Vec<f64>,
    /// Scratch for the Levinson update.
    previous: Vec<f64>,
    power: Vec<Complex>,
    autocorrelation_time: Vec<f32>,
}

impl LpcEnvelope {
    fn new(order: usize, window_size: usize, bins: usize) -> Self {
        LpcEnvelope {
            order,
            fft: RealFft::new(window_size),
            window_size,
            bins,
            autocorrelation: vec![0.0; order + 1],
            coefficients: vec![0.0; order + 1],
            previous: vec![0.0; order + 1],
            power: vec![Complex::ZERO; bins],
            autocorrelation_time: vec![0.0; window_size],
        }
    }

    /// Solve the normal equations. Returns `false` when the frame is
    /// degenerate (no energy, or the recursion loses positivity).
    fn solve(&mut self) -> bool {
        let r = &self.autocorrelation;
        if r[0].is_nan() || r[0] <= 0.0 || !r[0].is_finite() {
            return false;
        }
        self.coefficients.fill(0.0);
        self.coefficients[0] = 1.0;
        let mut error = r[0] * 1.0001;
        for i in 1..=self.order {
            let mut accumulator = r[i];
            for j in 1..i {
                accumulator += self.coefficients[j] * r[i - j];
            }
            if !accumulator.is_finite() || !error.is_finite() || error.abs() < 1e-30 {
                return false;
            }
            let reflection = -accumulator / error;
            if !reflection.is_finite() || reflection.abs() >= 1.0 {
                return false;
            }
            self.previous.copy_from_slice(&self.coefficients);
            for j in 1..i {
                self.coefficients[j] = self.previous[j] + reflection * self.previous[i - j];
            }
            self.coefficients[i] = reflection;
            error *= 1.0 - reflection * reflection;
            if error.is_nan() || error <= 0.0 {
                return false;
            }
        }
        true
    }

    /// Write the all-pole log magnitude at the STFT bin frequencies.
    fn evaluate(&mut self, out: &mut [f32]) {
        let n = self.window_size as f64;
        let two_pi = 2.0 * core::f64::consts::PI;
        for (k, slot) in out.iter_mut().enumerate() {
            let omega = two_pi * k as f64 / n;
            let (mut re, mut im) = (1.0f64, 0.0f64);
            for (j, &coefficient) in self.coefficients.iter().enumerate().skip(1) {
                let angle = omega * j as f64;
                re += coefficient * angle.cos();
                im -= coefficient * angle.sin();
            }
            let magnitude = (re * re + im * im).sqrt().max(1e-9);
            *slot = (-magnitude.ln()).clamp(-LPC_LOG_LIMIT, LPC_LOG_LIMIT) as f32;
        }
    }
}

impl FrameEnvelope for LpcEnvelope {
    fn reset(&mut self, window_size: usize, bins: usize) {
        self.fft = RealFft::new(window_size);
        self.window_size = window_size;
        self.bins = bins;
        self.power.resize(bins, Complex::ZERO);
        self.autocorrelation_time.resize(window_size, 0.0);
    }

    fn estimate(&mut self, bins: &[Complex], out: &mut [f32]) {
        debug_assert_eq!(bins.len(), self.bins);
        debug_assert_eq!(out.len(), self.bins);
        let mut mean = 0.0f64;
        for (slot, bin) in self.power.iter_mut().zip(bins) {
            let power = bin.magnitude_sq();
            mean += (power.max(1e-18) as f64).ln();
            *slot = Complex::new(power, 0.0);
        }

        let mut autocorrelation_time = core::mem::take(&mut self.autocorrelation_time);
        self.fft.inverse(&self.power, &mut autocorrelation_time);
        for (slot, &value) in self.autocorrelation.iter_mut().zip(&autocorrelation_time) {
            *slot = value as f64;
        }
        self.autocorrelation_time = autocorrelation_time;

        if self.solve() {
            self.evaluate(out);
        } else {
            // Degenerate frame: a flat envelope at the frame's mean level. The
            // correction gain is then zero, which is the safe no-op. `mean` is
            // the mean log *power*, so halve it for log magnitude.
            let flat = (0.5 * mean / self.bins as f64) as f32;
            out.fill(flat);
        }
    }
}

/// The estimator selected by [`EnvelopeMethod`].
enum Estimator {
    Cepstral(CepstralEnvelope),
    Lpc(LpcEnvelope),
}

impl Estimator {
    fn reset(&mut self, window_size: usize, bins: usize) {
        match self {
            Estimator::Cepstral(estimator) => estimator.reset(window_size, bins),
            Estimator::Lpc(estimator) => estimator.reset(window_size, bins),
        }
    }

    fn estimate(&mut self, bins: &[Complex], out: &mut [f32]) {
        match self {
            Estimator::Cepstral(estimator) => estimator.estimate(bins, out),
            Estimator::Lpc(estimator) => estimator.estimate(bins, out),
        }
    }
}

// ---------------------------------------------------------------------------
// Analyzer
// ---------------------------------------------------------------------------

/// Turns a [`Spectrogram`] into a [`SpectralEnvelope`].
///
/// The analyzer owns its FFT plan and scratch, so repeated renders allocate
/// nothing per frame. Geometry is taken from the spectrogram itself: the same
/// analyzer can analyse `N = 2048` and `N = 4096` spectrograms in sequence
/// (it rebuilds the plan when the window size changes).
///
/// ```
/// use rasp_dsp::envelope::{EnvelopeAnalyzer, EnvelopeConfig};
/// use rasp_dsp::stft::{Stft, StftConfig};
///
/// let mut stft = Stft::new(StftConfig::default()).unwrap();
/// let mut analyzer = EnvelopeAnalyzer::new(48_000, EnvelopeConfig::default()).unwrap();
/// let audio = vec![0.0f32; 4096];
/// let spectrogram = stft.analyze(&audio);
/// let envelope = analyzer.analyze(&spectrogram).unwrap();
/// assert_eq!(envelope.frames(), spectrogram.frames());
/// assert_eq!(envelope.bins(), spectrogram.bins());
/// ```
pub struct EnvelopeAnalyzer {
    sample_rate: u32,
    config: EnvelopeConfig,
    estimator: Estimator,
    window_size: usize,
    bins: usize,
    hop: usize,
    /// Frame-major copy used while temporal smoothing reads neighbours.
    smoothing_scratch: Vec<f32>,
}

impl fmt::Debug for EnvelopeAnalyzer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvelopeAnalyzer")
            .field("sample_rate", &self.sample_rate)
            .field("config", &self.config)
            .field("window_size", &self.window_size)
            .field("bins", &self.bins)
            .field("hop", &self.hop)
            .finish_non_exhaustive()
    }
}

impl EnvelopeAnalyzer {
    /// Build an analyzer for `sample_rate`. Configuration values are clamped;
    /// only a zero sample rate is an error.
    pub fn new(sample_rate: u32, config: EnvelopeConfig) -> Result<Self, EnvelopeError> {
        if sample_rate == 0 {
            return Err(EnvelopeError::ZeroSampleRate);
        }
        let config = config.sanitised();
        // Start with the D2 default geometry; `analyze` rebuilds when a
        // spectrogram with another window size arrives.
        let window_size = 2048;
        let bins = window_size / 2 + 1;
        let estimator = match config.method {
            EnvelopeMethod::Cepstral => Estimator::Cepstral(CepstralEnvelope::new(
                sample_rate,
                config.lifter_seconds,
                config.true_envelope_passes,
                window_size,
                bins,
            )),
            EnvelopeMethod::Lpc => {
                Estimator::Lpc(LpcEnvelope::new(config.lpc_order, window_size, bins))
            }
        };
        Ok(EnvelopeAnalyzer {
            sample_rate,
            config,
            estimator,
            window_size,
            bins,
            hop: window_size / 4,
            smoothing_scratch: Vec::new(),
        })
    }

    /// The sanitised configuration in use.
    pub fn config(&self) -> EnvelopeConfig {
        self.config
    }

    /// Override the cepstral lifter cutoff for subsequent analyses.
    ///
    /// The value is sanitised exactly like a constructor value. A consumer that
    /// knows the analysed signal's F0 uses this to keep the lifter **below the
    /// pitch period**: the cepstral peak of a periodic signal sits at quefrency
    /// `1/F0`, and a lifter longer than that admits the pitch pulse into the
    /// "envelope", after which a correction pass chases individual harmonics
    /// instead of the vocal-tract shape. `EnvelopeMethod::Lpc` ignores the
    /// value (it has no lifter) but still records it in [`config`](Self::config).
    pub fn set_lifter_seconds(&mut self, seconds: f32) {
        let mut config = self.config;
        config.lifter_seconds = seconds;
        let config = config.sanitised();
        if (config.lifter_seconds - self.config.lifter_seconds).abs() <= 1e-9 {
            return;
        }
        self.config.lifter_seconds = config.lifter_seconds;
        if let Estimator::Cepstral(cepstral) = &mut self.estimator {
            cepstral.set_lifter_seconds(config.lifter_seconds);
        }
    }

    /// The sample rate the analyzer was built for.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of frames the temporal smoother spans (1 = disabled).
    ///
    /// Derived from `temporal_smoothing_seconds` and the spectrogram's hop; it
    /// is reported here for the geometry last analysed.
    pub fn smoothing_frames(&self) -> usize {
        self.smoothing_taps()
    }

    fn smoothing_taps(&self) -> usize {
        if self.config.temporal_smoothing_seconds <= 0.0 || self.sample_rate == 0 || self.hop == 0 {
            return 1;
        }
        let frames = (self.config.temporal_smoothing_seconds * self.sample_rate as f32
            / self.hop as f32)
            .round() as usize;
        frames.max(1)
    }

    /// Analyse a whole spectrogram into a new envelope.
    pub fn analyze(
        &mut self,
        spectrogram: &Spectrogram,
    ) -> Result<SpectralEnvelope, EnvelopeError> {
        let mut envelope = SpectralEnvelope::new(
            spectrogram.frames(),
            spectrogram.bins(),
            spectrogram.hop(),
            spectrogram.window_size(),
            self.sample_rate,
        );
        self.analyze_into(spectrogram, &mut envelope)?;
        Ok(envelope)
    }

    /// Analyse into an existing envelope, reusing its allocation.
    ///
    /// Temporal smoothing is applied across frames before returning. The
    /// envelope geometry is reset to match the spectrogram.
    pub fn analyze_into(
        &mut self,
        spectrogram: &Spectrogram,
        envelope: &mut SpectralEnvelope,
    ) -> Result<(), EnvelopeError> {
        let frames = spectrogram.frames();
        let bins = spectrogram.bins();
        if bins == 0 {
            return Err(EnvelopeError::EmptySpectrum);
        }
        self.ensure_geometry(spectrogram.window_size(), bins, spectrogram.hop())?;
        envelope.reset(
            frames,
            bins,
            spectrogram.hop(),
            spectrogram.window_size(),
            self.sample_rate,
        );

        for frame in 0..frames {
            let source = spectrogram
                .frame(frame)
                .ok_or(EnvelopeError::FrameIndex { frame, frames })?;
            let destination = envelope
                .frame_mut(frame)
                .ok_or(EnvelopeError::FrameIndex { frame, frames })?;
            self.estimator.estimate(source, destination);
        }

        self.smooth_frames(envelope);
        Ok(())
    }

    /// Estimate one frame's log-magnitude envelope into `out`.
    ///
    /// For callers that already hold a spectrum and want a per-frame envelope —
    /// the streaming/vocoder path (B-02) — without the whole-buffer temporal
    /// smoothing that [`analyze`](EnvelopeAnalyzer::analyze) applies.
    ///
    /// `bins` and `out` must both have the analyzer's current bin count; call
    /// [`analyze`](EnvelopeAnalyzer::analyze) at least once (or
    /// [`prepare`](EnvelopeAnalyzer::prepare)) before this method.
    pub fn estimate_frame(
        &mut self,
        bins: &[Complex],
        out: &mut [f32],
    ) -> Result<(), EnvelopeError> {
        if bins.len() != self.bins {
            return Err(EnvelopeError::BinLength {
                expected: self.bins,
                actual: bins.len(),
            });
        }
        if out.len() != self.bins {
            return Err(EnvelopeError::BinLength {
                expected: self.bins,
                actual: out.len(),
            });
        }
        self.estimator.estimate(bins, out);
        Ok(())
    }

    /// Pre-build the plan for a given window size, so the first
    /// [`estimate_frame`](EnvelopeAnalyzer::estimate_frame) call does not have
    /// to allocate.
    pub fn prepare(&mut self, window_size: usize) -> Result<(), EnvelopeError> {
        let bins = window_size / 2 + 1;
        if bins == 0 {
            return Err(EnvelopeError::EmptySpectrum);
        }
        self.ensure_geometry(window_size, bins, (window_size / 4).max(1))
    }

    /// Bins per frame for the current geometry.
    pub fn bins(&self) -> usize {
        self.bins
    }

    /// Window size of the current geometry.
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    fn ensure_geometry(
        &mut self,
        window_size: usize,
        bins: usize,
        hop: usize,
    ) -> Result<(), EnvelopeError> {
        if window_size < 8 || bins != window_size / 2 + 1 {
            return Err(EnvelopeError::EmptySpectrum);
        }
        if self.window_size == window_size && self.bins == bins {
            self.hop = hop.max(1);
            return Ok(());
        }
        self.window_size = window_size;
        self.bins = bins;
        self.hop = hop.max(1);
        self.estimator.reset(window_size, bins);
        Ok(())
    }

    /// Centred rectangular average across frames, computed out of place in
    /// `smoothing_scratch` so the result is symmetric and order-independent.
    fn smooth_frames(&mut self, envelope: &mut SpectralEnvelope) {
        let taps = self.smoothing_taps();
        let frames = envelope.frames();
        let bins = envelope.bins();
        if taps <= 1 || frames == 0 {
            return;
        }
        self.smoothing_scratch.clear();
        self.smoothing_scratch
            .extend_from_slice(&envelope.data[..frames * bins]);
        let scratch = &self.smoothing_scratch;
        let radius = taps / 2;
        for frame in 0..frames {
            let low = frame.saturating_sub(radius);
            let high = (frame + radius).min(frames - 1);
            let count = (high - low + 1) as f32;
            let destination = &mut envelope.data[frame * bins..(frame + 1) * bins];
            destination.fill(0.0);
            for source_frame in low..=high {
                let source = &scratch[source_frame * bins..(source_frame + 1) * bins];
                for (slot, &value) in destination.iter_mut().zip(source) {
                    *slot += value;
                }
            }
            for slot in destination.iter_mut() {
                *slot /= count;
            }
        }
    }
}

#[cfg(test)]
mod tests;
