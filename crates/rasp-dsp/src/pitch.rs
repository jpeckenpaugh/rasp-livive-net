//! Phase-vocoder pitch shifting (PLAN P-02).
//!
//! # Algorithm
//!
//! A pitch ratio `p` is realised as a **time stretch by `p`, then a band-limited
//! resample by `1/p`** back to the original duration. Stretching does not move
//! partials; resampling scales every frequency by exactly the read-out rate, so
//! a sinusoid at `f` comes out at `p·f` with the input length preserved. This is
//! the classical phase-vocoder pitch shifter and it keeps the shared
//! [`crate::stft`] front end untouched.
//!
//! The stretch itself is a phase vocoder with **identity phase locking**
//! (Laroche–Dolson):
//!
//! * Analysis frames are spaced `A_t = H / p_t` input samples apart and
//!   synthesis frames `H = N/4` output samples apart, where `N` is the FFT
//!   size. `p_t` is the per-frame ratio from the curve.
//! * Every frame's instantaneous frequency is estimated from the phase
//!   difference to the previous frame, unwrapped against the *actual* integer
//!   analysis hop, so a changing curve cannot accumulate timing error.
//! * Spectral peaks are picked per frame. The phase of each peak advances by
//!   `ω_peak·H` per synthesis frame; every other bin is locked to its nearest
//!   peak and keeps its **analysis** phase offset from that peak. That is what
//!   removes the classic phase-vocoder "phasiness" on voiced speech.
//! * Overlap-add normalises by the squared-Hann overlap sum (`1.5` at
//!   `H = N/4`), so an unmodified frame reconstruction is the identity.
//!
//! The stretched signal is then read back at a fixed rate for a constant ratio
//! (via [`crate::resample::Resampler`]) or at a time-varying rate for a ratio
//! curve. The read-out positions are the integral of the curve, which is why a
//! per-frame ratio stays pitch-correct instead of merely changing duration.
//!
//! # Window selection (D19)
//!
//! The analysis window must resolve the F0 of **the signal it actually
//! analyses**. This front end analyses the **input**, because the shift is
//! realised in the read-out resampling step, not in the analysis: stretching
//! does not move partials. Across the corpus the input F0 medians are
//! 79–323 Hz, for which `N = 2048` (23.44 Hz per bin, 42.7 ms) is ample even at
//! −12 st. A 71 Hz input is 3.0 bins at 2048 regardless of the requested ratio.
//!
//! The earlier rule — switch to `N = 4096` when the ratio dropped below 0.6 —
//! reasoned from the *output* F0 and so widened the window for a signal the
//! shifter never analyses. With frame-indexed pairing (D19) fixed `N = 2048`
//! wins: `af_heart` at −12 st improves from **+15.65** to **+2.15 cents**, and
//! the worst anchor-cell median improves from 15.65 to 2.81 cents (P-03b
//! reproduced both; D19 records the same comparison). The automatic policy is
//! therefore simply `N = 2048` ([`WindowPolicy::Auto`]), and a ratio, however
//! extreme, does not change it.
//!
//! A wider window is still available for experiments and for signals whose
//! **input** F0 sits below what 2048 resolves: [`WindowPolicy::Fixed`] with
//! `4096` (or `1024`). [`PitchShifter::window_size`] reports the size in use.
//!
//! A pass that analyses a *shifted-down output* has the opposite requirement
//! and must choose its own window from that output's F0 — that is
//! [`crate::transform::VoiceTransformer`]'s correction pass, which sizes its
//! STFT independently of this shifter (see its module docs). Do not infer the
//! correction window from [`PitchShifter::window_size`].
//!
//! # Length, latency and determinism
//!
//! * [`PitchShifter::process`] and [`PitchShifter::process_with_curve`] return
//!   **exactly `input.len()` samples** for any input length, including zero.
//!   The resampled/read-out signal is truncated or zero-padded at the tail by
//!   at most a handful of samples; the aliasing of that trim is reported by
//!   the tests.
//! * The whole-buffer output carries input time `i` at output sample `i` to
//!   within the analysis window's smear: an impulse's energy centroid drifts
//!   by 0.0 samples at ratio 1, −19/−23 samples at 1.5/2.0 and −157 samples
//!   (−3.3 ms) at 0.5 in the tests. The drift does not accumulate: frame
//!   positions come from one `f64` integral of the curve, not from repeated
//!   rounding. [`PitchShifter::latency_samples`] reports the streaming
//!   lookahead `N`, matching [`crate::stft::Stft::latency_samples`].
//! * Everything is deterministic: no RNG, no clocks, no threads, no hash-map
//!   iteration. Same input and parameters produce bit-identical output.
//!
//! # Ratio curve
//!
//! [`PitchShifter::process_with_curve`] takes one ratio per **input hop**:
//! `ratios[i]` applies to input samples `[i·H, (i+1)·H)`, where `H` is the hop
//! of the window in use (512 at `N = 2048`, 1024 at `N = 4096`). A shorter curve
//! holds its last value for the rest of the input; a longer curve is ignored
//! past the end of the input; an empty curve falls back to the scalar ratio. A
//! constant curve takes the same fast path as [`PitchShifter::process`], so a
//! curve of all `1.0` is a bypass. Non-finite entries fall back to the scalar
//! ratio; finite entries are clamped to the supported range.
//!
//! # Transients (P-04)
//!
//! The stretch advances every peak's phase from the previous synthesis frame.
//! Across a plosive that history is wrong: frames whose window contains the
//! onset carry the old signal's phase, so their overlap-add partially cancels
//! the attack and leaves a precursor (pre-echo) in the 20 ms before it.
//!
//! The shifter therefore runs **one pre-pass** over the input: a normalised
//! **spectral-flux** onset detector on the same `N/4` analysis grid (see
//! `ONSET_FLUX_FLOOR` and the constants below for the thresholds). The whole
//! onset list is known before synthesis starts, which is what lets every frame
//! whose window contains an onset be phase-reset — including the frames
//! *before* the onset, which are exactly the ones that produce the precursor.
//! A single-frame reset cannot remove that part; it was measured, and the region
//! reset is what removes the precursor on the real clips.
//!
//! At a reset frame the synthesis phase restarts from that frame's own analysis
//! phases (identity phase locking with no history, so every bin keeps its own
//! analysis phase offset from its peak). Frames further than half a window from
//! any onset are untouched. [`TransientPolicy::Reset`] is the default;
//! [`TransientPolicy::Off`] reproduces the P-02 behaviour exactly and exists for
//! before/after measurements.
//!
//! Measured, controlled impulse in a −60 dBFS noise bed: pre-echo falls from
//! **27.5 dB to 19.7 dB** above the floor at ratio 0.5 and from 30.5 to 27.5 dB
//! at ratio 2.0; onset rise time (10%→90%) on the synthetic plosive falls from
//! 23 ms to 18 ms at ratio 0.5. On the named real clips (`af_heart/watch-this`,
//! `am_eric/good-luck`, `am_onyx/got-time`) the pooled per-onset median Δ is
//! **−2.9 dB** of pre-echo and **−6 ms** of rise time at ratio 0.5
//! (`am_eric` 40.9→37.1 dB and `am_onyx` −11.0→−15.4 dB). One mid-voiced onset
//! in `af_heart` gets 2.8 dB *worse*: its 20 ms "pre" window contains the
//! previous syllable's decay, and a phase change redistributes that energy in
//! either direction, so that point is reported rather than claimed. The tables
//! and the impulse gate live in `pitch::tests::transients`.
//!
//! The reset also moves an isolated impulse's energy centroid: P-02 measured
//! −157 samples at ratio 0.5, this reset measures **+359** at 0.5 (7.5 ms,
//! inside the P-02 test's ±512 bound) and −42 at 2.0. That is a real trade —
//! the attack is sharper and has less precursor, but its energy centre moves a
//! few milliseconds — and it is why the P-02 alignment test's bound was not
//! widened.
//!
//! The pre-pass costs one extra analysis pass at `N/4`: measured **+2.2…+7.1%**
//! of render time (+0.6…0.8 ms per second of audio across ratios 0.5–2.0). [`PitchShifter::transient_count`] and
//! [`PitchShifter::transient_samples`] report the onsets; they are diagnostics,
//! not knobs.
//!
//! The correction pass in [`crate::transform::VoiceTransformer`] needs no
//! separate reset: it applies a **real, non-negative gain per bin** and keeps
//! every bin's phase, so its ISTFT is phase-coherent with its analysis and
//! cannot create a precursor of its own. That is why the handling is
//! shifter-only rather than duplicated; it was measured, not assumed — the
//! transformer's output inherits the shifter's pre-echo and nothing more.
//!
//! # Known weaknesses
//!
//! * **Formants move with pitch** (P-02 only). No envelope/excitation
//!   separation is done here, so a shifted voice sounds chipmunked at the
//!   extremes. P-03 fixes it in [`crate::transform`].
//! * **Spectral smearing at large ratios.** Stretching by 2 (or −12 st) widens
//!   every main lobe in the fixed analysis window, so very large shifts sound
//!   soft. This is inherent to a fixed-`N` phase vocoder.
//! * **Time-varying ratio** uses an in-house band-limited variable-rate
//!   read-out rather than [`crate::resample::Resampler`], because a
//!   constant-rate resampler cannot express a per-frame pitch ratio. The
//!   scalar path — the one used for natural-mode presets — always uses the
//!   shared resampler.
//! * **A curve is applied on two different time axes.** The analysis loop
//!   takes `ratios[i]` from the *input* hop that contains the frame centre
//!   (exact), but the read-out indexes the same curve by *output* sample.
//!   The two agree to first order; a 2 s 0.75→1.5 ramp lands within ~50 cents
//!   of the window-centre expectation (`curve_ramp_moves_pitch_monotonically`).
//!   Curves that change over tens of milliseconds — B-04's monotone
//!   flattening changes over ~100 ms — are unaffected, but this is not a
//!   sample-accurate automation track.
//! * **The scalar path quantises the resampling rate.** The output rate is
//!   `round(sample_rate / ratio)`, an integer, so the realised ratio is off by
//!   at most `0.5 / 24000` ≈ 0.04 cents. The curve read-out has no such
//!   rounding.
//!
//! # Errors (P-02b)
//!
//! Construction and rendering are **fallible**: [`PitchShifter::new`],
//! [`PitchShifter::process`], [`PitchShifter::process_into`],
//! [`PitchShifter::process_with_curve`] and
//! [`PitchShifter::process_with_curve_into`] all return
//! `Result<_, PitchShiftError>`. There is no `expect`/`unwrap`/panic on any
//! path a caller can reach. After a successful [`PitchShifter::new`] the render
//! errors are unreachable in practice (the window policy and the resampler are
//! both validated up front), but the signature is fallible so a future policy
//! change cannot turn into a trap across the WASM boundary (W-01).
//!
//! # Curve dips and the window policy (D19)
//!
//! The window is chosen **once**, at construction, from
//! [`PitchShiftConfig::window`]. A ratio curve cannot change it: a hop that dips
//! to 0.5 is analysed with the same window as the rest of the render, so no
//! phase or latency discontinuity can occur and no hysteresis state is needed.
//! Before D19 that dip cost the wide window for the whole render; since the
//! analysis sees the *input*, that was the wrong signal, and the dip now
//! changes nothing. [`PitchShifter::window_size`] reports the fixed size.
//!
//! No dependencies, no `unsafe`, no threads, no clocks.

