#!/usr/bin/env python3
"""P-07: objective metrics harness for RASP renders.

One command compares a reference clip against a test clip, or runs the whole
fixture set against itself.  The measurement code lives in
``scripts/audio_metrics.py`` and nowhere else; this file is argument parsing,
pass/fail logic and presentation.

Quick start (from the repo root):

    # identity: must be 0 cents, 0% formant delta, <= -100 dB null
    scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \\
                       --test fixtures/f32/af_heart__friends-now.f32 --check

    # actual decoder difference: reference (libopus) vs f32 (ffmpeg native)
    scripts/metrics.py --ref fixtures/reference/af_heart__friends-now.f32 \\
                       --test fixtures/f32/af_heart__friends-now.f32

    # judge a shifted render once rasp-cli exists
    scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \\
                       --test out.wav --pitch-st 4 --formant-ratio 1.1 --check

    # all 12 fixtures: f32 vs reference (same count, nonzero SNR, no NaN)
    scripts/metrics.py --manifest

    # machine-readable for CI
    scripts/metrics.py --manifest --json --check

Input is 48 kHz mono.  ``.f32``/``.raw`` is headerless little-endian float32
and 48 kHz is *assumed*; WAV is checked and rejected if it is not 48 kHz mono.
Run this file directly or through ``scripts/.venv/bin/python`` -- it re-execs
into the pinned venv when needed.

Targets (PLAN §8, D17):
    pitch error            median |error| <= 5 c, p90 |error| <= 25 c,
                           0 octave outliers (whole clip, frames voiced in both)
    formant ratio error    ±8% (F1/F2)
    null residual          <= -100 dB for identity/bypass
    sample count           equal (decoder and render comparisons)

``--check`` exits 1 when an applicable check fails.  Without ``--check`` the
table still shows PASS/FAIL but the exit status is 0.  ``--json`` writes one
JSON object to stdout and nothing else, so it can be piped into ``jq``.
"""

import os
import sys

# Re-exec into the pinned venv when the file is run as ./scripts/metrics.py
# and numpy/scipy are not on the interpreter's path.  sys.prefix is a reliable
# venv marker; the venv interpreter does not re-exec itself.
_HERE = os.path.dirname(os.path.abspath(__file__))
_VENV = os.path.join(_HERE, ".venv")
_VENV_PY = os.path.join(_VENV, "bin", "python")
if (os.path.isfile(_VENV_PY)
        and os.path.abspath(sys.prefix) != os.path.abspath(_VENV)):
    os.execv(_VENV_PY, [_VENV_PY, os.path.abspath(__file__)] + sys.argv[1:])

import argparse
import csv
import glob
import hashlib
import json
import math
import re

try:
    import numpy as np
    from audio_metrics import (SAMPLE_RATE, AudioFormatError, cents_between,
                               formant_calibration, harmonic_formants,
                               level_stats, load_audio, lpc_formants,
                               lufs_integrated, null_test, percent_delta,
                               spectral_centroid, f0_stats, f0_track,
                               paired_f0_ratio, resample_ratio,
                               resample_ratio_value, synthesize_glide,
                               synthesize_harmonic, synthesize_vowel,
                               PITCH_WIN_S, PITCH_HOP_S,
                               PITCH_F0_MIN, PITCH_F0_MAX,
                               FRAME_RMS_FLOOR_DBFS,
                               PITCH_MIN_PAIRED_FRAMES,
                               PITCH_RELIABLE_MIN_HZ, PITCH_RELIABLE_MAX_HZ,
                               PITCH_SELF_TEST_TOLERANCE_CENTS,
                               PITCH_P90_TOLERANCE_CENTS,
                               PITCH_OCTAVE_OUTLIER_CENTS,
                               FORMANT_FIT_MAX_RESIDUAL_DB,
                               FORMANT_FIT_TOLERANCE_PCT,
                               FORMANT_FIT_WIN_S, FORMANT_FIT_HOP_S,
                               FORMANT_CALIBRATION_MAX_HZ,
                               FORMANT_CALIBRATION_MAX_RESIDUAL_DB,
                               FORMANT_CALIBRATION_MAX_HALF_WIDTH_PCT,
                               FORMANT_CALIBRATION_MIN_FRAMES)
except ImportError as exc:
    sys.stderr.write(
        "metrics.py: missing dependency: %s\n"
        "Bootstrap the pinned venv from the repo root:\n"
        "  python3 -m venv scripts/.venv\n"
        "  scripts/.venv/bin/pip install -r scripts/requirements.txt\n"
        % exc)
    raise SystemExit(2)

PITCH_TOLERANCE_CENTS = 5.0
FORMANT_TOLERANCE_PCT = 8.0
NULL_TARGET_DB = -100.0

# D17's whole-clip pitch gate (P-07d).  All three clauses must hold; a cell
# that satisfies the median but not the p90 or the octave count is a FAIL, and
# the failing clause is named.  P-07b enforced only the median, which reported
# a cell with 21 octave outliers and a 731-cent p90 as PASS.

# P-07c: the formant ratio check is gated only inside the region D1 validates
# (D-01 amendment, 2026-10-28).  Pitch 0 allows the whole 0.75-1.33 knob range;
# any shift up to +/-7 st allows only 1.00-1.10, because the opposite corners
# ask the correction to move formants against an already-scaled excitation.
D1_FORMANT_RANGE_ZERO_PITCH = (0.75, 1.33)
D1_FORMANT_RANGE_SHIFTED = (1.00, 1.10)
D1_MAX_ABS_PITCH_ST = 7.0
D1_ZERO_PITCH_MAX_ABS_ST = 0.5

# `--formant-self-test`: synthetic vowels with formants known by construction,
# at every F0 the corpus spans.  P-03's Rust table used 90/110 Hz; the high
# female range is where the legacy LPC ruler breaks, so it has to be in the
# control, not just in the commentary.
FORMANT_SELF_TEST_F0 = (90.0, 110.0, 150.0, 220.0, 250.0, 300.0)
FORMANT_SELF_TEST_VOWELS = (
    ("700/1220/2600", (700.0, 1220.0, 2600.0), (80.0, 90.0, 120.0)),
    ("500/1500/2500", (500.0, 1500.0, 2500.0), (70.0, 90.0, 120.0)),
)
FORMANT_SELF_TEST_SECONDS = 0.6
FORMANT_RATIO_SELF_TEST = 1.10
# Falsifiability check inside the self-test: a 10% error in the expectation
# must be rejected by the 1% gate.  If someone ever widens the tolerance past
# this, the control stops proving anything and the self-test says so.
FORMANT_SELF_TEST_NEGATIVE_PCT = 10.0

REPO_ROOT = os.path.dirname(_HERE)


# ------------------------------------------------------------------ helpers

def fnum(v, nd=1, dash="n/a"):
    """Fixed decimals; ``dash`` for NaN/None, explicit ±inf otherwise."""
    if v is None:
        return dash
    try:
        v = float(v)
    except (TypeError, ValueError):
        return dash
    if math.isnan(v):
        return dash
    if math.isinf(v):
        return "-inf" if v < 0 else "inf"
    return ("%%.%df" % nd) % v


def fsgn(v, nd=1, dash="n/a"):
    """Signed fixed decimals, with an unsigned zero ("0.0", not "+0.0")."""
    if v is None:
        return dash
    try:
        v = float(v)
    except (TypeError, ValueError):
        return dash
    if not math.isfinite(v):
        return dash
    if abs(v) < 0.5 * (10.0 ** -nd):
        v = 0.0
    if v == 0:
        return "%.*f" % (nd, 0.0)
    return "%+.*f" % (nd, v)


def human_int(n):
    return "{:,}".format(int(n))


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def files_identical(a, b):
    """True when the two paths name the same bytes (path, hardlink or hash)."""
    try:
        return os.path.samefile(a, b)
    except OSError:
        return sha256_file(a) == sha256_file(b)


def display_path(path):
    """Repo-relative when possible; avoids baking absolute paths into JSON."""
    absolute = os.path.abspath(path)
    if absolute.startswith(REPO_ROOT + os.sep):
        return os.path.relpath(absolute, REPO_ROOT)
    return path


def print_table(headers, rows, aligns=None):
    """Plain ASCII table; ``aligns`` is 'l' or 'r' per column."""
    n = len(headers)
    aligns = aligns or ["l"] * n
    cells = [[str(c) for c in r] for r in rows]
    widths = [len(headers[i]) for i in range(n)]
    for r in cells:
        for i in range(n):
            widths[i] = max(widths[i], len(r[i]))
    def line(r):
        parts = []
        for i in range(n):
            parts.append(r[i].rjust(widths[i]) if aligns[i] == "r"
                         else r[i].ljust(widths[i]))
        return "  ".join(parts).rstrip()
    out = [line(headers), "  ".join("-" * w for w in widths)]
    out.extend(line(r) for r in cells)
    return "\n".join(out)


def check(name, target, measured, passed, applicable=True, reason=None):
    return {
        "name": name,
        "target": target,
        "measured": measured,
        "applicable": bool(applicable),
        "pass": bool(passed) if applicable else None,
        "reason": reason if not applicable else None,
    }


def verdict(checks, name):
    """PASS/FAIL from the check with this name, or '-' when not applicable."""
    for c in checks:
        if c["name"] == name:
            if not c["applicable"]:
                return "-"
            return "PASS" if c["pass"] else "FAIL"
    return "-"


def expected_output_hz(ref_median_hz, expected_cents):
    """Where the render's F0 should land, or NaN when the reference is unusable."""
    if (expected_cents is None or ref_median_hz is None
            or not math.isfinite(ref_median_hz) or ref_median_hz <= 0):
        return float("nan")
    return ref_median_hz * 2.0 ** (expected_cents / 1200.0)


def pitch_applicability(pitch, ref_median_hz):
    """Whether the paired pitch check can arbitrate, and why not when it cannot.

    ``N/A`` is a statement about the ruler, not the DSP: the expected output
    left the band where a 40 ms YIN + spectral-peak tracker is trustworthy, or
    there were too few frames voiced in both signals.  It is never a pass, and
    it is never a failure.
    """
    if pitch["expected_cents"] is None:
        return False, "no expected shift was given"
    if (ref_median_hz is None or not math.isfinite(ref_median_hz)
            or ref_median_hz <= 0):
        return False, "reference F0 is unavailable (silent or unvoiced reference)"
    expected_hz = expected_output_hz(ref_median_hz, pitch["expected_cents"])
    if expected_hz < PITCH_RELIABLE_MIN_HZ:
        return False, (
            "expected output F0 %.1f Hz is below the reliable tracker band "
            "(%.0f-%.0f Hz)" % (expected_hz, PITCH_RELIABLE_MIN_HZ,
                                PITCH_RELIABLE_MAX_HZ))
    if expected_hz > PITCH_RELIABLE_MAX_HZ:
        return False, (
            "expected output F0 %.1f Hz is above the reliable tracker band "
            "(%.0f-%.0f Hz)" % (expected_hz, PITCH_RELIABLE_MIN_HZ,
                                PITCH_RELIABLE_MAX_HZ))
    if pitch["paired_frames"] == 0:
        return False, "no frames voiced in both signals"
    if pitch["paired_frames"] < PITCH_MIN_PAIRED_FRAMES:
        return False, ("only %d paired voiced frames (need %d)"
                       % (pitch["paired_frames"], PITCH_MIN_PAIRED_FRAMES))
    return True, None


def pitch_measured_text(pitch):
    return ("median %s c (p10 %s, p90 %s, IQR %s, n=%d)"
            % (fsgn(pitch["median_cents"], 2),
               fsgn(pitch["p10_cents"], 1),
               fsgn(pitch["p90_cents"], 1),
               fnum(pitch["iqr_cents"], 2),
               pitch["paired_frames"]))


def pitch_gate(pitch):
    """D17's whole-clip gate: median, p90 and octave outliers, all required.

    Returns per-clause booleans plus a list of human-readable failures naming
    the clause that missed.  ``median`` compares the *error* against the
    requested shift; ``p90`` compares the 90th percentile of the **absolute**
    error, which is the guard against a distribution with a clean middle and a
    polluted tail (21 octave outliers used to hide there); ``octave`` requires
    no frame more than ``PITCH_OCTAVE_OUTLIER_CENTS`` from the request.
    """
    error = pitch["error_cents"]
    p90_abs = pitch["p90_abs_error_cents"]
    median_ok = (math.isfinite(error)
                 and abs(error) <= PITCH_TOLERANCE_CENTS)
    p90_ok = (math.isfinite(p90_abs)
              and p90_abs <= PITCH_P90_TOLERANCE_CENTS)
    octave_ok = pitch["octave_outliers"] == 0
    failures = []
    if not median_ok:
        failures.append("median %s c (limit %.0f c)"
                        % (fsgn(error, 2), PITCH_TOLERANCE_CENTS))
    if not p90_ok:
        failures.append("p90 |error| %s c (limit %.0f c)"
                        % (fnum(p90_abs, 2), PITCH_P90_TOLERANCE_CENTS))
    if not octave_ok:
        failures.append("%d octave outlier(s) (limit 0)"
                        % pitch["octave_outliers"])
    return {
        "median": bool(median_ok),
        "p90": bool(p90_ok),
        "octave": bool(octave_ok),
        "passed": bool(median_ok and p90_ok and octave_ok),
        "failures": failures,
    }


def pitch_gate_text(gate):
    """``median<=5, p90<=25, octave=0`` with the failed clauses spelled out."""
    target = ("median <= %.0f c, p90 <= %.0f c, %d octave"
              % (PITCH_TOLERANCE_CENTS, PITCH_P90_TOLERANCE_CENTS, 0))
    if gate["passed"]:
        return target
    return "%s; failed: %s" % (target, ", ".join(gate["failures"]))


