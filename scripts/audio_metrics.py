#!/usr/bin/env python3
"""P-07: audio measurement primitives for RASP.

This module is the single implementation of every objective metric the project
uses.  ``scripts/measure_corpus.py`` (F-06) and ``scripts/metrics.py`` (P-07)
both import from here; nothing is re-implemented in either caller.

Assumptions, stated once:

* **Sample rate is 48 kHz.**  Every window, lag and BS.1770 coefficient below
  is defined at 48 kHz.  Raw ``.f32`` input has no header, so 48 kHz is an
  assumption, not a measurement.  WAV input is checked against 48 kHz and
  rejected otherwise -- resample first, do not silently measure at the wrong
  rate.  Mono only.
* **Input is 1.0 == full scale.**  16-bit PCM WAV is divided by 32768, which is
  the same convention ffmpeg uses for its f32 output.
* **No network, no randomness.**  Given the same bytes and the same pinned
  numpy/scipy (see ``scripts/requirements.txt``) the output is deterministic.
* **These are engineering metrics, not perceptual truth.**  A number passing a
  target does not mean the render sounds good; PLAN §9 sends that decision to
  a human round.

Metric caveats worth knowing before trusting a number:

F0 (YIN)
    The estimator reports the first local minimum of the cumulative mean
    normalised difference function below an absolute threshold.  That is
    deliberately biased against the classic octave-up error caused by a strong
    second harmonic.  It is *not* immune to octave errors: on breathy frames
    with HNR below ~8 dB, or on frames whose period is near the 60/400 Hz band
    edges, the tracker can still pick 2x or 0.5x the true F0.  Always compare
    medians (Q-01 uses medians only, for this reason) and treat p10/p90 as
    movement, not range.

    P-07b's pitch check is a separate mode: ``paired_f0_ratio`` widens the band
    to 50-600 Hz, uses 40 ms frames / 20 ms hop, cross-checks each YIN estimate
    against spectral peaks, and compares **per-frame ratios at the same
    instant** after intersecting the two voiced sets **on frame index**.  It
    never pools two whole-clip medians, and it never pairs by voiced rank
    (P-07d: rank pairing shifted every later pair whenever the two signals
    disagreed about one marginal frame).  Frames outside the reliable 80-550 Hz
    band, frames the spectral check rejects, and isolated octave errors on the
    *reference* are dropped before the statistics and counted; see
    ``docs/testing.md`` section 9.9.

Formants (F1/F2/F3) -- two estimators, one certified
    ``harmonic_formants`` (P-07c) is the ruler: it fits the **generative model**
    of P-03's synthetic vowel -- three two-pole resonators with free bandwidths
    plus a free ``C + alpha*log f`` source tilt -- to the harmonic amplitudes of
    100 ms frames, exactly as ``crates/rasp-dsp/src/testutil.rs::fit_formants``
    does.  Because it fits the model the signal was built from, it recovers a
    synthetic vowel's formants to well under 1%; that is the known-exact
    control ``metrics.py --formant-self-test`` enforces.  It refuses to report
    a number when the model does not explain the harmonics (see the ``N/A``
    policy on ``harmonic_formants``): on real 32 kbps speech the harmonic
    amplitudes deviate from the model by several dB, so the honest answer is
    ``N/A``, not a confident wrong formant.

    ``lpc_formants`` is the **legacy** estimator, kept for comparison only.
    Autocorrelation LPC of order 18 on 25 ms frames after pre-emphasis,
    operating on 8 kHz-decimated audio, peaks of ``1/|A(f)|``.  P-03 measured
    its bias on synthetic ground truth: 586 Hz for a true 700 Hz formant at
    F0 = 220 Hz, and worse at F0 = 300 Hz (see ``--formant-self-test``).  Use
    it to compare two renders of the same clip at the same F0; never publish an
    absolute formant from it.

    ``formant_calibration`` (P-07e) is the Q-01 path.  It fits the same model
    in the 3.5 kHz band where 32 kbps Opus is transparent, and reports a 95%
    interval from the fit's own residual sensitivity rather than a bare point.
    A formant is ``graded`` only when the residual, the degrees of freedom, the
    conditioning and that interval all support publication; otherwise it is
    ``uncalibrated`` with a reason.  D20: an uncalibrated formant is not
    published, and LPC is never substituted for it.

HNR
    Crude Praat-style ``10*log10(r/(1-r))`` from the normalised autocorrelation
    at the detected lag.  Rank voices with it; do not read it as a studio
    figure.

LUFS-I
    BS.1770-4 K-weighting, 400 ms blocks / 100 ms hop, -70 LUFS absolute and
    -10 LU relative gating.  This is integrated loudness for offline material,
    not a real-time meter.

Null test
    ``residual_db`` is the RMS of (reference - test) *relative to the reference
    RMS*: -100 dB means the error is 100 dB below the signal.  -200 dB is the
    numerical floor and means "bit-identical for practical purposes".  For
    identity/bypass renders the target is <= -100 dB.  For two different Opus
    decoders the residual is expected to be small but nonzero (tens of dB down
    at best); that is not a failure, that is D-02's actual measurement.
"""

from __future__ import annotations

import math
import os

import numpy as np
from scipy import linalg, signal
from scipy.io import wavfile

# ------------------------------------------------------------------ constants

SAMPLE_RATE = 48000
SR = SAMPLE_RATE  # measure_corpus.py's historical name

SILENCE_DBFS = -45.0
SILENCE_MIN_S = 0.020
CLIP_LEVEL = 0.999
DC_LIMIT = 0.01

F0_MIN, F0_MAX = 60.0, 400.0
F0_WIN_S, F0_HOP_S = 0.040, 0.010
YIN_THRESHOLD = 0.20
YIN_VOICED = 0.35
FRAME_RMS_FLOOR_DBFS = -55.0

# P-07b pitch ruler.  These are deliberately separate from the corpus defaults
# above: ``docs/corpus.md`` and Q-01 are built on the 60-400 Hz / 10 ms-hop
# tracker, and changing those defaults would silently rewrite a committed
# document.  The paired pitch check uses a wider band (50-600 Hz), 50% overlap
# (40 ms window / 20 ms hop), and spectral-peak refinement.
PITCH_F0_MIN = 50.0
PITCH_F0_MAX = 600.0
# A 40 ms window resolves about three periods at 75 Hz.  Below that the YIN
# estimate is still finite but no longer trustworthy to a cent, so the ruler
# reports N/A instead of FAIL: the DSP may be perfect and the ruler simply
# cannot reach.  The upper edge leaves headroom before the 600 Hz tracker edge.
PITCH_RELIABLE_MIN_HZ = 80.0
PITCH_RELIABLE_MAX_HZ = 550.0
PITCH_WIN_S = 0.040
PITCH_HOP_S = 0.020
PITCH_MIN_PAIRED_FRAMES = 4
PITCH_TOLERANCE_CENTS = 5.0
PITCH_SELF_TEST_TOLERANCE_CENTS = 1.0

# D17's whole-clip gate (P-07d): the median alone let a cell with 21 octave
# outliers and a 731-cent p90 through.  All three clauses are required.
PITCH_P90_TOLERANCE_CENTS = 25.0
PITCH_OCTAVE_OUTLIER_CENTS = 600.0

# P-07d reference reliability.  The reference is a *fixed recording*: its F0
# contour is continuous at a 20 ms hop, so a single frame that jumps an octave
# away from its neighbours is a tracker failure, not speech.  This gate is
# applied to the reference only -- applying it to the render would silently
# delete exactly the octave artifacts D17's zero-outlier clause exists to catch.
PITCH_REF_NEIGHBOUR_FRAMES = 2
PITCH_REF_NEIGHBOUR_CENTS = 600.0

# P-07d: a peak must stand out from its neighbourhood to count as spectral
# evidence.  A plain local maximum is not enough: a Hann window's sidelobes are
# local maxima about 31 dB below the mainlobe, and the cross-check was
# accepting them as "harmonics", so it could confirm a YIN reading that the
# frame did not support (e.g. 347.7 Hz on a 228 Hz synthetic vowel, or 173.7 Hz
# on the same vowel shifted to 114 Hz).  2% of the frame's peak magnitude
# (-34 dB) is below every real harmonic in these signals and above the
# sidelobes; `--formant-self-test` is unchanged to the last digit.
PITCH_PEAK_PROMINENCE = 0.02

# P-07d harmonic-support (sub-harmonic / octave) rejection.  YIN decides the
# octave from the first dip of its difference function, and on a 228 Hz
# synthetic vowel it reported 347.77 Hz on 11/99 frames -- a spurious dip, not
# a harmonic of anything.  The fix is not to trust a single dip: the frame's
# prominent spectral peaks are used to score candidate fundamentals, and a
# reading survives only when no materially different candidate explains the
# spectrum better.  The score is ``captured energy fraction x harmonic hit
# rate``: captured alone always prefers a sub-harmonic (more teeth fit more
# energy), the hit rate restores the penalty for teeth that land on nothing.
PITCH_HARMONIC_MAX_HZ = 4000.0
PITCH_HARMONIC_TOL_HZ = 8.0
PITCH_HARMONIC_TOL_REL = 0.02
# Two candidates closer than a semitone are the same note for this purpose;
# rejecting on a 0.4% difference would only remove frames over rounding.
PITCH_HARMONIC_SEPARATION_CENTS = 100.0
PITCH_HARMONIC_MARGIN = 1.5
# Below this the frame has no usable harmonic structure at all, and comparing
# candidates is comparing noise (winner scores measured 0.0-0.2 on the
# fixtures' noisy frames).
PITCH_HARMONIC_MIN_SCORE = 0.05

# BS.1770-4 K-weighting coefficients for 48 kHz.
K_B1 = [1.53512485958697, -2.69169618940638, 1.19839281085285]
K_A1 = [1.0, -1.69065929318241, 0.73248077421585]
K_B2 = [1.0, -2.0, 1.0]
K_A2 = [1.0, -1.99004745483398, 0.99007225036621]

# Formant analysis (P-07 / Q-01).  Order 18 is D3's LPC fallback; 8 kHz keeps
# the normal equations well conditioned without moving formant frequencies
# (resampling is linear and preserves analog frequency).
FORMANT_ORDER = 18
FORMANT_WIN_S = 0.025
FORMANT_HOP_S = 0.010
FORMANT_PRE_EMPHASIS = 0.97
FORMANT_MAX_HZ = 3800.0
FORMANT_MIN_HZ = 90.0
FORMANT_ANALYSIS_RATE = 8000
FORMANT_RMS_FLOOR_DBFS = -50.0

# P-07c harmonic-model ruler.  These describe the fit ported from P-03
# (``crates/rasp-dsp/src/testutil.rs::fit_formants``): three two-pole
# resonators with free bandwidths plus a free C + alpha*log(f) tilt, fitted to
# the amplitudes of the harmonics below ``FORMANT_FIT_MAX_HZ``.
#
# A 100 ms window keeps the Hann mainlobe narrower than the harmonic spacing at
# 90 Hz F0, so adjacent harmonics can be projected separately; 50 ms hop keeps
# enough frames for a median on a 0.4 s clip.  The first and last frame of a
# clip are skipped so fades and endpoints cannot bias the fit.
FORMANT_FIT_MAX_HZ = 6000.0
FORMANT_FIT_WIN_S = 0.100
FORMANT_FIT_HOP_S = 0.050
# Fewer than this many harmonics and the 6-parameter fit has essentially no
# evidence; P-03's Rust fit uses the same minimum.
FORMANT_FIT_MIN_HARMONICS = 8
# "fewer than N harmonics below F3" in the N/A policy: F3 must be bracketed by
# a harmonic grid, not extrapolated past the last observed harmonic.
FORMANT_FIT_MIN_HARMONICS_BELOW_F3 = 6
# The fit's own goodness: RMS log-amplitude residual per harmonic.  Synthetic
# vowels built from the same model land at 0.01-0.05 dB; real 32 kbps speech
# lands at 3-8 dB because it has anti-resonances and source ripple the model
# cannot represent.  1.0 dB therefore separates "the model holds" from "do not
# report a number" with a wide margin on both sides.
FORMANT_FIT_MAX_RESIDUAL_DB = 1.0
FORMANT_FIT_MIN_FRAMES = 3
# The control's tolerance (`--formant-self-test`): every formant within 1%.
FORMANT_FIT_TOLERANCE_PCT = 1.0
# Search bounds: an adult vocal tract.  They stop the optimiser from spending
# a resonance at 120 Hz or 7 kHz to chase a real-speech spectral shape, and
# they exclude nothing the synthetic control asks for.
FORMANT_FIT_BOUNDS = ((200.0, 1200.0), (600.0, 3000.0), (1500.0, 3600.0))
# Multi-start seeds for the first usable frame.  Deliberately generic vowel
# positions, not the self-test's answers; the coarse 0.72-1.28 grid around each
# seed covers F1 288-1024, F2 792-2432, F3 1728-3840 Hz.
FORMANT_FIT_INITS = ((400.0, 1100.0, 2400.0),
                     (600.0, 1500.0, 2700.0),
                     (800.0, 1900.0, 3000.0))
FORMANT_FIT_INIT_BANDWIDTHS = (90.0, 90.0, 120.0)