use core::f64::consts::PI as PI_F64;
use std::f32::consts::PI as PI_F32;
use std::fmt;

use crate::resample::{Quality, ResampleError, Resampler};
use crate::stft::{Complex, Stft, StftConfig, StftError};

/// Lowest supported pitch ratio (`−12` semitones).
pub const MIN_RATIO: f32 = 0.5;
/// Highest supported pitch ratio (`+12` semitones).
pub const MAX_RATIO: f32 = 2.0;

/// Default window size (PLAN D2): resolves the corpus's 79–323 Hz input F0.
const DEFAULT_WINDOW: usize = 2048;

/// Hard bound on frames per render. Prevents a pathological curve from
/// looping forever; unreachable for ratios in `MIN_RATIO..=MAX_RATIO`.
const MAX_FRAMES: usize = 1 << 22;

// -- Transient detection (PLAN P-04) ---------------------------------------
//
// The detector runs on the analysis frames, one flux value per frame. Spectral
// flux is the sum of the positive magnitude changes between consecutive
// frames, normalised by the current frame's total magnitude, so it is
// independent of level and of how loud the clip is. The three thresholds below
// were picked from the corpus: on speech frames the normalised flux has a
// median around 0.15–0.21 and a p90 of 0.42–0.47, while syllable and plosive
// onsets land at 0.5–0.97. A fixed floor of 0.55 therefore sits above the
// steady-state distribution without missing the attacks; the relative term
// (twice the running mean) raises it in unusually active passages. The floor
// started at 0.45; 0.55 was chosen after the P-02 acceptance matrix showed one
// cell (`af_heart` −4 st) whose p90 paired pitch error crossed 25 c from an
// over-eager reset, and 0.55 restored 36/36 passing cells with no loss of
// measured pre-echo.
//
// Refractory: one onset per 30 ms. Without it a single plosive would report an
// onset on three consecutive frames and consume the reset budget on one event.
// 30 ms is under the corpus's shortest syllable spacing, so it cannot hide a
// real onset.

