//! Natural-mode parameters, smoothing and player-facing presets (PLAN P-05).
//!
//! D1 defines natural mode as **two independent axes** — pitch in semitones and
//! formant ratio — plus a timbre trim. This module is the product-facing
//! surface for that knob space:
//!
//! * [`NaturalParams`] — the three values, validated and clamped. Non-finite
//!   input never reaches the DSP: it becomes the documented neutral value for
//!   that axis.
//! * [`ParamSmoother`] — a time-based ramp between two parameter points, for a
//!   caller that changes a knob while audio is flowing.
//! * [`NaturalPreset`] — the D12 player-facing names with their operating
//!   points, each measured at *its own* point (see `docs/presets.md`).
//! * [`NaturalRenderer`] — the whole chain the product plays: transform →
//!   loudness policy ([`crate::loudness`]). It is the natural-mode composition;
//!   the robot chain (D8) is not wired here.
//!
//! # Ranges
//!
//! | Parameter | Range | Neutral | Why this bound |
//! |---|---|---|---|
//! | `pitch_st` | −12 … +12 st | 0 | D1: one octave either way |
//! | `formant_ratio` | 0.75 … 1.33 | 1.0 | D1: ≈ ±25% vocal-tract length |
//! | `tilt_db_per_oct` | −6 … +6 dB/oct | 0 | a strong but usable source-tilt trim; anchored at 1 kHz |
//!
//! The pitch range is exactly what [`crate::pitch`] supports
//! (`MIN_RATIO..=MAX_RATIO`); the formant range is exactly
//! [`crate::transform`]'s. Clamping here and in the transform agree by
//! construction, so a value that survives this module is never silently moved
//! again downstream.
//!
//! # The validated region is sparse, not a rectangle
//!
//! D1's amendment measured only two rows of the plane: **pitch 0 with formant
//! 0.75–1.33**, and **±7 st with formant 1.00–1.10**. A preset at `+4 st /
//! 1.10` is therefore *untested* until it is measured, not "inside the
//! validated region". [`NaturalPreset`] ships only operating points that were
//! measured at their own coordinates by the ignored
//! `params::tests::preset_measurement_table`; `docs/presets.md` records the
//! numbers and the caveat for each.
//!
//! # Smoothing is for the worklet, not for a render
//!
//! The DSP is whole-buffer: one [`NaturalRenderer::process`] call is one clip at
//! one fixed operating point. A preset change between clips is a step, and a
//! step is exactly what the product contains — so measurements use settled
//! parameters and smoothing never perturbs a render. [`ParamSmoother`] exists
//! for the streaming case (W-03), where a slider can move mid-phrase: it ramps
//! pitch linearly in semitones, formant **geometrically** (in octaves, so a
//! 1.0→1.2 move has the same halfway point as 1.0→0.833 reversed), and tilt
//! linearly in dB/oct.

use std::fmt;

use crate::loudness::{LoudnessConfig, LoudnessError, LoudnessReport, LoudnessStage};
use crate::transform::{
    VoiceTransformConfig, VoiceTransformError, VoiceTransformer, MAX_FORMANT_RATIO,
    MAX_TILT_DB_PER_OCT, MIN_FORMANT_RATIO, MIN_TILT_DB_PER_OCT, TILT_REFERENCE_HZ,
};

/// Lowest pitch shift accepted, in semitones (D1).
pub const MIN_PITCH_ST: f32 = -12.0;
/// Highest pitch shift accepted, in semitones (D1).
pub const MAX_PITCH_ST: f32 = 12.0;
/// Default parameter ramp length for [`ParamSmoother`], in seconds.
///
/// 20 ms is long enough to remove a zipper artefact at a 512-sample hop (10.7 ms)
/// and short enough that a knob still feels immediate.
pub const DEFAULT_SMOOTHING_SECONDS: f32 = 0.020;
/// Lowest frequency the tilt curve is evaluated at, in Hz. Guards `log2(0)`.
const TILT_MIN_HZ: f32 = 1.0;
/// Conversion from nepers to decibels (20 / ln 10).
const DB_PER_NEPER: f32 = 8.685_889;