# P-07e calibration profile -- the Q-01 path, not the synthetic control.
#
# The 6 kHz default above is the band in which the 3-resonator model is exact
# on the signal it was built from.  It is *not* the band in which 32 kbps Opus
# is transparent: measured on synthetic vowels round-tripped through the corpus
# codec, the median fit residual is 0.3-1.1 dB up to 3.5 kHz and rises to
# 3.4-5.2 dB at 6 kHz, because above ~3.5 kHz the codec's noise-filling and
# band coding dominate the harmonic amplitudes.  Fitting where the codec is
# transparent removes that contribution from the residual budget; it does not
# make real speech fit (see `formant_calibration`).
FORMANT_CALIBRATION_MAX_HZ = 3500.0
# Residual gate for a publishable real-speech measurement.  It is set from the
# codec control, not from the synthetic model: the worst synthetic-through-Opus
# cell has a median residual of 1.08 dB at this band, so 1.5 dB leaves margin
# above the codec's own contribution.  Every real fixture lands above it
# (2.0-5.7 dB), which is the measurement, not a tolerance failure.
FORMANT_CALIBRATION_MAX_RESIDUAL_DB = 1.5
# A formant may be published only when its 95% fit interval is at most this
# fraction of the value.  8% is PLAN section 8's formant accuracy target: a
# baseline whose own uncertainty is wider than the tolerance it is used to
# calibrate cannot support a preset calibration.
FORMANT_CALIBRATION_MAX_HALF_WIDTH_PCT = 8.0
# Two-sided 95% normal quantile for the fit interval.
FORMANT_CALIBRATION_Z = 1.96
FORMANT_CALIBRATION_MIN_FRAMES = 3
# A fit whose normal matrix is this ill-conditioned does not identify its
# parameters at all (one direction of the residual surface is flat); report it
# as uncalibrated instead of letting the pseudo-inverse invent a number.
FORMANT_CALIBRATION_MAX_CONDITION = 1.0e8


class AudioFormatError(ValueError):
    """Input is not the 48 kHz mono audio this module promises to measure."""


# ------------------------------------------------------------------ utilities

def db(x, floor=-200.0):
    """20*log10(x), with a finite floor instead of -inf/NaN for x <= 0."""
    return floor if x <= 0 else 20.0 * math.log10(x)


def runs_of(mask):
    """Start/end indices of True runs in a boolean mask (half-open)."""
    m = np.concatenate(([False], np.asarray(mask, dtype=bool), [False]))
    d = np.diff(m.astype(np.int8))
    return np.flatnonzero(d == 1), np.flatnonzero(d == -1)


def cents_between(ref_hz, test_hz):
    """Signed cents of test relative to ref: +100 = one semitone up."""
    if not (ref_hz and test_hz and ref_hz > 0 and test_hz > 0):
        return float("nan")
    return 1200.0 * math.log2(float(test_hz) / float(ref_hz))


def percent_delta(ref, test):
    """(test - ref) / ref as a percentage, NaN when ref is unusable."""
    if not (ref and ref > 0 and math.isfinite(ref) and math.isfinite(test)):
        return float("nan")
    return 100.0 * (float(test) - float(ref)) / float(ref)


# ------------------------------------------------------------------- loading

def load_f32le(path):
    """Headerless mono little-endian f32 (ffmpeg ``-f f32le``), 48 kHz assumed.

    The file carries no sample rate, channel count or scale.  An odd byte count
    means a truncated write, not something to round down silently.
    """
    size = os.path.getsize(path)
    if size % 4:
        raise AudioFormatError(
            "%s: %d bytes is not a whole number of f32 samples (truncated?)"
            % (path, size))
    return np.fromfile(path, dtype="<f4").astype(np.float64)


def load_wav(path):
    """48 kHz mono WAV -> float64 in [-1, 1].

    Accepts 16-bit PCM (the fixture format), 32-bit PCM and 32/64-bit float.
    Rejects any other sample rate or channel count instead of resampling or
    downmixing behind the caller's back.
    """
    rate, data = wavfile.read(path, mmap=False)
    if rate != SAMPLE_RATE:
        raise AudioFormatError(
            "%s: sample rate is %d Hz; these metrics assume %d Hz. Resample "
            "first, e.g. ffmpeg -i in.wav -ar %d out.wav"
            % (path, rate, SAMPLE_RATE, SAMPLE_RATE))
    if data.ndim == 2:
        if data.shape[1] != 1:
            raise AudioFormatError(
                "%s: %d channels; these metrics are mono only"
                % (path, data.shape[1]))
        data = data[:, 0]

    if data.dtype == np.int16:
        x = data.astype(np.float64) / 32768.0
    elif data.dtype == np.int32:
        x = data.astype(np.float64) / 2147483648.0
    elif data.dtype == np.uint8:
        x = (data.astype(np.float64) - 128.0) / 128.0
    elif data.dtype in (np.float32, np.float64):
        x = data.astype(np.float64)
    else:
        raise AudioFormatError("%s: unsupported WAV dtype %s"
                               % (path, data.dtype))
    return x


def load_audio(path, fmt="auto"):
    """Load ``path`` as mono float64 at 48 kHz.

    ``fmt`` is ``auto`` (suffix, then RIFF magic), ``f32`` or ``wav``.
    """
    fmt = (fmt or "auto").lower()
    if fmt == "auto":
        ext = os.path.splitext(path)[1].lower()
        if ext in (".f32", ".f32le", ".raw", ".pcm"):
            fmt = "f32"
        elif ext in (".wav", ".wave"):
            fmt = "wav"
        else:
            with open(path, "rb") as f:
                head = f.read(12)
            fmt = "wav" if head[:4] == b"RIFF" and head[8:12] == b"WAVE" else "f32"
    if fmt == "f32":
        return load_f32le(path)
    if fmt == "wav":
        return load_wav(path)
    raise AudioFormatError("unknown input format %r for %s" % (fmt, path))


# -------------------------------------------------------------- level metrics

def level_stats(x, clip_level=CLIP_LEVEL):
    """Sample count, duration, peak, RMS, DC and clipping for the whole clip."""
    x = np.asarray(x, dtype=np.float64)
    n = len(x)
    peak = float(np.max(np.abs(x))) if n else 0.0
    rms = float(np.sqrt(np.mean(x ** 2))) if n else 0.0
    dc = float(np.mean(x)) if n else 0.0
    clipped = int(np.count_nonzero(np.abs(x) >= clip_level)) if n else 0
    finite = bool(np.all(np.isfinite(x)))
    return {
        "samples": n,
        "duration_s": n / float(SAMPLE_RATE),
        "peak": peak,
        "peak_dbfs": db(peak),
        "rms": rms,
        "rms_dbfs": db(rms),
        "dc": dc,
        "clipped_samples": clipped,
        "finite": finite,
    }


def null_test(ref, test):
    """Compare two waveforms sample by sample.

    The comparison uses the common prefix when lengths differ; ``same_length``
    says whether that happened, so a caller can fail a sample-count gate rather
    than silently comparing less audio.

    Returns ``residual_db`` (RMS error relative to reference RMS, more negative
    is better) and ``max_residual_db`` (peak error relative to reference RMS),
    plus the same quantities in dBFS.  A bit-identical pair yields -200 dB,
    the finite numerical floor.
    """
    ref = np.asarray(ref, dtype=np.float64)
    test = np.asarray(test, dtype=np.float64)
    n = min(len(ref), len(test))
    r = ref[:n]
    t = test[:n]
    diff = r - t
    rms_ref = float(np.sqrt(np.mean(r ** 2))) if n else 0.0
    rms_diff = float(np.sqrt(np.mean(diff ** 2))) if n else 0.0
    max_diff = float(np.max(np.abs(diff))) if n else 0.0
    finite = bool(np.all(np.isfinite(diff)))

    if rms_ref > 0:
        residual_db = db(rms_diff / rms_ref)
        max_residual_db = db(max_diff / rms_ref)
    else:
        # Silent reference: only "also silent" can be a null.
        residual_db = -200.0 if rms_diff == 0 else 200.0
        max_residual_db = residual_db

    return {
        "n_ref": int(len(ref)),
        "n_test": int(len(test)),
        "n_compared": int(n),
        "same_length": len(ref) == len(test),
        "finite": finite,
        "rms_ref": rms_ref,
        "rms_diff": rms_diff,
        "max_diff": max_diff,
        "rms_diff_dbfs": db(rms_diff),
        "max_diff_dbfs": db(max_diff),
        "residual_db": residual_db,
        "max_residual_db": max_residual_db,
        # Conventional SNR sign: positive means the error is below the signal.
        "snr_db": -residual_db,
    }


# -------------------------------------------------------------------- pitch

def _weighted_median(values, weights):
    """Median of ``values`` weighted by non-negative ``weights``."""
    values = np.asarray(values, dtype=np.float64)
    weights = np.maximum(np.asarray(weights, dtype=np.float64), 0.0)
    order = np.argsort(values)
    values = values[order]
    weights = weights[order]
    total = float(weights.sum())
    if total <= 0.0:
        return float(np.median(values))
    cumulative = np.cumsum(weights)
    index = int(np.searchsorted(cumulative, 0.5 * total))
    return float(values[min(index, len(values) - 1)])


def _spectral_peaks(frame, sample_rate, nfft):
    """Local magnitude maxima of a Hann-windowed frame, parabolically refined.

    A peak must be *prominent* (``PITCH_PEAK_PROMINENCE`` of the frame's peak
    magnitude), not merely a local maximum.  Hann sidelobes are local maxima
    ~31 dB down; without the prominence floor they were returned as "spectral
    peaks" and could confirm a spurious YIN reading instead of rejecting it
    (P-07d).  This is the estimator's own hygiene: no tolerance depends on it.
    """
    magnitude = np.abs(np.fft.rfft(frame * np.hanning(len(frame)), n=nfft))
    if len(magnitude) < 3:
        return np.empty(0), np.empty(0)
    peak_level = float(magnitude.max())
    if not math.isfinite(peak_level) or peak_level <= 0:
        return np.empty(0), np.empty(0)
    indices, _ = signal.find_peaks(
        magnitude, prominence=PITCH_PEAK_PROMINENCE * peak_level)
    indices = indices[(indices > 0) & (indices < len(magnitude) - 1)]
    if len(indices) == 0:
        return np.empty(0), np.empty(0)
    left = np.log(np.maximum(magnitude[indices - 1], 1e-30))
    centre = np.log(np.maximum(magnitude[indices], 1e-30))
    right = np.log(np.maximum(magnitude[indices + 1], 1e-30))
    curvature = left - 2.0 * centre + right
    with np.errstate(divide="ignore", invalid="ignore"):
        delta = np.where(np.abs(curvature) > 1e-12,
                         0.5 * (left - right) / curvature, 0.0)
    delta = np.clip(delta, -0.5, 0.5)
    step_hz = sample_rate / float(nfft)
    return (indices + delta) * step_hz, magnitude[indices]


def _refine_f0_with_peaks(frame, f0_hz, sample_rate, min_hz, max_hz):
    """Cross-check one YIN estimate against the frame's spectral peaks.

    YIN decides the octave (its first-dip rule is what makes it resistant to
    the classic octave-up error).  The spectral peaks then refine *within* that
    octave: every harmonic peak consistent with the YIN estimate votes for a
    candidate ``peak_hz / harmonic``, weighted by peak energy, and the weighted
    median becomes the refined value.  Peaks far from the expected harmonic
    grid are ignored, so noise cannot drag the estimate.

    Returns ``(refined_hz, agreed)``.  ``agreed`` says whether at least one
    spectral peak confirmed the YIN estimate; a failure to agree leaves the YIN
    value untouched rather than inventing a replacement.
    """
    nfft = 1 << int(math.ceil(math.log2(max(4 * len(frame), 4096))))
    peak_hz, peak_mag = _spectral_peaks(frame, sample_rate, nfft)
    if len(peak_hz) == 0 or not math.isfinite(f0_hz) or f0_hz <= 0:
        return f0_hz, False
    search_lo = max(min_hz * 0.6, f0_hz * 0.75)
    search_hi = min(max_hz * 1.5, f0_hz * 1.25, 0.5 * sample_rate)
    usable = (peak_hz >= search_lo) & (peak_hz <= search_hi) & (peak_mag > 0)
    if not usable.any():
        return f0_hz, False
    peak_hz = peak_hz[usable]
    peak_mag = peak_mag[usable]
    harmonic = np.round(peak_hz / f0_hz).astype(np.int64)
    valid = (harmonic >= 1) & (harmonic <= 16)
    if not valid.any():
        return f0_hz, False
    candidate = peak_hz[valid] / harmonic[valid]
    weight = peak_mag[valid] ** 2
    close = np.abs(candidate - f0_hz) <= 0.08 * f0_hz
    if not close.any():
        return f0_hz, False
    refined = _weighted_median(candidate[close], weight[close])
    if not (min_hz <= refined <= max_hz):
        return f0_hz, False
    if abs(refined - f0_hz) > 0.08 * f0_hz:
        return f0_hz, False
    return float(refined), True