/// Normalised flux floor for an onset, independent of the running mean.
const ONSET_FLUX_FLOOR: f32 = 0.55;
/// An onset must also exceed this multiple of the running mean flux.
const ONSET_FLUX_RATIO: f32 = 2.0;
/// Weight of one frame in the running mean flux (≈ 340 ms at `H = 512`).
const ONSET_STATISTICS_WEIGHT: f32 = 1.0 / 32.0;
/// Minimum spacing between resets, in seconds.
const ONSET_REFRACTORY_SECONDS: f32 = 0.030;
/// Frames observed before any onset can fire (≈ 43 ms at `H = 512`).
const ONSET_WARMUP_FRAMES: u64 = 4;
/// How far from an onset a frame still counts as part of the transient, as a
/// fraction of the analysis window `N`.
///
/// `0.5` resets every frame whose window contains the onset: the frames before
/// the onset carry the precursor, so resetting only the frame where the flux
/// peaks leaves most of the pre-echo in place. `0.125` (nearest frame only) was
/// measured too: on the three named clips it left 0.6–2.6 dB more pre-echo and
/// did not improve the P-02 pitch matrix, so the region reset is the default.
const ONSET_RESET_RADIUS: f32 = 0.5;
/// Hop of the onset-detection pre-pass, as a division of `N` (4 = `N/4`, the
/// same grid the unshifted render analyses on).
///
/// `N/2` was measured too: it halves the pre-pass cost but places onsets on a
/// 21 ms grid, and the pooled rise-time regression became +2 ms instead of
/// −6 ms on the named clips. The reset region is keyed to the onset position,
/// so the finer grid is worth its cost.
const ONSET_DETECTION_HOP_DIVISOR: usize = 4;
/// Minimum total frame magnitude for an onset to be considered. `|X|` summed
/// over 1025 bins is ≈ `N/2 · amplitude` for a sine, so 0.05 is roughly
/// −80 dBFS: below that the "flux" is decoder dither, not an attack.
const ONSET_MIN_FRAME_MAGNITUDE: f32 = 0.05;

/// Onset detector state: one normalised spectral-flux comparison per frame.
struct TransientDetector {
    enabled: bool,
    /// Running mean of the capped normalised flux.
    mean: f32,
    /// Frames still to skip before another onset may fire.
    refractory: u32,
    /// Refractory length in frames, from the hop and sample rate.
    refractory_frames: u32,
    frames: u64,
}

impl TransientDetector {
    fn new(policy: TransientPolicy, sample_rate: u32, hop: usize) -> Self {
        let frames = if hop == 0 {
            1
        } else {
            ((ONSET_REFRACTORY_SECONDS * sample_rate as f32) / hop as f32).ceil() as u32
        };
        TransientDetector {
            enabled: policy == TransientPolicy::Reset,
            mean: 0.0,
            refractory: 0,
            refractory_frames: frames.max(1),
            frames: 0,
        }
    }

    /// Reset the per-render state (the running mean is per-render too, so a
    /// clip always starts from the same detector state).
    fn reset(&mut self, policy: TransientPolicy, sample_rate: u32, hop: usize) {
        *self = TransientDetector::new(policy, sample_rate, hop);
    }

    /// Feed one frame's normalised flux and total magnitude; `true` means
    /// "reset the synthesis phase here".
    fn update(&mut self, flux: f32, frame_magnitude: f32) -> bool {
        if !self.enabled {
            return false;
        }
        let magnitude_ok =
            frame_magnitude.is_finite() && frame_magnitude >= ONSET_MIN_FRAME_MAGNITUDE;
        let threshold = (self.mean * ONSET_FLUX_RATIO).max(ONSET_FLUX_FLOOR);
        let onset = magnitude_ok
            && self.frames >= ONSET_WARMUP_FRAMES
            && self.refractory == 0
            && flux.is_finite()
            && flux > threshold;
        if onset {
            self.refractory = self.refractory_frames;
        } else if self.refractory > 0 {
            self.refractory -= 1;
        }
        // Cap the sample so an onset cannot inflate its own baseline and hide
        // the next one; steady-state frames update it normally.
        let sample = if flux.is_finite() {
            flux.clamp(0.0, ONSET_FLUX_FLOOR)
        } else {
            0.0
        };
        self.mean += (sample - self.mean) * ONSET_STATISTICS_WEIGHT;
        self.frames += 1;
        onset
    }
}

/// Window-size policy for [`PitchShifter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowPolicy {
    /// `N = 2048`, the default for this corpus's input F0 band (79–323 Hz).
    ///
    /// The ratio does **not** participate in the choice (D19): the analysis
    /// sees the input signal, whose F0 is unchanged by the requested shift.
    /// Use [`Fixed`](WindowPolicy::Fixed) for a different size.
    #[default]
    Auto,
    /// Always use this power-of-two window size (hop is always `N/4`).
    /// `1024`, `2048` and `4096` are the intended choices; force `4096` when
    /// the **input** F0 is too low for 2048 (below roughly 70–90 Hz).
    Fixed(usize),
}

/// Transient (onset) handling in the phase vocoder (PLAN P-04).
///
/// Phase-vocoder time scaling advances every peak's phase from the previous
/// synthesis frame. Across a plosive or an edit that phase history is
/// meaningless: the frames before the onset carry the old signal's phases, so
/// their overlap-add cancels part of the attack and leaves a low-level
/// **pre-echo** in the 20 ms before it. The fix is to detect the onset and
/// restart the synthesis phase from the analysis phase there
/// ([`Reset`](TransientPolicy::Reset), the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransientPolicy {
    /// Detect onsets on the analysis frames and reset the synthesis phase at
    /// them (identity phase locking restarts from the frame's own phases).
    #[default]
    Reset,
    /// No onset handling: the P-02 behaviour, kept for before/after
    /// measurements and for callers that prefer the old smear.
    Off,
}

