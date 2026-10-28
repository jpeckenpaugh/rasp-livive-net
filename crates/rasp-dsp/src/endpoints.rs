//! Endpoint detection and explicit trimming (PLAN P-04).
//!
//! The corpus is padded with silence: F-06 measured a median of **40 ms** of
//! leading and **65 ms** of trailing silence, p90 of 55/108 ms, 170 clips with
//! more than 100 ms of tail, and a worst case of **524 ms**
//! (`bf_isabella/one-more`). 11.7% of the corpus by time is silence. A fixed
//! pad cannot serve that distribution: it would either leave half a second of
//! dead air on the worst clip or cut into speech on the rest.
//!
//! # Detection rule (the same one F-06 measured with)
//!
//! * A **10 ms sliding RMS envelope** — a boxcar of `0.010·sr` samples,
//!   evaluated at every sample — exactly as `scripts/measure_corpus.py` does it,
//!   including its half-window front pad, so the numbers here are directly
//!   comparable with `docs/corpus.md` §3.
//! * Threshold **−45 dBFS** ([`DEFAULT_THRESHOLD_DBFS`]), matching F-06.
//! * A **minimum run of 20 ms** above the threshold ([`DEFAULT_MIN_RUN_SECONDS`]),
//!   so an isolated click inside silence is ignored.
//! * The leading silence ends at the first qualifying run and the trailing
//!   silence begins at the last one. If a clip has **no** qualifying run, both
//!   head and tail report the full duration (F-06's convention) and the clip is
//!   left untrimmed — a silent clip trims to nothing, which is never useful.
//!
//! # Safety margins
//!
//! Detection is deliberately never applied raw. [`default`](EndpointConfig) keeps
//! **10 ms before the detected onset** and **20 ms after the detected offset**,
//! configurable on [`EndpointConfig`]. The console/plosive onset detected by an
//! RMS envelope is late by anything up to a window length, and speech decays
//! slowly; keeping a margin means a trim can never remove the first consonant
//! or clip a final fricative tail. The margin is what makes "trimming" safe on
//! 32 kbps speech rather than merely aggressive.
//!
//! # Trimming is explicit, and it changes the length
//!
//! [`crate::transform::VoiceTransformer`] and [`crate::pitch::PitchShifter`]
//! **never** trim. Their contract is exactly `input.len()` samples out, and the
//! A/B path (PLAN W-05), the metrics harness (PLAN P-07) and the null test all
//! depend on that length equality. Trimming happens only when a caller asks
//! [`trim_clip`] (or [`trim_into`]) for it:
//!
//! ```text
//! decoded clip ──(optional)──► trim_clip ──► render ──► playback
//!                                  │
//!                                  └── length is shorter than the source
//! ```
//!
//! Rendering a **trimmed** clip produces a **shorter** result than rendering
//! the source. That is the intended contract, and it has three consequences
//! worth stating loudly:
//!
//! 1. Do not use a trimmed render and the source render as a sample-aligned
//!    A/B pair — they are not aligned. Compare the trimmed input's bypass
//!    render, or compare the two renders after the same trim.
//! 2. `scripts/metrics.py` assumes equal lengths by default. For a trimmed
//!    render use `--allow-length-delta <samples>`, which scores the common
//!    prefix (pass the number of samples the trim removed).
//! 3. `registry.durationMs` describes the **untrimmed** container duration
//!    (F-06 §4.3 already warns it is approximate), so a trimmed clip's real
//!    duration is the returned `samples.len()`, not the registry field.
//!
//! # Fades
//!
//! [`trim_clip`] applies a **5 ms cosine fade** in and out ([`DEFAULT_FADE_SECONDS`])
//! to the **trimmed** signal — not to the source — so the fade lands on the new
//! boundary rather than somewhere inside what was removed. The fade is a
//! half-Hann: its first and last gain are exactly `0` and it is `C¹` at the
//! join to the untrimmed interior, so the boundary has no click and no slope
//! discontinuity. A clip that needed no trim is returned byte-identical with
//! `fade_applied = false`.
//!
//! # Measured behaviour
//!
//! Over the 1404-clip corpus the detector reports a median of 40.0 ms head and
//! 64.6 ms tail, p90 54.8/107.8 ms, max 142.2/524.0 ms — F-06's numbers to
//! within a sample. Trimming removes 8.7% of the corpus's duration rather than
//! the 11.7% that is silence, because the 10 ms/20 ms margins are deliberately
//! kept. No clip loses more than **0.07%** of its energy to the trim, and no
//! clip trims to an empty signal. The full report is the ignored
//! `endpoints::tests::corpus_endpoint_report`.
//!
//! # Cost
//!
//! One linear pass and no allocation beyond the returned clip. Nothing here
//! panics, allocates per sample, or has a public-input failure path: an empty
//! input, a zero sample rate, a zero-length window, non-finite configuration and
//! non-finite samples all fall back to documented finite behaviour.

