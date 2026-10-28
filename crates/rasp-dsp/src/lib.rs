//! DSP core for RASP: STFT/ISTFT, pitch and formant shifting, and the
//! Robotize chain.
//!
//! This crate is filled in by Stage P (PLAN P-01 onward: STFT primitives,
//! pitch shifter, spectral envelope and formant warp) and Stage B
//! (PLAN B-01 onward: carriers and the vocoder / ring / servo styles).
//!
//! Implemented so far:
//!
//! * [`resample`] — band-limited sample-rate conversion (PLAN D-03), used to
//!   take the 48 kHz corpus to whatever rate the browser's `AudioContext`
//!   runs at.
//! * [`stft`] — periodic-Hann STFT/ISTFT with identity reconstruction, a
//!   radix-2 real FFT and a hop-sized streaming path (PLAN P-01).
//! * [`pitch`] — phase-vocoder pitch shifter with identity phase locking and a
//!   per-frame ratio curve (PLAN P-02).
//! * [`envelope`] — cepstral/LPC spectral envelope, frequency warping and
//!   excitation separation (PLAN P-03).
//! * [`transform`] — pitch and formant ratios together, formant-preserving
//!   pitch shift and formant-only shift (PLAN P-03).
//! * [`endpoints`] — silence endpoint detection, explicit trimming and the
//!   boundary fades (PLAN P-04). Trimming is **opt-in** and changes the length;
//!   rendering never trims.
//! * [`params`] — natural-mode parameters, clamping, smoothing, the D12 presets
//!   and the complete natural chain (PLAN P-05).
//! * [`loudness`] — whole-clip RMS match plus a 4× true-peak limiter (PLAN P-08),
//!   written as a reusable stage for D8's robot chain.
//! * [`headroom`] — the D22 headroom-proportional preset strength and the F0
//!   measurement it runs on (PLAN P-05b): effective = nominal × `w`, where `w`
//!   falls to 0 as a clip's measured median F0 approaches the corpus envelope
//!   edge (79–323 Hz) in the requested direction.
//! * [`variant`] — the seeded `(base_voice, npc_seed) → parameter set` generator
//!   (PLAN V-01, D23/D24): integer SplitMix64 draws, quantised steps, a
//!   versioned schema and twelve archetypes clamped to D1's validated region.
//!   Headroom is deliberately **not** applied here (D22 owns it per clip).
//!
//! # Pitch shifting (P-02)
//!
//! [`pitch::PitchShifter`] realises a pitch ratio `p` as a **time stretch by
//! `p` followed by a band-limited resample by `1/p`** back to the original
//! duration. The stretch is a phase vocoder over the shared [`stft`] front end
//! with identity phase locking (Laroche–Dolson): per-frame spectral peaks are
//! picked and their phases advanced at the estimated instantaneous frequency,
//! while every other bin keeps its analysis phase offset from its nearest peak.
//! A constant ratio goes through [`resample::Resampler`]; a per-frame curve is
//! read out at a time-varying, band-limited rate.
//!
//! `N = 2048` is the default and the *analysis* is of the input signal, so the
//! requested ratio does not change it (D19): a 71 Hz input is 3.0 bins at 2048
//! no matter how far down the shift goes. Output length always equals input
//! length, the whole-buffer output is sample-aligned with the input, and the
//! reported streaming latency is exactly `N` samples. The formant correction
//! pass analyses the *shifted output* and sizes its own window from that
//! signal's F0 — 2048 normally, 4096 when it is low. The two windows are
//! independent; see [`transform`].
//!
//! Onsets are detected from the analysis frames' spectral flux and the
//! synthesis phase is reset at each one (P-04), so a plosive does not smear
//! across the window. The detector and its thresholds are documented in
//! [`pitch`]; the correction pass modifies only magnitudes, never phases.
//!
//! The crate is `#![forbid(unsafe_code)]` and stays single-threaded and
//! dependency-free so it can run inside an `AudioWorklet`.

#![forbid(unsafe_code)]

pub mod endpoints;
pub mod envelope;
pub mod headroom;
pub mod loudness;
pub mod params;
pub mod pitch;
pub mod resample;
pub mod stft;
pub mod transform;
pub mod variant;

#[cfg(test)]
mod testutil;

pub use endpoints::{
    detect_endpoints, fade_edges, trim_clip, trim_into, EndpointConfig, Endpoints, TrimmedClip,
    DEFAULT_FADE_SECONDS, DEFAULT_FRAME_SECONDS, DEFAULT_MIN_RUN_SECONDS,
    DEFAULT_POST_OFFSET_SECONDS, DEFAULT_PRE_ONSET_SECONDS, DEFAULT_THRESHOLD_DBFS,
};
pub use envelope::{
    EnvelopeAnalyzer, EnvelopeConfig, EnvelopeError, EnvelopeMethod, SpectralEnvelope,
    DEFAULT_LIFTER_SECONDS, DEFAULT_LPC_ORDER, DEFAULT_MAX_GAIN_DB,
    DEFAULT_TEMPORAL_SMOOTHING_SECONDS, DEFAULT_TRUE_ENVELOPE_PASSES, MAX_WARP_RATIO,
    MIN_WARP_RATIO,
};
pub use headroom::{
    headroom_weight, median_f0_hz, room_down_hz, room_up_hz, scale_params, F0Tracker, HeadroomMode,
    HeadroomReport, HEADROOM_HIGH_HZ, HEADROOM_LOW_HZ, HEADROOM_REFERENCE_HZ,
};
pub use loudness::{
    dbfs, dbtp, match_rms_to, rms, rms_dbfs, sample_peak, true_peak, true_peak_dbtp,
    LoudnessConfig, LoudnessError, LoudnessReport, LoudnessStage, TruePeakMeter,
    DEFAULT_ATTACK_SECONDS, DEFAULT_CEILING_DBTP, DEFAULT_INTERNAL_MARGIN_DB,
    DEFAULT_RELEASE_SECONDS, TRUE_PEAK_OVERSAMPLE,
};
pub use params::{
    tilt_db_at, NaturalParams, NaturalPreset, NaturalRenderError, NaturalRenderReport,
    NaturalRenderer, ParamSmoother, DEFAULT_SMOOTHING_SECONDS, MAX_PITCH_ST, MIN_PITCH_ST,
    NEPERS_PER_DB,
};
pub use pitch::{
    PitchShiftConfig, PitchShiftError, PitchShifter, TransientPolicy, WindowPolicy, MAX_RATIO,
    MIN_RATIO,
};
pub use resample::{Quality, ResampleError, Resampler};
pub use stft::{Complex, Spectrogram, Stft, StftConfig, StftError, StftStream};
pub use transform::{
    CorrectionWindowPolicy, VoiceTransformConfig, VoiceTransformError, VoiceTransformer,
    DEFAULT_CORRECTION_WINDOW, MAX_FORMANT_RATIO, MAX_TILT_DB_PER_OCT, MIN_FORMANT_RATIO,
    MIN_TILT_DB_PER_OCT, TILT_REFERENCE_HZ, WIDE_CORRECTION_F0_HZ, WIDE_CORRECTION_WINDOW,
};
pub use variant::{
    derive, derive_with_schema, Archetype, ArchetypeNominal, VariantError, VariantSpec,
    FORMANT_JITTER_STEPS, FORMANT_STEP, PITCH_JITTER_STEPS, PITCH_STEP, TILT_JITTER_STEPS,
    TILT_STEP, VARIANT_SCHEMA,
};