def pitch_payload(pitch):
    """camelCase view of ``audio_metrics.paired_f0_ratio`` for JSON output."""
    gate = pitch_gate(pitch)
    return {
        "method": pitch["method"],
        "timeScale": pitch["time_scale"],
        "bandHz": pitch["band_hz"],
        "refBandHz": pitch["ref_band_hz"],
        "expectedRatio": pitch["expected_ratio"],
        "expectedCents": pitch["expected_cents"],
        "pairedFrames": pitch["paired_frames"],
        "sharedFrames": pitch["shared_frames"],
        "bothVoicedFrames": pitch["both_voiced_frames"],
        "refVoicedFrames": pitch["ref_voiced_frames"],
        "testVoicedFrames": pitch["test_voiced_frames"],
        "refOnlyVoicedFrames": pitch["ref_only_voiced_frames"],
        "testOnlyVoicedFrames": pitch["test_only_voiced_frames"],
        "refAgreedFrames": pitch["ref_agreed_frames"],
        "testAgreedFrames": pitch["test_agreed_frames"],
        "windowSeconds": pitch["window_seconds"],
        "frameHopSeconds": pitch["frame_hop_seconds"],
        "medianCents": pitch["median_cents"],
        "errorCents": pitch["error_cents"],
        "p10Cents": pitch["p10_cents"],
        "p25Cents": pitch["p25_cents"],
        "p75Cents": pitch["p75_cents"],
        "p90Cents": pitch["p90_cents"],
        "iqrCents": pitch["iqr_cents"],
        "p90AbsErrorCents": pitch["p90_abs_error_cents"],
        "octaveOutliers": pitch["octave_outliers"],
        "refMedianHz": pitch["ref_median_hz"],
        "testMedianHz": pitch["test_median_hz"],
        "droppedFrames": pitch["dropped_frames"],
        "droppedReasons": pitch["dropped_reasons"],
        "droppedRefBand": pitch["dropped_ref_band"],
        "droppedTestBand": pitch["dropped_test_band"],
        "droppedExpectedBand": pitch["dropped_expected_band"],
        "droppedRefDisagree": pitch["dropped_ref_disagree"],
        "droppedTestDisagree": pitch["dropped_test_disagree"],
        "droppedRefOutlier": pitch["dropped_ref_outlier"],
        "droppedRefSupport": pitch["dropped_ref_support"],
        "droppedTestSupport": pitch["dropped_test_support"],
        "gate": {
            "medianLimitCents": PITCH_TOLERANCE_CENTS,
            "p90LimitCents": PITCH_P90_TOLERANCE_CENTS,
            "octaveLimit": 0,
            "octaveOutlierCents": PITCH_OCTAVE_OUTLIER_CENTS,
            **gate,
        },
    }


def d1_formant_region(pitch_st):
    """The formant ratios D1 validates at this pitch, or ``(None, reason)``.

    ``pitch_st`` is the *requested* pitch when the caller knows it, otherwise
    the measured paired pitch.  ``|pitch| <= 0.5 st`` counts as pitch 0: below
    half a semitone the two rows of D1's table describe the same physics and
    measurement noise must not push a valid pitch-0 cell into the narrower row.
    """
    if pitch_st is None or not math.isfinite(pitch_st):
        return None, ("pitch position is unknown, so the D1 validated region "
                      "cannot be chosen")
    if abs(pitch_st) <= D1_ZERO_PITCH_MAX_ABS_ST:
        return D1_FORMANT_RANGE_ZERO_PITCH, None
    if abs(pitch_st) <= D1_MAX_ABS_PITCH_ST:
        return D1_FORMANT_RANGE_SHIFTED, None
    return None, ("pitch %+.1f st is outside the D1 validated region "
                  "(|pitch| <= %.0f st)" % (pitch_st, D1_MAX_ABS_PITCH_ST))


def formant_ratio_applicability(expected_ratio, pitch_st, ref_formants,
                                test_formants):
    """Whether the harmonic formant ratio can be gated, and why not.

    ``N/A`` here mirrors the pitch policy: it is a statement about the ruler
    and the validated region, never a pass.  A ratio is gated only when all of
    these hold:

    * an expected ratio was supplied;
    * the pitch position lies in D1's table and the requested ratio is inside
      that row (pitch 0: 0.75-1.33, +/-7 st: 1.00-1.10);
    * both signals' formants are resolvable by the harmonic-model estimator
      (``harmonic_formants`` sets ``resolved`` and ``na_reason``).
    """
    if expected_ratio is None:
        return False, "no expected formant ratio was given"
    region, reason = d1_formant_region(pitch_st)
    if region is None:
        return False, reason
    if not (region[0] - 1e-9 <= expected_ratio <= region[1] + 1e-9):
        return False, (
            "requested formant ratio %.2f is outside the D1 validated region "
            "%.2f-%.2f for pitch %+.1f st"
            % (expected_ratio, region[0], region[1], pitch_st))
    for label, formants in (("reference", ref_formants),
                            ("test", test_formants)):
        if not formants.get("resolved"):
            return False, ("%s formants are N/A: %s"
                           % (label, formants.get("na_reason")
                              or "not resolvable"))
    return True, None


def formant_fit_text(formants):
    """One-line fit description for the human table."""
    if not formants.get("resolved"):
        return "n/a"
    return ("F1-F3 %.0f/%.0f/%.0f, %d frames, %.2f dB"
            % (formants["f1_hz"], formants["f2_hz"], formants["f3_hz"],
               formants["frames_used"], formants["residual_db_median"]))