/// Threshold used by F-06 and by [`default`](EndpointConfig) for the silence
/// test, in dBFS.
pub const DEFAULT_THRESHOLD_DBFS: f32 = -45.0;
/// Analysis window of the RMS envelope: 10 ms.
pub const DEFAULT_FRAME_SECONDS: f32 = 0.010;
/// Minimum run above the threshold for it to count: 20 ms.
pub const DEFAULT_MIN_RUN_SECONDS: f32 = 0.020;
/// Leading safety margin kept before the detected onset: 10 ms.
pub const DEFAULT_PRE_ONSET_SECONDS: f32 = 0.010;
/// Trailing safety margin kept after the detected offset: 20 ms.
pub const DEFAULT_POST_OFFSET_SECONDS: f32 = 0.020;
/// Cosine fade applied at each new boundary: 5 ms.
pub const DEFAULT_FADE_SECONDS: f32 = 0.005;

/// Configuration for [`detect_endpoints`] and [`trim_clip`].
///
/// Every field has a documented default in [`Default`], and every field is
/// sanitised at use: non-finite or negative values are replaced by the default
/// or by zero, so a caller cannot make the detector loop forever or divide by a
/// zero-length window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointConfig {
    /// Silence threshold in dBFS. Values above 0 dBFS are clamped to 0 (a
    /// threshold of unity amplitude), non-finite values become
    /// [`DEFAULT_THRESHOLD_DBFS`].
    pub threshold_dbfs: f32,
    /// RMS envelope window in seconds (default 10 ms).
    pub frame_seconds: f32,
    /// Minimum above-threshold run in seconds (default 20 ms).
    pub min_run_seconds: f32,
    /// How much audio to keep **before** the detected onset (default 10 ms).
    pub pre_onset_seconds: f32,
    /// How much audio to keep **after** the detected offset (default 20 ms).
    pub post_offset_seconds: f32,
    /// Cosine fade length at each trimmed boundary (default 5 ms).
    pub fade_seconds: f32,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        EndpointConfig {
            threshold_dbfs: DEFAULT_THRESHOLD_DBFS,
            frame_seconds: DEFAULT_FRAME_SECONDS,
            min_run_seconds: DEFAULT_MIN_RUN_SECONDS,
            pre_onset_seconds: DEFAULT_PRE_ONSET_SECONDS,
            post_offset_seconds: DEFAULT_POST_OFFSET_SECONDS,
            fade_seconds: DEFAULT_FADE_SECONDS,
        }
    }
}

impl EndpointConfig {
    /// The measurement defaults: F-06's −45 dBFS / 10 ms / 20 ms rule, 10 ms of
    /// pre-onset and 20 ms of post-offset margin, 5 ms fades.
    pub const fn corpus() -> Self {
        EndpointConfig {
            threshold_dbfs: DEFAULT_THRESHOLD_DBFS,
            frame_seconds: DEFAULT_FRAME_SECONDS,
            min_run_seconds: DEFAULT_MIN_RUN_SECONDS,
            pre_onset_seconds: DEFAULT_PRE_ONSET_SECONDS,
            post_offset_seconds: DEFAULT_POST_OFFSET_SECONDS,
            fade_seconds: DEFAULT_FADE_SECONDS,
        }
    }

    /// Effective threshold as a linear amplitude in `0.0..=1.0`.
    pub fn threshold_amplitude(&self) -> f32 {
        let db = if self.threshold_dbfs.is_finite() {
            self.threshold_dbfs.clamp(-200.0, 0.0)
        } else {
            DEFAULT_THRESHOLD_DBFS
        };
        10.0f32.powf(db / 20.0)
    }

    /// Effective envelope window length in samples (at least 1).
    pub fn frame_samples(&self, sample_rate: u32) -> usize {
        seconds_to_samples(self.frame_seconds, DEFAULT_FRAME_SECONDS, sample_rate, true)
    }

    /// Effective minimum run in samples (0 accepts a single sample).
    pub fn min_run_samples(&self, sample_rate: u32) -> usize {
        seconds_to_samples(self.min_run_seconds, 0.0, sample_rate, false)
    }

    /// Effective leading safety margin in samples.
    pub fn pre_onset_samples(&self, sample_rate: u32) -> usize {
        seconds_to_samples(self.pre_onset_seconds, 0.0, sample_rate, false)
    }

    /// Effective trailing safety margin in samples.
    pub fn post_offset_samples(&self, sample_rate: u32) -> usize {
        seconds_to_samples(self.post_offset_seconds, 0.0, sample_rate, false)
    }

