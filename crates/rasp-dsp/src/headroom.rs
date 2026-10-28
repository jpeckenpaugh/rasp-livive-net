//! Headroom-proportional preset strength (PLAN P-05b, decision D22).
//!
//! The owner's listening complaint was directional: "a deep voice should not be
//! pushed deeper, and a high voice should not be pushed higher." Measured
//! against the corpus's own F0 envelope (54 voices, **79–323 Hz**, median
//! **178 Hz**), the fixed presets break that rule at both ends: `shift-up`
//! (+7 st) puts 16/54 voices above 323 Hz (worst `bf_alice` 248 → 372 Hz), and
//! `deep` (−4 st) puts 3/54 below 79 Hz (`am_onyx` 88 → 70, `bm_lewis` 79 → 63).
//! The middle of the range is fine — the median voice under `shift-up` lands at
//! 267 Hz, inside the population — so the defect is **directional headroom at
//! the extremes**, not global strength.
//!
//! # The rule
//!
//! ```text
//! room_up(f)   = 12·log2(HI / f)      HI = 323 Hz
//! room_down(f) = 12·log2(f / LO)      LO =  79 Hz
//! w            = clamp(room_direction(f) / room_direction(178 Hz), 0, 1)
//! effective    = nominal × w
//! ```
//!
//! `w` scales the **pitch shift in semitones** (equivalently `ratio^w`), the
//! **formant log-ratio** (`ratio^w`) and the **tilt** (`tilt × w`), so a voice
//! with no room in the requested direction gets no tract change either. `f` is
//! the clip's measured median F0 ([`median_f0_hz`]); `178 Hz` is the corpus
//! median. A voice in the middle of the range keeps the full nominal shift, and
//! `natural`/`bright` (nominal pitch 0) are untouched (`w = 1` trivially),
//! because "headroom" is a property of a direction and a zero shift has none.
//!
//! # The bounds are a population envelope, not physiology
//!
//! 79 Hz and 323 Hz are the **measured medians of the 54 corpus voices**
//! (`docs/corpus.md` §2), not the limits of the human voice. Two voices sit on
//! the edges and are called out in `docs/presets.md`: `zm_yunxia` (315 Hz,
//! male-labelled — the label is wrong, the measurement is not) and `bm_lewis`
//! (79 Hz, HNR 4.9 dB). A voice outside the envelope would clamp to `w = 0` in
//! the direction that would take it further out, which is the honest answer for
//! a **corpus-relative** mapping: this is "don't leave the cast's own range",
//! not "don't leave the human range".
//!
//! # Honesty at the boundary
//!
//! When `w = 0` the preset becomes a bit-exact bypass, and that is reported
//! rather than hidden: [`HeadroomReport::weight`] is exposed to the CLI/UI so it
//! can say "no headroom" instead of silently doing nothing. [HeadroomMode::Off]
//! reproduces the pre-D22 fixed behaviour for A/B.

use std::fmt;

use crate::params::NaturalParams;
use crate::stft::{Complex, RealFft};

/// Lowest frequency of the measured corpus envelope, in Hz (D22).
pub const HEADROOM_LOW_HZ: f32 = 79.0;
/// Highest frequency of the measured corpus envelope, in Hz (D22).
pub const HEADROOM_HIGH_HZ: f32 = 323.0;
/// Corpus median F0, the reference point at which `w = 1` (D22).
pub const HEADROOM_REFERENCE_HZ: f32 = 178.0;

/// Frame length of the F0 measurement, samples at 48 kHz (42.7 ms).
///
/// The same length as the STFT's analysis window, so the measurement sees the
/// same signal the shifter does.
const F0_FRAME: usize = 2048;
/// Lowest per-frame F0 accepted, Hz.
const F0_MIN_HZ: f32 = 55.0;
/// Highest per-frame F0 accepted, Hz.
const F0_MAX_HZ: f32 = 450.0;
/// Upper bound on frames analysed per clip (evenly spaced).
const F0_MAX_FRAMES: usize = 48;
/// Frames quieter than this fraction of the loudest frame are skipped (−40 dB).
const F0_POWER_FLOOR: f32 = 1e-4;
/// Minimum normalised autocorrelation for a frame to count as voiced.
const F0_VOICING: f32 = 0.30;
/// A local maximum must reach this fraction of the best peak to be chosen.
///
/// The **first** local maximum that clears it wins, which rejects a subharmonic
/// (double-period) peak that happens to be marginally stronger — the classic
/// octave-down failure of a plain global-max tracker.
const F0_PEAK_FRACTION: f32 = 0.92;