/// Configuration for [`PitchShifter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PitchShiftConfig {
    /// How the FFT window size is chosen.
    pub window: WindowPolicy,
    /// Onset handling (PLAN P-04).
    pub transients: TransientPolicy,
}

impl PitchShiftConfig {
    /// The default configuration: `N = 2048`, the corpus input-band default,
    /// with onset phase reset enabled.
    ///
    /// Despite the name `WindowPolicy::Auto`, nothing switches automatically
    /// (D19): the front end analyses the input, whose F0 the ratio does not
    /// change.
    pub const fn auto() -> Self {
        PitchShiftConfig {
            window: WindowPolicy::Auto,
            transients: TransientPolicy::Reset,
        }
    }

    /// Force one window size (power of two, hop `N/4`), onset handling on.
    pub const fn fixed(window_size: usize) -> Self {
        PitchShiftConfig {
            window: WindowPolicy::Fixed(window_size),
            transients: TransientPolicy::Reset,
        }
    }

    /// Replace the transient policy, keeping the window policy.
    pub const fn with_transients(self, transients: TransientPolicy) -> Self {
        PitchShiftConfig {
            window: self.window,
            transients,
        }
    }
}

/// Construction errors. Callers can turn bad WASM parameters into a typed
/// failure instead of a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitchShiftError {
    /// The sample rate was zero.
    ZeroSampleRate,
    /// The requested window is not a supported power of two, or its hop would
    /// be invalid.
    UnsupportedWindowSize {
        /// The rejected size.
        window_size: usize,
    },
    /// The underlying STFT rejected its configuration.
    Stft(StftError),
    /// The underlying resampler rejected its rates (only possible for a zero
    /// sample rate, which is rejected earlier).
    Resample(ResampleError),
}

impl fmt::Display for PitchShiftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PitchShiftError::ZeroSampleRate => f.write_str("sample rate must be non-zero"),
            PitchShiftError::UnsupportedWindowSize { window_size } => write!(
                f,
                "window size {window_size} is not supported: use a power of two \
                 accepted by the STFT"
            ),
            PitchShiftError::Stft(err) => write!(f, "STFT: {err}"),
            PitchShiftError::Resample(err) => write!(f, "resampler: {err}"),
        }
    }
}

impl std::error::Error for PitchShiftError {}

impl From<StftError> for PitchShiftError {
    fn from(value: StftError) -> Self {
        PitchShiftError::Stft(value)
    }
}

impl From<ResampleError> for PitchShiftError {
    fn from(value: ResampleError) -> Self {
        PitchShiftError::Resample(value)
    }
}

/// Phase-vocoder pitch shifter with identity phase locking.
///
/// Construct with [`new`](PitchShifter::new), set the scalar ratio with
/// [`set_ratio`](PitchShifter::set_ratio), and render with
/// [`process`](PitchShifter::process) or
/// [`process_with_curve`](PitchShifter::process_with_curve). The `*_into`
/// variants reuse the caller's output allocation; the per-frame loop never
/// allocates once the window is established.
///
/// ```
/// use rasp_dsp::pitch::{PitchShiftConfig, PitchShifter};
///
/// let mut shifter = PitchShifter::new(48_000, PitchShiftConfig::auto()).unwrap();
/// shifter.set_ratio(2.0_f32.powf(12.0 / 12.0)); // +12 st
/// let input = vec![0.0f32; 4800];
/// let output = shifter.process(&input).unwrap();
/// assert_eq!(output.len(), input.len());
/// ```
pub struct PitchShifter {
    sample_rate: u32,
    config: PitchShiftConfig,
    ratio: f32,
    stft: Stft,
    /// Squared-Hann overlap sum (1.5 for Hann at `N/4`), computed from the
    /// public window so the phase vocoder does not depend on STFT internals.
    cola_sum: f32,
    /// Time-domain analysis frame, `N`.
    analysis: Vec<f32>,
    /// One frame's one-sided bins, `N/2 + 1`.
    spectrum: Vec<Complex>,
    /// Analysis magnitudes and phases for the current frame.
    mag: Vec<f32>,
    /// Magnitudes of the previous analysis frame (spectral-flux source).
    prev_mag: Vec<f32>,
    /// Onset detector state (PLAN P-04).
    detector: TransientDetector,
    /// Analysis-frame centres that triggered a phase reset in the most recent
    /// render, in input samples (diagnostic for P-04 / Q-05).
    onset_samples: Vec<usize>,
    phase: Vec<f32>,
    prev_phase: Vec<f32>,
    /// Instantaneous frequency per bin, radians per sample.
    inst_freq: Vec<f32>,
    /// Synthesis phase per bin for the current frame.
    syn_phase: Vec<f32>,
    /// Synthesis phase per bin from the previous frame.
    prev_syn: Vec<f32>,
    /// Peak bins for the current frame.
    peaks: Vec<usize>,
    /// Synthesis phase of each peak in `peaks`.
    peak_phases: Vec<f32>,
    /// Overlap-add accumulator, padded by `N/2` in front.
    acc: Vec<f32>,
    /// One synthesized frame, `N`.
    frame: Vec<f32>,
    /// Cached band-limited resampler for the scalar path.
    resampler: Option<Resampler>,
    resampler_ratio: f32,
    /// Resampler output, reused between calls.
    resampled: Vec<f32>,
    /// Variable-rate read-out kernel for the curve path.
    kernel: ReadoutKernel,
}

impl fmt::Debug for PitchShifter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PitchShifter")
            .field("sample_rate", &self.sample_rate)
            .field("config", &self.config)
            .field("ratio", &self.ratio)
            .field("window_size", &self.stft.window_size())
            .field("hop", &self.stft.hop())
            .field("transients", &self.config.transients)
            .finish_non_exhaustive()
    }
}