def f0_track(x, sample_rate=SAMPLE_RATE, min_hz=None, max_hz=None,
             win_s=None, hop_s=None, refine=False, rms_floor_dbfs=None,
             spectral_check=False, return_agreement=False,
             return_indices=False):
    """YIN F0 + crude HNR, with optional band/window and spectral refinement.

    Defaults (60-400 Hz, 40 ms frames / 10 ms hop, no refinement) are the
    corpus tracker: ``docs/corpus.md`` and Q-01 depend on them and must not
    move.  The P-07b pitch ruler passes ``min_hz=50``, ``max_hz=600``,
    ``hop_s=0.020`` and ``spectral_check=True`` (it does not change the YIN
    value; disagreement only drops the frame).

    ``return_indices`` appends the **frame ordinal** of every voicing frame
    (frame ``i`` covers samples ``i*hop .. i*hop+wlen``).  Two tracks built with
    the same window/hop over two equally long signals therefore share one time
    grid, and P-07d pairs them by index instead of by compacted rank.  The
    default (``False``) leaves every existing caller, and ``docs/corpus.md``,
    byte-identical.

    Return order: ``(f0_hz, hnr_db, frame_rms_dbfs)``, then the agreement mask
    when ``return_agreement``, then the indices when ``return_indices``.  The
    first two arrays are voiced-only (compacted); the third covers every frame.

    ``d(tau)`` is the YIN difference function over a 20 ms integration window;
    the first local minimum below the absolute threshold wins, so a strong
    second harmonic is less likely to drag the estimate an octave up.  HNR
    comes from the normalised autocorrelation at the chosen lag (Praat-style,
    crude).

    Octave ambiguity is still possible: see the module docstring.  Use medians,
    and for the paired ruler keep only frames voiced in both signals.
    """
    min_hz = F0_MIN if min_hz is None else float(min_hz)
    max_hz = F0_MAX if max_hz is None else float(max_hz)
    win_s = F0_WIN_S if win_s is None else float(win_s)
    hop_s = F0_HOP_S if hop_s is None else float(hop_s)
    wlen = int(round(win_s * sample_rate))
    hop = max(1, int(round(hop_s * sample_rate)))

    def empty_track():
        base = (np.empty(0), np.empty(0), np.empty(0))
        if return_agreement:
            base += (np.empty(0, dtype=bool),)
        if return_indices:
            base += (np.empty(0, dtype=np.int64),)
        return base

    if len(x) < wlen or wlen < 16:
        return empty_track()
    frames = np.lib.stride_tricks.sliding_window_view(x, wlen)[::hop]
    frames = frames - frames.mean(axis=1, keepdims=True)
    frame_rms = np.sqrt(np.mean(frames ** 2, axis=1))

    lo = max(2, int(math.floor(sample_rate / max_hz)))
    hi = min(wlen - 2, int(math.ceil(sample_rate / min_hz)))
    if hi <= lo + 3 or wlen - hi - 2 < 8:
        return empty_track()
    w = wlen - hi - 2                      # integration window
    nfft = 1 << int(math.ceil(math.log2(len(frames[0]) + w)))

    a = frames[:, :w]
    spec_a = np.fft.rfft(a, n=nfft, axis=1)
    spec_b = np.fft.rfft(frames, n=nfft, axis=1)
    corr = np.fft.irfft(spec_b * np.conj(spec_a), n=nfft, axis=1)[:, :hi + 1]
    ac_full = np.fft.irfft(spec_b * np.conj(spec_b), n=nfft, axis=1)[:, :hi + 1]

    cs = np.concatenate((np.zeros((frames.shape[0], 1)),
                         np.cumsum(frames ** 2, axis=1)), axis=1)
    tau = np.arange(hi + 1)
    e1 = cs[:, tau + w] - cs[:, tau]
    e0 = (a ** 2).sum(axis=1)[:, None]
    d = np.maximum(e0 + e1 - 2.0 * corr, 0.0)
    d[:, 0] = 0.0
    cum = np.cumsum(d, axis=1)
    cmnd = np.ones_like(d)
    with np.errstate(divide="ignore", invalid="ignore"):
        cmnd[:, 1:] = d[:, 1:] * tau[None, 1:] / np.maximum(cum[:, 1:], 1e-20)

    seg = cmnd[:, lo:hi + 1]
    # YIN wants the first *local minimum* below the threshold, not the first
    # sample that happens to dip under it (that biases F0 sharp by ~10%).
    local_min = np.zeros(seg.shape, dtype=bool)
    local_min[:, 1:-1] = (seg[:, 1:-1] < seg[:, :-2]) & (seg[:, 1:-1] <= seg[:, 2:])
    below = local_min & (seg < YIN_THRESHOLD)
    has_dip = below.any(axis=1)
    idx = np.where(has_dip, np.argmax(below, axis=1), np.argmin(seg, axis=1))
    chosen = seg[np.arange(len(idx)), idx]
    voiced = chosen < YIN_VOICED
    if rms_floor_dbfs is not None:
        # Digital silence makes the YIN difference function degenerate (all
        # zero), which would otherwise read as a maximally periodic frame.
        # The corpus tracker keeps its historical behaviour; the paired pitch
        # ruler opts in so a silent tail can never become a "voiced" vote.
        frame_level = np.asarray([db(float(v)) for v in frame_rms])
        voiced &= frame_level > float(rms_floor_dbfs)

    f0s, hnrs, agreements = [], [], []
    for i in np.flatnonzero(voiced):
        kk = lo + int(idx[i])
        delta = 0.0
        if kk < hi:
            y0, y1, y2 = cmnd[i, kk - 1], cmnd[i, kk], cmnd[i, kk + 1]
            den = y0 - 2 * y1 + y2
            delta = 0.5 * (y0 - y2) / den if abs(den) > 1e-12 else 0.0
            delta = float(np.clip(delta, -0.5, 0.5))
        f0 = sample_rate / (kk + delta)
        agreed = True
        if refine or spectral_check:
            refined, agreed = _refine_f0_with_peaks(
                frames[i], f0, sample_rate, min_hz, max_hz)
            if refine and agreed:
                f0 = refined
        f0s.append(f0)
        agreements.append(agreed)
        # HNR from the full 40 ms frame's normalised autocorrelation at that
        # lag; the YIN difference function saturates near 40 dB on clean Opus.
        tot = float(cs[i, -1])
        e_hi = tot - float(cs[i, kk])
        r = float(ac_full[i, kk]) / math.sqrt(max(float(cs[i, -kk - 1]) * e_hi, 1e-20))
        r = float(np.clip(r, 1e-4, 0.999))
        hnrs.append(10.0 * math.log10(r / (1.0 - r)))

    result = (np.asarray(f0s), np.asarray(hnrs),
              np.asarray([db(float(v)) for v in frame_rms]))
    if return_agreement:
        result += (np.asarray(agreements, dtype=bool),)
    if return_indices:
        result += (np.flatnonzero(voiced).astype(np.int64),)
    return result


def f0_stats(x, sample_rate=SAMPLE_RATE, smooth=5):
    """Pooled F0/HNR summary, with the median smoothing the corpus run uses.

    Returns raw frame arrays plus the pooled median/p10/p90 over voiced frames.
    ``voiced_fraction`` is voiced frames / all frames, not voiced / total time.
    """
    f0, hnr, frame_rms = f0_track(x, sample_rate)
    if len(f0) > 2 and smooth >= 3:
        f0 = signal.medfilt(f0, kernel_size=smooth)
        hnr = signal.medfilt(hnr, kernel_size=smooth)
    if len(f0):
        f0_med = float(np.median(f0))
        f0_p10, f0_p90 = (float(v) for v in np.percentile(f0, [10, 90]))
        hnr_med = float(np.median(hnr))
    else:
        f0_med = f0_p10 = f0_p90 = float("nan")
        hnr_med = float("nan")
    return {
        "f0_hz": f0,
        "hnr_db": hnr,
        "frame_rms_dbfs": frame_rms,
        "f0_median_hz": f0_med,
        "f0_p10_hz": f0_p10,
        "f0_p90_hz": f0_p90,
        "hnr_median_db": hnr_med,
        "voiced_frames": int(len(f0)),
        "frames": int(len(frame_rms)),
        "voiced_fraction": len(f0) / max(1, len(frame_rms)),
    }


def hnr_median_db(x, sample_rate=SAMPLE_RATE):
    """Median HNR (dB) over voiced frames.

    Convenience wrapper over ``f0_stats`` so HNR has a discoverable name; the
    implementation and its caveats live in ``f0_track``.  HNR is only defined
    where the F0 tracker found a lag, so silence returns NaN.
    """
    return f0_stats(x, sample_rate)["hnr_median_db"]


# ---------------------------------------------------------------- spectrum

def spectral_centroid(x, sample_rate=SAMPLE_RATE):
    """Median spectral centroid (Hz) over 25 ms Hann frames above -55 dBFS."""
    wlen = int(round(0.025 * sample_rate))
    hop = int(round(0.010 * sample_rate))
    if len(x) < wlen:
        return float("nan")
    frames = np.lib.stride_tricks.sliding_window_view(x, wlen)[::hop]
    rms = np.sqrt(np.mean(frames ** 2, axis=1))
    keep = rms > 10 ** (FRAME_RMS_FLOOR_DBFS / 20.0)
    if not keep.any():
        return float("nan")
    frames = frames[keep] * np.hanning(wlen)
    nfft = 2048
    mag = np.abs(np.fft.rfft(frames, n=nfft, axis=1))
    freqs = np.fft.rfftfreq(nfft, 1.0 / sample_rate)
    den = mag.sum(axis=1)
    ok = den > 0
    if not ok.any():
        return float("nan")
    cents = (mag[ok] * freqs).sum(axis=1) / den[ok]
    return float(np.median(cents))


# ---------------------------------------------------------------- loudness

def lufs_integrated(x, sample_rate=SAMPLE_RATE):
    """BS.1770-4 integrated loudness (LUFS) for 48 kHz material.

    Returns NaN for clips shorter than one 400 ms block, -inf for digital
    silence.  The K-weighting coefficients are the 48 kHz set; do not call
    this on other rates.
    """
    if len(x) < int(0.4 * sample_rate):
        return float("nan")
    y = signal.lfilter(K_B1, K_A1, x)
    y = signal.lfilter(K_B2, K_A2, y)
    blk, hop = int(0.4 * sample_rate), int(0.1 * sample_rate)
    ms = y ** 2
    cs = np.concatenate(([0.0], np.cumsum(ms)))
    starts = np.arange(0, len(y) - blk + 1, hop)
    if len(starts) == 0:
        return float("nan")
    means = (cs[starts + blk] - cs[starts]) / blk
    ok = means > 1e-12
    if not ok.any():
        return float("-inf")
    l = -0.691 + 10.0 * np.log10(means[ok])
    gated = l[l > -70.0]
    if len(gated) == 0:
        return float("-inf")
    rel = -0.691 + 10.0 * math.log10(np.mean(10 ** ((gated + 0.691) / 10.0))) - 10.0
    gated = gated[gated > rel]
    if len(gated) == 0:
        return float("-inf")
    return float(-0.691 + 10.0 * math.log10(np.mean(10 ** ((gated + 0.691) / 10.0))))


# ---------------------------------------------------------------- formants

def _lpc(frame, order):
    """LPC coefficients ``[1, a1..ap]`` from a pre-emphasised frame.

    Autocorrelation method (Toeplitz normal equations).  Returns ``None`` for
    frames that cannot support the solve (silence, singular).
    """
    r = np.correlate(frame, frame, mode="full")[len(frame) - 1:len(frame) + order]
    if r[0] <= 1e-12 or not np.all(np.isfinite(r)):
        return None
    try:
        a = linalg.solve_toeplitz((r[:order], r[:order]), -r[1:order + 1])
    except Exception:
        return None
    if not np.all(np.isfinite(a)):
        return None
    return np.concatenate(([1.0], a))


def _formants_from_lpc(a, sample_rate, nfft=2048, min_sep_hz=150.0,
                       prominence=0.03):
    """F1/F2/F3 candidates from the LPC spectral envelope (Hz).

    Picking the lowest few *poles* is fragile: a broad low-frequency pole (the
    classic 300-600 Hz shoulder on a windowed frame) can outrank the true F1.
    Peaks of ``1/|A(f)|`` with a small prominence gate, merged when closer than
    ``min_sep_hz``, track the audible resonances far more reliably.
    """
    if not np.all(np.isfinite(a)) or a[0] == 0:
        return []
    f, h = signal.freqz([1.0], a, worN=nfft, fs=sample_rate)
    mag = np.abs(h)
    band = (f >= FORMANT_MIN_HZ) & (f <= FORMANT_MAX_HZ)
    if not band.any():
        return []
    fb = f[band]
    mb = mag[band]
    peak_level = float(mb.max())
    if not math.isfinite(peak_level) or peak_level <= 0:
        return []
    mb = mb / peak_level
    idx, props = signal.find_peaks(mb, prominence=prominence)
    if len(idx) == 0:
        return []
    freqs = fb[idx]
    proms = props["prominences"]

    # Merge shoulders: keep the more prominent of two peaks that are closer
    # than min_sep_hz (F1/F2 are never that close in speech).
    merged = []
    for i in np.argsort(freqs):
        f_i = float(freqs[i])
        p_i = float(proms[i])
        if merged and f_i - merged[-1][0] < min_sep_hz:
            if p_i > merged[-1][1]:
                merged[-1] = (f_i, p_i)
        else:
            merged.append((f_i, p_i))
    return [m[0] for m in merged[:3]]