/// One point in natural-mode parameter space (D1).
///
/// Constructing this type does **not** validate; every consumer calls
/// [`sanitised`](NaturalParams::sanitised) (or a constructor that does) before
/// the values reach the DSP. The fields are public because a UI binds to them;
/// the invariants are stated here and enforced at the boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NaturalParams {
    /// Pitch shift in semitones, clamped to [`MIN_PITCH_ST`]..=[`MAX_PITCH_ST`].
    pub pitch_st: f32,
    /// Formant (vocal-tract) ratio, clamped to
    /// [`MIN_FORMANT_RATIO`]..=[`MAX_FORMANT_RATIO`].
    pub formant_ratio: f32,
    /// Spectral tilt in dB per octave, clamped to
    /// [`MIN_TILT_DB_PER_OCT`]..=[`MAX_TILT_DB_PER_OCT`]. Positive is brighter.
    pub tilt_db_per_oct: f32,
}

impl NaturalParams {
    /// The neutral point: no pitch shift, no formant change, no tilt.
    ///
    /// This is the value [`Default`] produces as well (Rust's `f32` default is
    /// zero, which is the neutral pitch and tilt but **not** the neutral
    /// formant, so `Default` is overridden below).
    pub const fn natural() -> Self {
        NaturalParams {
            pitch_st: 0.0,
            formant_ratio: 1.0,
            tilt_db_per_oct: 0.0,
        }
    }

    /// Build a sanitised point from raw knob values.
    pub fn new(pitch_st: f32, formant_ratio: f32, tilt_db_per_oct: f32) -> Self {
        NaturalParams {
            pitch_st,
            formant_ratio,
            tilt_db_per_oct,
        }
        .sanitised()
    }

    /// Clamp every axis into its supported range, replacing non-finite values
    /// with that axis's neutral value.
    ///
    /// This is total: it never panics and always returns a point the DSP can
    /// render. Callers that own a knob should call it on every write rather than
    /// trusting the UI.
    pub fn sanitised(self) -> Self {
        NaturalParams {
            pitch_st: clamp_or(self.pitch_st, MIN_PITCH_ST, MAX_PITCH_ST, 0.0),
            formant_ratio: clamp_or(
                self.formant_ratio,
                MIN_FORMANT_RATIO,
                MAX_FORMANT_RATIO,
                1.0,
            ),
            tilt_db_per_oct: clamp_or(
                self.tilt_db_per_oct,
                MIN_TILT_DB_PER_OCT,
                MAX_TILT_DB_PER_OCT,
                0.0,
            ),
        }
    }

    /// `true` when this point is exactly the neutral point.
    pub fn is_natural(&self) -> bool {
        self.pitch_st == 0.0 && self.formant_ratio == 1.0 && self.tilt_db_per_oct == 0.0
    }

    /// `true` when the transform axes (pitch and formant) are both neutral, so
    /// only the tilt — or nothing at all — remains.
    pub fn is_transform_bypass(&self) -> bool {
        self.pitch_st == 0.0 && self.formant_ratio == 1.0
    }

    /// Pitch ratio `2^(pitch_st / 12)`.
    pub fn pitch_ratio(&self) -> f32 {
        if self.pitch_st.is_finite() {
            2.0f32.powf(self.pitch_st / 12.0)
        } else {
            1.0
        }
    }

    /// The tilt curve's gain at `hz`, in decibels.
    ///
    /// `gain(f) = tilt_db_per_oct · log2(f / 1000)`, so it is exactly `0 dB` at
    /// 1 kHz, `+tilt` one octave above and `−tilt` one octave below. Frequencies
    /// below 1 Hz are clamped to 1 Hz so the function is finite everywhere;
    /// non-finite input falls back to the same guard.
    pub fn tilt_gain_db_at(&self, hz: f32) -> f32 {
        if !hz.is_finite() {
            return 0.0;
        }
        let hz = hz.max(TILT_MIN_HZ);
        self.tilt_db_per_oct * (hz / TILT_REFERENCE_HZ).log2()
    }

    /// [`tilt_gain_db_at`](NaturalParams::tilt_gain_db_at) as a linear gain.
    pub fn tilt_gain_at(&self, hz: f32) -> f32 {
        let db = self.tilt_gain_db_at(hz);
        if db.is_finite() {
            10.0f32.powf(db / 20.0)
        } else {
            1.0
        }
    }
}