/// Whether the D22 headroom mapping is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HeadroomMode {
    /// Scale every preset by the voice's measured headroom (D22, default).
    #[default]
    On,
    /// Fixed nominal presets, the pre-D22 behaviour. A/B only.
    Off,
}

impl HeadroomMode {
    /// The CLI/config id: `"on"` or `"off"`.
    pub const fn id(self) -> &'static str {
        match self {
            HeadroomMode::On => "on",
            HeadroomMode::Off => "off",
        }
    }

    /// Parse `"on"`/`"off"`; `None` for anything else (no panic, no guessing).
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "on" => Some(HeadroomMode::On),
            "off" => Some(HeadroomMode::Off),
            _ => None,
        }
    }
}

impl fmt::Display for HeadroomMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Upward headroom from `hz` to [`HEADROOM_HIGH_HZ`], in semitones.
///
/// `12·log2(HI / f)`. Exposed so a doc or a plot can state the same curve the
/// mapping uses. Non-finite input returns `0`, and `hz <= 0` is treated as
/// "no measurement" (`0`) rather than an infinite room.
pub fn room_up_hz(hz: f32) -> f32 {
    if !hz.is_finite() || hz <= 0.0 {
        return 0.0;
    }
    let room = 12.0 * (HEADROOM_HIGH_HZ / hz).log2();
    if room.is_finite() {
        room
    } else {
        0.0
    }
}

/// Downward headroom from `hz` to [`HEADROOM_LOW_HZ`], in semitones.
///
/// `12·log2(f / LO)`. Same guards as [`room_up_hz`].
pub fn room_down_hz(hz: f32) -> f32 {
    if !hz.is_finite() || hz <= 0.0 {
        return 0.0;
    }
    let room = 12.0 * (hz / HEADROOM_LOW_HZ).log2();
    if room.is_finite() {
        room
    } else {
        0.0
    }
}

/// The D22 weight `w ∈ [0, 1]` for a nominal pitch shift and a measured F0.
///
/// * `nominal_pitch_st > 0` uses [`room_up_hz`]; `< 0` uses [`room_down_hz`].
/// * `nominal_pitch_st == 0` returns `1.0`: there is no direction to exhaust,
///   so `natural` and `bright` are unchanged by construction.
/// * An unavailable measurement (silence, unvoiced clip) returns `1.0`, which
///   is the documented fallback: keep the nominal preset rather than guess.
///   A caller that requires a measurement can check
///   [`HeadroomReport::measured_f0_hz`].
pub fn headroom_weight(measured_f0_hz: Option<f32>, nominal_pitch_st: f32) -> f32 {
    if !nominal_pitch_st.is_finite() || nominal_pitch_st == 0.0 {
        return 1.0;
    }
    let Some(f0) = measured_f0_hz.filter(|hz| hz.is_finite() && *hz > 0.0) else {
        return 1.0;
    };
    let reference = if nominal_pitch_st > 0.0 {
        room_up_hz(HEADROOM_REFERENCE_HZ)
    } else {
        room_down_hz(HEADROOM_REFERENCE_HZ)
    };
    if reference <= 0.0 {
        return 1.0;
    }
    let room = if nominal_pitch_st > 0.0 {
        room_up_hz(f0)
    } else {
        room_down_hz(f0)
    };
    (room / reference).clamp(0.0, 1.0)
}