def lpc_formants(x, sample_rate=SAMPLE_RATE):
    """Median F1/F2/F3 (Hz) over 25 ms LPC frames.

    Each frame is pre-emphasised, Hamming-windowed and analysed at 8 kHz with
    order 18.  Only frames above -50 dBFS are used.  A frame contributes only
    if at least three spectral-envelope peaks survive the 90-3800 Hz band and
    the prominence/merge gates, so the median is over comparable frames.

    Returns a dict with ``f1_hz``, ``f2_hz``, ``f3_hz``, ``frames_used``,
    ``frames_total`` and ``analysis_rate_hz``.  Values are NaN when the clip
    is too short or entirely silent.  See the module docstring: on lossy
    speech these are comparison numbers, not ground truth.
    """
    x = np.asarray(x, dtype=np.float64)
    decim = max(1, int(round(sample_rate / float(FORMANT_ANALYSIS_RATE))))
    rate = sample_rate / float(decim)
    if decim > 1:
        x = signal.resample_poly(x, 1, decim)

    wlen = int(round(FORMANT_WIN_S * rate))
    hop = int(round(FORMANT_HOP_S * rate))
    result = {
        "f1_hz": float("nan"),
        "f2_hz": float("nan"),
        "f3_hz": float("nan"),
        "frames_used": 0,
        "frames_total": 0,
        "analysis_rate_hz": float(rate),
    }
    if len(x) < wlen or wlen < 2:
        return result

    frames = np.lib.stride_tricks.sliding_window_view(x, wlen)[::hop]
    result["frames_total"] = int(len(frames))
    floor = 10 ** (FORMANT_RMS_FLOOR_DBFS / 20.0)
    window = np.hamming(wlen)
    f1s, f2s, f3s = [], [], []

    for frame in frames:
        frame = frame - float(np.mean(frame))
        rms = math.sqrt(float(np.mean(frame ** 2)))
        if rms <= floor:
            continue
        emph = np.empty_like(frame)
        emph[0] = frame[0]
        emph[1:] = frame[1:] - FORMANT_PRE_EMPHASIS * frame[:-1]
        a = _lpc(emph * window, FORMANT_ORDER)
        if a is None:
            continue
        f = _formants_from_lpc(a, rate)
        if len(f) < 3:
            continue
        f1s.append(f[0])
        f2s.append(f[1])
        f3s.append(f[2])

    if f1s:
        result["f1_hz"] = float(np.median(f1s))
        result["f2_hz"] = float(np.median(f2s))
        result["f3_hz"] = float(np.median(f3s))
        result["frames_used"] = len(f1s)
    return result


# ------------------------------------------------ P-07c harmonic-formant ruler

def _resonator_log_amplitude(frequency, formants, bandwidths,
                            sample_rate=SAMPLE_RATE):
    """Log magnitude of a cascade of two-pole resonators at ``frequency``.

    This is the generative model of P-03's synthetic vowel (and therefore of
    ``synthesize_vowel``): ``prod 1/|1 - 2r cos(w_j) z + r^2 z^2|`` with
    ``r = exp(-pi B_j / fs)`` evaluated at ``z = exp(-j*2*pi*f/fs)``.  The fit
    uses the same expression, which is what makes the control exact.
    """
    frequency = np.asarray(frequency, dtype=np.float64)
    total = np.zeros_like(frequency)
    for formant, bandwidth in zip(formants, bandwidths):
        radius = math.exp(-math.pi * float(bandwidth) / sample_rate)
        omega = 2.0 * math.pi * frequency / sample_rate
        z = np.exp(-1j * omega)
        centre = 2.0 * math.pi * float(formant) / sample_rate
        denominator = (1.0 + (-2.0 * radius * math.cos(centre)) * z
                       + (radius * radius) * z * z)
        total -= np.log(np.maximum(np.abs(denominator), 1e-12))
    return total