def json_safe(value):
    """Replace NaN/inf with null so the output is strict JSON."""
    if isinstance(value, dict):
        return {k: json_safe(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [json_safe(v) for v in value]
    if isinstance(value, float) and not math.isfinite(value):
        return None
    if isinstance(value, (np.floating, np.integer)):
        return json_safe(float(value))
    if isinstance(value, np.ndarray):
        return [json_safe(v) for v in value.tolist()]
    return value


# ----------------------------------------------------------- compare mode

def measure_clip(path, fmt):
    x = load_audio(path, fmt)
    level = level_stats(x)
    fs = f0_stats(x)
    formants = harmonic_formants(x)
    legacy = lpc_formants(x)
    metrics = {
        "samples": level["samples"],
        "duration_s": level["duration_s"],
        "peak": level["peak"],
        "peak_dbfs": level["peak_dbfs"],
        "rms_dbfs": level["rms_dbfs"],
        "dc": level["dc"],
        "clipped_samples": level["clipped_samples"],
        "finite": level["finite"],
        "f0_median_hz": fs["f0_median_hz"],
        "f0_p10_hz": fs["f0_p10_hz"],
        "f0_p90_hz": fs["f0_p90_hz"],
        "voiced_fraction": fs["voiced_fraction"],
        "hnr_median_db": fs["hnr_median_db"],
        "centroid_hz": spectral_centroid(x),
        "lufs_i": lufs_integrated(x),
        # P-07c: the gated numbers come from the harmonic-model ruler; the LPC
        # values stay beside them (labelled legacy) so the two can be compared.
        "f1_hz": formants["f1_hz"],
        "f2_hz": formants["f2_hz"],
        "f3_hz": formants["f3_hz"],
        "formants": formants,
        "lpc_f1_hz": legacy["f1_hz"],
        "lpc_f2_hz": legacy["f2_hz"],
        "lpc_f3_hz": legacy["f3_hz"],
        "lpc_formants": legacy,
    }
    return x, metrics


def run_compare(args):
    if not args.ref or not args.test:
        raise SystemExit("metrics.py: --ref and --test are required "
                         "(or use --manifest)")
    ref_x, ref = measure_clip(args.ref, args.format)
    test_x, test = measure_clip(args.test, args.format)

    identical = files_identical(args.ref, args.test)
    if args.pitch_st is not None:
        expected_cents = 1200.0 * args.pitch_st
        pitch_ratio = 2.0 ** (args.pitch_st / 12.0)
    elif identical:
        expected_cents = 0.0
        pitch_ratio = 1.0
    else:
        expected_cents = None
        pitch_ratio = None

    if args.formant_ratio is not None:
        expected_ratio = args.formant_ratio
    elif identical:
        expected_ratio = 1.0
    else:
        expected_ratio = None

    pitch = paired_f0_ratio(ref_x, test_x, expected_ratio=pitch_ratio)
    pitch_ok, pitch_reason = pitch_applicability(pitch, ref["f0_median_hz"])
    cents = cents_between(ref["f0_median_hz"], test["f0_median_hz"])

    # D1's validated region is a function of the pitch position.  The requested
    # shift is authoritative; a render without --pitch-st falls back to the
    # measured paired pitch (and identity is 0 by definition).
    if args.pitch_st is not None:
        pitch_position = args.pitch_st
        pitch_position_source = "requested"
    elif identical:
        pitch_position = 0.0
        pitch_position_source = "identity"
    elif math.isfinite(pitch["median_cents"]):
        pitch_position = pitch["median_cents"] / 100.0
        pitch_position_source = "measured"
    else:
        pitch_position = None
        pitch_position_source = "unknown"
    d1_region, d1_reason = d1_formant_region(pitch_position)

    formant_pct = {
        key: percent_delta(ref[key], test[key]) for key in ("f1_hz", "f2_hz", "f3_hz")
    }
    lpc_formant_pct = {
        key: percent_delta(ref[key], test[key])
        for key in ("lpc_f1_hz", "lpc_f2_hz", "lpc_f3_hz")
    }
    formant_ok, formant_reason = formant_ratio_applicability(
        expected_ratio, pitch_position, ref["formants"], test["formants"])
    null = null_test(ref_x, test_x)
    length_delta = test["samples"] - ref["samples"]
    length_ok = abs(length_delta) <= args.allow_length_delta

    checks = []
    checks.append(check(
        "all samples finite", "no NaN/Inf",
        "yes" if (ref["finite"] and test["finite"]) else "no",
        ref["finite"] and test["finite"]))
    checks.append(check(
        "sample count",
        "equal" if args.allow_length_delta == 0
        else "allow +/-%d" % args.allow_length_delta,
        "%s vs %s (delta %s)" % (human_int(ref["samples"]),
                                 human_int(test["samples"]),
                                 fsgn(length_delta, 0)),
        length_ok))

    pitch_measured = pitch_measured_text(pitch)
    if expected_cents is not None:
        pitch_measured += "; expected %s c" % fsgn(expected_cents, 1)
    gate = pitch_gate(pitch)
    if pitch_ok and not gate["passed"]:
        pitch_measured += "; failed: %s" % ", ".join(gate["failures"])
    checks.append(check(
        "pitch (D17 gate)", "median <= %.0f c, p90 <= %.0f c, 0 octave"
        % (PITCH_TOLERANCE_CENTS, PITCH_P90_TOLERANCE_CENTS),
        pitch_measured,
        pitch_ok and gate["passed"],
        applicable=pitch_ok, reason=pitch_reason))

    for key, label in (("f1_hz", "F1"), ("f2_hz", "F2")):
        pct = formant_pct[key]
        if expected_ratio is None:
            checks.append(check("formant ratio %s" % label, "+/-8%",
                                "%s%%" % fsgn(pct), False, applicable=False,
                                reason="no expected formant ratio was given"))
        elif not formant_ok:
            checks.append(check(
                "formant ratio %s" % label, "+/-8%",
                ("%s%% (expected %s%%)"
                 % (fsgn(pct), fsgn(100.0 * (expected_ratio - 1.0)))),
                False, applicable=False, reason=formant_reason))
        else:
            expected_pct = 100.0 * (expected_ratio - 1.0)
            checks.append(check(
                "formant ratio %s" % label, "+/-8%",
                "%s%% (expected %s%%)" % (fsgn(pct), fsgn(expected_pct)),
                math.isfinite(pct)
                and abs(pct - expected_pct) <= FORMANT_TOLERANCE_PCT,
                applicable=True))

    null_applicable = identical or args.expect_null
    checks.append(check(
        "null residual", "<= %.0f dB" % NULL_TARGET_DB,
        "%s dB" % fnum(null["residual_db"], 1),
        null["same_length"] and null["residual_db"] <= NULL_TARGET_DB,
        applicable=null_applicable))

    ok = all(c["pass"] for c in checks if c["applicable"])

    rows = [
        ["samples", human_int(ref["samples"]), human_int(test["samples"]),
         fsgn(length_delta, 0),
         "equal" if args.allow_length_delta == 0
         else "allow +/-%d" % args.allow_length_delta,
         verdict(checks, "sample count")],
        ["duration (s)", fnum(ref["duration_s"], 3), fnum(test["duration_s"], 3),
         fsgn(test["duration_s"] - ref["duration_s"], 3), "-", "-"],
        ["F0 median (Hz)", fnum(ref["f0_median_hz"], 1),
         fnum(test["f0_median_hz"], 1),
         "%s cents (legacy)" % fsgn(cents), "-", "-"],
        ["pitch (paired)", fnum(pitch["ref_median_hz"], 1),
         fnum(pitch["test_median_hz"], 1),
         "%s c (IQR %s, p90 %s, n=%d)"
         % (fsgn(pitch["median_cents"], 2),
            fnum(pitch["iqr_cents"], 2),
            fsgn(pitch["p90_cents"], 1),
            pitch["paired_frames"]),
         "median <= %.0f c, p90 <= %.0f c"
         % (PITCH_TOLERANCE_CENTS, PITCH_P90_TOLERANCE_CENTS),
         verdict(checks, "pitch (D17 gate)")],
        ["pitch frames", str(pitch["ref_voiced_frames"]),
         str(pitch["test_voiced_frames"]),
         "%d paired, %d octave outliers, %d dropped as unreliable"
         % (pitch["paired_frames"], pitch["octave_outliers"],
            pitch["dropped_frames"]),
         "octave outliers = 0", "-"],
        ["F1 (Hz, harmonic)", fnum(ref["f1_hz"], 0), fnum(test["f1_hz"], 0),
         "%s%%" % fsgn(formant_pct["f1_hz"]), "+/-8%",
         verdict(checks, "formant ratio F1")],
        ["F2 (Hz, harmonic)", fnum(ref["f2_hz"], 0), fnum(test["f2_hz"], 0),
         "%s%%" % fsgn(formant_pct["f2_hz"]), "+/-8%",
         verdict(checks, "formant ratio F2")],
        ["F3 (Hz, harmonic)", fnum(ref["f3_hz"], 0), fnum(test["f3_hz"], 0),
         "%s%%" % fsgn(formant_pct["f3_hz"]), "not gated", "-"],
        ["F1 (Hz, LPC legacy)", fnum(ref["lpc_f1_hz"], 0),
         fnum(test["lpc_f1_hz"], 0),
         "%s%%" % fsgn(lpc_formant_pct["lpc_f1_hz"]), "legacy", "-"],
        ["F2 (Hz, LPC legacy)", fnum(ref["lpc_f2_hz"], 0),
         fnum(test["lpc_f2_hz"], 0),
         "%s%%" % fsgn(lpc_formant_pct["lpc_f2_hz"]), "legacy", "-"],
        ["F3 (Hz, LPC legacy)", fnum(ref["lpc_f3_hz"], 0),
         fnum(test["lpc_f3_hz"], 0),
         "%s%%" % fsgn(lpc_formant_pct["lpc_f3_hz"]), "legacy", "-"],
        ["formant frames", "%d/%d" % (ref["formants"]["frames_used"],
                                      ref["formants"]["frames_total"]),
         "%d/%d" % (test["formants"]["frames_used"],
                    test["formants"]["frames_total"]),
         "%s dB residual" % fnum(
             ref["formants"]["residual_db_median"], 2), "-", "-"],
        ["centroid (Hz)", fnum(ref["centroid_hz"], 0),
         fnum(test["centroid_hz"], 0),
         "%s%%" % fsgn(percent_delta(ref["centroid_hz"],
                                     test["centroid_hz"])), "-", "-"],
        ["HNR (dB)", fnum(ref["hnr_median_db"], 1),
         fnum(test["hnr_median_db"], 1),
         "%s dB" % fsgn(test["hnr_median_db"] - ref["hnr_median_db"]), "-", "-"],
        ["LUFS-I (LUFS)", fnum(ref["lufs_i"], 1), fnum(test["lufs_i"], 1),
         "%s LU" % fsgn(test["lufs_i"] - ref["lufs_i"]), "-", "-"],
        ["peak (dBFS)", fnum(ref["peak_dbfs"], 1),
         fnum(test["peak_dbfs"], 1),
         "%s dB" % fsgn(test["peak_dbfs"] - ref["peak_dbfs"]), "-", "-"],
        ["RMS (dBFS)", fnum(ref["rms_dbfs"], 1), fnum(test["rms_dbfs"], 1),
         "%s dB" % fsgn(test["rms_dbfs"] - ref["rms_dbfs"]), "-", "-"],
        ["DC offset", fnum(ref["dc"], 5), fnum(test["dc"], 5),
         fsgn(test["dc"] - ref["dc"], 5), "-", "-"],
        ["clipped samples", human_int(ref["clipped_samples"]),
         human_int(test["clipped_samples"]),
         fsgn(test["clipped_samples"] - ref["clipped_samples"], 0), "-", "-"],
        ["null residual (dB)", "-", "-", "%s dB" % fnum(null["residual_db"], 1),
         "<= %.0f dB" % NULL_TARGET_DB, verdict(checks, "null residual")],
    ]

    payload = {
        "schemaVersion": 2,
        "task": "P-07d",
        "mode": "compare",
        "sampleRate": SAMPLE_RATE,
        "assumption": "48 kHz mono; .f32 input is headerless little-endian f32",
        "ref": {
            "path": display_path(args.ref),
            "samples": ref["samples"],
            "durationSeconds": ref["duration_s"],
            "sha256": sha256_file(args.ref),
        },
        "test": {
            "path": display_path(args.test),
            "samples": test["samples"],
            "durationSeconds": test["duration_s"],
            "sha256": sha256_file(args.test),
        },
        "identicalBytes": identical,
        "metrics": {
            "pitch": {
                **pitch_payload(pitch),
                "expectedOutputHz": expected_output_hz(ref["f0_median_hz"],
                                                       expected_cents),
                "reliableBandHz": [PITCH_RELIABLE_MIN_HZ,
                                   PITCH_RELIABLE_MAX_HZ],
                "naReason": pitch_reason,
            },
            "f0MedianHz": {
                "ref": ref["f0_median_hz"], "test": test["f0_median_hz"],
                "p10Hz": {"ref": ref["f0_p10_hz"], "test": test["f0_p10_hz"]},
                "p90Hz": {"ref": ref["f0_p90_hz"], "test": test["f0_p90_hz"]},
                "centsError": cents,
                "legacy": True,
            },
            "formantsHz": {
                "method": "harmonic-fit",
                "f1": {"ref": ref["f1_hz"], "test": test["f1_hz"],
                       "percentDelta": formant_pct["f1_hz"]},
                "f2": {"ref": ref["f2_hz"], "test": test["f2_hz"],
                       "percentDelta": formant_pct["f2_hz"]},
                "f3": {"ref": ref["f3_hz"], "test": test["f3_hz"],
                       "percentDelta": formant_pct["f3_hz"]},
                "resolved": {"ref": ref["formants"]["resolved"],
                             "test": test["formants"]["resolved"]},
                "naReason": formant_reason,
                "validatedRegion": {
                    "pitchSt": pitch_position,
                    "pitchSource": pitch_position_source,
                    "formantRange": (list(d1_region) if d1_region else None),
                    "naReason": d1_reason,
                },
                "fit": {
                    "ref": {
                        "framesUsed": ref["formants"]["frames_used"],
                        "framesTotal": ref["formants"]["frames_total"],
                        "residualDb": ref["formants"]["residual_db_median"],
                        "f0MedianHz": ref["formants"]["f0_median_hz"],
                        "bandwidthsHz": ref["formants"]["bandwidths_hz"],
                    },
                    "test": {
                        "framesUsed": test["formants"]["frames_used"],
                        "framesTotal": test["formants"]["frames_total"],
                        "residualDb": test["formants"]["residual_db_median"],
                        "f0MedianHz": test["formants"]["f0_median_hz"],
                        "bandwidthsHz": test["formants"]["bandwidths_hz"],
                    },
                },
                "legacyLpc": {
                    "method": "lpc-order-18",
                    "f1": {"ref": ref["lpc_f1_hz"], "test": test["lpc_f1_hz"],
                           "percentDelta": lpc_formant_pct["lpc_f1_hz"]},
                    "f2": {"ref": ref["lpc_f2_hz"], "test": test["lpc_f2_hz"],
                           "percentDelta": lpc_formant_pct["lpc_f2_hz"]},
                    "f3": {"ref": ref["lpc_f3_hz"], "test": test["lpc_f3_hz"],
                           "percentDelta": lpc_formant_pct["lpc_f3_hz"]},
                },
            },
            "centroidHz": {"ref": ref["centroid_hz"], "test": test["centroid_hz"],
                           "percentDelta": percent_delta(ref["centroid_hz"],
                                                         test["centroid_hz"])},
            "hnrDb": {"ref": ref["hnr_median_db"], "test": test["hnr_median_db"]},
            "lufsI": {"ref": ref["lufs_i"], "test": test["lufs_i"]},
            "peakDbfs": {"ref": ref["peak_dbfs"], "test": test["peak_dbfs"]},
            "rmsDbfs": {"ref": ref["rms_dbfs"], "test": test["rms_dbfs"]},
            "dc": {"ref": ref["dc"], "test": test["dc"]},
            "clippedSamples": {"ref": ref["clipped_samples"],
                               "test": test["clipped_samples"]},
            "voicedFraction": {"ref": ref["voiced_fraction"],
                               "test": test["voiced_fraction"]},
        },
        "null": null,
        "expectations": {
            "identityBytes": identical,
            "pitchSt": args.pitch_st,
            "expectedCents": expected_cents,
            "formantRatio": args.formant_ratio,
            "expectedFormantRatio": expected_ratio,
            "enforceNull": bool(args.expect_null or identical),
            "allowLengthDelta": args.allow_length_delta,
            "lengthDelta": length_delta,
        },
        "checks": checks,
        "ok": ok,
    }

    if args.json:
        print(json.dumps(json_safe(payload), indent=2, allow_nan=False))
    else:
        print("RASP P-07 metrics - 48 kHz mono assumed")
        print("ref   %s  (%s samples, %s s)"
              % (display_path(args.ref), human_int(ref["samples"]),
                 fnum(ref["duration_s"], 3)))
        print("test  %s  (%s samples, %s s)"
              % (display_path(args.test), human_int(test["samples"]),
                 fnum(test["duration_s"], 3)))
        print()
        print(print_table(
            ["metric", "ref", "test", "delta", "target", "check"],
            rows, ["l", "r", "r", "r", "r", "l"]))
        print()
        if not null_applicable:
            print("null residual is descriptive: ref and test are different files. "
                  "Pass --expect-null to enforce <= %.0f dB." % NULL_TARGET_DB)
        if pitch_reason:
            print("pitch: N/A - %s. This is not a DSP failure; the ruler "
                  "cannot arbitrate this cell." % pitch_reason)
        if formant_reason and expected_ratio is not None:
            print("formants: N/A - %s. The harmonic-model ruler reports N/A "
                  "rather than a number it cannot certify." % formant_reason)
        elif formant_ok:
            print("formants: harmonic-model fit (ref %s; test %s); the LPC rows "
                  "are the legacy estimator, shown for comparison only."
                  % (formant_fit_text(ref["formants"]),
                     formant_fit_text(test["formants"])))
        if length_delta != 0 and length_ok:
            print("length: delta %s samples is allowed by --allow-length-delta %d; "
                  "pitch and null use the common region."
                  % (fsgn(length_delta, 0), args.allow_length_delta))
        if pitch["dropped_frames"]:
            print("pitch: %d frame(s) dropped as unreliable before the statistics "
                  "(reference band %d, render band %d, expected band %d, "
                  "cross-check %d, reference contour %d, harmonic support %d)."
                  % (pitch["dropped_frames"], pitch["dropped_ref_band"],
                     pitch["dropped_test_band"], pitch["dropped_expected_band"],
                     pitch["dropped_ref_disagree"] + pitch["dropped_test_disagree"],
                     pitch["dropped_ref_outlier"],
                     pitch["dropped_ref_support"] + pitch["dropped_test_support"]))
        if pitch["octave_outliers"]:
            print("pitch: %d paired frame(s) are >%.0f cents from the expected "
                  "ratio (octave outliers). D17 requires zero, so this fails the "
                  "cell; a render-side octave artifact is never dropped."
                  % (pitch["octave_outliers"], PITCH_OCTAVE_OUTLIER_CENTS))
        if identical:
            print("identity: same bytes, so pitch 0 cents / null are enforced. "
                  "The formant ratio is 0% only when the ruler resolves the "
                  "formants; otherwise it is N/A as shown above.")
        failed = [c["name"] for c in checks if c["applicable"] and not c["pass"]]
        applicable = [c for c in checks if c["applicable"]]
        if failed:
            print("Result: FAIL - %s" % ", ".join(failed))
        else:
            print("Result: PASS (%d checks; %d not applicable)"
                  % (len(applicable), len(checks) - len(applicable)))

    return 0 if ok or not args.check else 1


# ---------------------------------------------------------- manifest mode

def run_manifest(args):
    path = args.manifest_path or os.path.join(REPO_ROOT, "fixtures",
                                              "manifest.json")
    if not os.path.isfile(path):
        sys.stderr.write(
            "metrics.py: %s not found. Generate the fixtures first:\n"
            "  python3 scripts/make_fixtures.py\n" % display_path(path))
        return 2
    with open(path, "r", encoding="utf-8") as f:
        manifest = json.load(f)

    manifest_rate = manifest.get("audio", {}).get("sampleRate")
    if manifest_rate is not None and manifest_rate != SAMPLE_RATE:
        sys.stderr.write(
            "metrics.py: %s declares %s Hz; this harness measures %d Hz only\n"
            % (display_path(path), manifest_rate, SAMPLE_RATE))
        return 2

    base = os.path.dirname(os.path.abspath(path))
    clips, rows = [], []
    for entry in manifest.get("clips", []):
        files = entry.get("files", {})
        if "f32" not in files or "reference" not in files:
            sys.stderr.write("metrics.py: %s has no f32/reference pair\n"
                             % entry.get("name", "?"))
            return 2
        ref_path = os.path.join(base, files["reference"]["path"])
        test_path = os.path.join(base, files["f32"]["path"])
        ref_x = load_audio(ref_path, "f32")
        test_x = load_audio(test_path, "f32")
        ref_level = level_stats(ref_x)
        test_level = level_stats(test_x)
        null = null_test(ref_x, test_x)

        finite = bool(ref_level["finite"] and test_level["finite"]
                      and null["finite"])
        same_count = ref_level["samples"] == test_level["samples"]
        nonzero = bool(null["rms_diff"] > 0 and math.isfinite(null["snr_db"]))
        checks = [
            check("sample count", "equal",
                  "%d vs %d" % (test_level["samples"], ref_level["samples"]),
                  same_count),
            check("no NaN/Inf", "yes", "yes" if finite else "no", finite),
            check("nonzero SNR", "> 0", fnum(null["snr_db"], 1) + " dB", nonzero),
        ]
        passed = all(c["pass"] for c in checks)
        clips.append({
            "name": entry.get("name"),
            "voice": entry.get("voice"),
            "phrase": entry.get("phrase"),
            "f32Samples": test_level["samples"],
            "referenceSamples": ref_level["samples"],
            "snrDb": null["snr_db"],
            "residualDb": null["residual_db"],
            "residualRmsDbfs": null["rms_diff_dbfs"],
            "maxResidualDbfs": null["max_diff_dbfs"],
            "finite": finite,
            "checks": checks,
            "pass": passed,
        })
        rows.append([
            entry.get("name", "?"),
            human_int(test_level["samples"]),
            human_int(ref_level["samples"]),
            fnum(null["snr_db"], 1),
            fnum(null["rms_diff_dbfs"], 1),
            "PASS" if passed else "FAIL",
        ])

    if not clips:
        sys.stderr.write("metrics.py: %s lists no clips\n" % display_path(path))
        return 2

    snrs = [c["snrDb"] for c in clips if math.isfinite(c["snrDb"])]
    median_snr = float(np.median(snrs)) if snrs else float("nan")
    passed_n = sum(1 for c in clips if c["pass"])
    total_samples = sum(c["f32Samples"] for c in clips)
    ok = passed_n == len(clips)

    payload = {
        "schemaVersion": 2,
        "task": "P-07d",
        "mode": "manifest",
        "sampleRate": SAMPLE_RATE,
        "manifest": display_path(path),
        "comparison": "f32 vs reference (different Opus decoders; expected "
                      "same sample count, nonzero SNR, no NaN)",
        "clips": clips,
        "summary": {
            "clips": len(clips),
            "passed": passed_n,
            "failed": len(clips) - passed_n,
            "samples": total_samples,
            "medianSnrDb": median_snr,
        },
        "ok": ok,
    }

    if args.json:
        print(json.dumps(json_safe(payload), indent=2, allow_nan=False))
    else:
        print("Fixtures self-check: %s" % display_path(path))
        print("f32 (ffmpeg native) vs reference (libopus) - different "
              "decoders by design")
        print()
        print(print_table(
            ["clip", "f32 samples", "ref samples", "SNR dB", "residual dBFS",
             "check"],
            rows, ["l", "r", "r", "r", "r", "l"]))
        print()
        print("%d/%d clips passed - %s samples - median SNR %s dB"
              % (passed_n, len(clips), human_int(total_samples),
                 fnum(median_snr, 1)))
        print("Result: %s" % ("PASS" if ok else "FAIL"))

    return 0 if ok or not args.check else 1


# ------------------------------------------------------------- renders mode

_RENDER_NAME_RE = re.compile(r"^(?P<base>.+?)[._](?P<step>[+-]?\d+)(?:st)?$")
SELF_TEST_STEPS = list(range(-12, 13))
SELF_TEST_WIN_S = 0.050
SELF_TEST_HOP_S = 0.025


def _normalise_row(raw):
    row = {}
    for key, value in raw.items():
        if key is None:
            continue
        row[key.strip().lower()] = (value or "").strip()
    return row


def _parse_step(text):
    text = (text or "").strip()
    if not text:
        return None
    try:
        return int(round(float(text)))
    except ValueError:
        return None


def _manifest_expected(row):
    """Return ``(step, cents, ratio)`` from a render manifest row."""
    step = _parse_step(row.get("step"))
    cents = None
    ratio = None
    if row.get("cents"):
        try:
            cents = float(row["cents"])
        except ValueError:
            pass
    if row.get("ratio"):
        try:
            ratio = float(row["ratio"])
        except ValueError:
            pass
    if step is None and cents is not None:
        step = int(round(cents / 100.0))
    if cents is None and step is not None:
        cents = 100.0 * step
    if ratio is None and cents is not None:
        ratio = 2.0 ** (cents / 1200.0)
    if ratio is None and step is not None:
        ratio = 2.0 ** (step / 12.0)
    if step is None and ratio is not None:
        step = int(round(1200.0 * math.log2(ratio) / 100.0))
    return step, cents, ratio


def _candidate_reference(fixture, directory):
    name = os.path.basename(fixture)
    for suffix in (".ref.f32", ".ref.wav", ".f32", ".wav"):
        path = os.path.join(directory, name + suffix)
        if os.path.isfile(path):
            return path
    for sub, suffix in (("f32", ".f32"), ("wav", ".wav")):
        path = os.path.join(REPO_ROOT, "fixtures", sub, name + suffix)
        if os.path.isfile(path):
            return path
    return None


def _candidate_render(fixture, step, directory):
    name = os.path.basename(fixture)
    stems = [
        "%s.%+03d" % (name, step),
        "%s.%+d" % (name, step),
        "%s_%+d" % (name, step),
        "%s__pitch%+d" % (name, step),
        "%s.pitch%+d" % (name, step),
        "%s-%+d" % (name, step),
    ]
    for stem in stems:
        for suffix in (".f32", ".wav"):
            path = os.path.join(directory, stem + suffix)
            if os.path.isfile(path):
                return path
    for pattern in ("%s.*%+03d.*" % (name, step),
                    "%s.*%+d.*" % (name, step)):
        for path in sorted(glob.glob(os.path.join(directory, pattern))):
            base = os.path.basename(path)
            if ".ref." in base or not base.lower().endswith((".f32", ".wav")):
                continue
            return path
    return None


def read_render_manifest(path):
    """Read P-02's ``manifest.tsv`` (or a compatible TSV) into entries."""
    with open(path, "r", newline="", encoding="utf-8") as handle:
        reader = csv.DictReader(handle, delimiter="\t")
        if not reader.fieldnames:
            raise ValueError("manifest has no header row")
        fields = {name.strip().lower() for name in reader.fieldnames if name}
        if not fields & {"fixture", "clip", "name"}:
            raise ValueError(
                "manifest needs a fixture/clip/name column; found %s"
                % ", ".join(sorted(fields)))
        entries = []
        for raw in reader:
            row = _normalise_row(raw)
            if not any(row.values()):
                continue
            fixture = row.get("fixture") or row.get("clip") or row.get("name")
            if not fixture:
                raise ValueError("manifest row has no fixture/clip/name: %r"
                                 % row)
            step, cents, ratio = _manifest_expected(row)
            entries.append({
                "fixture": fixture,
                "step": step,
                "expected_cents": cents,
                "expected_ratio": ratio,
                "file": (row.get("file") or row.get("render")
                         or row.get("test") or None),
                "ref": row.get("ref") or row.get("reference") or None,
            })
        return entries


def infer_render_entries(directory):
    """Infer ``<name>.<+NN>.f32`` renders when no manifest is present."""
    entries = []
    for name in sorted(os.listdir(directory)):
        path = os.path.join(directory, name)
        if not os.path.isfile(path):
            continue
        if not name.lower().endswith((".f32", ".wav")):
            continue
        if ".ref." in name:
            continue
        stem = os.path.splitext(name)[0]
        match = _RENDER_NAME_RE.match(stem)
        if not match:
            continue
        step = int(match.group("step"))
        entries.append({
            "fixture": match.group("base"),
            "step": step,
            "expected_cents": 100.0 * step,
            "expected_ratio": 2.0 ** (step / 12.0),
            "file": name,
            "ref": None,
        })
    return entries


def _resolve_path(directory, value):
    if not value:
        return None
    return value if os.path.isabs(value) else os.path.join(directory, value)


def score_render(entry, directory, allow_length_delta):
    """Score one render cell.  Returns a JSON-ready dict."""
    fixture = entry["fixture"]
    ref_path = _resolve_path(directory, entry.get("ref"))
    if ref_path is None:
        ref_path = _candidate_reference(fixture, directory)
    test_path = _resolve_path(directory, entry.get("file"))
    if test_path is None and entry["step"] is not None:
        test_path = _candidate_render(fixture, entry["step"], directory)

    cell = {
        "name": os.path.basename(test_path) if test_path else None,
        "fixture": fixture,
        "step": entry["step"],
        "ref": display_path(ref_path) if ref_path else None,
        "test": display_path(test_path) if test_path else None,
        "expectedCents": entry["expected_cents"],
        "expectedRatio": entry["expected_ratio"],
        "expectedHz": None,
        "samples": {},
        "pitch": None,
        "null": None,
        "checks": [],
        "status": "MISSING",
        "naReason": None,
        "ok": False,
    }
    missing = []
    if ref_path is None or not os.path.isfile(ref_path):
        missing.append("reference not found for %s" % fixture)
    if test_path is None or not os.path.isfile(test_path):
        missing.append("render not found for %s @ %s st"
                       % (fixture, entry["step"]))
    if missing:
        cell["naReason"] = "; ".join(missing)
        return cell

    try:
        ref_x = load_audio(ref_path)
        test_x = load_audio(test_path)
    except (AudioFormatError, OSError) as exc:
        cell["naReason"] = "cannot read input: %s" % exc
        return cell
    ref_level = level_stats(ref_x)
    test_level = level_stats(test_x)
    source_median = f0_stats(ref_x)["f0_median_hz"]
    pitch = paired_f0_ratio(ref_x, test_x,
                            expected_ratio=entry["expected_ratio"])
    applicable, reason = pitch_applicability(pitch, source_median)
    length_delta = test_level["samples"] - ref_level["samples"]
    length_ok = abs(length_delta) <= allow_length_delta
    null = null_test(ref_x, test_x)
    finite = bool(ref_level["finite"] and test_level["finite"]
                  and null["finite"])
    gate = pitch_gate(pitch)

    checks = [
        check("all samples finite", "no NaN/Inf",
              "yes" if finite else "no", finite),
        check("sample count",
              "equal" if allow_length_delta == 0
              else "allow +/-%d" % allow_length_delta,
              "%s vs %s (delta %s)"
              % (human_int(ref_level["samples"]),
                 human_int(test_level["samples"]), fsgn(length_delta, 0)),
              length_ok),
        check("pitch (D17 gate)",
              "median <= %.0f c, p90 <= %.0f c, 0 octave"
              % (PITCH_TOLERANCE_CENTS, PITCH_P90_TOLERANCE_CENTS),
              (pitch_measured_text(pitch)
               + ("; expected %s c" % fsgn(pitch["expected_cents"], 1)
                  if pitch["expected_cents"] is not None else "")
               + ("" if gate["passed"]
                  else "; failed: %s" % ", ".join(gate["failures"]))),
              applicable and gate["passed"],
              applicable=applicable, reason=reason),
    ]
    failed = [c["name"] for c in checks if c["applicable"] and not c["pass"]]
    if failed:
        status = "FAIL"
    elif not applicable:
        status = "N/A"
    else:
        status = "PASS"

    cell.update({
        "expectedHz": expected_output_hz(source_median,
                                          pitch["expected_cents"]),
        "samples": {
            "ref": ref_level["samples"],
            "test": test_level["samples"],
            "delta": length_delta,
            "allowLengthDelta": allow_length_delta,
        },
        "pitch": {
            **pitch_payload(pitch),
            "reliableBandHz": [PITCH_RELIABLE_MIN_HZ,
                               PITCH_RELIABLE_MAX_HZ],
            "naReason": reason,
        },
        "null": null,
        "checks": checks,
        "status": status,
        "naReason": reason,
        "failedClauses": list(gate["failures"]) if applicable else [],
        "ok": not failed,
    })
    return cell


def run_renders(args):
    directory = args.renders
    if not os.path.isdir(directory):
        sys.stderr.write("metrics.py: --renders %s is not a directory\n"
                         % directory)
        return 2

    manifest_path = args.manifest if isinstance(args.manifest, str) else None
    auto_manifest = os.path.join(directory, "manifest.tsv")
    if manifest_path is None and os.path.isfile(auto_manifest):
        manifest_path = auto_manifest

    if manifest_path is not None:
        try:
            entries = read_render_manifest(manifest_path)
        except (OSError, ValueError) as exc:
            sys.stderr.write("metrics.py: cannot read render manifest %s: %s\n"
                             % (display_path(manifest_path), exc))
            return 2
    else:
        entries = infer_render_entries(directory)

    if not entries:
        sys.stderr.write(
            "metrics.py: no renders found in %s. Name them "
            "<fixture>.<semitone>.f32 (e.g. af_heart__friends-now.+04.f32) "
            "or pass --manifest TSV.\n" % display_path(directory))
        return 2

    cells = [score_render(entry, directory, args.allow_length_delta)
             for entry in entries]
    passed = sum(1 for c in cells if c["status"] == "PASS")
    failed = [c for c in cells if c["status"] == "FAIL"]
    na = [c for c in cells if c["status"] == "N/A"]
    missing = [c for c in cells if c["status"] == "MISSING"]
    errors = [abs(c["pitch"]["errorCents"]) for c in cells
              if c["pitch"] is not None
              and c["pitch"]["errorCents"] is not None
              and math.isfinite(c["pitch"]["errorCents"])
              and c["status"] != "N/A"]
    worst = max(errors) if errors else float("nan")
    worst_p90 = max([c["pitch"]["p90AbsErrorCents"] for c in cells
                     if c["pitch"] is not None
                     and c["status"] != "N/A"
                     and c["pitch"]["p90AbsErrorCents"] is not None
                     and math.isfinite(c["pitch"]["p90AbsErrorCents"])]
                    or [float("nan")])
    octave_outliers = sum(c["pitch"]["octaveOutliers"] for c in cells
                          if c["pitch"] is not None and c["status"] != "N/A")
    dropped = sum(c["pitch"]["droppedFrames"] for c in cells
                  if c["pitch"] is not None)
    clause_counts = {"median": 0, "p90": 0, "octave": 0}
    for cell in failed:
        for clause, ok_clause in cell["pitch"]["gate"].items():
            if clause in clause_counts and not ok_clause:
                clause_counts[clause] += 1
    per_fixture = {}
    for cell in cells:
        bucket = per_fixture.setdefault(cell["fixture"], {
            "cells": 0, "passed": 0, "failed": 0, "na": 0,
            "worstMedianAbsCents": None,
            "worstP90AbsCents": None,
            "worstErrorCents": None, "failedClauses": []})
        bucket["cells"] += 1
        bucket[{"PASS": "passed", "FAIL": "failed",
                "N/A": "na"}.get(cell["status"], "failed")] += 1
        if cell["pitch"] is None or cell["status"] == "N/A":
            continue
        pitch = cell["pitch"]
        if (pitch["errorCents"] is not None
                and math.isfinite(pitch["errorCents"])):
            value = abs(pitch["errorCents"])
            if (bucket["worstMedianAbsCents"] is None
                    or value > bucket["worstMedianAbsCents"]):
                bucket["worstMedianAbsCents"] = value
            bucket["worstErrorCents"] = bucket["worstMedianAbsCents"]
        if (pitch["p90AbsErrorCents"] is not None
                and math.isfinite(pitch["p90AbsErrorCents"])):
            value = pitch["p90AbsErrorCents"]
            if (bucket["worstP90AbsCents"] is None
                    or value > bucket["worstP90AbsCents"]):
                bucket["worstP90AbsCents"] = value
        for clause in cell.get("failedClauses") or []:
            if clause not in bucket["failedClauses"]:
                bucket["failedClauses"].append(clause)
    ok = not failed and not missing

    payload = {
        "schemaVersion": 2,
        "task": "P-07d",
        "mode": "renders",
        "sampleRate": SAMPLE_RATE,
        "directory": display_path(directory),
        "manifest": display_path(manifest_path) if manifest_path else None,
        "comparison": "renders in %s vs reference clips; paired per-frame F0 by "
                      "frame index, same 40 ms grid, frames voiced in both, "
                      "unreliable frames dropped before the statistics"
                      % display_path(directory),
        "allowLengthDelta": args.allow_length_delta,
        "gate": {
            "medianLimitCents": PITCH_TOLERANCE_CENTS,
            "p90LimitCents": PITCH_P90_TOLERANCE_CENTS,
            "octaveLimit": 0,
            "octaveOutlierCents": PITCH_OCTAVE_OUTLIER_CENTS,
        },
        "cells": cells,
        "summary": {
            "cells": len(cells),
            "passed": passed,
            "failed": len(failed),
            "na": len(na),
            "missing": len(missing),
            "worstAbsErrorCents": worst,
            "worstMedianAbsCents": worst,
            "worstP90AbsCents": worst_p90,
            "octaveOutliers": octave_outliers,
            "droppedFrames": dropped,
            "failuresByClause": clause_counts,
            "perFixture": per_fixture,
        },
        "ok": ok,
    }

    if args.json:
        print(json.dumps(json_safe(payload), indent=2, allow_nan=False))
    else:
        print("Render batch: %s" % display_path(directory))
        if manifest_path:
            print("manifest: %s" % display_path(manifest_path))
        print("gate: median |error| <= %.0f c AND p90 |error| <= %.0f c AND "
              "octave outliers = 0 (D17)" % (PITCH_TOLERANCE_CENTS,
                                             PITCH_P90_TOLERANCE_CENTS))
        print()
        rows = []
        for cell in cells:
            pitch = cell["pitch"] or {}
            rows.append([
                cell["fixture"],
                "%+d" % cell["step"] if cell["step"] is not None else "-",
                fnum(cell["expectedHz"], 1),
                str(pitch.get("pairedFrames", "-")),
                fsgn(pitch.get("medianCents"), 2) if pitch else "-",
                fsgn(pitch.get("errorCents"), 2) if pitch else "-",
                fnum(pitch.get("p90AbsErrorCents"), 1) if pitch else "-",
                str(pitch.get("octaveOutliers", "-")) if pitch else "-",
                str(pitch.get("droppedFrames", "-")) if pitch else "-",
                cell["status"],
                "; ".join(cell.get("failedClauses") or [])
                or cell["naReason"] or "",
            ])
        print(print_table(
            ["cell", "st", "exp Hz", "paired", "median c", "err c",
             "p90 |err|", "oct", "drop", "status", "note"],
            rows, ["l", "r", "r", "r", "r", "r", "r", "r", "r", "l", "l"]))
        print()
        fixture_rows = []
        for name in sorted(per_fixture):
            b = per_fixture[name]
            fixture_rows.append([
                name, str(b["cells"]), str(b["passed"]), str(b["failed"]),
                str(b["na"]), fnum(b["worstMedianAbsCents"], 2),
                fnum(b["worstP90AbsCents"], 2),
                "; ".join(b["failedClauses"]),
            ])
        print(print_table(
            ["fixture", "cells", "pass", "fail", "N/A", "worst |median| c",
             "worst p90 c", "failed clauses"],
            fixture_rows, ["l", "r", "r", "r", "r", "r", "r", "l"]))
        print()
        print("%d cells - %d passed - %d failed - %d N/A - %d missing - "
              "worst |median| %s c, worst p90 %s c, %d octave outliers"
              % (len(cells), passed, len(failed), len(na), len(missing),
                 fnum(worst, 2), fnum(worst_p90, 2), octave_outliers))
        print("frames dropped as unreliable before the statistics: %d"
              % dropped)
        if failed:
            print("failures by clause: %s"
                  % ", ".join("%s %d" % (k, v)
                              for k, v in clause_counts.items() if v))
        print("Result: %s" % ("PASS" if ok else "FAIL"))

    return 0 if ok or not args.check else 1


# ------------------------------------------------------------ self-test mode

# P-07d pairing control.  The render is the reference with *different* voiced
# frames removed (a longer silent block on the reference side), so the two
# voiced sets differ and any rank-pairing ruler must slip.  The F0 contour is a
# glide, because a constant-F0 control is mathematically blind to rank pairing:
# every pair would read 0 cents however it was matched.  Outside the spliced
# silences the two signals are sample-identical, so index pairing must read
# exactly 0.00 cents -- the control does not depend on the tracker's absolute
# accuracy at all.  The tuple is (first hop, silent hops); a 40 ms window spans
# two hops, so ``h`` silent hops leave ``h+1`` whole frames silent.
PAIRING_CONTROL_SECONDS = 1.2
PAIRING_CONTROL_F0_START_HZ = 150.0
PAIRING_CONTROL_F0_END_HZ = 260.0
PAIRING_CONTROL_REF_SILENCE = (8, 3)     # 4 whole frames, reference side
PAIRING_CONTROL_TEST_SILENCE = (26, 1)   # 2 whole frames, render side


def pairing_mismatch_control(sample_rate=SAMPLE_RATE):
    """Exact-shift control whose render has extra and missing voiced frames.

    Returns a dict with the index-paired measurement, the voiced counts on each
    side (which must differ, or the control is not exercising anything), and a
    **negative control**: the number the old rank-pairing implementation would
    report on the same two signals.  That number must exceed the tolerance, or
    the control has stopped being falsifiable and fails.
    """
    samples = int(round(PAIRING_CONTROL_SECONDS * sample_rate))
    hop = max(1, int(round(PITCH_HOP_S * sample_rate)))
    glide = synthesize_glide(PAIRING_CONTROL_F0_START_HZ,
                             PAIRING_CONTROL_F0_END_HZ, samples, sample_rate)
    ref = glide.copy()
    test = glide.copy()
    for signal, (first_hop, hops) in ((ref, PAIRING_CONTROL_REF_SILENCE),
                                      (test, PAIRING_CONTROL_TEST_SILENCE)):
        # A 40 ms window spans two hops, so zeroing [a, a+h+2) hops leaves
        # exactly h+1 whole frames silent and keeps the rest sample-identical.
        start = first_hop * hop
        stop = min(samples, (first_hop + hops + 2) * hop)
        signal[start:stop] = 0.0

    pitch = paired_f0_ratio(ref, test, expected_ratio=1.0,
                            sample_rate=sample_rate)

    # Negative control: the *discarded* rank-pairing rule, reproduced here and
    # only here so the control can prove it would have caught it.
    tracks = [f0_track(x, sample_rate, PITCH_F0_MIN, PITCH_F0_MAX,
                       PITCH_WIN_S, PITCH_HOP_S, spectral_check=True,
                       rms_floor_dbfs=FRAME_RMS_FLOOR_DBFS,
                       return_agreement=True)
              for x in (ref, test)]
    ref_f0, ref_agree = tracks[0][0], tracks[0][3]
    test_f0, test_agree = tracks[1][0], tracks[1][3]
    rank_pairs = min(len(ref_f0), len(test_f0))
    rank_cents = float("nan")
    if rank_pairs:
        both = (np.isfinite(ref_f0[:rank_pairs])
                & np.isfinite(test_f0[:rank_pairs])
                & ref_agree[:rank_pairs] & test_agree[:rank_pairs])
        if both.any():
            rank_cents = float(np.median(
                1200.0 * np.log2(test_f0[:rank_pairs][both]
                                 / ref_f0[:rank_pairs][both])))

    voiced_ref = pitch["ref_voiced_frames"]
    voiced_test = pitch["test_voiced_frames"]
    failures = []
    if abs(pitch["error_cents"]) > PITCH_SELF_TEST_TOLERANCE_CENTS:
        failures.append(
            "index-paired error %s c exceeds %.0f c"
            % (fsgn(pitch["error_cents"], 3),
               PITCH_SELF_TEST_TOLERANCE_CENTS))
    if pitch["paired_frames"] < PITCH_MIN_PAIRED_FRAMES:
        failures.append("only %d paired frames (need %d)"
                        % (pitch["paired_frames"],
                           PITCH_MIN_PAIRED_FRAMES))
    if abs(voiced_test - voiced_ref) < 2:
        failures.append(
            "voiced counts are %d/%d: the control did not create a mismatch "
            "and cannot see rank pairing" % (voiced_ref, voiced_test))
    if not (math.isfinite(rank_cents)
            and abs(rank_cents) > PITCH_SELF_TEST_TOLERANCE_CENTS):
        failures.append(
            "the rank-pairing negative control reads %s c, so the control is "
            "no longer falsifiable" % fsgn(rank_cents, 2))
    return {
        "description": "reference and render are the same pitch glide; a "
                       "different number of whole voiced frames is silenced on "
                       "each side (counts reported below), so the voiced sets "
                       "differ. Samples are identical outside the silences, so "
                       "index pairing must read 0 cents.",
        "seconds": PAIRING_CONTROL_SECONDS,
        "f0StartHz": PAIRING_CONTROL_F0_START_HZ,
        "f0EndHz": PAIRING_CONTROL_F0_END_HZ,
        "refSilence": {"firstHop": PAIRING_CONTROL_REF_SILENCE[0],
                       "hops": PAIRING_CONTROL_REF_SILENCE[1]},
        "testSilence": {"firstHop": PAIRING_CONTROL_TEST_SILENCE[0],
                        "hops": PAIRING_CONTROL_TEST_SILENCE[1]},
        "refVoicedFrames": voiced_ref,
        "testVoicedFrames": voiced_test,
        "voicedCountDelta": voiced_test - voiced_ref,
        "pairedFrames": pitch["paired_frames"],
        "droppedFrames": pitch["dropped_frames"],
        "errorCents": pitch["error_cents"],
        "p90AbsErrorCents": pitch["p90_abs_error_cents"],
        "rankPairingCents": rank_cents,
        "toleranceCents": PITCH_SELF_TEST_TOLERANCE_CENTS,
        "passed": not failures,
        "failures": failures,
    }


def run_self_test(args):
    manifest_path = args.manifest_path or os.path.join(
        REPO_ROOT, "fixtures", "manifest.json")
    if not os.path.isfile(manifest_path):
        sys.stderr.write(
            "metrics.py: %s not found. Generate the fixtures first:\n"
            "  python3 scripts/make_fixtures.py\n" % display_path(manifest_path))
        return 2
    with open(manifest_path, "r", encoding="utf-8") as handle:
        manifest = json.load(handle)
    base_dir = os.path.dirname(os.path.abspath(manifest_path))

    cells = []
    for entry in manifest.get("clips", []):
        files = entry.get("files", {})
        if "f32" not in files:
            continue
        name = entry.get("name")
        ref_x = load_audio(os.path.join(base_dir, files["f32"]["path"]))
        base = f0_stats(ref_x)["f0_median_hz"]
        if not math.isfinite(base) or base <= 0:
            continue
        harmonic = synthesize_harmonic(base, len(ref_x))

        for step in SELF_TEST_STEPS:
            ratio = resample_ratio_value(2.0 ** (step / 12.0))
            expected_hz = base * ratio
            in_band = (PITCH_RELIABLE_MIN_HZ <= expected_hz
                       <= PITCH_RELIABLE_MAX_HZ)

            control = resample_ratio(harmonic, ratio)
            control = control[:len(harmonic)]
            if len(control) < len(harmonic):
                control = np.pad(control, (0, len(harmonic) - len(control)))
            same = paired_f0_ratio(harmonic, control, expected_ratio=ratio,
                                   spectral_check=True)
            same_ok, same_reason = pitch_applicability(same, base)
            same_pass = (same_ok
                         and abs(same["error_cents"])
                         <= PITCH_SELF_TEST_TOLERANCE_CENTS)

            control_speech = resample_ratio(ref_x, ratio)
            content = paired_f0_ratio(
                ref_x, control_speech, expected_ratio=ratio,
                time_scale=ratio, win_s=SELF_TEST_WIN_S,
                hop_s=SELF_TEST_HOP_S, spectral_check=False)
            content_ok, content_reason = pitch_applicability(content, base)
            content_pass = (content_ok
                            and abs(content["error_cents"])
                            <= PITCH_SELF_TEST_TOLERANCE_CENTS)

            legacy_test = f0_stats(control_speech)["f0_median_hz"]
            legacy_cents = cents_between(base, legacy_test)
            legacy_error = (legacy_cents - content["expected_cents"]
                            if math.isfinite(legacy_cents) else float("nan"))

            applicable = [ok for ok in (same_ok, content_ok) if ok]
            failures = []
            if same_ok and not same_pass:
                failures.append("same-time")
            if content_ok and not content_pass:
                failures.append("resample")
            if failures:
                status = "FAIL"
            elif not applicable or not in_band:
                status = "N/A"
            else:
                status = "PASS"

            reasons = []
            if not in_band:
                reasons.append("expected output F0 %.1f Hz outside reliable "
                               "band %.0f-%.0f Hz"
                               % (expected_hz, PITCH_RELIABLE_MIN_HZ,
                                  PITCH_RELIABLE_MAX_HZ))
            else:
                for reason in (same_reason, content_reason):
                    if reason and reason not in reasons:
                        reasons.append(reason)

            cells.append({
                "fixture": name,
                "step": step,
                "expectedHz": expected_hz,
                "inBand": in_band,
                "sameTime": {
                    "applicable": same_ok,
                    "passed": same_pass,
                    "errorCents": same["error_cents"],
                    "p90AbsErrorCents": same["p90_abs_error_cents"],
                    "pairedFrames": same["paired_frames"],
                    "naReason": same_reason,
                },
                "resample": {
                    "applicable": content_ok,
                    "passed": content_pass,
                    "errorCents": content["error_cents"],
                    "p90AbsErrorCents": content["p90_abs_error_cents"],
                    "pairedFrames": content["paired_frames"],
                    "naReason": content_reason,
                    "windowSeconds": SELF_TEST_WIN_S,
                },
                "legacyMedianErrorCents": legacy_error,
                "status": status,
                "reason": "; ".join(reasons) or None,
            })

    if not cells:
        sys.stderr.write("metrics.py: %s has no usable fixtures\n"
                         % display_path(manifest_path))
        return 2

    in_band = [c for c in cells if c["inBand"]]
    failed = [c for c in cells if c["status"] == "FAIL"]
    na = [c for c in cells if c["status"] == "N/A"]
    passed = [c for c in cells if c["status"] == "PASS"]
    same_errors = [abs(c["sameTime"]["errorCents"]) for c in in_band
                   if c["sameTime"]["applicable"]
                   and math.isfinite(c["sameTime"]["errorCents"])]
    content_errors = [abs(c["resample"]["errorCents"]) for c in in_band
                      if c["resample"]["applicable"]
                      and math.isfinite(c["resample"]["errorCents"])]
    legacy_errors = [abs(c["legacyMedianErrorCents"]) for c in in_band
                     if math.isfinite(c["legacyMedianErrorCents"])]
    worst_same = max(same_errors) if same_errors else float("nan")
    worst_content = max(content_errors) if content_errors else float("nan")
    worst_legacy = max(legacy_errors) if legacy_errors else float("nan")
    pairing_control = pairing_mismatch_control()
    ok = not failed and pairing_control["passed"]

    payload = {
        "schemaVersion": 2,
        "task": "P-07d",
        "mode": "self-test",
        "sampleRate": SAMPLE_RATE,
        "manifest": display_path(manifest_path),
        "controls": {
            "sameTime": {
                "description": "stationary harmonic at each fixture's median "
                               "F0, resampled with scipy.signal.resample_poly; "
                               "measured on the normal same-time 40 ms grid",
                "toleranceCents": PITCH_SELF_TEST_TOLERANCE_CENTS,
                "windowSeconds": PITCH_WIN_S,
                "hopSeconds": PITCH_HOP_S,
            },
            "resample": {
                "description": "the fixture itself resampled with "
                               "scipy.signal.resample_poly; measured on the "
                               "resampler's own time base with every tracker "
                               "parameter scaled by the ratio",
                "toleranceCents": PITCH_SELF_TEST_TOLERANCE_CENTS,
                "windowSeconds": SELF_TEST_WIN_S,
                "hopSeconds": SELF_TEST_HOP_S,
            },
            "pairingMismatch": pairing_control,
            "reliableBandHz": [PITCH_RELIABLE_MIN_HZ, PITCH_RELIABLE_MAX_HZ],
            "steps": SELF_TEST_STEPS,
        },
        "cells": cells,
        "summary": {
            "cells": len(cells),
            "inBandCells": len(in_band),
            "passed": len(passed),
            "failed": len(failed),
            "na": len(na),
            "worstSameTimeCents": worst_same,
            "worstResampleCents": worst_content,
            "worstLegacyMedianCents": worst_legacy,
            "legacyCellsOver5Cents": sum(
                1 for c in in_band
                if math.isfinite(c["legacyMedianErrorCents"])
                and abs(c["legacyMedianErrorCents"]) > 5.0),
            "pairingControlPassed": pairing_control["passed"],
            "pairingControlErrorCents": pairing_control["errorCents"],
            "pairingControlRankCents": pairing_control["rankPairingCents"],
        },
        "ok": ok,
    }

    if args.json:
        print(json.dumps(json_safe(payload), indent=2, allow_nan=False))
    else:
        print("Pitch ruler self-test: %s" % display_path(manifest_path))
        print("Exact resampler controls at every semitone (-12..+12), "
              "%d fixtures." % len({c["fixture"] for c in cells}))
        print("same-time control: 40 ms window / 20 ms hop (the render path)")
        print("resample control:  %.0f ms window / %.0f ms hop on the "
              "resampler's own time base"
              % (SELF_TEST_WIN_S * 1000, SELF_TEST_HOP_S * 1000))
        print("gate: <= %.0f cent error in every in-band cell; N/A outside "
              "%.0f-%.0f Hz"
              % (PITCH_SELF_TEST_TOLERANCE_CENTS, PITCH_RELIABLE_MIN_HZ,
                 PITCH_RELIABLE_MAX_HZ))
        print()
        fixtures = sorted({c["fixture"] for c in cells})
        rows = []
        for fixture in fixtures:
            own = [c for c in cells if c["fixture"] == fixture]
            own_band = [c for c in own if c["inBand"]]
            own_same = [abs(c["sameTime"]["errorCents"]) for c in own_band
                        if c["sameTime"]["applicable"]
                        and math.isfinite(c["sameTime"]["errorCents"])]
            own_content = [abs(c["resample"]["errorCents"]) for c in own_band
                           if c["resample"]["applicable"]
                           and math.isfinite(c["resample"]["errorCents"])]
            own_legacy = [abs(c["legacyMedianErrorCents"]) for c in own_band
                          if math.isfinite(c["legacyMedianErrorCents"])]
            rows.append([
                fixture,
                "%d/%d" % (len(own_band), len(own)),
                fnum(max(own_same) if own_same else float("nan"), 2),
                fnum(max(own_content) if own_content else float("nan"), 2),
                fnum(max(own_legacy) if own_legacy else float("nan"), 1),
                str(sum(1 for c in own if c["status"] == "N/A")),
                str(sum(1 for c in own if c["status"] == "FAIL")),
            ])
        print(print_table(
            ["fixture", "in-band", "same max c", "resample max c",
             "legacy max c", "N/A", "FAIL"],
            rows, ["l", "r", "r", "r", "r", "r", "r"]))
        print()
        worst = sorted(
            (c for c in in_band if c["resample"]["applicable"]),
            key=lambda c: abs(c["resample"]["errorCents"]),
            reverse=True)[:3]
        for cell in worst:
            print("worst resample cell: %s %+d st  expected %.1f Hz  "
                  "paired %+.3f c  legacy %+.1f c"
                  % (cell["fixture"], cell["step"], cell["expectedHz"],
                     cell["resample"]["errorCents"],
                     cell["legacyMedianErrorCents"]))
        print()
        print("%d cells - %d in-band - %d passed - %d failed - %d N/A"
              % (len(cells), len(in_band), len(passed), len(failed), len(na)))
        print("worst same-time %s c - worst resample %s c - worst legacy "
              "median %s c (%d legacy cells > 5 c)"
              % (fnum(worst_same, 2), fnum(worst_content, 2),
                 fnum(worst_legacy, 1),
                 payload["summary"]["legacyCellsOver5Cents"]))
        print()
        print("pairing-mismatch control (P-07d): reference and render are the "
              "same %.1f-%.0f Hz glide"
              % (pairing_control["f0StartHz"], pairing_control["f0EndHz"]))
        print("  voiced frames ref %d / render %d (delta %+d) - index-paired "
              "error %s c - %d paired, %d dropped"
              % (pairing_control["refVoicedFrames"],
                 pairing_control["testVoicedFrames"],
                 pairing_control["voicedCountDelta"],
                 fsgn(pairing_control["errorCents"], 3),
                 pairing_control["pairedFrames"],
                 pairing_control["droppedFrames"]))
        print("  negative control (rank pairing on the same signals): %s c - "
              "must exceed %.0f c or the control proves nothing"
              % (fsgn(pairing_control["rankPairingCents"], 2),
                 PITCH_SELF_TEST_TOLERANCE_CENTS))
        for failure in pairing_control["failures"]:
            print("  FAIL: %s" % failure)
        print("Result: %s" % ("PASS" if ok else "FAIL"))

    return 0 if ok else 1


# --------------------------------------------------- formant self-test mode

def _formant_triple(values, nd=0):
    """``700/1220/2600``, or ``unresolved`` when any value is missing."""
    values = list(values)
    usable = [v for v in values if v is not None and math.isfinite(v)]
    if len(usable) != len(values):
        return "unresolved"
    return "/".join("%.*f" % (nd, v) for v in values)


def _error_triple(errors, nd=1):
    if not all(e is not None and math.isfinite(e) for e in errors):
        return "n/a"
    return "/".join(fsgn(e, nd) for e in errors)


def run_formant_self_test(args):
    """Synthetic ground truth for the formant ruler (P-07c).

    Six F0 values spanning the corpus (90-300 Hz) times two vowel formant sets,
    synthesized from the same 6-parameter model the estimator fits, so the true
    F1/F2/F3 are known by construction and no estimator is in the loop when the
    expectation is written down.  The harmonic fit must recover every formant
    within 1%; the legacy LPC estimate is printed beside it, because the whole
    point of P-07c is that LPC is not merely noisier but *biased* -- most
    visibly at high F0 and on the 700/1220/2600 set.

    A second table feeds the estimator a synthetic 1.10x formant render through
    the same D1/ratio applicability logic that ``--formant-ratio`` uses, so the
    wiring is covered too, not just the fit.
    """
    samples = int(round(FORMANT_SELF_TEST_SECONDS * SAMPLE_RATE))
    expected_pct = 100.0 * (FORMANT_RATIO_SELF_TEST - 1.0)
    cells, ratio_cells = [], []

    for f0 in FORMANT_SELF_TEST_F0:
        for name, true_formants, bandwidths in FORMANT_SELF_TEST_VOWELS:
            signal = synthesize_vowel(f0, true_formants, bandwidths, samples)
            measured = harmonic_formants(signal)
            legacy = lpc_formants(signal)
            harmonic_errors = [
                percent_delta(true_formants[i - 1], measured["f%d_hz" % i])
                for i in (1, 2, 3)]
            lpc_errors = [
                percent_delta(true_formants[i - 1], legacy["f%d_hz" % i])
                for i in (1, 2, 3)]
            harmonic_ok = bool(
                measured["resolved"]
                and all(math.isfinite(e) and abs(e) <= FORMANT_FIT_TOLERANCE_PCT
                        for e in harmonic_errors))
            # A control that cannot fail is not a control.  Re-run the gate
            # against an expectation corrupted by 10%: it must reject, or the
            # tolerance has been widened past the point of the control.
            offset = 1.0 + FORMANT_SELF_TEST_NEGATIVE_PCT / 100.0
            corrupted_errors = [
                percent_delta(true_formants[i - 1] * offset,
                              measured["f%d_hz" % i])
                for i in (1, 2, 3)]
            negative_detected = all(
                math.isfinite(e) and abs(e) > FORMANT_FIT_TOLERANCE_PCT
                for e in corrupted_errors)
            reasons = []
            if not measured["resolved"]:
                reasons.append(measured["na_reason"] or "not resolvable")
            else:
                for i, error in enumerate(harmonic_errors):
                    if not (math.isfinite(error)
                            and abs(error) <= FORMANT_FIT_TOLERANCE_PCT):
                        reasons.append("F%d %+.2f%%" % (i + 1, error))
            if not negative_detected:
                reasons.append("a %.0f%% wrong expectation was not rejected"
                               % FORMANT_SELF_TEST_NEGATIVE_PCT)
            cells.append({
                "f0Hz": f0,
                "vowel": name,
                "trueFormantsHz": list(true_formants),
                "bandwidthsHz": list(bandwidths),
                "harmonic": {
                    "formantsHz": [measured["f%d_hz" % i] for i in (1, 2, 3)],
                    "errorsPercent": harmonic_errors,
                    "resolved": measured["resolved"],
                    "framesUsed": measured["frames_used"],
                    "residualDb": measured["residual_db_median"],
                    "naReason": measured["na_reason"],
                },
                "lpcLegacy": {
                    "formantsHz": [legacy["f%d_hz" % i] for i in (1, 2, 3)],
                    "errorsPercent": lpc_errors,
                    "framesUsed": legacy["frames_used"],
                },
                "negativeControlDetected": bool(negative_detected),
                "status": ("PASS" if (harmonic_ok and negative_detected)
                           else "FAIL"),
                "reason": "; ".join(reasons) or None,
            })

            target_formants = [f * FORMANT_RATIO_SELF_TEST
                               for f in true_formants]
            target = synthesize_vowel(f0, target_formants, bandwidths, samples)
            target_measured = harmonic_formants(target)
            applicable, reason = formant_ratio_applicability(
                FORMANT_RATIO_SELF_TEST, 0.0, measured, target_measured)
            deltas = [
                percent_delta(measured["f%d_hz" % i],
                              target_measured["f%d_hz" % i])
                for i in (1, 2, 3)]
            ratio_ok = bool(
                applicable
                and all(math.isfinite(d)
                        and abs(d - expected_pct) <= FORMANT_FIT_TOLERANCE_PCT
                        for d in deltas))
            ratio_cells.append({
                "f0Hz": f0,
                "vowel": name,
                "expectedRatio": FORMANT_RATIO_SELF_TEST,
                "expectedDeltaPercent": expected_pct,
                "deltasPercent": deltas,
                "applicable": applicable,
                "naReason": reason,
                "status": "PASS" if ratio_ok else "FAIL",
            })

    failed = [c for c in cells if c["status"] == "FAIL"]
    ratio_failed = [c for c in ratio_cells if c["status"] == "FAIL"]
    passed = len(cells) - len(failed)
    harmonic_worst = max(
        (abs(e) for c in cells for e in c["harmonic"]["errorsPercent"]
         if math.isfinite(e)), default=float("nan"))
    lpc_worst = max(
        (abs(e) for c in cells for e in c["lpcLegacy"]["errorsPercent"]
         if math.isfinite(e)), default=float("nan"))
    lpc_over = sum(
        1 for c in cells
        if any(math.isfinite(e) and abs(e) > FORMANT_FIT_TOLERANCE_PCT
               for e in c["lpcLegacy"]["errorsPercent"]))
    negative_detected = sum(
        1 for c in cells if c["negativeControlDetected"])
    ratio_worst = max(
        (abs(d - expected_pct) for c in ratio_cells
         for d in c["deltasPercent"] if math.isfinite(d)),
        default=float("nan"))
    ok = not failed and not ratio_failed

    payload = {
        "schemaVersion": 2,
        "task": "P-07c",
        "mode": "formant-self-test",
        "sampleRate": SAMPLE_RATE,
        "controls": {
            "description": "synthetic vowels built from the estimator's own "
                           "generative model; true formants known by "
                           "construction (P-03 testutil::vowel / "
                           "audio_metrics.synthesize_vowel)",
            "tolerancePercent": FORMANT_FIT_TOLERANCE_PCT,
            "f0Hz": list(FORMANT_SELF_TEST_F0),
            "vowels": [{"name": name, "formantsHz": list(formants),
                        "bandwidthsHz": list(bandwidths)}
                       for name, formants, bandwidths in FORMANT_SELF_TEST_VOWELS],
            "seconds": FORMANT_SELF_TEST_SECONDS,
            "maxResidualDb": FORMANT_FIT_MAX_RESIDUAL_DB,
            "ratioControl": FORMANT_RATIO_SELF_TEST,
        },
        "cells": cells,
        "ratioCells": ratio_cells,
        "summary": {
            "cells": len(cells),
            "passed": passed,
            "failed": len(failed),
            "worstHarmonicPercent": harmonic_worst,
            "worstLpcPercent": lpc_worst,
            "lpcCellsOverTolerance": lpc_over,
            "negativeControlDetected": negative_detected,
            "negativeControlPercent": FORMANT_SELF_TEST_NEGATIVE_PCT,
            "ratioCells": len(ratio_cells),
            "ratioPassed": len(ratio_cells) - len(ratio_failed),
            "ratioFailed": len(ratio_failed),
            "worstRatioDeviationPercent": ratio_worst,
        },
        "ok": ok,
    }

    if args.json:
        print(json.dumps(json_safe(payload), indent=2, allow_nan=False))
    else:
        print("Harmonic-model formant ruler self-test (P-07c)")
        print("%d cells: %d F0 values x %d vowel sets, %.1f s synthetic "
              "vowels with formants known by construction."
              % (len(cells), len(FORMANT_SELF_TEST_F0),
                 len(FORMANT_SELF_TEST_VOWELS), FORMANT_SELF_TEST_SECONDS))
        print("gate: harmonic fit within %.2f%% on F1/F2/F3; the LPC column is "
              "the legacy estimator, printed for comparison, not gated."
              % FORMANT_FIT_TOLERANCE_PCT)
        print()
        rows = []
        for cell in cells:
            lpc = cell["lpcLegacy"]
            harmonic = cell["harmonic"]
            rows.append([
                "%d" % cell["f0Hz"],
                cell["vowel"],
                _formant_triple(cell["trueFormantsHz"]),
                _formant_triple(lpc["formantsHz"]),
                _error_triple(lpc["errorsPercent"], 1),
                _formant_triple(harmonic["formantsHz"]),
                _error_triple(harmonic["errorsPercent"], 2),
                fnum(harmonic["residualDb"], 3),
                cell["status"],
            ])
        print(print_table(
            ["F0", "vowel", "true F1/F2/F3", "LPC F1/F2/F3", "LPC err %",
             "harmonic F1/F2/F3", "harm err %", "res dB", "check"],
            rows, ["r", "l", "r", "r", "r", "r", "r", "r", "l"]))

        print()
        print("Formant-ratio control: source vs a synthetic %.2fx target, "
              "judged through the same D1 path as --formant-ratio"
              % FORMANT_RATIO_SELF_TEST)
        ratio_rows = []
        for cell in ratio_cells:
            ratio_rows.append([
                "%d" % cell["f0Hz"],
                cell["vowel"],
                _error_triple(cell["deltasPercent"]),
                ("%.1f" % cell["expectedDeltaPercent"]),
                cell["status"] if cell["applicable"] else "N/A",
                cell["naReason"] or "",
            ])
        print(print_table(
            ["F0", "vowel", "measured F1/F2/F3 delta %", "expected %",
             "check", "note"],
            ratio_rows, ["r", "l", "r", "r", "l", "l"]))

        print()
        print("%d cells - %d passed - %d failed"
              % (len(cells), passed, len(failed)))
        print("worst harmonic %s%% (gate %.2f%%) - worst LPC %s%% - "
              "%d/%d LPC cells over %.2f%%"
              % (fnum(harmonic_worst, 3), FORMANT_FIT_TOLERANCE_PCT,
                 fnum(lpc_worst, 1), lpc_over, len(cells),
                 FORMANT_FIT_TOLERANCE_PCT))
        print("negative control: a %.0f%% wrong expectation is rejected in "
              "%d/%d cells" % (FORMANT_SELF_TEST_NEGATIVE_PCT,
                               negative_detected, len(cells)))
        print("%d ratio cells - %d passed - %d failed - worst deviation from "
              "expected delta %s pp"
              % (len(ratio_cells), len(ratio_cells) - len(ratio_failed),
                 len(ratio_failed), fnum(ratio_worst, 3)))
        for cell in failed:
            print("FAIL: F0 %d, %s - %s"
                  % (cell["f0Hz"], cell["vowel"], cell["reason"] or "?"))
        for cell in ratio_failed:
            print("FAIL: ratio F0 %d, %s - %s"
                  % (cell["f0Hz"], cell["vowel"],
                     cell["naReason"] or "delta outside tolerance"))
        print("Result: %s" % ("PASS" if ok else "FAIL"))

    return 0 if ok else 1


# ------------------------------------------------- formant calibration mode

# P-07e: the codec control's tolerance is the same 8% the project gates a
# formant ratio with.  The negative control offsets the *expectation* by 20%,
# twice the tolerance, so estimator noise (worst 2.5% on a graded coded cell)
# cannot make a deliberately wrong truth look acceptable.
FORMANT_CALIBRATION_CODEC_TOLERANCE_PCT = 8.0
FORMANT_CALIBRATION_CODEC_NEGATIVE_PCT = 20.0
FORMANT_CALIBRATION_CLEAN_NEGATIVE_PCT = 10.0
# The codec control must grade a majority of its cells, or the Q-01 path is not
# being exercised and "everything is uncalibrated" would be unfalsifiable.
FORMANT_CALIBRATION_MIN_GRADED_CODED = 8
FORMANT_CALIBRATION_CONTROL_SECONDS = 0.6


def _opus_32k_roundtrip(x, sample_rate=SAMPLE_RATE):
    """Encode -> decode ``x`` with the corpus codec settings.

    ``ffmpeg -c:a libopus -b:a 32k -ar 48000 -ac 1`` with 20 ms frames, i.e.
    the same settings as the vendored corpus (PLAN F-03).  Decoding also uses
    libopus so the round trip measures the codec, not the decoder.  Returns
    ``(decoded, detail)``; ``decoded`` is ``None`` when ffmpeg/libopus is not
    available, with ``detail`` naming the missing capability.
    """
    import shutil
    import subprocess
    import tempfile

    from scipy.io import wavfile

    if shutil.which("ffmpeg") is None:
        return None, "ffmpeg is not on PATH"
    encoders = subprocess.run(
        ["ffmpeg", "-hide_banner", "-encoders"],
        capture_output=True, text=True, check=False).stdout
    if "libopus" not in encoders:
        return None, "this ffmpeg has no libopus encoder"
    version = subprocess.run(
        ["ffmpeg", "-hide_banner", "-version"],
        capture_output=True, text=True, check=False).stdout.splitlines()
    directory = tempfile.mkdtemp(prefix="rasp-p07e-")
    wav = os.path.join(directory, "in.wav")
    webm = os.path.join(directory, "coded.webm")
    out = os.path.join(directory, "out.wav")
    try:
        wavfile.write(wav, sample_rate,
                      (np.clip(np.asarray(x, dtype=np.float64), -1.0, 1.0)
                       * 32767.0).astype(np.int16))
        subprocess.run(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", wav,
             "-c:a", "libopus", "-b:a", "32k", "-ar", str(sample_rate),
             "-ac", "1", "-frame_duration", "20", webm],
            check=True, capture_output=True)
        subprocess.run(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
             "-c:a", "libopus", "-i", webm, "-ar", str(sample_rate),
             "-ac", "1", "-f", "wav", out],
            check=True, capture_output=True)
        rate, decoded = wavfile.read(out)
        if rate != sample_rate:
            return None, "ffmpeg returned %d Hz, expected %d" % (rate,
                                                                 sample_rate)
        return decoded.astype(np.float64) / 32768.0, {
            "encoder": "libopus", "bitrate": "32k", "sampleRate": sample_rate,
            "channels": 1, "frameDurationMs": 20,
            "ffmpeg": version[0] if version else "unknown",
        }
    except (OSError, subprocess.SubprocessError) as exc:
        return None, "codec round trip failed: %s" % exc
    finally:
        shutil.rmtree(directory, ignore_errors=True)