impl Default for NaturalParams {
    fn default() -> Self {
        NaturalParams::natural()
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

/// Interpolate two points at `t ∈ [0, 1]`.
///
/// Pitch and tilt are linear; the formant ratio is geometric (linear in
/// octaves), because it is a frequency-scale factor. Both endpoints are
/// sanitised, so the result is inside the supported ranges for every `t`.
fn lerp_params(from: NaturalParams, to: NaturalParams, t: f32) -> NaturalParams {
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        1.0
    };
    NaturalParams {
        pitch_st: from.pitch_st + (to.pitch_st - from.pitch_st) * t,
        formant_ratio: (from.formant_ratio.ln()
            + (to.formant_ratio.ln() - from.formant_ratio.ln()) * t)
            .exp(),
        tilt_db_per_oct: from.tilt_db_per_oct + (to.tilt_db_per_oct - from.tilt_db_per_oct) * t,
    }
}

/// A time-based ramp between two [`NaturalParams`] points.
///
/// The smoother holds a `current` point and a `target`. [`set_target`] starts a
/// ramp from the current point; [`advance`] moves it forward by a number of
/// samples and returns the point at the new position. Advancing past the end
/// snaps exactly to the target, so a settled smoother reports the target
/// bit-for-bit and a measurement at a preset point is not perturbed by ramp
/// residue.
///
/// A zero (or non-finite) ramp length makes every change immediate. A zero
/// sample rate is treated the same way rather than dividing by zero.
///
/// [`set_target`]: ParamSmoother::set_target
/// [`advance`]: ParamSmoother::advance
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSmoother {
    sample_rate: u32,
    ramp_samples: u32,
    start: NaturalParams,
    target: NaturalParams,
    current: NaturalParams,
    position: u32,
}

impl ParamSmoother {
    /// A smoother at the neutral point with a
    /// [`DEFAULT_SMOOTHING_SECONDS`] ramp.
    pub fn new(sample_rate: u32) -> Self {
        Self::with_initial(
            sample_rate,
            DEFAULT_SMOOTHING_SECONDS,
            NaturalParams::natural(),
        )
    }

    /// A smoother settled at `initial`, with a `smoothing_seconds` ramp.
    pub fn with_initial(sample_rate: u32, smoothing_seconds: f32, initial: NaturalParams) -> Self {
        let initial = initial.sanitised();
        let ramp_samples = ramp_samples(sample_rate, smoothing_seconds);
        ParamSmoother {
            sample_rate,
            ramp_samples,
            start: initial,
            target: initial,
            current: initial,
            position: ramp_samples,
        }
    }

    /// The sample rate (0 is allowed; it makes ramps immediate).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The configured ramp length in samples.
    pub fn ramp_samples(&self) -> u32 {
        self.ramp_samples
    }