/// Scale a nominal point by `weight` (D22).
///
/// Pitch semitones and tilt scale linearly; the formant ratio scales
/// geometrically in octaves, which is the same operation as `ratio^weight` and
/// keeps a no-headroom voice's tract unchanged (`ratio^0 = 1`). `weight` is
/// clamped to `[0, 1]` and a non-finite weight is treated as `1`.
pub fn scale_params(nominal: NaturalParams, weight: f32) -> NaturalParams {
    let weight = if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let formant = if nominal.formant_ratio.is_finite() && nominal.formant_ratio > 0.0 {
        nominal.formant_ratio.powf(weight)
    } else {
        nominal.formant_ratio
    };
    NaturalParams {
        pitch_st: nominal.pitch_st * weight,
        formant_ratio: formant,
        tilt_db_per_oct: nominal.tilt_db_per_oct * weight,
    }
    .sanitised()
}

/// What the headroom mapping did to one render (D22).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeadroomReport {
    /// `On` when the mapping was active.
    pub mode: HeadroomMode,
    /// The clip's measured median F0, Hz. `None` when the clip had no voiced
    /// frames — the weight is then `1.0` (nominal), and the UI can say so.
    pub measured_f0_hz: Option<f32>,
    /// The D22 weight `w`.
    pub weight: f32,
    /// The nominal operating point (the preset / knob values).
    pub nominal: NaturalParams,
    /// The point actually rendered: nominal scaled by [`weight`](Self::weight).
    pub effective: NaturalParams,
}

impl HeadroomReport {
    /// A pass-through report for an inactive mapping (mode `Off`) or a render
    /// that needs no measurement (nominal pitch 0).
    pub fn pass_through(mode: HeadroomMode, nominal: NaturalParams) -> Self {
        HeadroomReport {
            mode,
            measured_f0_hz: None,
            weight: 1.0,
            nominal,
            effective: nominal,
        }
    }

    /// Build the report for a measured F0 under `mode`.
    ///
    /// `measured_f0_hz = None` means "not measured"; in `Off` mode it is
    /// ignored and the nominal point is returned unchanged.
    pub fn new(mode: HeadroomMode, nominal: NaturalParams, measured_f0_hz: Option<f32>) -> Self {
        if mode == HeadroomMode::Off {
            return HeadroomReport {
                mode,
                measured_f0_hz,
                ..Self::pass_through(mode, nominal)
            };
        }
        let weight = headroom_weight(measured_f0_hz, nominal.pitch_st);
        HeadroomReport {
            mode,
            measured_f0_hz,
            weight,
            nominal,
            effective: scale_params(nominal, weight),
        }
    }

    /// Effective pitch shift in semitones (the number the UI shows).
    pub fn effective_pitch_st(&self) -> f32 {
        self.effective.pitch_st
    }

    /// Effective formant ratio.
    pub fn effective_formant_ratio(&self) -> f32 {
        self.effective.formant_ratio
    }

    /// Effective tilt, dB per octave.
    pub fn effective_tilt_db_per_oct(&self) -> f32 {
        self.effective.tilt_db_per_oct
    }

    /// `true` when the requested direction has no room left: the preset is a
    /// bypass and the UI should say "no headroom" rather than "no change".
    pub fn no_headroom(&self) -> bool {
        self.mode == HeadroomMode::On && self.nominal.pitch_st != 0.0 && self.weight <= 0.0
    }
}

/// Median F0 of `input`, in Hz, by normalised autocorrelation.
///
/// This is the P-05b measurement behind D22. It is deterministic, allocation is
/// per call only (no state), and it never panics on public input: an empty
/// clip, silence, a zero sample rate or an unvoiced clip returns `None`.
///
/// # Method
///
/// Short frames (2048 samples, 42.7 ms) are spread evenly over the clip, at
/// most [`F0_MAX_FRAMES`]. Frames more than 40 dB below the loudest are skipped.
/// Each frame is mean-removed and its **linear** autocorrelation is computed by
/// a zero-padded real FFT (4096 points), then normalised by the overlapping
/// frame energies, which is the unbiased normalised cross-correlation
/// `r(τ) = Σx[i]x[i+τ] / sqrt(Σx[i]²·Σx[i+τ]²)`. The **first** local maximum
/// reaching 92 % of the best peak in the 55–450 Hz lag band is taken, with
/// parabolic refinement; a subharmonic peak is therefore not chosen by accident.
/// Frame estimates below an `r` of 0.30 are discarded, and the result is the
/// **median** over the rest (the D17/Q-01 medians-only rule).
///
/// Measured accuracy on the 12 fixtures is **2.16 % median / 8.42 % worst**
/// against the official D17 ruler's per-clip medians (see `docs/presets.md`
/// §13.6), and exact to under 0.01 % on synthetic vowels with a known F0.
pub fn median_f0_hz(input: &[f32], sample_rate: u32) -> Option<f32> {
    if input.is_empty() || sample_rate < 8_000 {
        return None;
    }
    F0Tracker::new().estimate(input, sample_rate)
}