def _calibration_control_cell(f0, true_formants, bandwidths, signal,
                              tolerance_pct, negative_pct):
    """One synthetic ground-truth cell for the calibration path."""
    measured = formant_calibration(signal)
    errors = [percent_delta(true_formants[i],
                            measured["formants"]["f%d" % (i + 1)]["hz"])
              for i in range(3)]
    offset = 1.0 + negative_pct / 100.0
    corrupted = [percent_delta(true_formants[i] * offset,
                               measured["formants"]["f%d" % (i + 1)]["hz"])
                 for i in range(3)]
    graded = measured["status"] == "graded"
    # The negative control only has to reject where the ruler claims a number.
    negative_rejected = (all(math.isfinite(e) and abs(e) > tolerance_pct
                             for e in corrupted) if graded else None)
    return {
        "f0Hz": float(f0),
        "trueFormantsHz": list(true_formants),
        "measuredHz": [measured["formants"]["f%d" % (i + 1)]["hz"]
                       for i in range(3)],
        "rangesHz": [measured["formants"]["f%d" % (i + 1)]["range_hz"]
                     for i in range(3)],
        "halfWidthPercent": [measured["formants"]["f%d" % (i + 1)]
                             ["half_width_percent"] for i in range(3)],
        "errorsPercent": errors,
        "residualDb": measured["residual_db_median"],
        "framesUsed": measured["frames_used"],
        "status": measured["status"],
        "reasons": measured["reasons"],
        "negativeControlRejected": negative_rejected,
        "negativeControlPercent": negative_pct,
        "passed": bool(graded
                       and all(math.isfinite(e) and abs(e) <= tolerance_pct
                               for e in errors)
                       and negative_rejected),
    }