    /// The configured ramp length in seconds.
    pub fn smoothing_seconds(&self) -> f32 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.ramp_samples as f32 / self.sample_rate as f32
        }
    }

    /// Change the ramp length, snapping to the current target first.
    ///
    /// Snapping is deliberate: a shorter ramp applied mid-flight would otherwise
    /// jump the `current` point discontinuously.
    pub fn set_smoothing_seconds(&mut self, smoothing_seconds: f32) {
        self.start = self.target;
        self.current = self.target;
        self.ramp_samples = ramp_samples(self.sample_rate, smoothing_seconds);
        self.position = self.ramp_samples;
    }

    /// The point the smoother is currently at.
    pub fn current(&self) -> NaturalParams {
        self.current
    }

    /// The point the smoother is heading for.
    pub fn target(&self) -> NaturalParams {
        self.target
    }

    /// `true` when no ramp is in progress.
    pub fn is_settled(&self) -> bool {
        self.position >= self.ramp_samples
    }

    /// Aim at `target`, ramping from the current point.
    ///
    /// The target is sanitised. Re-aiming mid-ramp restarts from the current
    /// point, so the output never jumps.
    pub fn set_target(&mut self, target: NaturalParams) {
        let target = target.sanitised();
        self.start = self.current;
        self.target = target;
        if self.start == target || self.ramp_samples == 0 {
            self.current = target;
            self.position = self.ramp_samples;
        } else {
            self.position = 0;
        }
    }

    /// Snap to `target` without ramping.
    pub fn set_target_now(&mut self, target: NaturalParams) {
        let target = target.sanitised();
        self.start = target;
        self.target = target;
        self.current = target;
        self.position = self.ramp_samples;
    }

    /// Advance the ramp by `samples` and return the point at the new position.
    ///
    /// `samples == 0` returns the current point unchanged.
    pub fn advance(&mut self, samples: usize) -> NaturalParams {
        if samples == 0 {
            return self.current;
        }
        if self.is_settled() {
            self.current = self.target;
            return self.current;
        }
        let step = samples.min(u32::MAX as usize) as u32;
        self.position = self.position.saturating_add(step).min(self.ramp_samples);
        if self.position >= self.ramp_samples {
            self.current = self.target;
        } else {
            let t = self.position as f32 / self.ramp_samples as f32;
            self.current = lerp_params(self.start, self.target, t);
        }
        self.current
    }

    /// [`advance`](ParamSmoother::advance) with the block length in seconds.
    pub fn advance_seconds(&mut self, seconds: f32) -> NaturalParams {
        if !seconds.is_finite() || seconds <= 0.0 || self.sample_rate == 0 {
            return self.advance(0);
        }
        let samples = (seconds * self.sample_rate as f32).round().max(1.0);
        self.advance(samples as usize)
    }
}

/// Ramp length in samples for a requested duration.
fn ramp_samples(sample_rate: u32, seconds: f32) -> u32 {
    if sample_rate == 0 || !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let samples = (seconds * sample_rate as f32).round();
    if !samples.is_finite() || samples <= 0.0 {
        0
    } else {
        samples.min(u32::MAX as f32) as u32
    }
}

/// The player-facing presets of D12, with their measured operating points.
///
/// `natural` is the neutral point included so the UI has a complete row; it is
/// a bit-exact bypass and needs no measurement. The other four ship only
/// because `preset_measurement_table` measured them at their own coordinates.
///
/// # Physical intent
///
/// | Preset | Point | Intent |
/// |---|---|---|
/// | `shift-up` | +7 st, 1.10, +0.75 dB/oct | a **smaller speaker**: shorter vocal tract (formants up) and lighter vocal folds (F0 up a fifth), plus a little extra HF |
/// | `shift-down` | −3 st, 0.88, 0 dB/oct | a **larger adult**: a minor third lower F0 and a 12% longer tract |
/// | `bright` | 0 st, 1.00, +2.5 dB/oct | the **same speaker, brighter mic/less source tilt**; explicitly no size change, because brightness is not size |
/// | `deep` | −4 st, 0.85, 0 dB/oct | a **large chest voice**: a fourth lower F0 and a 15% longer tract, which darkens the timbre by itself |
///
/// The two size presets move pitch and formants **in the same direction**,
/// which is the physically consistent one: a shorter tract has both higher
/// formants and (on average) a higher F0. The opposing corners are the ones D1
/// measured as broken and are deliberately absent.
///
/// `shift-up` sits on the one upward row D1 measured (`+7 st`, formant
/// 1.00–1.10). Even there, the two high-F0 / 500 Hz-F1 synthetic cells do not
/// meet the 8% formant gate; `docs/presets.md` records those cells as measured
/// but **unsupported**, with the reason. The other three presets pass every
/// required cell at their own operating point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NaturalPreset {
    /// No shift, no tilt (bit-exact bypass).
    Natural,
    /// Smaller vocal tract + higher F0.
    ShiftUp,
    /// Larger vocal tract + lower F0.
    ShiftDown,
    /// Timbre only: brighter, same size.
    Bright,
    /// Large, dark chest voice.
    Deep,
}

impl NaturalPreset {
    /// Every preset, in D12's displayed order.
    pub const ALL: [NaturalPreset; 5] = [
        NaturalPreset::Natural,
        NaturalPreset::ShiftUp,
        NaturalPreset::ShiftDown,
        NaturalPreset::Bright,
        NaturalPreset::Deep,
    ];