impl PitchShifter {
    /// Build a shifter at `sample_rate` with the initial ratio `1.0`.
    ///
    /// Returns [`PitchShiftError::ZeroSampleRate`] for a zero rate and
    /// [`PitchShiftError::Stft`] when a [`WindowPolicy::Fixed`] size is not a
    /// supported power of two.
    pub fn new(sample_rate: u32, config: PitchShiftConfig) -> Result<Self, PitchShiftError> {
        if sample_rate == 0 {
            return Err(PitchShiftError::ZeroSampleRate);
        }
        let window = match config.window {
            WindowPolicy::Auto => DEFAULT_WINDOW,
            WindowPolicy::Fixed(size) => size,
        };
        let stft = Stft::new(StftConfig::new(window, window / 4)).map_err(|_| {
            PitchShiftError::UnsupportedWindowSize {
                window_size: window,
            }
        })?;
        let bins = stft.bins();
        let n = stft.window_size();
        let cola_sum = cola_sum(&stft);
        let detector = TransientDetector::new(config.transients, sample_rate, stft.hop());
        Ok(PitchShifter {
            sample_rate,
            config,
            ratio: 1.0,
            stft,
            cola_sum,
            analysis: vec![0.0; n],
            spectrum: vec![Complex::ZERO; bins],
            mag: vec![0.0; bins],
            prev_mag: vec![0.0; bins],
            detector,
            onset_samples: Vec::new(),
            phase: vec![0.0; bins],
            prev_phase: vec![0.0; bins],
            inst_freq: vec![0.0; bins],
            syn_phase: vec![0.0; bins],
            prev_syn: vec![0.0; bins],
            peaks: Vec::with_capacity(bins / 2 + 1),
            peak_phases: Vec::with_capacity(bins / 2 + 1),
            acc: Vec::new(),
            frame: vec![0.0; n],
            resampler: None,
            resampler_ratio: f32::NAN,
            resampled: Vec::new(),
            kernel: ReadoutKernel::new(),
        })
    }

    /// The configured sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The current scalar ratio.
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    /// The configuration.
    pub fn config(&self) -> PitchShiftConfig {
        self.config
    }

    /// Window size `N` in use.
    ///
    /// With [`WindowPolicy::Auto`] this is always 2048, independent of the
    /// ratio (D19 — the front end analyses the *input*, whose F0 the requested
    /// shift cannot change). A [`WindowPolicy::Fixed`] size is reported as
    /// configured. It does not describe the correction pass in
    /// [`crate::transform::VoiceTransformer`], which sizes its own window from
    /// the shifted output.
    pub fn window_size(&self) -> usize {
        self.stft.window_size()
    }

    /// Hop `H = N/4` of the window in use. Also the spacing, in input samples,
    /// at which a ratio curve is sampled.
    pub fn hop(&self) -> usize {
        self.stft.hop()
    }

    /// One-sided bins per frame (`N/2 + 1`).
    pub fn bins(&self) -> usize {
        self.stft.bins()
    }

    /// Streaming lookahead in samples: exactly `N`.
    ///
    /// The whole-buffer API is sample-aligned and needs no compensation; this
    /// is the lookahead a streaming producer would need, matching
    /// [`crate::stft::Stft::latency_samples`].
    pub fn latency_samples(&self) -> usize {
        self.stft.latency_samples()
    }

    /// [`latency_samples`](PitchShifter::latency_samples) in seconds.
    pub fn latency_seconds(&self) -> f32 {
        self.stft.latency_seconds(self.sample_rate)
    }