/// Reusable F0 analysis state, so a renderer can measure without re-allocating.
///
/// [`median_f0_hz`] is a convenience wrapper around this type. The buffers are
/// reused across calls, which matters because the renderer measures once per
/// clip.
pub struct F0Tracker {
    fft: RealFft,
    padded: Vec<f32>,
    spectrum: Vec<Complex>,
    autocorrelation: Vec<f32>,
    prefix_energy: Vec<f32>,
    centred: Vec<f32>,
    lags: Vec<f32>,
}

impl fmt::Debug for F0Tracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("F0Tracker")
            .field("frame", &F0_FRAME)
            .finish_non_exhaustive()
    }
}

impl Default for F0Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl F0Tracker {
    /// Build the analysis plan for the fixed frame length.
    pub fn new() -> Self {
        let fft_len = F0_FRAME * 2;
        F0Tracker {
            fft: RealFft::new(fft_len),
            padded: vec![0.0; fft_len],
            spectrum: vec![Complex::ZERO; fft_len / 2 + 1],
            autocorrelation: vec![0.0; fft_len],
            prefix_energy: vec![0.0; F0_FRAME + 1],
            centred: vec![0.0; F0_FRAME],
            lags: Vec::with_capacity(F0_FRAME / 2),
        }
    }

    /// Measure the clip's median F0. See [`median_f0_hz`].
    pub fn estimate(&mut self, input: &[f32], sample_rate: u32) -> Option<f32> {
        if input.is_empty() || sample_rate < 8_000 {
            return None;
        }
        let hop = frame_hop(input.len());
        let mut frame_powers: Vec<f32> = Vec::new();
        let mut loudest = 0.0f32;
        let mut start = 0usize;
        loop {
            let available = input.len().saturating_sub(start).min(F0_FRAME);
            if available == 0 {
                break;
            }
            let mut energy = 0.0f32;
            for (slot, sample) in self.centred[..available]
                .iter_mut()
                .zip(&input[start..start + available])
            {
                let value = if sample.is_finite() { *sample } else { 0.0 };
                *slot = value;
                energy += value * value;
            }
            // A short final frame is zero-padded by leaving the tail at 0.
            for slot in &mut self.centred[available..] {
                *slot = 0.0;
            }
            let power = energy / F0_FRAME as f32;
            frame_powers.push(power);
            loudest = loudest.max(power);
            if start + F0_FRAME >= input.len() {
                break;
            }
            start += hop;
        }
        if frame_powers.is_empty() || !loudest.is_finite() || loudest <= 0.0 {
            return None;
        }
        let floor = loudest * F0_POWER_FLOOR;

        let lag_min = (sample_rate as f32 / F0_MAX_HZ).floor().max(2.0) as usize;
        let lag_max = ((sample_rate as f32 / F0_MIN_HZ).ceil() as usize).min(F0_FRAME - 2);
        if lag_max <= lag_min + 2 {
            return None;
        }

        let mut estimates: Vec<f32> = Vec::with_capacity(frame_powers.len());
        let mut start = 0usize;
        for power in frame_powers.iter().copied() {
            let available = input.len().saturating_sub(start).min(F0_FRAME);
            if available == 0 {
                break;
            }
            if power >= floor {
                for (slot, sample) in self.centred[..available]
                    .iter_mut()
                    .zip(&input[start..start + available])
                {
                    *slot = if sample.is_finite() { *sample } else { 0.0 };
                }
                for slot in &mut self.centred[available..] {
                    *slot = 0.0;
                }
                // Remove the frame mean: a DC term would otherwise bias the
                // autocorrelation at long lags.
                let mean = self.centred[..available].iter().sum::<f32>() / available as f32;
                for slot in &mut self.centred[..available] {
                    *slot -= mean;
                }
                if let Some(f0) = self.frame_f0(sample_rate, lag_min, lag_max) {
                    estimates.push(f0);
                }
            }
            if start + F0_FRAME >= input.len() {
                break;
            }
            start += hop;
        }
        median(&mut estimates)
    }