    /// The four presets that ship with a measured operating point.
    ///
    /// `natural` is excluded because it is the null and measures nothing.
    pub const MEASURED: [NaturalPreset; 4] = [
        NaturalPreset::ShiftUp,
        NaturalPreset::ShiftDown,
        NaturalPreset::Bright,
        NaturalPreset::Deep,
    ];

    /// D12's player-facing id, e.g. `"shift-up"`.
    pub const fn id(self) -> &'static str {
        match self {
            NaturalPreset::Natural => "natural",
            NaturalPreset::ShiftUp => "shift-up",
            NaturalPreset::ShiftDown => "shift-down",
            NaturalPreset::Bright => "bright",
            NaturalPreset::Deep => "deep",
        }
    }

    /// Parse a D12 id; `None` for anything unknown (no panic, no guessing).
    pub fn from_id(id: &str) -> Option<Self> {
        NaturalPreset::ALL
            .into_iter()
            .find(|preset| preset.id() == id)
    }

    /// The preset's operating point, already inside the documented ranges.
    pub const fn params(self) -> NaturalParams {
        match self {
            NaturalPreset::Natural => NaturalParams {
                pitch_st: 0.0,
                formant_ratio: 1.0,
                tilt_db_per_oct: 0.0,
            },
            NaturalPreset::ShiftUp => NaturalParams {
                pitch_st: 7.0,
                formant_ratio: 1.10,
                tilt_db_per_oct: 0.75,
            },
            NaturalPreset::ShiftDown => NaturalParams {
                pitch_st: -3.0,
                formant_ratio: 0.88,
                tilt_db_per_oct: 0.0,
            },
            NaturalPreset::Bright => NaturalParams {
                pitch_st: 0.0,
                formant_ratio: 1.0,
                tilt_db_per_oct: 1.5,
            },
            NaturalPreset::Deep => NaturalParams {
                pitch_st: -4.0,
                formant_ratio: 0.85,
                tilt_db_per_oct: 0.0,
            },
        }
    }

    /// The preset whose operating point is exactly `params`, if any.
    ///
    /// Exact equality is intentional: this answers "is the UI still on a
    /// preset?", and a knob nudged by 0.01 semitones is no longer that preset.
    pub fn matching(params: NaturalParams) -> Option<Self> {
        NaturalPreset::ALL
            .into_iter()
            .find(|preset| preset.params() == params)
    }
}

impl fmt::Display for NaturalPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Errors from [`NaturalRenderer`]. Every failure is typed and comes from a
/// stage constructor or a fallible render; nothing here panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NaturalRenderError {
    /// The transform rejected its configuration or a render.
    Transform(VoiceTransformError),
    /// The loudness stage rejected its configuration.
    Loudness(LoudnessError),
}

impl fmt::Display for NaturalRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NaturalRenderError::Transform(err) => write!(f, "transform: {err}"),
            NaturalRenderError::Loudness(err) => write!(f, "loudness: {err}"),
        }
    }
}

impl std::error::Error for NaturalRenderError {}

impl From<VoiceTransformError> for NaturalRenderError {
    fn from(value: VoiceTransformError) -> Self {
        NaturalRenderError::Transform(value)
    }
}

impl From<LoudnessError> for NaturalRenderError {
    fn from(value: LoudnessError) -> Self {
        NaturalRenderError::Loudness(value)
    }
}

/// The complete natural-mode chain: transform → loudness (P-05 + P-08).
///
/// ```text
/// input ──► VoiceTransformer (pitch, formant, tilt) ──► LoudnessStage ──► output
///                                                        (RMS match, then
///                                                         true-peak limiter)
/// ```
///
/// The chain order is fixed by D8 (natural → robot → loudness). This type is
/// the *natural* composition and stops before the robot chain; the loudness
/// stage it owns is exposed via [`loudness`](NaturalRenderer::loudness) so a
/// later robot renderer can put its own output through the same policy without
/// duplicating it.
///
/// Level contract: the output's whole-clip RMS equals the **source clip's**
/// whole-clip RMS, unless the true-peak limiter has to reduce gain, in which
/// case it is quieter by exactly the amount the limiter removed. The stage's
/// [`LoudnessReport`] states both numbers.
pub struct NaturalRenderer {
    sample_rate: u32,
    params: NaturalParams,
    transformer: VoiceTransformer,
    loudness: LoudnessStage,
    /// Transform output, reused across renders.
    scratch: Vec<f32>,
}