    /// Effective fade length in samples.
    pub fn fade_samples(&self, sample_rate: u32) -> usize {
        seconds_to_samples(self.fade_seconds, 0.0, sample_rate, false)
    }
}

/// Convert a seconds field to samples.
///
/// Non-finite or negative values fall back to `fallback`; the result is clamped
/// to one second so no configuration can make a pass pathologically long.
/// `at_least_one` is used for the envelope window (which must be non-zero).
fn seconds_to_samples(seconds: f32, fallback: f32, sample_rate: u32, at_least_one: bool) -> usize {
    if sample_rate == 0 {
        return 0;
    }
    let value = if seconds.is_finite() && seconds >= 0.0 {
        seconds
    } else {
        fallback
    };
    let value = if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    };
    let samples = (value.min(1.0) * sample_rate as f32).round() as usize;
    if at_least_one {
        samples.max(1)
    } else {
        samples
    }
}

/// Detected endpoints, in samples.
///
/// `start` and `end` are **after** the safety margins, so they are the indices
/// [`trim_clip`] keeps. `head_ms` and `tail_ms` are the *measured* silence
/// durations (before margins), matching F-06's numbers, so a caller can compare
/// them with `docs/corpus.md` directly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Endpoints {
    /// Index of the first sample to keep (`0..=len`).
    pub start: usize,
    /// Exclusive index of the last sample to keep (`start..=len`).
    pub end: usize,
    /// Measured leading silence in milliseconds, before the safety margin.
    pub head_ms: f32,
    /// Measured trailing silence in milliseconds, before the safety margin.
    pub tail_ms: f32,
}

impl Endpoints {
    /// Endpoints that keep the whole signal.
    pub fn full(len: usize) -> Self {
        Endpoints {
            start: 0,
            end: len,
            head_ms: 0.0,
            tail_ms: 0.0,
        }
    }