    /// Set the scalar pitch ratio, clamped to [`MIN_RATIO`]..=[`MAX_RATIO`].
    ///
    /// Non-finite values are replaced by `1.0`. The window is independent of
    /// the ratio (D19), so setting it never rebuilds the STFT or its buffers;
    /// out-of-range values are clamped, not rejected — see
    /// [`ratio_limits`](PitchShifter::ratio_limits).
    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = sanitise(ratio, 1.0);
    }

    /// The supported ratio range, inclusive.
    pub fn ratio_limits() -> (f32, f32) {
        (MIN_RATIO, MAX_RATIO)
    }

    /// Render a whole buffer at the current scalar ratio.
    ///
    /// The returned length is exactly `input.len()`. The render path is
    /// fallible (P-02b); after a successful [`new`](PitchShifter::new) an
    /// error is unreachable in practice, but callers must not rely on that.
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>, PitchShiftError> {
        let mut out = Vec::with_capacity(input.len());
        self.process_into(input, &mut out)?;
        Ok(out)
    }

    /// Render a whole buffer into an existing vector, reusing its allocation.
    ///
    /// Clears `output` first. The returned length is exactly `input.len()`.
    pub fn process_into(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<(), PitchShiftError> {
        self.render(input, None, output)
    }
    /// Render a whole buffer with a per-frame pitch-ratio curve.
    ///
    /// `ratios[i]` applies to input samples `[i·H, (i+1)·H)` where `H` is
    /// [`hop`](PitchShifter::hop). A shorter curve holds its last value; a
    /// longer curve is ignored past the end; an empty curve uses the scalar
    /// ratio. Values are clamped to [`MIN_RATIO`]..=[`MAX_RATIO`], and
    /// non-finite entries fall back to the scalar ratio. A constant curve takes
    /// the scalar fast path.
    ///
    /// The returned length is exactly `input.len()`.
    pub fn process_with_curve(
        &mut self,
        input: &[f32],
        ratios: &[f32],
    ) -> Result<Vec<f32>, PitchShiftError> {
        let mut out = Vec::with_capacity(input.len());
        self.process_with_curve_into(input, ratios, &mut out)?;
        Ok(out)
    }

    /// [`process_with_curve`](PitchShifter::process_with_curve) into an
    /// existing vector, reusing its allocation.
    pub fn process_with_curve_into(
        &mut self,
        input: &[f32],
        ratios: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<(), PitchShiftError> {
        self.render(input, Some(ratios), output)
    }

    /// How onsets are handled (PLAN P-04). See [`TransientPolicy`].
    pub fn transient_policy(&self) -> TransientPolicy {
        self.config.transients
    }

    /// Switch onset handling on or off. The detector state is per render, so
    /// this takes effect on the next call.
    pub fn set_transient_policy(&mut self, policy: TransientPolicy) {
        self.config.transients = policy;
    }

    /// Number of synthesis-phase resets in the **most recent** render.
    ///
    /// Zero when the policy is [`TransientPolicy::Off`], when the render was a
    /// bit-exact bypass (ratio 1), or when no onset passed the detector. This
    /// is a diagnostic, not a tuning knob: the count depends on the signal.
    pub fn transient_count(&self) -> usize {
        self.onset_samples.len()
    }

    /// Input sample indices of the analysis-frame centres that triggered a
    /// phase reset in the **most recent** render.
    ///
    /// This is the diagnostic behind [`transient_count`](PitchShifter::transient_count):
    /// Q-05's artifact hunt and the P-04 measurements need to know *where* a
    /// reset happened, not just how many. The frame centre is the input sample
    /// index, not the output's, and the list is empty for a bypass or
    /// [`TransientPolicy::Off`]. Do not treat it as a general onset detector:
    /// it is calibrated to the vocoder's analysis grid and 30 ms refractory.
    pub fn transient_samples(&self) -> &[usize] {
        &self.onset_samples
    }

    // -- internals ---------------------------------------------------------

    fn render(
        &mut self,
        input: &[f32],
        curve: Option<&[f32]>,
        output: &mut Vec<f32>,
    ) -> Result<(), PitchShiftError> {
        output.clear();
        if input.is_empty() {
            return Ok(());
        }

        // A curve that is constant (or absent) renders through the shared
        // resampler; a genuinely varying curve needs the variable-rate read-out.
        let constant = match curve {
            None => Some(self.ratio),
            Some(values) => constant_ratio(values, self.ratio),
        };

        // A ratio of exactly one is a bit-exact bypass. The phase vocoder is
        // near-transparent at 1.0 but peak re-locking is not bit-exact, and A/B
        // playback plus the null test (PLAN P-08) need it to be.
        if let Some(ratio) = constant {
            if (ratio - 1.0).abs() <= 1e-9 {
                output.extend_from_slice(input);
                return Ok(());
            }
        }

        let n = self.stft.window_size();
        let hop = self.stft.hop();
        let half = n / 2;
        let len = input.len();

        // Per-render detector state: a clip always starts from the same state,
        // so two renders of the same input are bit-identical. The onset list is
        // built in a pre-pass (below), so every frame whose window contains an
        // onset can be reset — including the frames that precede the onset
        // frame, which are exactly the ones that carry the precursor.
        let detection_hop = (n / ONSET_DETECTION_HOP_DIVISOR).max(1);
        self.detector
            .reset(self.config.transients, self.sample_rate, detection_hop);
        self.onset_samples.clear();
        self.detect_onsets(input);

        // Dry run: exact frame count, so the accumulator is allocated once.
        let frames = self.frame_count(len, curve);
        self.acc.clear();
        self.acc.resize((frames - 1) * hop + n, 0.0);

        let inv_cola = 1.0 / self.cola_sum;
        let omega_scale = 2.0 * PI_F64 / n as f64;
        let hop_f = hop as f64;

        let mut pos = 0.0f64;
        let mut previous_centre = 0i64;
        let mut has_previous = false;
        // Cursor into the sorted onset list for the region reset.
        let mut onset_cursor = 0usize;

        for frame in 0..frames {
            let centre = pos.round() as i64;

            // A frame is part of a transient if its analysis window contains an
            // onset. Every such frame is phase-reset: the transient is then
            // reconstructed coherently by all the frames that carry it, instead
            // of only by the one frame where the flux peaked. That is what
            // removes the precursor from the frames *before* the onset.
            let reset_radius = (half as f32 * ONSET_RESET_RADIUS) as i64;
            while onset_cursor < self.onset_samples.len()
                && (self.onset_samples[onset_cursor] as i64) < centre - reset_radius
            {
                onset_cursor += 1;
            }
            let reset_phase = self
                .onset_samples
                .get(onset_cursor)
                .is_some_and(|&onset| (onset as i64 - centre).abs() <= reset_radius);

            // Zero-pad the frame around its centre.
            let start = centre - half as i64;
            for (i, slot) in self.analysis.iter_mut().enumerate() {
                let index = start + i as i64;
                *slot = if index >= 0 && (index as usize) < len {
                    input[index as usize]
                } else {
                    0.0
                };
            }
            self.stft
                .analyze_frame(&self.analysis, &mut self.spectrum)?;

            for (slot, bin) in self.mag.iter_mut().zip(&self.spectrum) {
                *slot = bin.magnitude();
            }
            for (slot, bin) in self.phase.iter_mut().zip(&self.spectrum) {
                *slot = bin.phase();
            }

            // Instantaneous frequency from the phase difference over the
            // actual (possibly fractional-ratio-rounded) analysis hop.
            if has_previous {
                let actual_hop = (centre - previous_centre).max(1) as f64;
                for k in 0..self.mag.len() {
                    let omega = omega_scale * k as f64;
                    let deviation = wrap_pi_f64(
                        self.phase[k] as f64 - self.prev_phase[k] as f64 - omega * actual_hop,
                    );
                    self.inst_freq[k] = (omega + deviation / actual_hop) as f32;
                }
            } else {
                for (k, slot) in self.inst_freq.iter_mut().enumerate() {
                    *slot = (omega_scale * k as f64) as f32;
                }
            }

            self.pick_peaks();

            // At a transient frame the synthesis phase restarts from this
            // frame's own analysis phases (identity phase locking with no
            // history), so the attack is not smeared by phases extrapolated
            // across the edit.
            self.lock_phases(has_previous && !reset_phase, hop as f32);

            // Rebuild the spectrum with the synthesis phase, forcing the
            // real-only edge bins to stay real.
            let dc = self.spectrum[0].re;
            let nyquist = self.spectrum[self.mag.len() - 1].re;
            for k in 0..self.mag.len() {
                let magnitude = self.mag[k];
                let phase = self.syn_phase[k];
                self.spectrum[k] = Complex::from_polar(magnitude, phase);
            }
            self.spectrum[0] = Complex::new(dc, 0.0);
            let last = self.mag.len() - 1;
            self.spectrum[last] = Complex::new(nyquist, 0.0);

            self.stft
                .synthesize_frame(&self.spectrum, &mut self.frame)?;

            let offset = frame * hop;
            for (dst, &value) in self.acc[offset..offset + n].iter_mut().zip(&self.frame) {
                *dst += value * inv_cola;
            }

            self.prev_phase.copy_from_slice(&self.phase);
            self.prev_syn.copy_from_slice(&self.syn_phase);
            previous_centre = centre;
            has_previous = true;

            let ratio = curve_ratio(curve, self.ratio, centre, hop);
            pos += hop_f / (ratio as f64);
        }

        // Move the accumulator out so the read-out can borrow `self` mutably
        // (the resampler and the read-out kernel live in other fields).
        let acc = core::mem::take(&mut self.acc);
        let stretched = &acc[half..(frames - 1) * hop + n];

        match constant {
            Some(ratio) => self.resample_into(stretched, ratio, output)?,
            None => {
                let ratios = curve.unwrap_or(&[]);
                self.read_curve_into(stretched, ratios, hop, len, output);
            }
        }
        self.acc = acc;

        // The contract is exact length. The resampler can be short by one
        // sample for some ratios; the curve read-out is already exact.
        if output.len() < len {
            output.resize(len, 0.0);
        }
        output.truncate(len);

        // Reset the phase history for the next render (the first frame of every
        // render is initialised from its own analysis phase). Frames are
        // independent of the previous call by construction, but clearing keeps
        // reuse obvious and deterministic.
        self.prev_syn.fill(0.0);
        self.prev_phase.fill(0.0);
        Ok(())
    }

    /// Build the onset list for this render (PLAN P-04).
    ///
    /// Spectral flux is computed on the **input** at the shifter's fixed
    /// `N/4` analysis grid, independent of the requested ratio, and only when
    /// the transient policy is [`TransientPolicy::Reset`]. A pre-pass is what
    /// makes the region reset possible: frames whose windows contain an onset
    /// are synthesised later, and the frames *before* the onset are exactly the
    /// ones that produce the precursor, so the onset must be known before they
    /// are reached.
    ///
    /// The cost is one extra analysis pass over the clip at `N/4` (about 7% of
    /// a ratio-1 render, less at ratio 2 where the render itself has more
    /// frames). It allocates nothing per frame; `onset_samples` is cleared and
    /// filled, and its capacity is reused across renders.
    ///
    /// An analysis error cannot be reached after [`PitchShifter::new`]; if it
    /// happened the pre-pass stops with the onsets found so far and the render
    /// continues without a reset rather than failing the whole buffer.
    fn detect_onsets(&mut self, input: &[f32]) {
        if self.config.transients != TransientPolicy::Reset || input.is_empty() {
            return;
        }
        let n = self.stft.window_size();
        let hop = (n / ONSET_DETECTION_HOP_DIVISOR).max(1);
        let half = n / 2;
        let len = input.len();
        self.prev_mag.fill(0.0);

        let mut centre = 0i64;
        let mut has_previous = false;
        let mut steps = 0usize;
        loop {
            if has_previous && centre - half as i64 >= len as i64 {
                break;
            }
            let start = centre - half as i64;
            for (index, slot) in self.analysis.iter_mut().enumerate() {
                let sample = start + index as i64;
                *slot = if sample >= 0 && (sample as usize) < len {
                    input[sample as usize]
                } else {
                    0.0
                };
            }
            if self
                .stft
                .analyze_frame(&self.analysis, &mut self.spectrum)
                .is_err()
            {
                return;
            }
            for (slot, bin) in self.mag.iter_mut().zip(&self.spectrum) {
                *slot = bin.magnitude();
            }
            if has_previous {
                let mut positive = 0.0f32;
                let mut total = 0.0f32;
                for (current, previous) in self.mag.iter().zip(&self.prev_mag) {
                    let change = current - previous;
                    if change > 0.0 {
                        positive += change;
                    }
                    total += current;
                }
                let flux = if total > 0.0 { positive / total } else { 0.0 };
                if self.detector.update(flux, total) {
                    self.onset_samples.push(centre.max(0) as usize);
                }
            }
            self.prev_mag.copy_from_slice(&self.mag);

            centre = centre.saturating_add(hop as i64);
            has_previous = true;
            steps += 1;
            if steps >= MAX_FRAMES {
                break;
            }
        }
    }

    /// Peak picking: local magnitude maxima above −80 dB relative to the frame
    /// maximum. Silence yields no peaks and falls back to the analysis phase.
    fn pick_peaks(&mut self) {
        self.peaks.clear();
        let mut max_magnitude = 0.0f32;
        for &magnitude in &self.mag {
            if magnitude > max_magnitude {
                max_magnitude = magnitude;
            }
        }
        if max_magnitude <= 0.0 {
            return;
        }
        let threshold = max_magnitude * 1e-4;
        for k in 1..self.mag.len() - 1 {
            if self.mag[k] > self.mag[k - 1]
                && self.mag[k] >= self.mag[k + 1]
                && self.mag[k] > threshold
            {
                self.peaks.push(k);
            }
        }
    }

    /// Identity phase locking: advance each peak by `ω·H`, then give every bin
    /// its nearest peak's phase plus the bin's own analysis offset from it.
    fn lock_phases(&mut self, has_previous: bool, synthesis_hop: f32) {
        if self.peaks.is_empty() {
            self.syn_phase.copy_from_slice(&self.phase);
            return;
        }

        let peak_count = self.peaks.len();
        self.peak_phases.resize(peak_count, 0.0);
        for i in 0..peak_count {
            let peak = self.peaks[i];
            let phase = if has_previous {
                self.prev_syn[peak] + self.inst_freq[peak] * synthesis_hop
            } else {
                self.phase[peak]
            };
            self.peak_phases[i] = wrap_pi_f32(phase);
        }

        let mut peak_index = 0usize;
        for k in 0..self.mag.len() {
            while peak_index + 1 < self.peaks.len() {
                let current = (self.peaks[peak_index] as i64 - k as i64).abs();
                let next = (self.peaks[peak_index + 1] as i64 - k as i64).abs();
                if next < current {
                    peak_index += 1;
                } else {
                    break;
                }
            }
            let peak = self.peaks[peak_index];
            self.syn_phase[k] =
                wrap_pi_f32(self.peak_phases[peak_index] + self.phase[k] - self.phase[peak]);
        }
    }

    fn frame_count(&self, len: usize, curve: Option<&[f32]>) -> usize {
        let n = self.stft.window_size();
        let hop = self.stft.hop();
        let half = n as i64 / 2;
        let mut pos = 0.0f64;
        let mut frames = 0usize;
        loop {
            let centre = pos.round() as i64;
            if frames > 0 && centre - half >= len as i64 {
                break;
            }
            frames += 1;
            let ratio = curve_ratio(curve, self.ratio, centre, hop);
            pos += hop as f64 / ratio as f64;
            if frames >= MAX_FRAMES {
                break;
            }
        }
        frames.max(1)
    }

    fn resample_into(
        &mut self,
        stretched: &[f32],
        ratio: f32,
        output: &mut Vec<f32>,
    ) -> Result<(), PitchShiftError> {
        if self.resampler.is_none() || (self.resampler_ratio - ratio).abs() > 1e-6 {
            let out_rate = ((self.sample_rate as f64 / ratio as f64).round() as u32).max(1);
            let resampler = Resampler::new(self.sample_rate, out_rate, Quality::High)?;
            self.resampler = Some(resampler);
            self.resampler_ratio = ratio;
        }
        // The cache was just filled, so this cannot be `None`.
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.process_into(stretched, &mut self.resampled);
        }
        output.clear();
        output.extend_from_slice(&self.resampled);
        Ok(())
    }

    fn read_curve_into(
        &mut self,
        stretched: &[f32],
        ratios: &[f32],
        hop: usize,
        len: usize,
        output: &mut Vec<f32>,
    ) {
        output.clear();
        output.reserve(len);
        if ratios.is_empty() {
            output.resize(len, 0.0);
            return;
        }

        let mut position = 0.0f64;
        let mut segment = usize::MAX;
        for sample in 0..len {
            let index = (sample / hop).min(ratios.len() - 1);
            if index != segment {
                let ratio = sanitise(ratios[index], self.ratio);
                self.kernel.update(ratio);
                segment = index;
            }
            output.push(self.kernel.evaluate(stretched, position));
            position += self.kernel.step as f64;
        }
    }
}