impl fmt::Debug for NaturalRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NaturalRenderer")
            .field("sample_rate", &self.sample_rate)
            .field("params", &self.params)
            .field("transform", &self.transformer)
            .field("loudness", &self.loudness)
            .finish_non_exhaustive()
    }
}

impl NaturalRenderer {
    /// Build the chain at `sample_rate`, neutral, with explicit stage configs.
    pub fn new(
        sample_rate: u32,
        transform: VoiceTransformConfig,
        loudness: LoudnessConfig,
    ) -> Result<Self, NaturalRenderError> {
        let transformer = VoiceTransformer::new(sample_rate, transform)?;
        let loudness = LoudnessStage::new(sample_rate, loudness)?;
        Ok(NaturalRenderer {
            sample_rate,
            params: NaturalParams::natural(),
            transformer,
            loudness,
            scratch: Vec::new(),
        })
    }

    /// Build with D2's transform defaults and the P-08 loudness defaults.
    pub fn d2(sample_rate: u32) -> Result<Self, NaturalRenderError> {
        Self::new(
            sample_rate,
            VoiceTransformConfig::d2(),
            LoudnessConfig::default(),
        )
    }

    /// The sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Apply a validated operating point.
    ///
    /// The point is sanitised first, and the three values are pushed to the
    /// transform in one call so a render can never see half of a preset.
    pub fn set_params(&mut self, params: NaturalParams) {
        let params = params.sanitised();
        self.transformer.set_pitch_semitones(params.pitch_st);
        self.transformer.set_formant_ratio(params.formant_ratio);
        self.transformer.set_tilt_db_per_oct(params.tilt_db_per_oct);
        self.params = params;
    }

    /// The operating point in use.
    pub fn params(&self) -> NaturalParams {
        self.params
    }

    /// Apply a preset by name (D12).
    pub fn set_preset(&mut self, preset: NaturalPreset) {
        self.set_params(preset.params());
    }

    /// The preset whose point is exactly in use, if any.
    pub fn preset(&self) -> Option<NaturalPreset> {
        NaturalPreset::matching(self.params)
    }

    /// The underlying transform, for diagnostics (windows, exposed envelope).
    pub fn transformer(&self) -> &VoiceTransformer {
        &self.transformer
    }

    /// The loudness stage (D8 reuse point: the robot chain applies this after
    /// its own processing, and nothing else changes).
    pub fn loudness(&self) -> &LoudnessStage {
        &self.loudness
    }

    /// Mutable loudness stage, e.g. to change the ceiling for a test.
    pub fn loudness_mut(&mut self) -> &mut LoudnessStage {
        &mut self.loudness
    }

    /// Render a whole clip, returning the processed samples.
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>, NaturalRenderError> {
        let mut output = Vec::with_capacity(input.len());
        self.process_into(input, &mut output)?;
        Ok(output)
    }

    /// Render into an existing vector and return the loudness report.
    ///
    /// The output is exactly `input.len()` samples: neither the transform nor
    /// the loudness stage trims or pads (P-03/P-08). An empty input gives an
    /// empty output and a silent report.
    pub fn process_into(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<LoudnessReport, NaturalRenderError> {
        self.transformer.process_into(input, &mut self.scratch)?;
        let report = self.loudness.process_into(input, &mut self.scratch);
        output.clear();
        output.extend_from_slice(&self.scratch);
        Ok(report)
    }
}

/// The tilt curve in decibels at `hz`, for documentation and tests.
///
/// `tilt_db_per_oct · log2(hz / 1000 Hz)`. Exposed as a free function so a doc
/// or a plot can state the same curve the DSP applies without building a
/// [`NaturalParams`].
pub fn tilt_db_at(tilt_db_per_oct: f32, hz: f32) -> f32 {
    NaturalParams {
        tilt_db_per_oct,
        ..NaturalParams::natural()
    }
    .tilt_gain_db_at(hz)
}

/// Nepers per decibel, re-exported for callers that pair this module with the
/// envelope API.
pub const NEPERS_PER_DB: f32 = 1.0 / DB_PER_NEPER;

#[cfg(test)]
mod tests;