def _run_calibration_controls():
    """Clean synthetic ground truth, then the same vowels through Opus@32k."""
    samples = int(round(FORMANT_CALIBRATION_CONTROL_SECONDS * SAMPLE_RATE))
    clean, coded, codec_detail, codec_error = [], [], None, None
    for f0 in FORMANT_SELF_TEST_F0:
        for name, formants, bandwidths in FORMANT_SELF_TEST_VOWELS:
            signal = synthesize_vowel(f0, formants, bandwidths, samples)
            clean.append(_calibration_control_cell(
                f0, formants, bandwidths, signal,
                FORMANT_FIT_TOLERANCE_PCT,
                FORMANT_CALIBRATION_CLEAN_NEGATIVE_PCT))
            if codec_error is None:
                decoded, detail = _opus_32k_roundtrip(signal)
                if decoded is None:
                    codec_error = detail
                    continue
                codec_detail = detail
            else:
                decoded, _ = _opus_32k_roundtrip(signal)
                if decoded is None:
                    codec_error = "codec round trip failed after success"
                    continue
            coded.append(_calibration_control_cell(
                f0, formants, bandwidths, decoded,
                FORMANT_CALIBRATION_CODEC_TOLERANCE_PCT,
                FORMANT_CALIBRATION_CODEC_NEGATIVE_PCT))

    clean_passed = [c for c in clean if c["passed"]]
    clean_ok = len(clean_passed) == len(clean)
    coded_graded = [c for c in coded if c["status"] == "graded"]
    coded_ok = (
        codec_error is None
        and len(coded_graded) >= FORMANT_CALIBRATION_MIN_GRADED_CODED
        and all(c["passed"] for c in coded_graded)
    )
    clean_residuals = [c["residualDb"] for c in clean
                       if math.isfinite(c["residualDb"])]
    coded_residuals = [c["residualDb"] for c in coded
                       if math.isfinite(c["residualDb"])]
    return {
        "clean": {
            "description": "synthetic vowels built from the estimator's own "
                           "generative model; formants known by construction",
            "cells": clean,
            "passed": len(clean_passed),
            "failed": len(clean) - len(clean_passed),
            "ok": clean_ok,
            "tolerancePercent": FORMANT_FIT_TOLERANCE_PCT,
            "residualMedianDb": (float(np.median(clean_residuals))
                                 if clean_residuals else float("nan")),
            "residualMaxDb": (float(max(clean_residuals))
                              if clean_residuals else float("nan")),
        },
        "coded": {
            "description": "the same vowels encoded to WebM with libopus at "
                           "32 kbps / 48 kHz / 20 ms frames, then decoded",
            "codec": codec_detail,
            "codecError": codec_error,
            "cells": coded,
            "graded": len(coded_graded),
            "requiredGraded": FORMANT_CALIBRATION_MIN_GRADED_CODED,
            "worstGradedErrorPercent": max(
                (abs(e) for c in coded_graded for e in c["errorsPercent"]
                 if math.isfinite(e)), default=float("nan")),
            "worstResidualDb": max(
                (c["residualDb"] for c in coded_graded
                 if math.isfinite(c["residualDb"])), default=float("nan")),
            "residualMedianDb": (float(np.median(coded_residuals))
                                 if coded_residuals else float("nan")),
            "residualMaxDb": (float(max(coded_residuals))
                              if coded_residuals else float("nan")),
            "ok": coded_ok,
            "tolerancePercent": FORMANT_CALIBRATION_CODEC_TOLERANCE_PCT,
        },
    }