    /// One frame's F0, or `None` when it is not periodic enough.
    fn frame_f0(&mut self, sample_rate: u32, lag_min: usize, lag_max: usize) -> Option<f32> {
        self.padded[..F0_FRAME].copy_from_slice(&self.centred);
        self.padded[F0_FRAME..].fill(0.0);
        self.fft.forward(&self.padded, &mut self.spectrum);
        for bin in &mut self.spectrum {
            *bin = Complex::new(bin.re * bin.re + bin.im * bin.im, 0.0);
        }
        self.fft.inverse(&self.spectrum, &mut self.autocorrelation);

        self.prefix_energy[0] = 0.0;
        for index in 0..F0_FRAME {
            let value = self.centred[index];
            self.prefix_energy[index + 1] = self.prefix_energy[index] + value * value;
        }
        let total = self.prefix_energy[F0_FRAME];
        if !total.is_finite() || total <= 1e-20 {
            return None;
        }

        self.lags.clear();
        self.lags.reserve(lag_max - lag_min + 1);
        for lag in lag_min..=lag_max {
            let count = F0_FRAME - lag;
            let left = self.prefix_energy[count];
            let right = total - self.prefix_energy[lag];
            let denominator = (left * right).max(0.0).sqrt();
            let value = if denominator > 1e-20 {
                self.autocorrelation[lag] / denominator
            } else {
                0.0
            };
            self.lags.push(value);
        }

        let (index, _) = first_strong_peak(&self.lags)?;
        let lag = lag_min + index;
        let centre = self.lags[index];
        if centre < F0_VOICING {
            return None;
        }
        let left = if index > 0 {
            self.lags[index - 1]
        } else {
            centre
        };
        let right = if index + 1 < self.lags.len() {
            self.lags[index + 1]
        } else {
            centre
        };
        let offset = parabolic_offset(left, centre, right);
        let refined = lag as f32 + offset;
        if !refined.is_finite() || refined <= 0.0 {
            return None;
        }
        let f0 = sample_rate as f32 / refined;
        (f0.is_finite() && (F0_MIN_HZ..=F0_MAX_HZ).contains(&f0)).then_some(f0)
    }
}

/// Distance between frame starts for a clip of `len` samples.
fn frame_hop(len: usize) -> usize {
    let half = F0_FRAME / 2;
    if len <= F0_FRAME {
        return half;
    }
    let span = len - F0_FRAME;
    let frames = span / half + 1;
    if frames <= F0_MAX_FRAMES {
        half
    } else {
        span.div_ceil(F0_MAX_FRAMES).max(1)
    }
}

/// Index of the first local maximum reaching [`F0_PEAK_FRACTION`] of the best.
///
/// Returns `None` when the array is shorter than three points or has no
/// non-negative peak at all.
fn first_strong_peak(values: &[f32]) -> Option<(usize, f32)> {
    if values.len() < 3 {
        return None;
    }
    let best = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !best.is_finite() || best <= 0.0 {
        return None;
    }
    let threshold = best * F0_PEAK_FRACTION;
    for index in 1..values.len() - 1 {
        let value = values[index];
        if value >= values[index - 1] && value >= values[index + 1] && value >= threshold {
            return Some((index, value));
        }
    }
    // No interior local maximum (a monotone ramp, e.g. a pure tone at the band
    // edge): fall back to the global maximum so the measurement still exists.
    values
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal))
        .map(|(index, value)| (index, *value))
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

#[cfg(test)]
mod tests;