def synthesize_vowel(f0_hz, formants, bandwidths, samples,
                     sample_rate=SAMPLE_RATE, fade_s=0.005):
    """A stationary synthetic vowel whose formants are known by construction.

    Port of ``crates/rasp-dsp/src/testutil.rs::vowel`` (P-03): harmonic ``h`` at
    ``h*f0`` gets the cascade-of-resonators envelope times ``1/h``.  Peak
    normalised to 0.5, with a short fade so the file has no clicks.  This is
    primary ground truth, not a measurement: no estimator is involved.
    """
    formants = [float(v) for v in formants]
    bandwidths = [float(v) for v in bandwidths]
    if len(formants) != len(bandwidths):
        raise ValueError("formants and bandwidths must have the same length")
    n = int(samples)
    out = np.zeros(n, dtype=np.float64)
    if n == 0 or f0_hz <= 0:
        return out
    t = np.arange(n, dtype=np.float64) / float(sample_rate)
    nyquist = 0.5 * float(sample_rate)
    harmonic = 1
    while harmonic * f0_hz < nyquist:
        frequency = harmonic * float(f0_hz)
        log_envelope = float(_resonator_log_amplitude(
            np.array([frequency]), formants, bandwidths, sample_rate)[0])
        out += (math.exp(log_envelope) / harmonic) * np.sin(
            2.0 * math.pi * frequency * t)
        harmonic += 1
    peak = float(np.max(np.abs(out))) if n else 0.0
    if peak > 0:
        out *= 0.5 / peak
    fade = min(int(round(fade_s * sample_rate)), n // 4)
    if fade > 0:
        ramp = np.linspace(0.0, 1.0, fade, endpoint=False)
        out[:fade] *= ramp
        out[-fade:] *= ramp[::-1]
    return out


def harmonic_amplitudes(frame, f0_hz, sample_rate=SAMPLE_RATE,
                        max_hz=FORMANT_FIT_MAX_HZ):
    """Amplitude of every harmonic of ``frame``, by direct Fourier projection.

    Each harmonic is projected at its exact frequency through a Hann window and
    normalised by the window's coherent gain.  That is more accurate than
    reading the nearest FFT bin (which suffers scalloping loss up to ~1.4 dB)
    and, unlike a longer transform, does not blur adjacent harmonics.  Returns
    ``(frequencies, amplitudes)``.
    """
    frame = np.asarray(frame, dtype=np.float64)
    n = len(frame)
    nyquist = 0.5 * float(sample_rate)
    limit = min(float(max_hz), nyquist - 100.0)
    count = int(math.floor(limit / float(f0_hz))) if f0_hz > 0 else 0
    if n == 0 or count < 1:
        return np.empty(0), np.empty(0)
    frequencies = np.arange(1, count + 1, dtype=np.float64) * float(f0_hz)
    window = np.hanning(n)
    t = np.arange(n, dtype=np.float64) / float(sample_rate)
    basis = np.exp(-2j * np.pi * np.outer(frequencies, t))
    amplitudes = 2.0 * np.abs(basis @ (frame * window)) / float(window.sum())
    return frequencies, amplitudes


def _formant_model_residual(frequencies, log_amplitudes, formants, bandwidths,
                            sample_rate=SAMPLE_RATE):
    """Log-amplitude residual of the 3-resonator + ``C + alpha*log f`` model.

    The source tilt is not searched: for any candidate formant/bandwidth set
    the residual is regressed on ``[1, log f]`` and the remaining sum of
    squares is returned, exactly as P-03's Rust fit does.  That makes the tilt
    free without adding two parameters to the search.
    """
    count = len(frequencies)
    if count < FORMANT_FIT_MIN_HARMONICS:
        return float("inf")
    shaped = (log_amplitudes
              - _resonator_log_amplitude(frequencies, formants, bandwidths,
                                         sample_rate))
    logs = np.log(frequencies)
    mean_x = float(logs.mean())
    mean_y = float(shaped.mean())
    sxx = float(((logs - mean_x) ** 2).sum())
    sxy = float(((logs - mean_x) * (shaped - mean_y)).sum())
    alpha = sxy / sxx if sxx > 1e-12 else 0.0
    intercept = mean_y - alpha * mean_x
    error = shaped - intercept - alpha * logs
    return float((error * error).sum())


def _formants_within_bounds(formants, sample_rate=SAMPLE_RATE):
    """Search bounds: adult vocal-tract ranges plus the ordering constraint."""
    (f1_lo, f1_hi), (f2_lo, f2_hi), (f3_lo, f3_hi) = FORMANT_FIT_BOUNDS
    return (f1_lo < formants[0] < f1_hi
            and f2_lo < formants[1] < f2_hi
            and f3_lo < formants[2] < f3_hi
            and formants[0] * 1.15 < formants[1]
            and formants[1] * 1.15 < formants[2]
            and formants[2] < 0.95 * 0.5 * sample_rate)


def _fit_harmonic_formants(frequencies, log_amplitudes, init, init_bandwidths,
                           sample_rate=SAMPLE_RATE, coarse=True):
    """Six-parameter fit of the generative model (P-03's method).

    A coarse 0.72-1.28 grid around ``init`` is searched before coordinate
    descent with halving steps, because descent alone locks onto a harmonic
    instead of a formant -- the failure mode that would let the *ruler* decide
    the answer.  ``coarse=False`` is for a frame that continues a previous one:
    it starts from that frame's fit with 5% steps and no grid, which is what
    keeps a whole clip affordable.

    Returns ``(formants, bandwidths, residual_sum)``.
    """
    formants = [float(v) for v in init]
    bandwidths = [min(900.0, max(20.0, float(v))) for v in init_bandwidths]
    if coarse:
        fractions = (0.20, 0.20, 0.20, 0.50, 0.50, 0.50)
    else:
        fractions = (0.05, 0.05, 0.05, 0.50, 0.50, 0.50)
    steps = [init[i] * fractions[i] for i in range(3)]
    steps += [init_bandwidths[i] * fractions[3 + i] for i in range(3)]

    best = _formant_model_residual(frequencies, log_amplitudes, formants,
                                   bandwidths, sample_rate)
    if coarse:
        grid_best = (best, list(formants))
        for i1 in range(9):
            f1 = init[0] * (0.72 + 0.07 * i1)
            for i2 in range(9):
                f2 = init[1] * (0.72 + 0.07 * i2)
                if f2 <= f1 * 1.15:
                    continue
                for i3 in range(9):
                    f3 = init[2] * (0.72 + 0.07 * i3)
                    if f3 <= f2 * 1.15:
                        continue
                    candidate = [f1, f2, f3]
                    if not _formants_within_bounds(candidate, sample_rate):
                        continue
                    residual = _formant_model_residual(
                        frequencies, log_amplitudes, candidate, bandwidths,
                        sample_rate)
                    if residual < grid_best[0]:
                        grid_best = (residual, candidate)
        best, formants = grid_best

    for _ in range(7):
        improved = True
        while improved:
            improved = False
            for index in range(3):
                for direction in (-1.0, 1.0):
                    candidate = list(formants)
                    candidate[index] += direction * steps[index]
                    if not _formants_within_bounds(candidate, sample_rate):
                        continue
                    residual = _formant_model_residual(
                        frequencies, log_amplitudes, candidate, bandwidths,
                        sample_rate)
                    if residual < best:
                        best, formants, improved = residual, candidate, True
            for index in range(3):
                for direction in (-1.0, 1.0):
                    candidate = list(bandwidths)
                    candidate[index] = min(
                        900.0, max(20.0,
                                   candidate[index] + direction
                                   * steps[3 + index]))
                    residual = _formant_model_residual(
                        frequencies, log_amplitudes, formants, candidate,
                        sample_rate)
                    if residual < best:
                        best, bandwidths, improved = residual, candidate, True
        steps = [s * 0.5 for s in steps]
    return formants, bandwidths, best


def _frame_f0_hz(frame, sample_rate=SAMPLE_RATE):
    """The F0 of one analysis frame, or None when the frame is not voiced."""
    f0, _, _ = f0_track(
        np.asarray(frame, dtype=np.float64), sample_rate,
        min_hz=PITCH_F0_MIN, max_hz=PITCH_F0_MAX,
        win_s=len(frame) / float(sample_rate),
        hop_s=1.0 / float(sample_rate), refine=True,
        rms_floor_dbfs=FORMANT_RMS_FLOOR_DBFS)
    if len(f0) == 0:
        return None
    value = float(f0[0])
    if not math.isfinite(value) or not (PITCH_F0_MIN <= value <= PITCH_F0_MAX):
        return None
    return value


def harmonic_formants(x, sample_rate=SAMPLE_RATE, win_s=FORMANT_FIT_WIN_S,
                      hop_s=FORMANT_FIT_HOP_S, max_hz=FORMANT_FIT_MAX_HZ,
                      max_residual_db=FORMANT_FIT_MAX_RESIDUAL_DB):
    """Median F1/F2/F3 (Hz) from the P-03 harmonic-model fit.

    Every 100 ms frame is voiced-checked with YIN, its harmonic amplitudes are
    measured, and the 6-parameter model is fitted.  A frame is used only if the
    fit explains its harmonic amplitudes to ``max_residual_db`` RMS **and** the
    fitted formants sit inside the adult-vocal-tract bounds.  The measurement is
    the median over used frames.

    ``N/A`` policy (P-07c).  A formant is reported as NaN -- never as a pass and
    never as a confident wrong number -- with ``na_reason`` saying why, when:

    * no frame's harmonic amplitudes fit the model (e.g. real speech with
      anti-resonances and source ripple: residuals of 3-8 dB, versus 0.01-0.05
      dB for a synthetic vowel built from the model);
    * fewer than ``FORMANT_FIT_MIN_FRAMES`` frames survive those gates;
    * F1 lies below the first harmonic (F0), where the harmonic grid cannot
      shape it;
    * fewer than ``FORMANT_FIT_MIN_HARMONICS_BELOW_F3`` harmonics lie below F3,
      so F3 is extrapolation rather than measurement;
    * fewer than ``FORMANT_FIT_MIN_HARMONICS`` harmonics are available at all.

    The estimator is certified by ``metrics.py --formant-self-test`` against
    synthetic vowels with known formants, not against real speech.
    """
    x = np.asarray(x, dtype=np.float64)
    result = {
        "method": "harmonic-fit",
        "f1_hz": float("nan"),
        "f2_hz": float("nan"),
        "f3_hz": float("nan"),
        "f0_median_hz": float("nan"),
        "bandwidths_hz": [float("nan")] * 3,
        "frames_total": 0,
        "frames_voiced": 0,
        "frames_used": 0,
        "harmonics_median": 0,
        "residual_db_median": float("nan"),
        "resolved": False,
        "na_reason": None,
        "reject_reasons": {},
    }
    if len(x) == 0:
        result["na_reason"] = "empty input"
        return result

    wlen = int(round(win_s * sample_rate))
    wlen = min(wlen, len(x))
    hop = int(round(hop_s * sample_rate))
    if wlen < 16 or hop < 1:
        result["na_reason"] = "input is too short for a formant frame"
        return result
    starts = list(range(0, len(x) - wlen + 1, hop))
    if len(starts) > 2:
        starts = starts[1:-1]          # fades, endpoint effects
    result["frames_total"] = len(starts)

    floor = 10.0 ** (FORMANT_RMS_FLOOR_DBFS / 20.0)
    used = []
    previous = None
    rejections = {}
    for index, start in enumerate(starts):
        frame = x[start:start + wlen] - float(np.mean(x[start:start + wlen]))
        if math.sqrt(float(np.mean(frame ** 2))) <= floor:
            rejections["silent frame"] = rejections.get("silent frame", 0) + 1
            continue
        f0 = _frame_f0_hz(frame, sample_rate)
        if f0 is None:
            rejections["unvoiced frame"] = rejections.get("unvoiced frame", 0) + 1
            continue
        result["frames_voiced"] += 1
        frequencies, amplitudes = harmonic_amplitudes(frame, f0, sample_rate,
                                                      max_hz)
        if len(frequencies) < FORMANT_FIT_MIN_HARMONICS:
            rejections["too few harmonics"] = (
                rejections.get("too few harmonics", 0) + 1)
            continue
        log_amplitudes = np.log(np.maximum(amplitudes, 1e-12))
        if previous is None or index % 20 == 0:
            candidates = [(seed, FORMANT_FIT_INIT_BANDWIDTHS, True)
                          for seed in FORMANT_FIT_INITS]
        else:
            candidates = [(previous[0], previous[1], False)]
        best = None
        for seed, seed_bandwidths, coarse in candidates:
            formants, bandwidths, residual = _fit_harmonic_formants(
                frequencies, log_amplitudes, seed, seed_bandwidths,
                sample_rate, coarse=coarse)
            if best is None or residual < best[2]:
                best = (formants, bandwidths, residual)
        residual_db = math.sqrt(best[2] / len(frequencies)) * 8.685889638
        previous = best
        if not math.isfinite(residual_db) or residual_db > max_residual_db:
            key = "model residual above %.1f dB" % max_residual_db
            rejections[key] = rejections.get(key, 0) + 1
            continue
        used.append({
            "f0_hz": f0,
            "formants": list(best[0]),
            "bandwidths": list(best[1]),
            "residual_db": residual_db,
            "harmonics": len(frequencies),
        })

    result["reject_reasons"] = rejections
    if len(used) < FORMANT_FIT_MIN_FRAMES:
        if not used:
            top = max(rejections, key=rejections.get) if rejections else None
            detail = ("" if top is None
                      else " (%d of %d frames: %s)"
                      % (rejections[top], len(starts), top))
            result["na_reason"] = (
                "no frame's harmonic amplitudes fit the 3-resonator model "
                "within %.1f dB%s" % (max_residual_db, detail))
        else:
            result["na_reason"] = (
                "only %d frame(s) fit the 3-resonator model within %.1f dB "
                "(need %d)" % (len(used), max_residual_db,
                               FORMANT_FIT_MIN_FRAMES))
        return result

    medians = [float(np.median([frame["formants"][i] for frame in used]))
               for i in range(3)]
    f0_median = float(np.median([frame["f0_hz"] for frame in used]))
    result.update({
        "f0_median_hz": f0_median,
        "bandwidths_hz": [float(np.median([frame["bandwidths"][i]
                                           for frame in used]))
                          for i in range(3)],
        "frames_used": len(used),
        "harmonics_median": int(np.median([frame["harmonics"]
                                           for frame in used])),
        "residual_db_median": float(np.median([frame["residual_db"]
                                               for frame in used])),
    })

    f1, f2, f3 = medians
    harmonics_below_f3 = int(math.floor(f3 / f0_median)) if f0_median > 0 else 0
    if f1 < f0_median:
        result["na_reason"] = (
            "F1 %.0f Hz is below the first harmonic (F0 %.0f Hz): the harmonic "
            "grid cannot shape a formant there" % (f1, f0_median))
        return result
    if harmonics_below_f3 < FORMANT_FIT_MIN_HARMONICS_BELOW_F3:
        result["na_reason"] = (
            "only %d harmonics below F3 %.0f Hz (need %d): F3 would be "
            "extrapolated" % (harmonics_below_f3, f3,
                              FORMANT_FIT_MIN_HARMONICS_BELOW_F3))
        return result
    if f3 >= 0.99 * max_hz:
        result["na_reason"] = (
            "F3 %.0f Hz is at the %.0f Hz analysis ceiling"
            % (f3, max_hz))
        return result

    result.update({"f1_hz": f1, "f2_hz": f2, "f3_hz": f3, "resolved": True})
    return result


# ------------------------------------------------------- P-07e calibration

def _formant_residual_vector(frequencies, log_amplitudes, formants, bandwidths,
                             logs, sample_rate=SAMPLE_RATE, order=1):
    """Residual vector of the harmonic model after profiling out the tilt.

    ``order=1`` is the certified 3-resonator model (``C + alpha*log f``);
    higher orders exist for the model-extension experiments but are not used
    by the shipped ruler.  Keeping the vector (rather than only its sum of
    squares) is what lets the calibration path differentiate the residual with
    respect to a formant and obtain an interval.
    """
    shaped = (log_amplitudes
              - _resonator_log_amplitude(frequencies, formants, bandwidths,
                                         sample_rate))
    basis = np.column_stack([logs ** k for k in range(order + 1)])
    coefficients = np.linalg.lstsq(basis, shaped, rcond=None)[0]
    return shaped - basis @ coefficients


def _formant_covariance(frequencies, log_amplitudes, formants, bandwidths, rss,
                        sample_rate=SAMPLE_RATE, order=1):
    """95% half-widths of the fitted formants from the residual sensitivity.

    Linearised covariance of the nonlinear least-squares fit:
    ``Cov = sigma^2 (J^T J)^-1`` with ``sigma^2 = RSS / dof`` and ``J`` the
    numerical Jacobian of the tilt-profiled residual with respect to the six
    searched parameters (three formants, three bandwidths).  The tilt is
    regressed out, so it consumes ``order + 1`` degrees of freedom as well.

    This is a *lower bound* on the true uncertainty: when the residual is
    systematic model mismatch (real speech) rather than noise, the fit is
    biased and the interval does not cover that bias.  It does widen exactly as
    the residual grows, which is the honest signal Q-01 needs: a fit that
    cannot localise a formant given its own residual reports a wide interval
    and is then refused by the width gate.

    Returns ``(half_widths, dof, condition)`` or ``(None, dof, inf)`` when the
    frame has no residual degrees of freedom left.
    """
    n = len(frequencies)
    nr = len(formants)
    p = 2 * nr
    dof = n - p - (order + 1)
    if n == 0 or dof <= 0 or not math.isfinite(rss):
        return None, dof, float("inf")
    logs = np.log(frequencies)
    theta = np.asarray(list(formants) + list(bandwidths), dtype=np.float64)
    jacobian = np.zeros((n, p))
    for k in range(p):
        step = (1e-3 * abs(theta[k]) if k < nr else 1e-2 * abs(theta[k]))
        step = max(step, 1e-3)
        plus = theta.copy()
        minus = theta.copy()
        plus[k] += step
        minus[k] -= step
        r_plus = _formant_residual_vector(
            frequencies, log_amplitudes, list(plus[:nr]), list(plus[nr:]),
            logs, sample_rate, order)
        r_minus = _formant_residual_vector(
            frequencies, log_amplitudes, list(minus[:nr]), list(minus[nr:]),
            logs, sample_rate, order)
        jacobian[:, k] = (r_plus - r_minus) / (2.0 * step)
    normal = jacobian.T @ jacobian
    eigenvalues = np.linalg.eigvalsh(normal)
    largest = float(eigenvalues[-1]) if len(eigenvalues) else 0.0
    smallest = float(eigenvalues[0]) if len(eigenvalues) else 0.0
    condition = (largest / smallest if smallest > 1e-30 else float("inf"))
    sigma2 = max(float(rss), 0.0) / float(dof)
    try:
        covariance = sigma2 * np.linalg.pinv(normal)
    except np.linalg.LinAlgError:
        return None, dof, float("inf")
    variance = np.maximum(np.diag(covariance)[:nr], 0.0)
    return (FORMANT_CALIBRATION_Z * np.sqrt(variance)), dof, condition


def _round_floats(value, nd=6):
    """Round every float in a nested result to ``nd`` decimals.

    LAPACK's small-matrix solves can differ in the last 1e-13 between runs
    (BLAS threading), which would make a JSON dump non-byte-identical even
    though the measurement is unchanged.  The ruler is reported to far fewer
    digits than that, so rounding at the source is both deterministic and
    honest about precision.
    """
    if isinstance(value, dict):
        return {key: _round_floats(item, nd) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_round_floats(item, nd) for item in value]
    if isinstance(value, float) and math.isfinite(value):
        return round(value, nd)
    return value


def formant_calibration(x, sample_rate=SAMPLE_RATE,
                        max_hz=FORMANT_CALIBRATION_MAX_HZ,
                        win_s=FORMANT_FIT_WIN_S, hop_s=FORMANT_FIT_HOP_S,
                        max_residual_db=FORMANT_CALIBRATION_MAX_RESIDUAL_DB,
                        max_half_width_pct=FORMANT_CALIBRATION_MAX_HALF_WIDTH_PCT):
    """F1/F2/F3 as a measured range, for the Q-01 baseline path.

    Same 3-resonator model and fit as :func:`harmonic_formants`, with two
    deliberate differences:

    * it fits the **calibration band** (``FORMANT_CALIBRATION_MAX_HZ``, 3.5 kHz)
      where 32 kbps Opus is transparent, instead of the 6 kHz band of the
      synthetic control; and
    * every frame carries a 95% interval derived from the fit's own residual
      sensitivity (:func:`_formant_covariance`), so the answer is a range, not
      a point that hides how little the data constrain it.

    The result is ``status == "graded"`` only when all of the following hold:

    * the median residual over fitted frames is at most ``max_residual_db`` and
      at least ``FORMANT_CALIBRATION_MIN_FRAMES`` frames survive that gate with
      a usable covariance;
    * the fit has positive degrees of freedom and is not pathologically
      ill-conditioned (``FORMANT_CALIBRATION_MAX_CONDITION``);
    * each formant's 95% interval is at most ``max_half_width_pct`` of its
      value (8% by default, PLAN section 8's formant target);
    * the extrapolation gates of :func:`harmonic_formants` hold (F1 above the
      first harmonic, at least six harmonics below F3, F3 below the analysis
      ceiling).

    Otherwise every formant is ``uncalibrated`` with a reason and the caller
    **must not publish it** (D20).  The range is still reported -- it is what
    the residual supports -- but it is diagnostic, not a baseline.

    The range is a linearised lower bound: it assumes the model is true and the
    residual is noise.  On real speech the residual is largely systematic model
    mismatch, so the true error can exceed the interval; the interval is still
    the right thing to compare against the 8% gate, because a fit whose
    *optimistic* uncertainty already exceeds the tolerance cannot be published
    on any reading.
    """
    x = np.asarray(x, dtype=np.float64)
    result = {
        "method": "harmonic-fit-calibration-v1",
        "profile": {
            "maxHz": float(max_hz),
            "windowSeconds": float(win_s),
            "hopSeconds": float(hop_s),
            "residualGateDb": float(max_residual_db),
            "maxHalfWidthPercent": float(max_half_width_pct),
            "intervalZ": float(FORMANT_CALIBRATION_Z),
            "minFrames": int(FORMANT_CALIBRATION_MIN_FRAMES),
        },
        "f0_median_hz": float("nan"),
        "frames_total": 0,
        "frames_voiced": 0,
        "frames_fitted": 0,
        "frames_used": 0,
        "residual_db_median": float("nan"),
        "residual_db_p90": float("nan"),
        "formants": {
            key: {"hz": float("nan"), "range_hz": [float("nan")] * 2,
                  "half_width_hz": float("nan"),
                  "half_width_percent": float("nan"),
                  "status": "uncalibrated", "reason": None}
            for key in ("f1", "f2", "f3")
        },
        "bandwidths_hz": [float("nan")] * 3,
        "stability": {"framesCompared": 0, "f1MedianAbsStepHz": float("nan"),
                      "f2MedianAbsStepHz": float("nan"),
                      "f3MedianAbsStepHz": float("nan"),
                      "f1StepSdHz": float("nan"),
                      "f2StepSdHz": float("nan"),
                      "f3StepSdHz": float("nan")},
        "status": "uncalibrated",
        "reasons": [],
        "na_reason": None,
        "reject_reasons": {},
    }
    if len(x) == 0:
        result["na_reason"] = "empty input"
        result["reasons"] = ["empty input"]
        return _round_floats(result)

    wlen = min(int(round(win_s * sample_rate)), len(x))
    hop = int(round(hop_s * sample_rate))
    if wlen < 16 or hop < 1:
        result["na_reason"] = "input is too short for a formant frame"
        result["reasons"] = [result["na_reason"]]
        return _round_floats(result)
    starts = list(range(0, len(x) - wlen + 1, hop))
    if len(starts) > 2:
        starts = starts[1:-1]
    result["frames_total"] = len(starts)

    floor = 10.0 ** (FORMANT_RMS_FLOOR_DBFS / 20.0)
    records = []
    previous = None
    rejections = {}
    for index, start in enumerate(starts):
        frame = x[start:start + wlen] - float(np.mean(x[start:start + wlen]))
        if math.sqrt(float(np.mean(frame ** 2))) <= floor:
            rejections["silent frame"] = rejections.get("silent frame", 0) + 1
            continue
        f0 = _frame_f0_hz(frame, sample_rate)
        if f0 is None:
            rejections["unvoiced frame"] = rejections.get("unvoiced frame", 0) + 1
            continue
        result["frames_voiced"] += 1
        frequencies, amplitudes = harmonic_amplitudes(frame, f0, sample_rate,
                                                      max_hz)
        if len(frequencies) < FORMANT_FIT_MIN_HARMONICS:
            rejections["too few harmonics"] = (
                rejections.get("too few harmonics", 0) + 1)
            continue
        log_amplitudes = np.log(np.maximum(amplitudes, 1e-12))
        if previous is None or index % 20 == 0:
            seeds = [(seed, FORMANT_FIT_INIT_BANDWIDTHS, True)
                     for seed in FORMANT_FIT_INITS]
        else:
            seeds = [(previous[0], previous[1], False)]
        best = None
        for seed, seed_bandwidths, coarse in seeds:
            formants, bandwidths, rss = _fit_harmonic_formants(
                frequencies, log_amplitudes, seed, seed_bandwidths,
                sample_rate, coarse=coarse)
            if best is None or rss < best[2]:
                best = (formants, bandwidths, rss)
        formants, bandwidths, rss = best
        previous = (formants, bandwidths)
        residual_db = math.sqrt(rss / len(frequencies)) * 8.685889638
        half, dof, condition = _formant_covariance(
            frequencies, log_amplitudes, formants, bandwidths, rss,
            sample_rate)
        records.append({
            "f0_hz": f0,
            "formants": list(formants),
            "bandwidths": list(bandwidths),
            "residual_db": residual_db,
            "dof": int(dof),
            "condition": condition,
            "half": None if half is None else [float(v) for v in half],
            "harmonics": len(frequencies),
        })

    result["frames_fitted"] = len(records)
    result["reject_reasons"] = rejections
    if not records:
        result["na_reason"] = "no frame could be fitted"
        result["reasons"] = [result["na_reason"]]
        return _round_floats(result)

    residuals = [r["residual_db"] for r in records]
    result["residual_db_median"] = float(np.median(residuals))
    result["residual_db_p90"] = float(np.percentile(residuals, 90))

    survivors = [r for r in records
                 if r["residual_db"] <= max_residual_db
                 and r["half"] is not None and r["dof"] > 0]
    result["frames_used"] = len(survivors)
    summary = survivors if len(survivors) >= FORMANT_CALIBRATION_MIN_FRAMES \
        else records

    formants = np.array([r["formants"] for r in summary], dtype=np.float64)
    medians = [float(np.median(formants[:, i])) for i in range(3)]
    f0_median = float(np.median([r["f0_hz"] for r in summary]))
    result["f0_median_hz"] = f0_median
    result["bandwidths_hz"] = [
        float(np.median([r["bandwidths"][i] for r in summary]))
        for i in range(3)]

    reasons = []
    if result["residual_db_median"] > max_residual_db:
        reasons.append(
            "median harmonic residual %.2f dB exceeds the %.1f dB calibration "
            "gate" % (result["residual_db_median"], max_residual_db))
    if len(survivors) < FORMANT_CALIBRATION_MIN_FRAMES:
        reasons.append(
            "only %d frame(s) pass the residual gate with a usable covariance "
            "(need %d)" % (len(survivors), FORMANT_CALIBRATION_MIN_FRAMES))
    if medians[0] < f0_median:
        reasons.append(
            "F1 %.0f Hz is below the first harmonic (F0 %.0f Hz)"
            % (medians[0], f0_median))
    harmonics_below_f3 = int(math.floor(medians[2] / f0_median)) \
        if f0_median > 0 else 0
    if harmonics_below_f3 < FORMANT_FIT_MIN_HARMONICS_BELOW_F3:
        reasons.append(
            "only %d harmonics below F3 %.0f Hz (need %d)"
            % (harmonics_below_f3, medians[2],
               FORMANT_FIT_MIN_HARMONICS_BELOW_F3))
    if medians[2] >= 0.99 * max_hz:
        reasons.append(
            "F3 %.0f Hz is at the %.0f Hz analysis ceiling"
            % (medians[2], max_hz))

    for i, key in enumerate(("f1", "f2", "f3")):
        entry = result["formants"][key]
        entry["hz"] = medians[i]
        halves = [r["half"][i] for r in summary if r["half"] is not None]
        dofs = [r["dof"] for r in summary]
        conditions = [c for c in (r["condition"] for r in summary)
                      if math.isfinite(c)]
        entry["reason"] = None
        if not halves:
            entry["reason"] = "no frame has a finite parameter covariance"
        else:
            half = float(np.median(halves))
            entry["range_hz"] = [medians[i] - half, medians[i] + half]
            entry["half_width_hz"] = half
            entry["half_width_percent"] = (
                100.0 * half / medians[i] if medians[i] > 0 else float("inf"))
            min_harmonics = min(r["harmonics"] for r in summary)
            if min(dofs) <= 0:
                entry["reason"] = (
                    "the fit has no residual degrees of freedom at %.0f Hz F0 "
                    "(%d harmonics, 8 parameters)"
                    % (f0_median, min_harmonics))
            elif conditions and max(conditions) > \
                    FORMANT_CALIBRATION_MAX_CONDITION:
                entry["reason"] = (
                    "the fit is ill-conditioned (normal-matrix condition "
                    "%.1e): the residual does not identify this formant"
                    % max(conditions))
            elif entry["half_width_percent"] > max_half_width_pct:
                entry["reason"] = (
                    "95%% fit interval +/-%.1f Hz (+/-%.1f%%) is wider than "
                    "the %.1f%% publishable width"
                    % (half, entry["half_width_percent"], max_half_width_pct))
        if entry["reason"] is None and not reasons:
            entry["status"] = "graded"
        else:
            entry["status"] = "uncalibrated"

    # Frame-to-frame stability: adjacent fitted frames, every hop.  A
    # diagnostic, not a gate -- real speech genuinely changes formants between
    # vowels.  It is what separates a measurement from a model that fits only
    # by wandering (the extended models move F1 by 120-400 Hz SD on the same
    # frames; see docs/testing.md section 9.10).
    if len(summary) >= 2:
        steps = np.abs(np.diff(formants, axis=0))
        result["stability"] = {
            "framesCompared": int(len(summary) - 1),
            "f1MedianAbsStepHz": float(np.median(steps[:, 0])),
            "f2MedianAbsStepHz": float(np.median(steps[:, 1])),
            "f3MedianAbsStepHz": float(np.median(steps[:, 2])),
            "f1StepSdHz": float(np.std(steps[:, 0])),
            "f2StepSdHz": float(np.std(steps[:, 1])),
            "f3StepSdHz": float(np.std(steps[:, 2])),
        }

    result["reasons"] = reasons + [
        "%s: %s" % (key.upper(), result["formants"][key]["reason"])
        for key in ("f1", "f2", "f3")
        if result["formants"][key]["reason"]]
    result["status"] = ("graded"
                        if all(result["formants"][key]["status"] == "graded"
                               for key in ("f1", "f2", "f3"))
                        and not reasons else "uncalibrated")
    if result["status"] != "graded":
        result["na_reason"] = (result["reasons"][0]
                               if result["reasons"] else None)
    return _round_floats(result)


# ------------------------------------------------------- P-07b paired pitch

def resample_ratio_value(ratio):
    """The rational ratio ``resample_poly`` actually realises for ``ratio``."""
    from fractions import Fraction

    fraction = Fraction(float(ratio)).limit_denominator(1000)
    return fraction.numerator / float(fraction.denominator)


def resample_ratio(x, ratio):
    """Exact-by-construction pitch shift control using SciPy's resampler.

    The returned signal has pitch multiplied by ``ratio`` and duration divided
    by ``ratio``: this is a pure sample-rate conversion, with no vocoder and no
    estimator in the path.  It is *not* a render (a render keeps the input
    duration); it exists to give the pitch ruler a ground truth whose F0 ratio
    is known exactly.  Use :func:`paired_f0_ratio` with
    ``time_scale=ratio`` to compare it on its own time base.
    """
    from fractions import Fraction

    value = float(ratio)
    if not math.isfinite(value) or value <= 0:
        raise ValueError("ratio must be a positive finite number")
    fraction = Fraction(value).limit_denominator(1000)
    # resample_poly's pitch factor is down/up.
    return signal.resample_poly(np.asarray(x, dtype=np.float64),
                                fraction.denominator, fraction.numerator)


def synthesize_harmonic(f0_hz, samples, sample_rate=SAMPLE_RATE,
                        harmonics=10, fade_s=0.005):
    """A stationary, band-limited harmonic complex at ``f0_hz``.

    Used by ``metrics.py --self-test`` for the same-time control: resampling a
    constant-F0 signal is an exact pitch shift that keeps the time grid, so it
    exercises the ordinary paired measurement (unlike the resampler control,
    which has to be compared on its own time base).
    """
    n = int(samples)
    out = np.zeros(n, dtype=np.float64)
    if n == 0 or f0_hz <= 0:
        return out
    t = np.arange(n, dtype=np.float64) / float(sample_rate)
    for h in range(1, int(harmonics) + 1):
        if h * f0_hz >= 0.45 * sample_rate:
            break
        out += (1.0 / h) * np.sin(2.0 * np.pi * h * f0_hz * t)
    peak = float(np.max(np.abs(out))) if n else 0.0
    if peak > 0:
        out *= 0.5 / peak
    fade = min(int(round(fade_s * sample_rate)), n // 4)
    if fade > 0:
        ramp = np.linspace(0.0, 1.0, fade, endpoint=False)
        out[:fade] *= ramp
        out[-fade:] *= ramp[::-1]
    return out


def _indexed_track(track, total_frames):
    """Scatter a compacted ``f0_track`` result back onto its time grid.

    ``track`` must have been built with ``return_agreement=True`` and
    ``return_indices=True``.  The returned arrays are indexed by frame ordinal
    (``start = index*hop``), with NaN / False where the frame was not voiced,
    which is what makes same-instant pairing possible at all (P-07d).
    """
    total = int(total_frames)
    f0 = np.full(total, float("nan"))
    agree = np.zeros(total, dtype=bool)
    indices = np.asarray(track[4], dtype=np.int64)
    if len(indices):
        f0[indices] = track[0]
        agree[indices] = track[3]
    return f0, agree


def _reference_unreliable_mask(values,
                               half_window=PITCH_REF_NEIGHBOUR_FRAMES,
                               max_cents=PITCH_REF_NEIGHBOUR_CENTS):
    """Frame-indexed mask of reference F0 readings that are tracker failures.

    The reference is a fixed recording, so its true F0 contour is continuous at
    a 20 ms hop.  A frame that sits more than ``max_cents`` (half an octave in
    either direction, 600 c) from the median of its voiced neighbours *within
    ``half_window``* is a YIN failure -- e.g. the 510.65 Hz reading on an 88 Hz
    male frame that produced a -2788 cent outlier, or the 347.7 Hz readings on a
    228 Hz synthetic vowel.  Isolated spikes are removed; a genuine glide is
    kept because its neighbours move with it.

    **Reference side only.**  Applying the same rule to the render would delete
    exactly the octave artifacts D17's zero-outlier clause exists to catch, so
    the render keeps every frame the absolute band and the spectral check keep.
    """
    values = np.asarray(values, dtype=np.float64)
    mask = np.zeros(len(values), dtype=bool)
    finite = np.isfinite(values)
    if np.count_nonzero(finite) < 3:
        return mask
    half = max(0, int(half_window))
    for i in np.flatnonzero(finite):
        lo = max(0, i - half)
        hi = min(len(values), i + half + 1)
        window = values[lo:hi]
        positions = np.arange(lo, hi)
        neighbours = window[np.isfinite(window) & (positions != i)]
        if len(neighbours) < 2:
            continue
        centre = float(np.median(neighbours))
        if centre <= 0:
            continue
        cents = 1200.0 * math.log2(float(values[i]) / centre)
        if abs(cents) > max_cents:
            mask[i] = True
    return mask


def _comb_score(peaks_hz, peaks_mag, f0_hz,
                max_hz=PITCH_HARMONIC_MAX_HZ):
    """How well a harmonic comb at ``f0_hz`` explains a frame's spectral peaks.

    ``captured * hit_rate``: the fraction of prominent-peak energy that lies on
    the comb, times the fraction of teeth that land on a peak.  Either term
    alone is degenerate -- captured energy always prefers a sub-harmonic (it has
    more teeth to catch things with), while hit rate alone always prefers a
    low harmonic (every tooth lands on a real harmonic).  Together they peak at
    the true fundamental.  Measured: 1.00 on the stationary synthetic vowels
    (which is why they drop nothing), and low and noisy on real 32 kbps speech,
    where formant peaks dilute the captured fraction -- only the ranking of
    candidates *within* one frame is used, never an absolute threshold on the
    winner's identity.
    """
    if not (math.isfinite(f0_hz) and f0_hz > 0) or len(peaks_hz) == 0:
        return 0.0
    peaks_hz = np.asarray(peaks_hz, dtype=np.float64)
    peaks_mag = np.asarray(peaks_mag, dtype=np.float64)
    total = float(peaks_mag.sum())
    teeth = int(math.floor(max_hz / f0_hz))
    if teeth < 2 or total <= 0.0 or len(peaks_hz) < 2:
        return 0.0
    targets = np.arange(1, teeth + 1, dtype=np.float64) * f0_hz
    tolerance = np.maximum(PITCH_HARMONIC_TOL_HZ,
                           PITCH_HARMONIC_TOL_REL * targets)
    right = np.clip(np.searchsorted(peaks_hz, targets), 1, len(peaks_hz) - 1)
    left = right - 1
    use_left = np.abs(peaks_hz[left] - targets) <= np.abs(peaks_hz[right]
                                                          - targets)
    chosen = np.where(use_left, left, right)
    hit = np.abs(peaks_hz[chosen] - targets) <= tolerance
    if not hit.any():
        return 0.0
    captured = float(peaks_mag[np.unique(chosen[hit])].sum())
    return (captured / total) * float(hit.mean())


def _f0_candidate_hz(peaks_hz, f0_hz, min_hz, max_hz):
    """Candidate fundamentals to test against a YIN reading (Hz, sorted).

    Two families: the octave/sub-octave family of the reading itself (which is
    what "compare the candidate lag against lag/2 and lag/3" means in the
    frequency domain), and the sub-multiples of the frame's lowest prominent
    peak.  The second family is what finds 228 Hz when YIN has locked onto a
    spurious 347.77 Hz dip: 347.77 is not an integer multiple of anything in
    the signal, but 228 is the lowest peak and the reading is not consistent
    with it.
    """
    candidates = {f0_hz * factor for factor in (1.0 / 3.0, 0.5, 1.0, 2.0, 3.0)}
    band = peaks_hz[(peaks_hz >= min_hz) & (peaks_hz <= 1200.0)]
    if len(band):
        lowest = float(band[0])
        candidates.update(lowest / h for h in (1.0, 2.0, 3.0, 4.0))
        candidates.update(lowest * m for m in (2.0, 3.0))
    return sorted(c for c in candidates if min_hz <= c <= max_hz)


def _frame_f0_supported(frame, f0_hz, sample_rate=SAMPLE_RATE,
                        min_hz=PITCH_F0_MIN, max_hz=PITCH_F0_MAX):
    """Whether the frame's spectrum supports ``f0_hz`` as its fundamental.

    Returns ``(supported, best_hz, score_hz, best_score)``.  A reading is
    rejected when a materially different candidate (more than
    ``PITCH_HARMONIC_SEPARATION_CENTS`` away) explains the peaks by at least
    ``PITCH_HARMONIC_MARGIN`` and the winning score is not noise
    (``PITCH_HARMONIC_MIN_SCORE``).  This is sub-harmonic/octave rejection, not
    a tolerance: a render that genuinely moves an octave keeps its energy at
    the new octave, so the new value is the best-supported candidate and the
    error is still measured (and counted as an outlier).
    """
    nfft = 1 << int(math.ceil(math.log2(max(4 * len(frame), 4096))))
    peaks_hz, peaks_mag = _spectral_peaks(frame, sample_rate, nfft)
    if len(peaks_hz) == 0:
        return False, f0_hz, 0.0, 0.0
    order = np.argsort(peaks_hz)
    peaks_hz = peaks_hz[order]
    peaks_mag = peaks_mag[order]
    band = peaks_hz <= PITCH_HARMONIC_MAX_HZ
    score = _comb_score(peaks_hz[band], peaks_mag[band], f0_hz)
    best_hz, best_score = f0_hz, score
    for candidate in _f0_candidate_hz(peaks_hz, f0_hz, min_hz, max_hz):
        candidate_score = _comb_score(peaks_hz[band], peaks_mag[band],
                                      candidate)
        if candidate_score > best_score * (1.0 + 1e-12):
            best_hz, best_score = candidate, candidate_score
    separation = (1200.0 * math.log2(best_hz / f0_hz)
                  if best_hz > 0 and f0_hz > 0 else 0.0)
    supported = not (
        abs(separation) > PITCH_HARMONIC_SEPARATION_CENTS
        and best_score >= PITCH_HARMONIC_MIN_SCORE
        and best_score > PITCH_HARMONIC_MARGIN * score)
    return supported, best_hz, score, best_score


def synthesize_glide(f0_start_hz, f0_end_hz, samples, sample_rate=SAMPLE_RATE,
                     harmonics=10, fade_s=0.005):
    """A phase-continuous harmonic complex whose F0 glides exponentially.

    The instantaneous frequency sweeps from ``f0_start_hz`` to ``f0_end_hz``
    along a straight line in cents (``f(t) = f0_start * r**(t/T)``), so the
    signal is a realistic intonation contour rather than a stepped or
    stationary tone.  ``--self-test``'s pairing-mismatch control needs a
    contour that moves: with a perfectly constant F0, pairing the n-th voiced
    frame by rank and pairing it by index give the same answer, which is
    exactly why the older control could not see rank pairing (P-07d).
    """
    n = int(samples)
    out = np.zeros(n, dtype=np.float64)
    if n == 0 or f0_start_hz <= 0 or f0_end_hz <= 0:
        return out
    duration = n / float(sample_rate)
    ratio = float(f0_end_hz) / float(f0_start_hz)
    t = np.arange(n, dtype=np.float64) / float(sample_rate)
    if abs(ratio - 1.0) < 1e-12:
        base_phase = 2.0 * np.pi * float(f0_start_hz) * t
    else:
        # Integral of f0_start * r**(t/T) dt, normalised to zero at t = 0.
        scale = float(f0_start_hz) * duration / math.log(ratio)
        base_phase = 2.0 * np.pi * scale * (ratio ** (t / duration) - 1.0)
    nyquist = 0.5 * float(sample_rate)
    for h in range(1, int(harmonics) + 1):
        if h * max(f0_start_hz, f0_end_hz) >= 0.45 * sample_rate:
            break
        out += (1.0 / h) * np.sin(h * base_phase)
    peak = float(np.max(np.abs(out))) if n else 0.0
    if peak > 0:
        out *= 0.5 / peak
    fade = min(int(round(fade_s * sample_rate)), n // 4)
    if fade > 0:
        ramp = np.linspace(0.0, 1.0, fade, endpoint=False)
        out[:fade] *= ramp
        out[-fade:] *= ramp[::-1]
    return out


def paired_f0_ratio(ref, test, sample_rate=SAMPLE_RATE, expected_ratio=None,
                    min_hz=PITCH_F0_MIN, max_hz=PITCH_F0_MAX,
                    win_s=PITCH_WIN_S, hop_s=PITCH_HOP_S, time_scale=1.0,
                    spectral_check=True):
    """Paired per-frame F0 ratio between a reference and a render/control.

    Both signals are framed on the same grid (default 40 ms windows, 50%
    overlap), F0 is estimated independently per frame with YIN, and frames are
    paired **by frame index** -- the same instant in both signals -- after
    intersecting ``ref_voiced & test_voiced`` on those indices.  P-07b paired
    the compacted voiced-only arrays by rank, so one marginal-frame
    disagreement shifted every later pair: ``am_onyx`` at +7 st read +105.94 c
    that way and +2.60 c when paired by time.  Rank pairing is gone.

    Four gates are then applied **per index, before the statistics** (their
    counts are returned under ``dropped_*``):

    ``dropped_ref_band`` / ``dropped_test_band``
        the measured F0 is outside the reliable 80-550 Hz band, where a 40 ms
        YIN estimate is not certifiable to a cent;
    ``dropped_expected_band``
        the frame's *expected* output F0 (reference F0 x requested ratio) is
        outside that band, so no in-band reading can certify it -- this is the
        per-frame form of the cell-level N/A rule and uses the reference only;
    ``dropped_ref_disagree`` / ``dropped_test_disagree``
        with ``spectral_check`` (the default) no prominent spectral peak
        confirms the YIN estimate; this also removes digital silence, which has
        no peaks.  Silence is already unvoiced here: the ruler passes
        ``rms_floor_dbfs=FRAME_RMS_FLOOR_DBFS`` so a degenerate all-zero frame
        cannot read as a maximally periodic one.
    ``dropped_ref_outlier``
        the reference reading is an isolated octave error (see
        :func:`_reference_unreliable_mask`).  Reference only, so a render-side
        octave artifact still reaches ``octave_outliers`` and fails the cell;
    ``dropped_ref_support`` / ``dropped_test_support``
        sub-harmonic/octave rejection (see :func:`_frame_f0_supported`): a
        materially different fundamental explains the frame's harmonic peaks
        better than the reading does.  Applied to both sides, because a render
        that genuinely moves an octave keeps its energy there and the moved
        value wins -- the error is then measured and counted, not hidden.

    The result is the distribution of ``1200*log2(test/ref)`` in cents: the
    median is the measurement, the IQR/p10/p90 describe its spread, and the
    paired frame count says how much evidence there is.

    ``time_scale`` is for exact-resampler controls only.  A resampler changes
    duration by ``1/ratio``, so its frame at output time ``t`` holds the
    reference content at ``ratio*t``.  Passing ``time_scale=ratio`` analyses
    the reference with every tracker parameter scaled by the same factor
    (window, hop and lag band).  The index mapping is then explicit rather than
    assumed: reference frame ``i`` starts at ``i*hop*scale`` seconds of content
    and test frame ``j`` starts at ``j*hop`` seconds of output, which is
    ``scale*j*hop`` seconds of content, so ``i = round(j*scale*hop_test/hop_ref)``.
    With ``hop_ref = round(hop_s*scale*fs)`` the factor is within a rounding
    error of 1, and every frame after the first is mapped, not paired by rank.
    Render comparisons always use the default ``time_scale=1``: a render is
    time-aligned by contract.

    Returns a dict; ``error_cents`` is ``median_cents - expected_cents`` and is
    NaN when no ``expected_ratio`` was supplied.  Nothing here decides
    pass/fail; the caller applies the D17 gate (median, p90, octave outliers).
    """
    ref = np.asarray(ref, dtype=np.float64)
    test = np.asarray(test, dtype=np.float64)
    scale = float(time_scale)
    if not math.isfinite(scale) or scale <= 0:
        scale = 1.0

    same_time = abs(scale - 1.0) < 1e-12
    if same_time:
        ref_track = f0_track(ref, sample_rate, min_hz, max_hz, win_s, hop_s,
                             spectral_check=spectral_check,
                             rms_floor_dbfs=FRAME_RMS_FLOOR_DBFS,
                             return_agreement=True, return_indices=True)
        test_track = f0_track(test, sample_rate, min_hz, max_hz, win_s, hop_s,
                              spectral_check=spectral_check,
                              rms_floor_dbfs=FRAME_RMS_FLOOR_DBFS,
                              return_agreement=True, return_indices=True)
        ref_band = (min_hz, max_hz)
        method = "paired-same-time"
        hop_ref = int(round(hop_s * sample_rate))
        hop_test = hop_ref
    else:
        ref_track = f0_track(ref, sample_rate, min_hz / scale, max_hz / scale,
                             win_s * scale, hop_s * scale,
                             spectral_check=spectral_check,
                             rms_floor_dbfs=FRAME_RMS_FLOOR_DBFS,
                             return_agreement=True, return_indices=True)
        test_track = f0_track(test, sample_rate, min_hz, max_hz, win_s, hop_s,
                              spectral_check=spectral_check,
                              rms_floor_dbfs=FRAME_RMS_FLOOR_DBFS,
                              return_agreement=True, return_indices=True)
        ref_band = (min_hz / scale, max_hz / scale)
        method = "paired-content-time"
        hop_ref = max(1, int(round(hop_s * scale * sample_rate)))
        hop_test = max(1, int(round(hop_s * sample_rate)))

    ref_frames = len(ref_track[2])       # frame_rms covers every frame
    test_frames = len(test_track[2])
    ref_f0, ref_agree = _indexed_track(ref_track, ref_frames)
    test_f0, test_agree = _indexed_track(test_track, test_frames)
    wlen_ref = max(1, int(round(win_s * scale * sample_rate)))
    wlen_test = max(1, int(round(win_s * sample_rate)))

    # Test frame -> reference frame on the (possibly scaled) content grid.
    test_index = np.arange(test_frames, dtype=np.int64)
    if same_time:
        ref_index = test_index.copy()
    else:
        factor = scale * hop_test / float(hop_ref)
        ref_index = np.rint(test_index * factor).astype(np.int64)
        # A scale far from 1 could map two output frames onto one reference
        # frame; keep the first so no instant is counted twice.
        _, first = np.unique(ref_index, return_index=True)
        keep_unique = np.sort(first)
        test_index = test_index[keep_unique]
        ref_index = ref_index[keep_unique]

    shared = min(ref_frames, test_frames)
    on_grid = (ref_index >= 0) & (ref_index < shared) & (test_index < shared)
    ref_index = ref_index[on_grid]
    test_index = test_index[on_grid]
    ref_values = ref_f0[ref_index]
    test_values = test_f0[test_index]
    ref_agreed = ref_agree[ref_index]
    test_agreed = test_agree[test_index]

    ref_band_ok = ((ref_values >= PITCH_RELIABLE_MIN_HZ)
                   & (ref_values <= PITCH_RELIABLE_MAX_HZ))
    test_band_ok = ((test_values >= PITCH_RELIABLE_MIN_HZ)
                    & (test_values <= PITCH_RELIABLE_MAX_HZ))
    ref_outlier = _reference_unreliable_mask(ref_f0)[ref_index]
    # A frame whose *expected* output F0 is outside the reliable band cannot be
    # certified even if the tracker returns an in-band number: at -12 st a
    # frame that dips to 82 Hz should render at 41 Hz, and the only in-band
    # reading available is an octave artifact.  This is the per-frame form of
    # the cell-level N/A rule, and it uses the reference measurement and the
    # requested ratio only, never the render, so it cannot hide a DSP error.
    if expected_ratio is not None:
        expected_frame_hz = ref_values * float(expected_ratio)
        expected_band_ok = ((expected_frame_hz >= PITCH_RELIABLE_MIN_HZ)
                            & (expected_frame_hz <= PITCH_RELIABLE_MAX_HZ))
    else:
        expected_band_ok = np.ones(len(ref_values), dtype=bool)

    both = np.isfinite(ref_values) & np.isfinite(test_values)
    ref_only = int(np.count_nonzero(
        np.isfinite(ref_values) & ~np.isfinite(test_values)))
    test_only = int(np.count_nonzero(
        np.isfinite(test_values) & ~np.isfinite(ref_values)))

    # Each frame is charged to the first gate that rejects it, so the counts
    # sum to the number of frames removed instead of double-counting.
    keep_mask = both.copy()
    dropped = {
        "reference F0 outside reliable band": 0,
        "test F0 outside reliable band": 0,
        "expected output outside reliable band": 0,
        "reference spectral check disagreement": 0,
        "test spectral check disagreement": 0,
        "reference octave-inconsistent frame": 0,
        "reference harmonic support": 0,
        "test harmonic support": 0,
    }

    def _drop(reason, reject):
        nonlocal keep_mask
        hit = keep_mask & reject
        if hit.any():
            dropped[reason] = int(np.count_nonzero(hit))
            keep_mask = keep_mask & ~reject

    _drop("reference F0 outside reliable band", ~ref_band_ok)
    _drop("test F0 outside reliable band", ~test_band_ok)
    _drop("expected output outside reliable band", ~expected_band_ok)
    _drop("reference spectral check disagreement", ~ref_agreed)
    _drop("test spectral check disagreement", ~test_agreed)
    _drop("reference octave-inconsistent frame", ref_outlier)

    if spectral_check:
        # Harmonic-support (sub-harmonic/octave) rejection, on both sides.
        # This is spectral evidence, so it belongs with the cross-check.
        ref_unsupported = np.zeros(len(ref_index), dtype=bool)
        test_unsupported = np.zeros(len(test_index), dtype=bool)

        def _unsupported(signal, index, value, wlen, hop_samples):
            start = int(index) * hop_samples
            frame = signal[start:start + wlen]
            if len(frame) < wlen or not math.isfinite(value):
                return False
            supported, _, _, _ = _frame_f0_supported(
                frame, float(value), sample_rate, min_hz, max_hz)
            return not supported

        for position in np.flatnonzero(keep_mask):
            ref_unsupported[position] = _unsupported(
                ref, ref_index[position], ref_values[position], wlen_ref,
                hop_ref)
            test_unsupported[position] = _unsupported(
                test, test_index[position], test_values[position], wlen_test,
                hop_test)
        _drop("reference harmonic support", ref_unsupported)
        _drop("test harmonic support", test_unsupported)

    ref_paired = ref_values[keep_mask]
    test_paired = test_values[keep_mask]

    expected_cents = (None if expected_ratio is None
                      else 1200.0 * math.log2(float(expected_ratio)))
    if len(ref_paired) == 0:
        stats = {
            "median_cents": float("nan"),
            "error_cents": float("nan"),
            "p10_cents": float("nan"),
            "p25_cents": float("nan"),
            "p75_cents": float("nan"),
            "p90_cents": float("nan"),
            "iqr_cents": float("nan"),
            "p90_abs_error_cents": float("nan"),
            "octave_outliers": 0,
            "ref_median_hz": float("nan"),
            "test_median_hz": float("nan"),
        }
    else:
        measured = 1200.0 * np.log2(test_paired / ref_paired)
        error = (measured - expected_cents if expected_cents is not None
                 else np.full_like(measured, np.nan))
        p10, p25, p75, p90 = (float(v) for v in np.percentile(
            measured, [10, 25, 75, 90]))
        stats = {
            "median_cents": float(np.median(measured)),
            "error_cents": (float(np.median(error))
                            if expected_cents is not None else float("nan")),
            "p10_cents": p10,
            "p25_cents": p25,
            "p75_cents": p75,
            "p90_cents": p90,
            "iqr_cents": p75 - p25,
            "p90_abs_error_cents": (float(np.percentile(np.abs(error), 90))
                                    if expected_cents is not None
                                    else float("nan")),
            "octave_outliers": (int(np.count_nonzero(
                np.abs(error) > PITCH_OCTAVE_OUTLIER_CENTS))
                if expected_cents is not None else 0),
            "ref_median_hz": float(np.median(ref_paired)),
            "test_median_hz": float(np.median(test_paired)),
        }

    return {
        "method": method,
        "time_scale": scale,
        "band_hz": [float(min_hz), float(max_hz)],
        "ref_band_hz": [float(ref_band[0]), float(ref_band[1])],
        "expected_ratio": (float(expected_ratio)
                           if expected_ratio is not None else None),
        "expected_cents": expected_cents,
        "paired_frames": int(len(ref_paired)),
        "ref_voiced_frames": int(len(ref_track[0])),
        "test_voiced_frames": int(len(test_track[0])),
        "ref_agreed_frames": int(np.count_nonzero(ref_track[3])),
        "test_agreed_frames": int(np.count_nonzero(test_track[3])),
        "shared_frames": int(len(ref_index)),
        "both_voiced_frames": int(np.count_nonzero(both)),
        "ref_only_voiced_frames": ref_only,
        "test_only_voiced_frames": test_only,
        "dropped_ref_band": dropped["reference F0 outside reliable band"],
        "dropped_test_band": dropped["test F0 outside reliable band"],
        "dropped_expected_band": dropped["expected output outside reliable band"],
        "dropped_ref_disagree": dropped["reference spectral check disagreement"],
        "dropped_test_disagree": dropped["test spectral check disagreement"],
        "dropped_ref_outlier": dropped["reference octave-inconsistent frame"],
        "dropped_ref_support": dropped["reference harmonic support"],
        "dropped_test_support": dropped["test harmonic support"],
        "dropped_frames": int(sum(dropped.values())),
        "dropped_reasons": {k: v for k, v in dropped.items() if v},
        "frame_hop_seconds": float(hop_s * scale),
        "window_seconds": float(win_s * scale),
        **stats,
    }


# ------------------------------------------------------------------ summary

def measure(x, sample_rate=SAMPLE_RATE):
    """All scalar metrics for one clip, in one call.

    Keeps callers from forgetting a metric or mixing sample rates.  Raw frame
    arrays are not returned; use ``f0_track``/``f0_formants`` when you need
    them.

    Formants here are the **legacy LPC** values (P-07c).  The certified ruler is
    :func:`harmonic_formants`; it is not called here because it is an order of
    magnitude more expensive and returns ``N/A`` on material its model does not
    fit, which callers must be able to see rather than have averaged away.
    """
    level = level_stats(x)
    fs = f0_stats(x, sample_rate)
    formants = lpc_formants(x, sample_rate)
    return {
        "samples": level["samples"],
        "duration_s": level["duration_s"],
        "peak_dbfs": level["peak_dbfs"],
        "rms_dbfs": level["rms_dbfs"],
        "dc": level["dc"],
        "clipped_samples": level["clipped_samples"],
        "finite": level["finite"],
        "f0_median_hz": fs["f0_median_hz"],
        "f0_p10_hz": fs["f0_p10_hz"],
        "f0_p90_hz": fs["f0_p90_hz"],
        "hnr_median_db": fs["hnr_median_db"],
        "voiced_fraction": fs["voiced_fraction"],
        "centroid_hz": spectral_centroid(x, sample_rate),
        "lufs_i": lufs_integrated(x, sample_rate),
        "f1_hz": formants["f1_hz"],
        "f2_hz": formants["f2_hz"],
        "f3_hz": formants["f3_hz"],
    }


__all__ = [
    "SAMPLE_RATE", "SR", "SILENCE_DBFS", "SILENCE_MIN_S", "CLIP_LEVEL",
    "DC_LIMIT", "F0_MIN", "F0_MAX", "F0_WIN_S", "F0_HOP_S", "YIN_THRESHOLD",
    "YIN_VOICED", "FRAME_RMS_FLOOR_DBFS", "K_B1", "K_A1", "K_B2", "K_A2",
    "PITCH_F0_MIN", "PITCH_F0_MAX", "PITCH_RELIABLE_MIN_HZ",
    "PITCH_RELIABLE_MAX_HZ", "PITCH_WIN_S", "PITCH_HOP_S",
    "PITCH_MIN_PAIRED_FRAMES", "PITCH_TOLERANCE_CENTS",
    "PITCH_SELF_TEST_TOLERANCE_CENTS", "PITCH_P90_TOLERANCE_CENTS",
    "PITCH_OCTAVE_OUTLIER_CENTS", "PITCH_REF_NEIGHBOUR_FRAMES",
    "PITCH_REF_NEIGHBOUR_CENTS", "PITCH_PEAK_PROMINENCE",
    "PITCH_HARMONIC_MAX_HZ", "PITCH_HARMONIC_TOL_HZ",
    "PITCH_HARMONIC_TOL_REL", "PITCH_HARMONIC_SEPARATION_CENTS",
    "PITCH_HARMONIC_MARGIN", "PITCH_HARMONIC_MIN_SCORE",
    "FORMANT_ORDER", "AudioFormatError", "db", "runs_of", "cents_between",
    "percent_delta", "load_f32le", "load_wav", "load_audio", "level_stats",
    "null_test", "f0_track", "f0_stats", "hnr_median_db", "spectral_centroid",
    "lufs_integrated", "lpc_formants", "resample_ratio",
    "resample_ratio_value", "synthesize_harmonic", "synthesize_glide",
    "paired_f0_ratio", "measure",
    "FORMANT_FIT_MAX_HZ", "FORMANT_FIT_WIN_S", "FORMANT_FIT_HOP_S",
    "FORMANT_FIT_MIN_HARMONICS", "FORMANT_FIT_MIN_HARMONICS_BELOW_F3",
    "FORMANT_FIT_MAX_RESIDUAL_DB", "FORMANT_FIT_MIN_FRAMES",
    "FORMANT_FIT_TOLERANCE_PCT", "FORMANT_FIT_BOUNDS", "FORMANT_FIT_INITS",
    "FORMANT_FIT_INIT_BANDWIDTHS", "synthesize_vowel", "harmonic_amplitudes",
    "harmonic_formants",
    "FORMANT_CALIBRATION_MAX_HZ", "FORMANT_CALIBRATION_MAX_RESIDUAL_DB",
    "FORMANT_CALIBRATION_MAX_HALF_WIDTH_PCT", "FORMANT_CALIBRATION_Z",
    "FORMANT_CALIBRATION_MIN_FRAMES", "FORMANT_CALIBRATION_MAX_CONDITION",
    "formant_calibration",
]