    /// Kept length in samples (`end - start`).
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether nothing is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A clip after explicit trimming.
#[derive(Debug, Clone, PartialEq)]
pub struct TrimmedClip {
    /// The trimmed samples, with the cosine fades already applied.
    pub samples: Vec<f32>,
    /// Index in the **source** of the first kept sample (after margins).
    pub start: usize,
    /// Exclusive index in the **source** of the last kept sample.
    pub end: usize,
    /// Measured leading silence of the source in milliseconds (before margins).
    pub head_ms: f32,
    /// Measured trailing silence of the source in milliseconds (before margins).
    pub tail_ms: f32,
    /// Whether a fade was applied (false when the clip needed no trim).
    pub fade_applied: bool,
}

impl TrimmedClip {
    /// Length of the trimmed samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether the trimmed clip is empty.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Detect the leading and trailing silence of `samples`.
///
/// Returns the indices to keep after the configured safety margins plus the
/// measured head/tail silence (before margins). Empty input, a zero sample rate
/// and a fully silent clip all return [`Endpoints::full`]'s indices (nothing to
/// trim) with finite durations.
///
/// The envelope is F-06's exactly: a boxcar RMS of `frame_samples`, evaluated
/// at every sample, with the same half-window front pad, so `head_ms` here is
/// comparable with the corpus table to within a sample.
pub fn detect_endpoints(samples: &[f32], sample_rate: u32, config: &EndpointConfig) -> Endpoints {
    let len = samples.len();
    if len == 0 || sample_rate == 0 {
        return Endpoints::full(len);
    }
    let frame = config.frame_samples(sample_rate).min(len).max(1);
    let threshold = config.threshold_amplitude() as f64;
    let min_run = config.min_run_samples(sample_rate);

    // Sliding boxcar energy with a front pad of `frame / 2` zeros, mirroring
    // `scripts/measure_corpus.py`: `env[j]` is the RMS of the window that
    // starts at source index `j - pad`, and `j` itself is the sample index a
    // run is reported at. Keeping the pad (instead of centring the window)
    // reproduces F-06's numbers rather than shifting every head/tail by 5 ms.
    let pad = (frame - 1) / 2;
    let raw_len = len - frame + 1;
    let mut running = 0.0f64;
    for &value in &samples[..frame] {
        running += f64::from(value) * f64::from(value);
    }
    let inverse_frame = 1.0 / frame as f64;

    let mut first_start: Option<usize> = None;
    let mut last_end = 0usize;
    let mut run_start = 0usize;
    let mut in_run = false;

    for raw in 0..raw_len {
        let mean_square = if running.is_finite() {
            (running * inverse_frame).max(0.0)
        } else {
            0.0
        };
        let above = mean_square.sqrt() >= threshold;
        let index = raw + pad;
        if above && !in_run {
            run_start = index;
            in_run = true;
        } else if !above && in_run {
            in_run = false;
            if index - run_start >= min_run {
                first_start.get_or_insert(run_start);
                last_end = index;
            }
        }
        // Advance the boxcar by one sample: drop `raw`, add `raw + frame`.
        if raw + frame < len {
            let outgoing = f64::from(samples[raw]);
            let incoming = f64::from(samples[raw + frame]);
            running += incoming * incoming - outgoing * outgoing;
        }
    }
    // A run that reaches the final envelope sample is still a run; its
    // exclusive end is one past the last envelope position.
    if in_run {
        let index = raw_len + pad;
        if index - run_start >= min_run {
            first_start.get_or_insert(run_start);
            last_end = index;
        }
    }
    let Some(onset) = first_start else {
        // Nothing above threshold: F-06 reports the whole clip as silence and
        // there is nothing safe to trim.
        return Endpoints {
            start: 0,
            end: len,
            head_ms: milliseconds(len, sample_rate),
            tail_ms: milliseconds(len, sample_rate),
        };
    };

    let head = onset.min(len);
    let offset = last_end.min(len);
    let tail = len - offset;

    let pre = config.pre_onset_samples(sample_rate);
    let post = config.post_offset_samples(sample_rate);
    let mut start = head.saturating_sub(pre);
    let mut end = offset.saturating_add(post).min(len);
    if start >= end {
        // Unreachable given `head + tail <= len` and non-negative margins, but
        // a defensive fallback is cheaper than a panic on a public path.
        start = 0;
        end = len;
    }

    Endpoints {
        start,
        end,
        head_ms: milliseconds(head, sample_rate),
        tail_ms: milliseconds(tail, sample_rate),
    }
}

/// Trim `samples` to [`detect_endpoints`] and apply the cosine fades.
///
/// The returned `samples.len()` is `end - start`, which is **shorter than the
/// source** whenever the clip had more than the safety margin of silence. The
/// fade is applied to the trimmed slice, and `fade_applied` is `false` when the
/// clip needed no trim at all (in that case the samples are copied unchanged).
///
/// See the module docs for the length-change contract.
pub fn trim_clip(samples: &[f32], sample_rate: u32, config: &EndpointConfig) -> TrimmedClip {
    let mut trimmed = TrimmedClip {
        samples: Vec::new(),
        start: 0,
        end: samples.len(),
        head_ms: 0.0,
        tail_ms: 0.0,
        fade_applied: false,
    };
    trim_into(samples, sample_rate, config, &mut trimmed);
    trimmed
}

/// [`trim_clip`] into an existing report, reusing its `samples` allocation.
///
/// Clears and fills `out`; the other fields describe the report as in
/// [`trim_clip`].
pub fn trim_into(
    samples: &[f32],
    sample_rate: u32,
    config: &EndpointConfig,
    out: &mut TrimmedClip,
) {
    let endpoints = detect_endpoints(samples, sample_rate, config);
    out.samples.clear();
    out.samples
        .extend_from_slice(&samples[endpoints.start..endpoints.end]);
    out.start = endpoints.start;
    out.end = endpoints.end;
    out.head_ms = endpoints.head_ms;
    out.tail_ms = endpoints.tail_ms;

    let trimmed = endpoints.start > 0 || endpoints.end < samples.len();
    let fade = config.fade_samples(sample_rate);
    if trimmed && fade > 0 && !out.samples.is_empty() {
        fade_edges(&mut out.samples, fade);
        out.fade_applied = true;
    } else {
        out.fade_applied = false;
    }
}

/// Apply a half-Hann (cosine) fade of `fade_samples` at both ends.
///
/// The first and last gains are exactly `0`; the ramp is
/// `0.5 − 0.5·cos(π·i/(L−1))`, so the derivative at the boundary is zero as
/// well. The fade is limited to half the signal so the two ramps cannot
/// overlap, and a `fade_samples` of 0 or 1 is a no-op (a one-sample fade cannot
/// reach zero and leave a continuous interior). Non-finite samples are left
/// alone; the gain is always finite.
pub fn fade_edges(samples: &mut [f32], fade_samples: usize) {
    let len = samples.len();
    let fade = fade_samples.min(len / 2);
    if fade < 2 {
        return;
    }
    let denominator = (fade - 1) as f32;
    let pi = std::f32::consts::PI;
    for i in 0..fade {
        let gain = 0.5 - 0.5 * (pi * i as f32 / denominator).cos();
        if let Some(sample) = samples.get_mut(i) {
            *sample *= gain;
        }
        if let Some(sample) = samples.get_mut(len - 1 - i) {
            *sample *= gain;
        }
    }
}

fn milliseconds(samples: usize, sample_rate: u32) -> f32 {
    if sample_rate == 0 {
        return 0.0;
    }
    (samples as f64 * 1000.0 / sample_rate as f64) as f32
}

#[cfg(test)]
mod tests;