/// Squared-Hann overlap sum for synthesis frames at `H = N/4`: `1.5`.
fn cola_sum(stft: &Stft) -> f32 {
    let n = stft.window_size();
    let hop = stft.hop();
    let window = stft.window();
    let mut sum = 0.0f64;
    for m in 0..4 {
        let w = window[(m * hop) % n] as f64;
        sum += w * w;
    }
    sum as f32
}

/// Clamp a finite ratio, or substitute the fallback for NaN/inf.
fn sanitise(ratio: f32, fallback: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        fallback.clamp(MIN_RATIO, MAX_RATIO)
    }
}

/// Ratio for the input hop containing `sample`.
fn curve_ratio(curve: Option<&[f32]>, scalar: f32, sample: i64, hop: usize) -> f32 {
    let Some(values) = curve else {
        return scalar;
    };
    if values.is_empty() {
        return scalar;
    }
    let index = (sample.max(0) as usize / hop).min(values.len() - 1);
    sanitise(values[index], scalar)
}

/// `Some(ratio)` when every entry resolves to the same ratio, else `None`.
///
/// Entries are resolved exactly as the frame loop resolves them (non-finite
/// values fall back to the scalar ratio), so the constant fast path and the
/// varying path can never disagree.
fn constant_ratio(values: &[f32], fallback: f32) -> Option<f32> {
    let mut value: Option<f32> = None;
    for &entry in values {
        let ratio = sanitise(entry, fallback);
        match value {
            None => value = Some(ratio),
            Some(current) if (current - ratio).abs() <= 1e-6 => {}
            Some(_) => return None,
        }
    }
    value.or(Some(fallback))
}