def run_formant_calibration(args):
    """P-07e: can the formant ruler certify real 32 kbps speech?

    Runs the two controls that can falsify the answer -- clean synthetic ground
    truth and the same vowels through the corpus codec -- then measures every
    fixture with the calibration profile and applies the D20 publication gate.
    """
    manifest_path = args.manifest_path or os.path.join(
        REPO_ROOT, "fixtures", "manifest.json")
    if not os.path.isfile(manifest_path):
        sys.stderr.write(
            "metrics.py: %s not found. Generate the fixtures first:\n"
            "  python3 scripts/make_fixtures.py\n" % display_path(manifest_path))
        return 2
    with open(manifest_path, "r", encoding="utf-8") as handle:
        manifest = json.load(handle)
    base = os.path.dirname(os.path.abspath(manifest_path))

    controls = _run_calibration_controls()
    if controls["coded"]["codecError"]:
        sys.stderr.write(
            "metrics.py: the codec control could not run: %s\n"
            "This control is mandatory: without it the codec's own residual "
            "contribution is unknown and no formant may be certified.\n"
            % controls["coded"]["codecError"])
        return 2

    clips = []
    for entry in manifest.get("clips", []):
        files = entry.get("files", {})
        if "f32" not in files:
            continue
        x = load_audio(os.path.join(base, files["f32"]["path"]))
        measured = formant_calibration(x)
        clips.append({
            "name": entry.get("name"),
            "voice": entry.get("voice"),
            "language": entry.get("language"),
            "measured": measured,
        })

    graded = [c for c in clips if c["measured"]["status"] == "graded"]
    tier = {}
    for clip in clips:
        for key in ("f1", "f2", "f3"):
            formant = clip["measured"]["formants"][key]
            tier.setdefault(key, {"graded": 0, "uncalibrated": 0})
            tier[key][formant["status"]] += 1
    reasons = {}
    for clip in clips:
        for reason in clip["measured"]["reasons"]:
            if reason.startswith("median harmonic residual"):
                category = "harmonic residual above the calibration gate"
            elif "frame(s) pass the residual gate" in reason:
                category = "too few frames pass the gate"
            elif "degrees of freedom" in reason:
                category = "no residual degrees of freedom (high F0)"
            elif "ill-conditioned" in reason:
                category = "ill-conditioned fit"
            elif "analysis ceiling" in reason:
                category = "F3 at the analysis ceiling"
            elif "below the first harmonic" in reason:
                category = "F1 below the first harmonic"
            elif "publishable width" in reason:
                category = "%s interval wider than 8%%" % reason[0:2]
            else:
                category = reason.split(":")[0]
            reasons[category] = reasons.get(category, 0) + 1

    fixture_ok = all(c["measured"]["status"] == "uncalibrated" for c in clips) \
        if not controls["coded"]["ok"] else True
    ok = controls["clean"]["ok"] and controls["coded"]["ok"]

    publication = {
        "policy": "D20",
        "rule": "A formant number is publishable only when its calibration "
                "status is 'graded' at this voice; an uncalibrated formant is "
                "written as null with a reason, never substituted from LPC or "
                "from a point estimate inside a diagnostic range.",
        "f0": {
            "publish": True,
            "field": "f0Hz",
            "method": "yin-median-60-400Hz-10ms-hop-5-frame-smoothing",
            "note": "the corpus tracker; unchanged by P-07e",
        },
        "formants": {
            "publish": "per formant, only where status == 'graded'",
            "fields": {
                "value": "f1Hz / f2Hz / f3Hz",
                "range": "f1RangeHz / f2RangeHz / f3RangeHz (95% fit interval)",
                "method": "harmonic-fit-calibration-v1",
                "profile": "%.1f kHz band, %.0f ms frames / %.0f ms hop"
                           % (FORMANT_CALIBRATION_MAX_HZ / 1000.0,
                              FORMANT_FIT_WIN_S * 1000.0,
                              FORMANT_FIT_HOP_S * 1000.0),
            },
            "ungraded": {
                "status": "uncalibrated",
                "reason": "one of: residual above the %.1f dB calibration "
                          "gate; too few frames or harmonics; ill-conditioned "
                          "fit; 95%% interval wider than %.0f%%"
                          % (FORMANT_CALIBRATION_MAX_RESIDUAL_DB,
                             FORMANT_CALIBRATION_MAX_HALF_WIDTH_PCT),
            },
        },
        "gradedFixtures": len(graded),
        "uncalibratedFixtures": len(clips) - len(graded),
    }

    fixture_residuals = [c["measured"]["residual_db_median"] for c in clips
                         if math.isfinite(c["measured"]["residual_db_median"])]
    payload = {
        "schemaVersion": 2,
        "task": "P-07e",
        "mode": "formant-calibration",
        "sampleRate": SAMPLE_RATE,
        "manifest": display_path(manifest_path),
        "controls": controls,
        "fixtures": [
            {
                "name": clip["name"],
                "voice": clip["voice"],
                "language": clip["language"],
                **clip["measured"],
            }
            for clip in clips
        ],
        "summary": {
            "fixtures": len(clips),
            "graded": len(graded),
            "uncalibrated": len(clips) - len(graded),
            "perFormant": tier,
            "reasons": reasons,
            "fixtureResidualMedianDb": (
                float(np.median(fixture_residuals))
                if fixture_residuals else float("nan")),
            "fixtureResidualMinDb": (min(fixture_residuals)
                                     if fixture_residuals else float("nan")),
            "fixtureResidualMaxDb": (max(fixture_residuals)
                                     if fixture_residuals else float("nan")),
            "codecFloorResidualDbMedian": controls["coded"]["residualMedianDb"],
            "codecWorstGradedErrorPercent":
                controls["coded"]["worstGradedErrorPercent"],
            "cleanWorstErrorPercent": max(
                (abs(e) for c in controls["clean"]["cells"]
                 for e in c["errorsPercent"] if math.isfinite(e)),
                default=float("nan")),
        },
        "publication": publication,
        "ok": ok,
    }

    if args.json:
        print(json.dumps(json_safe(payload), indent=2, allow_nan=False))
    else:
        clean = controls["clean"]
        coded = controls["coded"]
        print("P-07e formant calibration - 48 kHz mono, %s"
              % display_path(manifest_path))
        print("model: 3 resonators + free bandwidths + free C + alpha*log f, "
              "%.0f Hz band" % FORMANT_CALIBRATION_MAX_HZ)
        print("gate: median residual <= %.1f dB, %d+ frames, 95%% interval "
              "<= %.0f%% of the value"
              % (FORMANT_CALIBRATION_MAX_RESIDUAL_DB,
                 FORMANT_CALIBRATION_MIN_FRAMES,
                 FORMANT_CALIBRATION_MAX_HALF_WIDTH_PCT))
        print()
        print("Control 1 - synthetic ground truth (no codec): %d/%d passed, "
              "worst error %s%%, residual median %s dB"
              % (clean["passed"], len(clean["cells"]),
                 fnum(max((abs(e) for c in clean["cells"]
                           for e in c["errorsPercent"]), default=float("nan")),
                      3),
                 fnum(clean["residualMedianDb"], 3)))
        print("Control 2 - same vowels through Opus@32k WebM: %d/%d graded, "
              "worst graded error %s%% (tolerance %.0f%%), residual median "
              "%s dB (graded worst %s dB)"
              % (coded["graded"], len(coded["cells"]),
                 fnum(coded["worstGradedErrorPercent"], 2),
                 coded["tolerancePercent"],
                 fnum(coded["residualMedianDb"], 2),
                 fnum(coded["worstResidualDb"], 2)))
        print("codec: %s" % (coded["codec"]["ffmpeg"] if coded["codec"]
                             else "?"))
        for cell in coded["cells"]:
            if cell["status"] != "graded":
                print("  control cell %3.0f Hz refused: %s"
                      % (cell["f0Hz"], cell["reasons"][0]
                         if cell["reasons"] else "?"))
        print()
        rows = []
        for clip in clips:
            m = clip["measured"]
            values = []
            for key in ("f1", "f2", "f3"):
                f = m["formants"][key]
                values.append("%.0f+-%.0f (%.0f%%)"
                              % (f["hz"], f["half_width_hz"],
                                 f["half_width_percent"])
                              if math.isfinite(f["hz"]) else "n/a")
            rows.append([
                clip["name"], "%.0f" % m["f0_median_hz"],
                "%d/%d" % (m["frames_used"], m["frames_fitted"]),
                "%.2f" % m["residual_db_median"],
                values[0], values[1], values[2], m["status"],
                (m["na_reason"] or "")[:60],
            ])
        print(print_table(
            ["fixture", "f0", "used/fit", "resid dB", "F1 (Hz +-)",
             "F2 (Hz +-)", "F3 (Hz +-)", "status", "reason"],
            rows, ["l", "r", "r", "r", "r", "r", "r", "l", "l"]))
        print()
        print("%d fixtures - %d graded - %d uncalibrated"
              % (len(clips), len(graded), len(clips) - len(graded)))
        print("fixture residual median %s dB (range %s-%s dB)"
              % (fnum(payload["summary"]["fixtureResidualMedianDb"], 2),
                 fnum(payload["summary"]["fixtureResidualMinDb"], 2),
                 fnum(payload["summary"]["fixtureResidualMaxDb"], 2)))
        if reasons:
            print("reasons: %s" % ", ".join(
                "%s (%d)" % (k, v) for k, v in sorted(reasons.items())))
        print()
        print("What Q-01 may publish (D20):")
        print("  F0: the median from the corpus tracker, as before.")
        print("  F1/F2/F3: only a field whose status is 'graded', with its 95%")
        print("    range. Everything else is null with an 'uncalibrated' reason;")
        print("    LPC and the diagnostic range are never substituted.")
        print("Result: %s" % ("controls PASS" if ok else "CONTROLS FAIL"))
        if not fixture_ok:
            print("WARNING: a control failed but a fixture was graded; that is "
                  "not a publishable result.")

    return 0 if ok else 1




def build_parser():
    examples = """\
examples:
  # 1. identity / bypass: the strictest case
  scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \\
                     --test fixtures/f32/af_heart__friends-now.f32 --check

  # 2. actual decoder difference (reference = libopus, f32 = ffmpeg native)
  scripts/metrics.py --ref fixtures/reference/af_heart__friends-now.f32 \\
                     --test fixtures/f32/af_heart__friends-now.f32

  # 3. judge a render: +4 st pitch shift, formant ratio 1.10
  scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \\
                     --test out.wav --pitch-st 4 --formant-ratio 1.10 --check

  # 4. all 12 fixtures, self-check
  scripts/metrics.py --manifest

  # 5. CI: JSON, non-zero exit on failure
  scripts/metrics.py --manifest --json --check | python3 -m json.tool

  # 6. a directory of renders (reads DIR/manifest.tsv if it exists)
  scripts/metrics.py --renders /tmp/rasp-p02-renders --check

  # 7. the pitch ruler's controls (CI gate): exact resampler at every
  #    semitone plus the pairing-mismatch control
  scripts/metrics.py --self-test --check

  # 8. the formant ruler's synthetic ground truth (CI gate)
  scripts/metrics.py --formant-self-test

  # 9. P-07e: can the formant ruler certify real 32 kbps speech?
  #    Runs synthetic ground truth, synthetic-through-Opus@32k, and all 12
  #    fixtures, then applies the D20 publication gate.
  scripts/metrics.py --formant-calibration

  # 10. P-06 may trim/pad: make the decision visible
  scripts/metrics.py --ref fixtures/f32/af_heart__friends-now.f32 \\
                     --test out.wav --pitch-st 4 --allow-length-delta 48 --check

what is checked:
  samples          equality by default; --allow-length-delta N tolerates an
                   explicit |test-ref| up to N samples.
  finite           no NaN or Inf anywhere in either clip
  pitch (paired)   F0 per 40 ms frame / 50% overlap (50-600 Hz YIN with a
                   spectral-peak cross-check); frames are paired BY FRAME INDEX,
                   the same instant in both signals (P-07d), and the D17 gate
                   requires all three of: median |error| <= 5 c, p90 |error|
                   <= 25 c, and zero octave outliers (>600 c). The failing
                   clause is named. Frames outside the reliable 80-550 Hz band,
                   frames with no spectral support, and reference-side octave
                   artifacts are dropped before the statistics and counted.
                   Cells whose expected output F0 leaves that band are N/A,
                   never FAIL.
  F1/F2            harmonic-model fit (P-07c): three two-pole resonators with
                   free bandwidths plus a free C + alpha*log f source tilt,
                   fitted to 100 ms frames of harmonic amplitudes; +/-8% of the
                   requested formant ratio, inside the D1 validated region only
                   (pitch 0: 0.75-1.33, +/-7 st: 1.00-1.10). Outside it, or when
                   the harmonics cannot resolve a formant, the check is N/A with
                   the reason. F3 is shown but not gated (PLAN section 8).
  LPC legacy       order-18 LPC medians, printed beside the harmonic numbers so
                   the two estimators can be compared. Never gated: P-03 showed
                   LPC reports 586 Hz for a true 700 Hz formant at F0 220 Hz.
  null residual    RMS(ref-test)/RMS(ref): -100 dB or lower for identity or
                   --expect-null. Different decoders are expected to differ;
                   that number is reported, not failed.

The input is 48 kHz mono. .f32/.raw is headerless f32le with 48 kHz assumed;
.wav is checked and rejected if it is not 48 kHz mono. Values are medians over
frames, not perceptual truth - PLAN §9 reserves that for human rounds.
"""
    ap = argparse.ArgumentParser(
        prog="scripts/metrics.py",
        description="Objective metrics for RASP renders (P-07d). Compare a "
                    "reference clip with a test clip, score a render "
                    "directory, or self-check the pitch ruler. Measurement "
                    "primitives: scripts/audio_metrics.py.",
        epilog=examples,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ref", metavar="FILE",
                    help="reference clip (.f32 raw f32le, or 48 kHz mono .wav)")
    ap.add_argument("--test", metavar="FILE",
                    help="test clip (same formats as --ref)")
    ap.add_argument("--format", choices=("auto", "f32", "wav"), default="auto",
                    help="input format for both files (default: by suffix / "
                         "RIFF magic)")
    ap.add_argument("--pitch-st", type=float, metavar="N", default=None,
                    help="requested pitch shift in semitones; enables the "
                         "+/-5 cent check")
    ap.add_argument("--formant-ratio", type=float, metavar="R", default=None,
                    help="requested formant ratio (e.g. 1.10); enables the "
                         "+/-8%% F1/F2 check with the harmonic-model estimator, "
                         "inside the D1 validated region only")
    ap.add_argument("--expect-null", action="store_true",
                    help="enforce the <= -100 dB null even when the files "
                         "differ (bypass/reconstruction test)")
    ap.add_argument("--manifest", nargs="?", const=True, default=None,
                    metavar="TSV",
                    help="fixture self-check when given no value; with "
                         "--renders, a TSV mapping renders to references "
                         "(P-02's manifest.tsv format)")
    ap.add_argument("--manifest-path", metavar="FILE", default=None,
                    help="fixture manifest to use with --manifest/--self-test "
                         "(default: fixtures/manifest.json)")
    ap.add_argument("--renders", metavar="DIR", default=None,
                    help="batch mode: score a directory of renders; reads "
                         "DIR/manifest.tsv if present, else infers "
                         "<fixture>.<semitone>.f32 names")
    ap.add_argument("--allow-length-delta", type=int, default=0, metavar="N",
                    help="allow |test-ref| sample count up to N (default 0); "
                         "pitch and null use the common region")
    ap.add_argument("--self-test", action="store_true",
                    help="run exact resampler controls at every semitone plus "
                         "the pairing-mismatch control on the fixtures; exits 1 "
                         "on any in-band error > 1 cent or a pairing regression")
    ap.add_argument("--formant-self-test", action="store_true",
                    help="run the harmonic-model formant ruler against "
                         "synthetic vowels with known formants at 90-300 Hz; "
                         "exits 1 on any formant error > 1%%")
    ap.add_argument("--formant-calibration", action="store_true",
                    help="P-07e: certify the formant ruler on real 32 kbps "
                         "speech. Runs clean synthetic ground truth, the same "
                         "vowels through Opus@32k, and the 12 fixtures; emits "
                         "per-voice ranges or 'uncalibrated' with a reason")
    ap.add_argument("--json", action="store_true",
                    help="print one JSON object instead of the table")
    ap.add_argument("--check", action="store_true",
                    help="exit 1 if any applicable check fails (default: "
                         "report only, exit 0). --self-test and "
                         "--formant-self-test always enforce their gate.")
    return ap


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)

    manifest_flag = args.manifest is True
    manifest_tsv = args.manifest if isinstance(args.manifest, str) else None
    selected = sum([bool(args.ref or args.test), manifest_flag,
                    bool(args.renders), bool(args.self_test),
                    bool(args.formant_self_test),
                    bool(args.formant_calibration)])
    if selected != 1:
        parser.error("give exactly one mode: --ref/--test, --manifest, "
                     "--renders DIR, --self-test, --formant-self-test, or "
                     "--formant-calibration")

    if args.self_test and args.formant_self_test:
        parser.error("--self-test and --formant-self-test are separate "
                     "controls; run them as two commands")
    if args.self_test or args.formant_self_test or args.formant_calibration:
        if (args.ref or args.test or args.renders or args.manifest
                or args.pitch_st is not None or args.formant_ratio is not None
                or args.expect_null):
            parser.error("--self-test/--formant-self-test/"
                         "--formant-calibration cannot be combined with other "
                         "modes or single-pair options")
        if args.allow_length_delta:
            parser.error("--allow-length-delta applies to --ref/--test and "
                         "--renders only")
    elif args.renders:
        if args.ref or args.test:
            parser.error("--renders cannot be combined with --ref/--test")
        if manifest_flag:
            parser.error("--manifest with --renders needs a TSV path")
        if args.manifest_path:
            parser.error("--manifest-path is the fixture manifest; use "
                         "--manifest TSV for renders")
        if (args.pitch_st is not None or args.formant_ratio is not None
                or args.expect_null):
            parser.error("--pitch-st/--formant-ratio/--expect-null apply only "
                         "to --ref/--test comparisons")
    elif manifest_flag:
        if args.ref or args.test:
            parser.error("--manifest cannot be combined with --ref/--test")
        if (args.pitch_st is not None or args.formant_ratio is not None
                or args.expect_null):
            parser.error("--pitch-st/--formant-ratio/--expect-null apply only "
                         "to --ref/--test comparisons")
        if args.allow_length_delta:
            parser.error("--allow-length-delta applies to --ref/--test and "
                         "--renders only")
    else:
        if manifest_tsv:
            parser.error("a --manifest TSV path requires --renders DIR")
        if args.manifest_path:
            parser.error("--manifest-path only applies with --manifest")
        if not (args.ref and args.test):
            parser.error("give --ref FILE --test FILE, or --manifest, or "
                         "--renders DIR, --self-test, or --formant-self-test")

    if args.pitch_st is not None:
        if not math.isfinite(args.pitch_st):
            parser.error("--pitch-st must be finite")
    if args.formant_ratio is not None:
        if not math.isfinite(args.formant_ratio) or args.formant_ratio <= 0:
            parser.error("--formant-ratio must be a positive finite number")
    if args.allow_length_delta < 0:
        parser.error("--allow-length-delta must be >= 0")

    try:
        if args.formant_calibration:
            return run_formant_calibration(args)
        if args.formant_self_test:
            return run_formant_self_test(args)
        if args.self_test:
            return run_self_test(args)
        if args.renders:
            return run_renders(args)
        if manifest_flag:
            return run_manifest(args)
        return run_compare(args)
    except AudioFormatError as exc:
        sys.stderr.write("metrics.py: %s\n" % exc)
        return 2
    except OSError as exc:
        sys.stderr.write("metrics.py: cannot read input: %s\n" % exc)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