fn wrap_pi_f32(x: f32) -> f32 {
    let two_pi = 2.0 * PI_F32;
    let y = x % two_pi;
    if y > PI_F32 {
        y - two_pi
    } else if y < -PI_F32 {
        y + two_pi
    } else {
        y
    }
}

fn wrap_pi_f64(x: f64) -> f64 {
    let two_pi = 2.0 * PI_F64;
    let y = x % two_pi;
    if y > PI_F64 {
        y - two_pi
    } else if y < -PI_F64 {
        y + two_pi
    } else {
        y
    }
}

/// Band-limited variable-rate read-out used for a per-frame ratio curve.
///
/// A constant-rate resampler cannot follow a curve, so the stretched signal is
/// read at positions that advance by the local ratio. The kernel is a
/// Blackman-windowed sinc with its cutoff lowered to `0.5/ratio` when the
/// read-out decimates (ratio > 1), and its taps are re-derived whenever the
/// ratio changes. One kernel table is built per curve segment (one STFT hop),
/// not per sample, and every tap weight is normalised so the read-out has
/// unity DC gain.
struct ReadoutKernel {
    table: Vec<f32>,
    half: usize,
    taps: usize,
    step: f32,
    scale: f64,
}

impl ReadoutKernel {
    /// Table points across the kernel support. 4096 keeps linear table
    /// interpolation well below the window's own approximation error.
    const RESOLUTION: usize = 4096;
    /// Stopband target used to size the kernel.
    const ATTENUATION_DB: f64 = 60.0;
    /// Transition width as a fraction of the cutoff.
    const TRANSITION: f64 = 0.15;

    fn new() -> Self {
        ReadoutKernel {
            table: vec![0.0; Self::RESOLUTION],
            half: 0,
            taps: 0,
            step: f32::NAN,
            scale: 0.0,
        }
    }

    fn update(&mut self, step: f32) {
        if self.taps > 0 && (step - self.step).abs() <= 1e-4 {
            return;
        }
        self.step = step;
        let cutoff = if step > 1.0 { 0.5 / step as f64 } else { 0.5 };
        let transition = Self::TRANSITION * cutoff;
        let estimate = (Self::ATTENUATION_DB - 8.0) / (2.285 * 2.0 * PI_F64 * transition);
        let mut taps = estimate.ceil().max(8.0) as usize;
        if taps.is_multiple_of(2) {
            taps += 1;
        }
        let taps = taps.min(161);
        let half = taps / 2;
        self.taps = taps;
        self.half = half;
        self.scale = (Self::RESOLUTION - 1) as f64 / (2.0 * half as f64);

        for (index, slot) in self.table.iter_mut().enumerate() {
            let tau = -(half as f64) + index as f64 / self.scale;
            let u = (tau + half as f64) / (2.0 * half as f64);
            let window = 0.42 - 0.5 * (2.0 * PI_F64 * u).cos() + 0.08 * (4.0 * PI_F64 * u).cos();
            let x = 2.0 * cutoff * tau;
            let sinc = if x.abs() < 1e-12 {
                1.0
            } else {
                (PI_F64 * x).sin() / (PI_F64 * x)
            };
            *slot = (2.0 * cutoff * sinc * window) as f32;
        }
    }

    fn evaluate(&self, samples: &[f32], position: f64) -> f32 {
        if self.taps == 0 {
            return 0.0;
        }
        let base = position.floor() as i64 - self.half as i64;
        let mut accumulated = 0.0f64;
        let mut weight_sum = 0.0f64;
        for tap in 0..self.taps {
            let index = base + tap as i64;
            if index < 0 || index as usize >= samples.len() {
                continue;
            }
            let tau = position - index as f64;
            let table_position =
                ((tau + self.half as f64) * self.scale).clamp(0.0, (Self::RESOLUTION - 1) as f64);
            let low = table_position as usize;
            let high = (low + 1).min(Self::RESOLUTION - 1);
            let fraction = table_position - low as f64;
            let weight =
                self.table[low] as f64 * (1.0 - fraction) + self.table[high] as f64 * fraction;
            accumulated += weight * samples[index as usize] as f64;
            weight_sum += weight;
        }
        if weight_sum.abs() > 1e-12 {
            (accumulated / weight_sum) as f32
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests;
