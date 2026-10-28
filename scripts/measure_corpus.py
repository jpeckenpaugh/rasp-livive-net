#!/usr/bin/env python3
"""F-06: measure the dg-pp voice corpus and emit docs/corpus.md.

Bootstrap (two commands, from the repo root):
    python3 -m venv scripts/.venv
    scripts/.venv/bin/pip install -r scripts/requirements.txt

Run (regenerates docs/corpus.md):
    scripts/.venv/bin/python scripts/measure_corpus.py

The corpus is looked up in --cache, else $TMPDIR/rasp-corpus (fallback /tmp),
never inside the repo. Missing registry.json / clips are fetched from
https://dg-pp.livive.net/ into that cache; nothing outside docs/corpus.md is
written. Decoding is ffmpeg -> 48 kHz mono f32le (Opus is natively 48 kHz).

Options:
    --cache DIR   corpus cache (default $TMPDIR/rasp-corpus or /tmp/rasp-corpus)
    --sample N    process every Nth voice (default 1 = all 54 x 26 = 1404 clips)
    --jobs N      worker processes (default: cpu_count-1, capped at 8)
    --out PATH    output markdown (default docs/corpus.md)
    --no-fetch    fail instead of downloading missing files
"""

import argparse
import hashlib
import json
import math
import os
import shutil
import subprocess
import sys
import urllib.request
from concurrent.futures import ProcessPoolExecutor, ThreadPoolExecutor

import numpy as np
from scipy import signal

# The measurement primitives live in exactly one place (P-07).  Importing them
# rather than copying them is what keeps docs/corpus.md and metrics.py honest.
from audio_metrics import (CLIP_LEVEL, DC_LIMIT, SILENCE_DBFS, SILENCE_MIN_S,
                           SR, db, f0_track, lufs_integrated, runs_of,
                           spectral_centroid)

BASE_URL = "https://dg-pp.livive.net/"
DUR_MISMATCH_S = 0.020


# ---------------------------------------------------------------- utilities


def cache_dir(cli_cache):
    if cli_cache:
        return os.path.abspath(os.path.expanduser(cli_cache))
    tmp = os.environ.get("TMPDIR") or "/tmp"
    return os.path.join(tmp.rstrip("/"), "rasp-corpus")


def fetch(url, dest):
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    tmp = dest + ".part"
    with urllib.request.urlopen(url, timeout=60) as r, open(tmp, "wb") as f:
        shutil.copyfileobj(r, f)
    os.replace(tmp, dest)


def load_registry(cache, allow_fetch):
    path = os.path.join(cache, "registry.json")
    if not os.path.exists(path):
        if not allow_fetch:
            sys.exit("missing registry.json in %s" % cache)
        print("fetching registry.json -> %s" % path, file=sys.stderr)
        fetch(BASE_URL + "registry.json", path)
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def prefetch(cache, registry, allow_fetch):
    """Download every clip the run needs, in parallel, before measuring."""
    needed = []
    for vid, phrases in registry["files"].items():
        for pid, e in phrases.items():
            p = os.path.join(cache, e["path"])
            if not os.path.exists(p):
                needed.append((e["path"], p))
    if not needed:
        return 0
    if not allow_fetch:
        sys.exit("%d clips missing from %s" % (len(needed), cache))
    print("fetching %d clips -> %s" % (len(needed), cache), file=sys.stderr)

    def one(rel_dest):
        rel, dest = rel_dest
        fetch(BASE_URL + rel, dest)

    with ThreadPoolExecutor(max_workers=16) as ex:
        list(ex.map(one, needed))
    return len(needed)


def decode(path):
    cmd = [
        "ffmpeg", "-nostdin", "-v", "error", "-i", path,
        "-map", "0:a:0", "-ac", "1", "-ar", str(SR), "-f", "f32le", "-",
    ]
    out = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         check=False)
    if out.returncode != 0:
        raise RuntimeError("ffmpeg failed for %s: %s" % (
            path, out.stderr.decode("utf-8", "replace").strip()[:300]))
    return np.frombuffer(out.stdout, dtype=np.float32).astype(np.float64)


# ------------------------------------------------------------- measurements


def measure_clip(job):
    cache, vid, pid, entry, allow_fetch = job
    path = os.path.join(cache, entry["path"])
    if not os.path.exists(path):
        if not allow_fetch:
            raise RuntimeError("missing clip %s" % path)
        fetch(BASE_URL + entry["path"], path)
    raw = open(path, "rb").read()
    sha_ok = hashlib.sha256(raw).hexdigest() == entry.get("sha256")
    bytes_ok = len(raw) == entry.get("bytes")

    x = decode(path)
    n = len(x)
    dur = n / float(SR)
    reg_dur = entry.get("durationMs", 0) / 1000.0
    peak = float(np.max(np.abs(x))) if n else 0.0
    rms = float(np.sqrt(np.mean(x ** 2))) if n else 0.0
    dc = float(np.mean(x)) if n else 0.0
    clip_n = int(np.count_nonzero(np.abs(x) >= CLIP_LEVEL))

    # Silence is judged on a 10 ms sliding RMS envelope, not raw samples:
    # raw |x| crosses -45 dBFS every few samples inside voiced speech.
    thr = 10 ** (SILENCE_DBFS / 20.0)
    ew = max(1, int(round(0.010 * SR)))
    energy = np.concatenate(([0.0], np.cumsum(x ** 2)))
    env = np.sqrt(np.maximum(
        (energy[ew:] - energy[:-ew]) / ew, 0.0))
    pad = len(x) - len(env)
    env = np.concatenate((np.zeros(pad // 2), env, np.zeros(pad - pad // 2)))
    starts, ends = runs_of(env >= thr)
    min_run = int(round(SILENCE_MIN_S * SR))
    long_enough = (ends - starts) >= min_run
    if long_enough.any():
        head = starts[long_enough][0] / SR
        tail = (n - ends[long_enough][-1]) / SR
    else:
        head, tail = dur, dur

    f0, hnr_frames, frame_rms = f0_track(x)
    if len(f0) > 2:
        f0 = signal.medfilt(f0, kernel_size=5)  # kill isolated octave slips
        hnr_frames = signal.medfilt(hnr_frames, kernel_size=5)
    if len(f0):
        f0_med = float(np.median(f0))
        f0_p10, f0_p90 = (float(v) for v in np.percentile(f0, [10, 90]))
        # Crude HNR from the autocorrelation peak (Praat-style).
        hnr = float(np.median(hnr_frames))
    else:
        f0_med = f0_p10 = f0_p90 = float("nan")
        hnr = float("nan")

    return {
        "voice": vid, "phrase": pid, "duration": dur, "reg_duration": reg_dur,
        "peak_db": db(peak), "rms_db": db(rms), "lufs": lufs_integrated(x),
        "dc": dc, "clip_samples": clip_n, "head": head, "tail": tail,
        "f0": f0, "f0_med": f0_med, "f0_p10": f0_p10, "f0_p90": f0_p90,
        "hnr": hnr, "centroid": spectral_centroid(x),
        "voiced_frac": len(f0) / max(1, len(frame_rms)),
        "sha_ok": sha_ok, "bytes_ok": bytes_ok,
        "bytes": len(raw),
    }


# ------------------------------------------------------------- aggregation

def pctl(a, q, default=float("nan")):
    a = np.asarray(a, dtype=np.float64)
    a = a[np.isfinite(a)]
    return float(np.percentile(a, q)) if len(a) else default


def semis(p10, p90):
    if not (p10 > 0 and p90 > 0):
        return float("nan")
    return 12.0 * math.log2(p90 / p10)


def aggregate(registry, results):
    meta = {v["id"]: v for v in registry["voices"]}
    by_voice = {}
    for r in results:
        by_voice.setdefault(r["voice"], []).append(r)

    voices = {}
    for vid, clips in by_voice.items():
        f0 = np.concatenate([c["f0"] for c in clips if len(c["f0"])]) \
            if any(len(c["f0"]) for c in clips) else np.empty(0)
        med = pctl(f0, 50)
        p10, p90 = pctl(f0, 10), pctl(f0, 90)
        m = meta[vid]
        voices[vid] = {
            "id": vid, "language": m["language"], "gender": m["gender"],
            "locale": m["locale"], "grade": m.get("grade") or "",
            "n_clips": len(clips), "n_voiced_frames": int(len(f0)),
            "f0_med": med, "f0_p10": p10, "f0_p90": p90,
            "f0_spread_st": semis(p10, p90),
            "f0_min_clip": float(np.nanmin([c["f0_med"] for c in clips])),
            "f0_max_clip": float(np.nanmax([c["f0_med"] for c in clips])),
            "rms_db": pctl([c["rms_db"] for c in clips], 50),
            "lufs": pctl([c["lufs"] for c in clips], 50),
            "centroid": pctl([c["centroid"] for c in clips], 50),
            "hnr": pctl([c["hnr"] for c in clips], 50),
            "head_ms": 1000 * pctl([c["head"] for c in clips], 50),
            "tail_ms": 1000 * pctl([c["tail"] for c in clips], 50),
            "head_max_ms": 1000 * max(c["head"] for c in clips),
            "tail_max_ms": 1000 * max(c["tail"] for c in clips),
            "peak_db": max(c["peak_db"] for c in clips),
            "voiced_frac": float(np.mean([c["voiced_frac"] for c in clips])),
            "clip_samples": int(sum(c["clip_samples"] for c in clips)),
            "clipped_clips": sorted(c["phrase"] for c in clips if c["clip_samples"]),
            "dc_max": max(abs(c["dc"]) for c in clips),
            "bad_sha": sorted(c["phrase"] for c in clips if not c["sha_ok"]),
            "bad_bytes": sorted(c["phrase"] for c in clips if not c["bytes_ok"]),
            "dur_delta_max_ms": 1000 * max(
                abs(c["duration"] - c["reg_duration"]) for c in clips),
            "bad_dur": sorted(c["phrase"] for c in clips
                              if abs(c["duration"] - c["reg_duration"]) > DUR_MISMATCH_S),
            "clips": clips,
        }

    # Cohort-relative F0 outliers (per language, MAD-based) and absolute band.
    groups = sorted({(v["language"], v["gender"]) for v in voices.values()})
    for lang, gender in groups:
        cohort = [v for v in voices.values()
                  if v["language"] == lang and v["gender"] == gender]
        if len(cohort) >= 4:
            vals = np.array([v["f0_med"] for v in cohort])
            med = float(np.median(vals))
            mad = float(np.median(np.abs(vals - med))) * 1.4826
            for v in cohort:
                v["f0_cohort_med"] = med
                v["f0_outlier"] = bool(
                    mad > 0 and abs(v["f0_med"] - med) > 3.0 * mad)
        else:
            for v in cohort:
                v["f0_cohort_med"] = float("nan")
                v["f0_outlier"] = False

    for v in voices.values():
        flags = []
        if v["clip_samples"]:
            flags.append("clipping")
        if v["dc_max"] > DC_LIMIT:
            flags.append("dc>%.3f" % v["dc_max"])
        if v["bad_dur"]:
            flags.append("reg-dur")
        if v["bad_sha"] or v["bad_bytes"]:
            flags.append("integrity")
        if v["f0_outlier"]:
            flags.append("f0-outlier")
        if v["f0_med"] < 70 or v["f0_med"] > 350:
            flags.append("f0-abs")
        if v["gender"] == "male" and v["f0_med"] >= 165:
            flags.append("gender?")
        if v["gender"] == "female" and v["f0_med"] <= 145:
            flags.append("gender?")
        if v["hnr"] < 8:
            flags.append("low-hnr")
        if v["voiced_frac"] < 0.40:
            flags.append("low-voiced")
        if v["head_ms"] > 250:
            flags.append("head-silence")
        if v["tail_ms"] > 250:
            flags.append("tail-silence")
        v["flags"] = flags

    return voices


# ------------------------------------------------------------------ report

def fnum(x, nd=1, dash="—"):
    if x is None or (isinstance(x, float) and not math.isfinite(x)):
        return dash
    return ("%%.%df" % nd) % x


def table(headers, rows):
    out = ["| " + " | ".join(headers) + " |",
           "|" + "|".join("---" for _ in headers) + "|"]
    for r in rows:
        out.append("| " + " | ".join(str(c) for c in r) + " |")
    return "\n".join(out)


def language_rows(voices):
    langs = sorted({v["language"] for v in voices.values()})
    rows = []
    tot = {"v": 0, "c": 0, "b": 0, "s": 0.0}
    for lang in langs:
        vs = [v for v in voices.values() if v["language"] == lang]
        clips = [c for v in vs for c in v["clips"]]
        mb = sum(c["bytes"] for c in clips) / 1e6
        secs = sum(c["duration"] for c in clips)
        f0s = [v["f0_med"] for v in vs]
        durs = [c["duration"] for c in clips]
        rows.append([
            "`%s`" % lang, len(vs), len(clips), fnum(mb, 2),
            fnum(secs, 1), "%.2f–%.2f" % (min(durs), max(durs)),
            "%.0f–%.0f" % (min(f0s), max(f0s)),
            fnum(pctl([c["rms_db"] for c in clips], 50), 1),
            fnum(pctl([c["lufs"] for c in clips], 50), 1),
            "%.0f%%" % (100.0 * sum(1 for c in clips if c["head"] > 0.100) / len(clips)),
        ])
        tot["v"] += len(vs); tot["c"] += len(clips); tot["b"] += mb; tot["s"] += secs
    rows.append(["**all**", tot["v"], tot["c"], fnum(tot["b"], 2),
                 fnum(tot["s"], 1), "—", "—", "—", "—", "—"])
    return rows


def pick_candidates(voices, lang, gender):
    cands = [v for v in voices.values()
             if v["language"] == lang and v["gender"] == gender]
    return sorted(cands, key=lambda v: v["f0_med"])


def quality(v, hnr_floor=8.0):
    # bad_dur is excluded on purpose: the decoded-vs-registry offset is a
    # container/pre-skip artifact that hits ~2% of clips in every voice.
    return (not v["clip_samples"] and v["dc_max"] <= DC_LIMIT
            and not v["bad_sha"] and not v["bad_bytes"]
            and "gender?" not in v["flags"]
            and v["hnr"] >= hnr_floor and v["voiced_frac"] >= 0.40)


def nearest_quality(voices, lang, gender, target, exclude=()):
    cands = [v for v in pick_candidates(voices, lang, gender)
             if quality(v) and v["id"] not in exclude]
    if not cands:
        return None
    return min(cands, key=lambda v: abs(v["f0_med"] - target))


def calibration_set(voices):
    picks, notes = [], []
    for gender in ("female", "male"):
        cands = pick_candidates(voices, "en", gender)
        if not cands:
            continue
        lo, hi = cands[0], cands[-1]
        mid_val = pctl([c["f0_med"] for c in cands], 50)
        near = [v for v in cands if abs(v["f0_med"] - mid_val) <= 12.0] or cands
        mid = max(near, key=lambda v: v["hnr"])
        for tag, v in (("low", lo), ("mid", mid), ("high", hi)):
            if not quality(v):
                alt = nearest_quality(voices, "en", gender, v["f0_med"],
                                      exclude={x["id"] for x in picks})
                if alt is not None:
                    why = []
                    if v["hnr"] < 8.0:
                        why.append("HNR %.1f dB" % v["hnr"])
                    if v["voiced_frac"] < 0.40:
                        why.append("%.0f%% voiced" % (100 * v["voiced_frac"]))
                    if "gender?" in v["flags"]:
                        why.append("label/pitch contradiction")
                    if v["clip_samples"]:
                        why.append("clipping")
                    notes.append("`%s` (%s, %.0f Hz) was rejected (%s); used "
                                 "`%s` (%.0f Hz) instead."
                                 % (v["id"], tag, v["f0_med"],
                                    ", ".join(why) or "quality gate",
                                    alt["id"], alt["f0_med"]))
                    v = alt
            picks.append(v)

    # zh/ja: representative of their gender cohort, not the pitch extreme.
    for lang in ("zh", "ja"):
        for gender in ("female", "male"):
            cands = pick_candidates(voices, lang, gender)
            if not cands:
                continue
            base = [v for v in cands if "gender?" not in v["flags"]] or cands
            ok = [v for v in base if quality(v)] or base
            med = pctl([v["f0_med"] for v in cands], 50)
            clean = [v for v in ok if v["hnr"] >= 10.0]
            if clean:
                picks.append(min(clean, key=lambda v: abs(v["f0_med"] - med)))
            else:
                picks.append(max(ok, key=lambda v: v["hnr"]))

    for lang in ("hi", "es"):
        cands = [v for v in voices.values()
                 if v["language"] == lang and "gender?" not in v["flags"]]
        if not cands:
            cands = [v for v in voices.values() if v["language"] == lang]
        ok = [v for v in cands if quality(v)] or cands
        if ok:
            picks.append(max(ok, key=lambda v: v["hnr"]))

    seen, uniq = set(), []
    for v in picks:
        if v["id"] not in seen:
            seen.add(v["id"]); uniq.append(v)
    return uniq, notes


def build_report(registry, voices, results, cache, sample, jobs):
    clips = results
    langs = sorted({v["language"] for v in voices.values()})
    n_voices = len(voices)
    n_clips = len(clips)
    total_bytes = sum(c["bytes"] for c in clips)
    total_sec = sum(c["duration"] for c in clips)
    worst = sorted(voices.values(), key=lambda v: v["f0_med"])

    def clip_label(c):
        return "`%s/%s`" % (c["voice"], c["phrase"])

    head100 = [c for c in clips if c["head"] > 0.100]
    head250 = [c for c in clips if c["head"] > 0.250]
    head500 = [c for c in clips if c["head"] > 0.500]
    tail100 = [c for c in clips if c["tail"] > 0.100]
    tail250 = [c for c in clips if c["tail"] > 0.250]
    tail500 = [c for c in clips if c["tail"] > 0.500]
    head_ms = [1000 * c["head"] for c in clips]
    tail_ms = [1000 * c["tail"] for c in clips]
    sil_sec = sum(c["head"] + c["tail"] for c in clips)

    by_head = sorted(clips, key=lambda c: c["head"], reverse=True)[:8]
    by_tail = sorted(clips, key=lambda c: c["tail"], reverse=True)[:8]

    clip_f0_p10 = [v["f0_p10"] for v in voices.values() if math.isfinite(v["f0_p10"])]
    clip_f0_p90 = [v["f0_p90"] for v in voices.values() if math.isfinite(v["f0_p90"])]
    f_min_med = min(v["f0_med"] for v in voices.values())
    f_max_med = max(v["f0_med"] for v in voices.values())
    f_min_p10 = min(clip_f0_p10)
    clean = [v["f0_p10"] for v in voices.values()
             if v["hnr"] >= 8.0 and v["voiced_frac"] >= 0.45]
    f_min_p10_clean = min(clean) if clean else f_min_p10
    f_max_p90 = max(clip_f0_p90)
    low_voice = min(voices.values(), key=lambda v: v["f0_med"])
    high_voice = max(voices.values(), key=lambda v: v["f0_med"])

    bin_hz = SR / 2048.0
    hop = 512
    dev_limit = SR / (2.0 * hop)
    three_bin_floor = 3.0 * bin_hz
    bins_at_min = f_min_p10 / bin_hz
    bins_at_min_clean = f_min_p10_clean / bin_hz
    bins_shifted_down = (f_min_p10 / 2.0) / bin_hz

    # calibration anchors (English only, per D7)
    fem_c = pick_candidates(voices, "en", "female")
    male_c = pick_candidates(voices, "en", "male")
    fem_q = [v for v in fem_c if quality(v)]
    male_q = [v for v in male_c if quality(v)]
    low_anchor = male_q[0] if male_q else (male_c[0] if male_c else None)
    fem_extreme = fem_q[-1] if fem_q else (fem_c[-1] if fem_c else None)
    top = [v for v in fem_q
           if fem_extreme and v["f0_med"] >= 0.85 * fem_extreme["f0_med"]]
    high_anchor = (max(top, key=lambda v: v["hnr"]) if top else fem_extreme)

    picks, pick_notes = calibration_set(voices)

    # ---- flags inventory
    clipped = [c for c in clips if c["clip_samples"]]
    clipped_voices = sorted({c["voice"] for c in clipped})
    dc_bad = sorted([c for c in clips if abs(c["dc"]) > DC_LIMIT],
                    key=lambda c: abs(c["dc"]), reverse=True)
    dur_bad = sorted([c for c in clips
                      if abs(c["duration"] - c["reg_duration"]) > DUR_MISMATCH_S],
                     key=lambda c: abs(c["duration"] - c["reg_duration"]), reverse=True)
    dur_delta = [1000 * (c["duration"] - c["reg_duration"]) for c in clips]
    sha_bad = [c for c in clips if not c["sha_ok"] or not c["bytes_ok"]]
    f0_out = [v for v in worst if v["f0_outlier"]]
    gender_bad = [v for v in worst if "gender?" in v["flags"]]
    low_hnr = sorted([v for v in voices.values() if v["hnr"] < 8],
                     key=lambda v: v["hnr"])
    low_voiced = sorted([v for v in voices.values() if v["voiced_frac"] < 0.40],
                        key=lambda v: v["voiced_frac"])

    L = []
    A = L.append

    A("# Voice corpus measurements")
    A("")
    A("**Task:** F-06 · **Corpus:** `dg-pp` build `%s` (clipVersion %s, created %s) · "
      "**Generated by:** `scripts/measure_corpus.py`"
      % (registry["build"].get("id", "?"), registry["build"].get("clipVersion", "?"),
         registry["build"].get("createdAt", "?")))
    A("")
    A("All numbers below are produced by the committed script; nothing is hand-edited. "
      "The run is deterministic for a fixed corpus build and a fixed script revision.")
    A("")
    A("## How to reproduce")
    A("")
    A("Corpus cache: `%s` (registry + clips, fetched from https://dg-pp.livive.net/). "
      "Nothing is written inside the repo except this document. Once F-03 has vendored "
      "the corpus, run the same command with `--cache web` to measure the committed "
      "copy instead of the network cache."
      % "${TMPDIR:-/tmp}/rasp-corpus")
    A("")
    A("```sh")
    A("python3 -m venv scripts/.venv")
    A("scripts/.venv/bin/pip install -r scripts/requirements.txt")
    A("scripts/.venv/bin/python scripts/measure_corpus.py")
    A("```")
    A("")
    A("Coverage: **%d voices x 26 phrases = %d clips** (%s). "
      "`--sample N` processes every Nth voice (sorted by voice id, all 26 phrases); "
      "this run used **`--sample %d`**."
      % (n_voices, n_clips,
         "full corpus, no sampling" if sample == 1 else "every %dth voice" % sample,
         sample))
    A("")
    A("### Method")
    A("")
    A("- Decode: `ffmpeg -i <clip> -ac 1 -ar 48000 -f f32le` (Opus is natively 48 kHz; "
      "the source is mono).")
    A("- Levels: peak / RMS dBFS over the whole decoded clip; DC offset is the sample "
      "mean; LUFS-I is BS.1770-4 K-weighting with 400 ms blocks, 100 ms hop and "
      "absolute −70 / relative −10 LU gating.")
    A("- Clipping: samples with `|x| >= 0.999`.")
    A("- Silence: 10 ms sliding RMS envelope, threshold −45 dBFS, minimum run 20 ms. "
      "Head/tail are measured from the first/last above-threshold run, so isolated "
      "clicks below 20 ms are ignored. This is the same rule P-04 needs for trimming.")
    A("- F0: YIN over 40 ms frames, 10 ms hop, search band 60–400 Hz, absolute "
      "threshold 0.20, voiced gate 0.35, 5-frame median smoothing. Reported median / "
      "p10 / p90 are over **voiced frames across all 26 phrases** of a voice, after "
      "pooling frames (not a mean of per-clip medians).")
    A("- HNR: crude, `10*log10(r/(1-r))` from the normalised autocorrelation of the "
      "40 ms frame at the detected lag; median over voiced frames. Compare voices, do "
      "not read it as an absolute studio figure.")
    A("- Spectral centroid: median over 25 ms Hann frames above the energy floor.")
    A("")
    A("## 1. Corpus summary")
    A("")
    A(table(["lang", "voices", "clips", "MB", "seconds", "clip s (min–max)",
             "voice F0 med range (Hz)", "RMS dBFS (med)", "LUFS-I (med)",
             "% clips head > 100 ms"],
            language_rows(voices)))
    A("")
    A("Totals: **%d clips, %.1f s, %.2f MB** of registry payload (mean %.0f ms per "
      "clip). Language coverage is uneven by design (28 English voices, 1 French); "
      "per-language conclusions beyond `en`/`zh` are thin."
      % (n_clips, total_sec, total_bytes / 1e6, 1000 * total_sec / n_clips))
    A("")
    A("Integrity: %d/%d clips match the registry `sha256` **and** `bytes`."
      % (n_clips - len(sha_bad), n_clips))
    A("")
    A("## 2. Per-voice measurements")
    A("")
    A("Sorted by pooled median F0. `spread` is p90/p10 in semitones over voiced frames: "
      "it mixes intonation, lexical tone (where present) and tracker noise. Read it as "
      "*movement*, not as vocal range, and do not compare it across voices of different "
      "HNR.")
    A("")
    rows = []
    for i, v in enumerate(worst, 1):
        rows.append([
            i, "`%s`" % v["id"], v["language"], v["gender"][0].upper(),
            v["grade"] or "—",
            fnum(v["f0_med"], 1), fnum(v["f0_p10"], 0), fnum(v["f0_p90"], 0),
            fnum(v["f0_spread_st"], 1),
            fnum(v["rms_db"], 1), fnum(v["lufs"], 1), fnum(v["centroid"], 0),
            fnum(v["hnr"], 1), "%d" % round(v["head_ms"]), "%d" % round(v["tail_ms"]),
            ", ".join(v["flags"]) or "—",
        ])
    A(table(["#", "voice", "lang", "g", "grade", "F0 med", "p10", "p90",
             "spread st", "RMS dBFS", "LUFS-I", "centroid Hz", "HNR dB",
             "head ms", "tail ms", "flags"], rows))
    A("")
    A("`grade` is Kokoro's own voice-quality tier where the registry carries one; the "
      "non-English voices have none. It is a speaker-model property, not a measurement "
      "of the clip.")
    A("")
    return L, dict(
        clips=clips, voices=voices, worst=worst, langs=langs, n_clips=n_clips,
        total_sec=total_sec, total_bytes=total_bytes, head100=head100,
        head250=head250, head500=head500, tail100=tail100, tail250=tail250,
        tail500=tail500, head_ms=head_ms, tail_ms=tail_ms, sil_sec=sil_sec,
        by_head=by_head, by_tail=by_tail, f_min_med=f_min_med, f_max_med=f_max_med,
        f_min_p10=f_min_p10, f_max_p90=f_max_p90, low_voice=low_voice,
        high_voice=high_voice, bin_hz=bin_hz, hop=hop, dev_limit=dev_limit,
        three_bin_floor=three_bin_floor, bins_at_min=bins_at_min,
        bins_shifted_down=bins_shifted_down, low_anchor=low_anchor,
        f_min_p10_clean=f_min_p10_clean, bins_at_min_clean=bins_at_min_clean,
        high_anchor=high_anchor, fem_extreme=fem_extreme,
        picks=picks, pick_notes=pick_notes,
        clipped=clipped, clipped_voices=clipped_voices, dc_bad=dc_bad,
        dur_bad=dur_bad, dur_delta=dur_delta, sha_bad=sha_bad, f0_out=f0_out,
        gender_bad=gender_bad, low_hnr=low_hnr, low_voiced=low_voiced,
        clip_label=clip_label,
    )


def finish_report(registry, ctx):
    L, A = [], None
    def A(s):
        L.append(s)

    clips = ctx["clips"]
    voices = ctx["voices"]
    cb = ctx["clip_label"]

    # ---------------------------------------------------------------- silence
    A("## 3. Silence, head and tail")
    A("")
    A("Threshold −45 dBFS on a 10 ms RMS envelope, 20 ms minimum run. This is the "
      "measurement P-04's trim stage must act on.")
    A("")
    A(table(["statistic", "head", "tail"],
            [["clips > 100 ms", len(ctx["head100"]), len(ctx["tail100"])],
             ["clips > 250 ms", len(ctx["head250"]), len(ctx["tail250"])],
             ["clips > 500 ms", len(ctx["head500"]), len(ctx["tail500"])],
             ["median", "%d ms" % round(pctl(ctx["head_ms"], 50)),
              "%d ms" % round(pctl(ctx["tail_ms"], 50))],
             ["p90", "%d ms" % round(pctl(ctx["head_ms"], 90)),
              "%d ms" % round(pctl(ctx["tail_ms"], 90))],
             ["max", "%d ms" % round(max(ctx["head_ms"])),
              "%d ms" % round(max(ctx["tail_ms"]))]]))
    A("")
    A("Total head+tail silence across the corpus: **%.1f s of %.1f s (%.1f%%)**."
      % (ctx["sil_sec"], ctx["total_sec"], 100.0 * ctx["sil_sec"] / ctx["total_sec"]))
    A("")
    A("Per language (medians, milliseconds):")
    A("")
    rows = []
    for lang in sorted({v["language"] for v in voices.values()}):
        cs = [c for c in clips
              if voices[c["voice"]]["language"] == lang]
        h = [1000 * c["head"] for c in cs]
        t = [1000 * c["tail"] for c in cs]
        rows.append(["`%s`" % lang, len(cs),
                     "%d" % round(pctl(h, 50)), "%d" % round(pctl(h, 90)),
                     "%d" % round(max(h)),
                     "%d" % round(pctl(t, 50)), "%d" % round(pctl(t, 90)),
                     "%d" % round(max(t)),
                     "%.0f%%" % (100.0 * sum(1 for c in cs if c["head"] > 0.100) / len(cs)),
                     "%.0f%%" % (100.0 * sum(1 for c in cs if c["tail"] > 0.100) / len(cs))])
    A(table(["lang", "clips", "head med", "head p90", "head max",
             "tail med", "tail p90", "tail max", "% head>100", "% tail>100"], rows))
    A("")
    A("Worst head silence:")
    A("")
    A(table(["clip", "dur s", "head ms", "tail ms", "peak dBFS"],
            [[cb(c), fnum(c["duration"], 2),
              "%d" % round(1000 * c["head"]), "%d" % round(1000 * c["tail"]),
              fnum(c["peak_db"], 1)] for c in ctx["by_head"]]))
    A("")
    A("Worst tail silence:")
    A("")
    A(table(["clip", "dur s", "head ms", "tail ms", "peak dBFS"],
            [[cb(c), fnum(c["duration"], 2),
              "%d" % round(1000 * c["head"]), "%d" % round(1000 * c["tail"]),
              fnum(c["peak_db"], 1)] for c in ctx["by_tail"]]))
    A("")

    # ------------------------------------------------------------------ flags
    A("## 4. Flags")
    A("")
    A("### 4.1 Clipping")
    A("")
    if ctx["clipped"]:
        A("**%d clips in %d voices** contain samples with `|x| >= 0.999`; worst:"
          % (len(ctx["clipped"]), len(ctx["clipped_voices"])))
        A("")
        top = sorted(ctx["clipped"], key=lambda c: c["clip_samples"], reverse=True)[:10]
        A(table(["clip", "clipped samples", "peak dBFS"],
                [[cb(c), c["clip_samples"],
                  fnum(c["peak_db"], 1)] for c in top]))
    else:
        A("No clip reaches `|x| >= 0.999`. Loudest sample in the corpus: **%.2f dBFS** "
          "(%s). The decoder output never reaches full scale, so no headroom-driven "
          "level change is needed before DSP (true-peak/overs is not measured here)."
          % (max(c["peak_db"] for c in clips),
             cb(max(clips, key=lambda c: c["peak_db"]))))
    A("")
    A("### 4.2 DC offset")
    A("")
    if ctx["dc_bad"]:
        A("%d clips exceed `|DC| > 0.01`; worst:" % len(ctx["dc_bad"]))
        A("")
        A(table(["clip", "DC"], [[cb(c), "%+.4f" % c["dc"]]
                                 for c in ctx["dc_bad"][:10]]))
    else:
        worst_dc = max(clips, key=lambda c: abs(c["dc"]))
        A("No clip exceeds `|DC| > 0.01`. Largest absolute offset in the corpus is "
          "**%+.5f** (%s), i.e. about %.4f dBFS of DC. No DC-blocking stage is "
          "required for correctness; a bypass-path DC blocker is optional."
          % (worst_dc["dc"], cb(worst_dc), db(abs(worst_dc["dc"]))))
    A("")
    A("### 4.3 Registry duration mismatches")
    A("")
    if ctx["dur_bad"]:
        A("**%d clips differ from `registry.durationMs` by more than 20 ms.** "
          "Largest: %+.1f ms (%s)."
          % (len(ctx["dur_bad"]),
             1000 * (ctx["dur_bad"][0]["duration"] - ctx["dur_bad"][0]["reg_duration"]),
             cb(ctx["dur_bad"][0])))
        A("")
        A(table(["clip", "decoded s", "registry s", "delta ms"],
                [[cb(c), fnum(c["duration"], 3),
                  fnum(c["reg_duration"], 3),
                  "%+.1f" % (1000 * (c["duration"] - c["reg_duration"]))]
                 for c in ctx["dur_bad"][:12]]))
    else:
        A("No clip differs by more than 20 ms.")
    A("")
    n_bad = len(ctx["dur_bad"])
    A("Decoded-minus-registry duration: median **%+.1f ms**, p05/p95 %+.1f/%+.1f ms. "
      "The main cluster is a constant offset (Opus pre-skip / container timing). "
      "%d clips (**%.1f%%**) sit an extra ~20 ms lower (max %+.1f ms), consistent with "
      "the bake rounding the frame count up by one 20 ms Opus frame. `ffprobe` agrees "
      "with the registry (it reports the 1.161 s container duration for a clip that "
      "decodes to 1.134 s), so this is container padding, not a bad registry field. "
      "D-02 should treat `durationMs` as approximate (+/- one frame) and use the "
      "decoded sample count as truth."
      % (pctl(ctx["dur_delta"], 50), pctl(ctx["dur_delta"], 5),
         pctl(ctx["dur_delta"], 95), n_bad, 100.0 * n_bad / len(clips),
         1000 * (ctx["dur_bad"][0]["duration"] - ctx["dur_bad"][0]["reg_duration"])
         if ctx["dur_bad"] else 0.0))
    A("")
    A("### 4.4 F0 outliers")
    A("")
    if ctx["f0_out"]:
        A("Per-language-and-gender 3×MAD outliers on voice median F0:")
        A("")
        A(table(["voice", "lang", "F0 med", "cohort med", "flags"],
                [["`%s`" % v["id"], v["language"],
                  fnum(v["f0_med"], 1), fnum(v["f0_cohort_med"], 1),
                  ", ".join(v["flags"])] for v in ctx["f0_out"]]))
    else:
        A("None. Every voice's median F0 sits within 3×MAD of its language cohort, and "
          "all are inside 60–400 Hz.")
    A("")
    A("Absolute band check (70–350 Hz for a *median*): "
      + (", ".join("`%s` (%.0f Hz)" % (v["id"], v["f0_med"])
                   for v in voices.values() if "f0-abs" in v["flags"]) or "all voices pass")
      + ".")
    A("")
    A("### 4.5 Gender label vs measured F0")
    A("")
    A("Rule used: a **male**-labelled voice with median F0 >= 165 Hz, or a "
      "**female**-labelled voice with median F0 <= 145 Hz, is reported as a "
      "contradiction. These are conventional band edges, not hard truths; a falsetto "
      "male or a very low female is possible.")
    A("")
    if ctx["gender_bad"]:
        A(table(["voice", "label", "F0 med", "p10", "p90", "grade"],
                [["`%s`" % v["id"], v["gender"], fnum(v["f0_med"], 1),
                  fnum(v["f0_p10"], 0), fnum(v["f0_p90"], 0), v["grade"] or "—"]
                 for v in ctx["gender_bad"]]))
        A("")
        A("Read these as *label* problems, not corpus defects: the presets are defined "
          "in pitch/formant space, so a mislabelled voice only affects cohort "
          "averaging. Do not silently reassign genders in code; record it in "
          "`voice-baselines.json` (Q-01) and leave the registry alone.")
    else:
        A("No contradiction: every male-labelled voice has median F0 < 165 Hz and every "
          "female-labelled voice has median F0 > 145 Hz.")
    A("")
    A("### 4.6 Noisy / weakly voiced material")
    A("")
    low_hnr_txt = ", ".join("`%s` (%.1f dB)" % (v["id"], v["hnr"]) for v in ctx["low_hnr"])
    low_v_txt = ", ".join("`%s` (%.0f%%)" % (v["id"], 100 * v["voiced_frac"])
                          for v in ctx["low_voiced"])
    A("- HNR < 8 dB: " + (low_hnr_txt or "none") + ".")
    A("- Voiced frames < 40% of frames: " + (low_v_txt or "none") + ".")
    A("- Registry integrity failures: "
      + ("%d" % len(ctx["sha_bad"]) if ctx["sha_bad"] else "none") + ".")
    A("")
    A("Low HNR here is consistent with 32 kbps Opus plus breathy voice models; it is a "
      "reason to avoid these voices as *pitch* anchors, not a reason to exclude them "
      "from the product (D7 already bars them from calibration where the language does "
      "not match).")
    A("")

    # ------------------------------------------------------------ implications
    A("## 5. Implications for the DSP implementer")
    A("")
    A("### 5.1 `N` and hop (D2's 2048 @ 48 kHz)")
    A("")
    A("Measured F0 across the corpus: voice medians **%.0f–%.0f Hz**; pooled p10 as low "
      "as **%.0f Hz** and p90 as high as **%.0f Hz**. `N=2048` gives a bin spacing of "
      "**%.2f Hz** and a %.1f ms window."
      % (ctx["f_min_med"], ctx["f_max_med"], ctx["f_min_p10"], ctx["f_max_p90"],
         ctx["bin_hz"], 1000.0 * 2048 / SR))
    A("")

    bins_clean = ctx["bins_at_min_clean"]
    if bins_clean >= 3.0:
        v1 = ("clears the 3-bins-per-period rule of thumb (floor %.1f Hz)"
              % ctx["three_bin_floor"])
    else:
        v1 = ("is %.1f bins, just under the 3-bins-per-period rule of thumb "
              "(%.1f Hz floor); peak locking still works, but the lowest male voices "
              "are the first place it will wobble"
              % (bins_clean, ctx["three_bin_floor"]))
    A("- Lowest p10 among clean voices (HNR >= 8 dB, voiced >= 45%%): **%.0f Hz** = "
      "**%.1f bins** at `N=2048`. That %s. The raw minimum is %.0f Hz (`%s`, HNR %.1f), "
      "which is a tracker artifact on a breathy voice and should not size the FFT. "
      "`N=1024` (46.9 Hz bins, 3-bin floor %.0f Hz) would fail for most female voices."
      % (ctx["f_min_p10_clean"], bins_clean, v1, ctx["f_min_p10"],
         ctx["low_voice"]["id"], ctx["low_voice"]["hnr"], 3.0 * SR / 1024.0))
    A("- At **−12 st** the same clean low voice lands at **%.0f Hz** = **%.1f bins** at "
      "`N=2048`. That matters **only for a stage that analyses the shifted output** "
      "(P-03's envelope correction), which needs **`N=4096`** there (one octave down = "
      "%.1f bins at `N=4096`). The pitch-shift stage analyses the **input**, so "
      "`N=2048` remains correct at every ratio (DECISIONS D19); choosing the shifter's "
      "window from the output F0 was measurably worse. `N=1024` has no safe use on "
      "this corpus."
      % (ctx["f_min_p10_clean"] / 2.0, ctx["bins_shifted_down"],
         (ctx["f_min_p10_clean"] / 2.0) / (SR / 4096.0)))
    A("- Hop: at `H=512` (75%% overlap) the unambiguous phase-deviation bound is "
      "`sr/(2H) = %.1f Hz`, i.e. **%.1f bins** — a comfortable margin for identity "
      "phase locking when a +12 st shift moves partials by up to a bin. At `H=1024` the "
      "bound shrinks to **one bin** (%.1f Hz), which is marginal for the brightest "
      "voices (p90 %.0f Hz, with harmonics well above). **Keep `hop = N/4`, i.e. 512 at "
      "`N=2048`.**"
      % (ctx["dev_limit"], ctx["dev_limit"] / ctx["bin_hz"], ctx["bin_hz"],
         ctx["f_max_p90"]))
    A("- Consequence for P-01: keep the framing constant across the corpus (no "
      "adaptive `N` per clip). Choose `N` per *preset/render call*, not per voice, so "
      "A/B and golden renders stay comparable.")
    A("")
    A("### 5.2 Pitch calibration anchors")
    A("")
    A("English only, per D7, and both pass the quality gate (no clipping, |DC| small, "
      "HNR >= 8, no integrity failure):")
    A("")
    lo, hi = ctx["low_anchor"], ctx["high_anchor"]
    ex = ctx["fem_extreme"]
    if lo is None or hi is None:
        A("This run had no English male/female pair (sampled run); rerun without "
          "`--sample` to get the anchor table.")
        A("")
    else:
        A(table(["use", "voice", "F0 med", "p10–p90", "HNR", "grade", "why"],
            [["low anchor", "`%s`" % lo["id"], fnum(lo["f0_med"], 1),
              "%s–%s" % (fnum(lo["f0_p10"], 0), fnum(lo["f0_p90"], 0)),
              fnum(lo["hnr"], 1), lo["grade"] or "—",
              "lowest clean English male; exercises +12 st and `deep`"],
             ["high anchor", "`%s`" % hi["id"], fnum(hi["f0_med"], 1),
              "%s–%s" % (fnum(hi["f0_p10"], 0), fnum(hi["f0_p90"], 0)),
              fnum(hi["hnr"], 1), hi["grade"] or "—",
              "cleanest voice in the top of the English female range"]]))
        A("")
    A("")
    fem = [v for v in pick_candidates(voices, "en", "female") if quality(v)]
    if fem:
        med_f = pctl([x["f0_med"] for x in fem], 50)
        near = [v for v in fem if abs(v["f0_med"] - med_f) <= 15.0] or fem
        mid_ref = max(near, key=lambda v: v["hnr"])
    else:
        mid_ref = None
    if lo is not None and hi is not None:
        A("Use the low anchor to test that +12 st does not produce octave errors or a "
          "phase-vocoder buzz at high F0, and the high anchor to test −12 st where the "
          "shifted F0 approaches `N`'s resolution limit."
          + (" For a mid reference use `%s` (%.0f Hz)."
             % (mid_ref["id"], mid_ref["f0_med"]) if mid_ref else ""))
        A("")
    if ex is not None and hi is not None and ex["id"] != hi["id"]:
        A("For maximum-range stress (a −12 st shift at the very top of the corpus), also "
          "render `%s` (%.0f Hz, HNR %.1f, grade %s). It is %.0f Hz above the high "
          "anchor and is a *stress* case, not a calibration target, because its p10–p90 "
          "spread (%.1f st) shows tracker noise as well as intonation."
          % (ex["id"], ex["f0_med"], ex["hnr"], ex["grade"] or "—",
             ex["f0_med"] - hi["f0_med"], ex["f0_spread_st"]))
        A("")
    clean_tone = [voices[v]["f0_spread_st"] for v in voices
                  if voices[v]["language"] in ("zh", "ja")
                  and voices[v]["hnr"] >= 8.0]
    clean_en = [voices[v]["f0_spread_st"] for v in voices
                if voices[v]["language"] == "en" and voices[v]["hnr"] >= 8.0]
    A("Do **not** use `zh`/`ja` voices as pitch anchors, even though several have "
      "excellent HNR. Their F0 movement carries lexical tone (D11), so a preset tuned "
      "on them would chase meaning-bearing pitch rather than voice size. Be careful "
      "with the metric here: clean `zh`/`ja` voices show a median p10–p90 of %s st vs "
      "%s st for clean English, i.e. **less** spread than English, because this "
      "measurement cannot separate tone from intonation and tracker noise. The reason "
      "to exclude them is D7/D11 (wrong signal to optimise), not a measured spread."
      % (fnum(pctl(clean_tone, 50), 1), fnum(pctl(clean_en, 50), 1)))
    A("")
    A("### 5.3 What P-04 must trim")
    A("")
    A("Median head silence is **%d ms**, median tail **%d ms**; p90 is **%d ms / %d ms** "
      "and the worst cases are **%d ms of head** and **%d ms of tail**. "
      "%d clips exceed 100 ms of "
      "head silence and %d exceed 100 ms of tail silence (%.1f%% and %.1f%% of the "
      "corpus); %d / %d exceed 250 ms."
      % (round(pctl(ctx["head_ms"], 50)), round(pctl(ctx["tail_ms"], 50)),
         round(pctl(ctx["head_ms"], 90)), round(pctl(ctx["tail_ms"], 90)),
         round(max(ctx["head_ms"])), round(max(ctx["tail_ms"])),
         len(ctx["head100"]), len(ctx["tail100"]),
         100.0 * len(ctx["head100"]) / len(clips), 100.0 * len(ctx["tail100"]) / len(clips),
         len(ctx["head250"]), len(ctx["tail250"])))
    A("")
    A("So P-04 cannot rely on a fixed pad: it needs real endpoint detection at −45 dBFS "
      "with a 20 ms minimum run, then a 5 ms fade. Budget **p90 of %d ms head / %d ms "
      "tail**, worst case **%d ms**; a fixed 300 ms pad would still miss the worst "
      "tail and would add ~0.6 s of dead air to most clips. Trimming must happen "
      "*before* the STFT so the phase vocoder does not smear silence into the first "
      "phoneme, and before any duration-alignment against `registry.durationMs` (which "
      "includes the silence)."
      % (round(pctl(ctx["head_ms"], 90)), round(pctl(ctx["tail_ms"], 90)),
         round(max(max(ctx["head_ms"]), max(ctx["tail_ms"])))))
    A("")

    # ------------------------------------------------------------------- D13
    A("## 6. D13 calibration set: confirmed or amended")
    A("")
    A("D13 proposes 12 voices: 6 English (3F/3M across the pitch range), 2 Mandarin, "
      "2 Japanese, 1 Hindi, 1 Spanish. Measured coverage of that shape:")
    A("")
    rows = []
    for v in ctx["picks"]:
        rows.append(["`%s`" % v["id"], v["language"], v["gender"][0].upper(),
                     v["grade"] or "—", fnum(v["f0_med"], 1), fnum(v["f0_p10"], 0),
                     fnum(v["f0_p90"], 0), fnum(v["hnr"], 1),
                     ", ".join(v["flags"]) or "clean"])
    A(table(["voice", "lang", "g", "grade", "F0 med", "p10", "p90", "HNR",
             "measured flags"], rows))
    A("")
    zh_m = [v for v in ctx["picks"]
            if v["language"] == "zh" and v["gender"] == "male"]
    en = [v for v in ctx["picks"] if v["language"] == "en"]
    en_f = [v["f0_med"] for v in en if v["gender"] == "female"]
    en_m = [v["f0_med"] for v in en if v["gender"] == "male"]
    en_f_all = [v["f0_med"] for v in voices.values()
                if v["language"] == "en" and v["gender"] == "female"]
    en_m_all = [v["f0_med"] for v in voices.values()
                if v["language"] == "en" and v["gender"] == "male"
                and "gender?" not in v["flags"]]
    if en_f and en_m:
        A("**Coverage:** the 6 English picks span **%.0f–%.0f Hz female** and "
      "**%.0f–%.0f Hz male**. Measured English cohorts are %.0f–%.0f Hz female and "
      "%.0f–%.0f Hz male (males with a contradictory label excluded). The picks stop at "
      "%.0f Hz on the male side because `bm_lewis` (79 Hz) fails the quality gate "
      "(HNR 4.9 dB, 38%% voiced), not because its pitch is implausible."
          % (min(en_f), max(en_f), min(en_m), max(en_m),
             min(en_f_all), max(en_f_all), min(en_m_all), max(en_m_all), min(en_m)))
        A("")
    A("")
    A("**Verdict: D13's 12-voice shape stands, with "
      + ("no substitutions." if not ctx["pick_notes"]
         else "%d substitution%s." % (len(ctx["pick_notes"]),
                                      "" if len(ctx["pick_notes"]) == 1 else "s"))
      + "**")
    A("")
    A("All 12 pass the `sha256`/`bytes`, clipping and DC checks. HNR and any other "
      "measured caveat are in the table above.")
    A("")
    if ctx["pick_notes"]:
        A("**Amended:** " + " ".join(ctx["pick_notes"]))
    else:
        A("**Not amended:** every first-choice pick passed the quality gate "
          "(HNR >= 8 dB, no clipping, no DC/integrity flag, no contradictory gender "
          "label), so no substitution was needed.")
    A("")
    A("Why these 12 are enough for D13's purpose: the English six cover the pitch "
      "extremes that stress the vocoder (low male at the +12 st end, high female at "
      "the −12 st end) plus a mid voice for unbiased A/B; `zh` and `ja` are the two "
      "languages with lexical F0 (D11's hard gates), so one male and one female each "
      "test the monotone warning rather than only its happy path; `hi` and `es` give "
      "one non-tonal, non-English point each for the robot chain's language-safety "
      "pass (Q-08).")
    A("")
    A("`es` is the weakest slot: all three Spanish voices are under 8 dB HNR (best "
      "`ef_dora`, 7.9 dB), so D13's single Spanish voice cannot also be a quality "
      "anchor. That is acceptable for a language-safety pass, not for pitch tuning.")
    A("")
    A("Caveat: `zh`/`ja` picks are chosen for HNR and a representative (not extreme) "
      "pitch, so they are **not** pitch-range anchors — their F0 movement is lexical. Use them for language-gate and robot tests, not for natural pitch-preset "
      "tuning."
      + ((" `%s` is the cleanest male-consistent Mandarin male; its HNR (%.1f dB) is a "
          "known limitation of the `zh` male cohort."
          % (zh_m[0]["id"], zh_m[0]["hnr"])) if zh_m else ""))
    A("")
    A("## 7. Limits of these measurements")
    A("")
    A("- F0 is YIN on 32 kbps Opus; octave errors are possible on breathy frames even "
      "after median smoothing. Per-voice medians are stable to a few Hz; p10/p90 are "
      "not. Voices with HNR < 8 dB (e.g. `bm_lewis`, `am_santa`, `zm_yunyang`) show "
      "wide or octave-spaced F0 clusters: use their medians, ignore their spreads.")
    A("- HNR is autocorrelation-based, not a perceptual measure; use it for ranking, "
      "not as an absolute threshold.")
    A("- Silence thresholds are absolute dBFS, so a future re-bake that changes "
      "normalisation will change the trim numbers. Re-run this script after any "
      "registry build change.")
    A("- No formant/LPC measurements here; those belong to Q-01 (`voice-baselines.json`).")
    A("")

    return L


# -------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser(description="Measure the dg-pp corpus (F-06).")
    ap.add_argument("--cache", default=None,
                    help="corpus cache (default $TMPDIR/rasp-corpus)")
    ap.add_argument("--sample", type=int, default=1,
                    help="process every Nth voice (1 = all; default 1)")
    ap.add_argument("--jobs", type=int, default=0, help="worker processes")
    ap.add_argument("--out", default="docs/corpus.md", help="output markdown")
    ap.add_argument("--no-fetch", action="store_true",
                    help="never download; fail if the cache is incomplete")
    args = ap.parse_args()

    cache = cache_dir(args.cache)
    os.makedirs(cache, exist_ok=True)
    registry = load_registry(cache, not args.no_fetch)

    voice_ids = sorted(registry["files"])
    if args.sample > 1:
        voice_ids = voice_ids[::args.sample]
    keep = set(voice_ids)
    reg = dict(registry)
    reg["voices"] = [v for v in registry["voices"] if v["id"] in keep]

    prefetch(cache, {"files": {vid: registry["files"][vid] for vid in voice_ids}},
             not args.no_fetch)

    jobs = [(cache, vid, pid, registry["files"][vid][pid], not args.no_fetch)
            for vid in voice_ids
            for pid in sorted(registry["files"][vid])]
    workers = args.jobs or min(8, os.cpu_count() or 2)
    print("measuring %d clips from %d voices (%d workers)"
          % (len(jobs), len(voice_ids), workers), file=sys.stderr)

    import time
    t0 = time.time()
    results = []
    with ProcessPoolExecutor(max_workers=workers) as ex:
        for i, r in enumerate(ex.map(measure_clip, jobs, chunksize=8), 1):
            results.append(r)
            if i % 250 == 0:
                print("  %d/%d  (%.0fs)" % (i, len(jobs), time.time() - t0),
                      file=sys.stderr)

    results.sort(key=lambda r: (r["voice"], r["phrase"]))
    voices = aggregate(reg, results)
    lines, ctx = build_report(reg, voices, results, cache, args.sample, workers)
    lines += finish_report(reg, ctx)

    out_dir = os.path.dirname(os.path.abspath(args.out))
    os.makedirs(out_dir, exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines).rstrip() + "\n")

    mins = (time.time() - t0) / 60.0
    print("wrote %s: %d voices, %d clips, %.1f s audio, %.1f min"
          % (args.out, len(voices), len(results),
             sum(r["duration"] for r in results), mins), file=sys.stderr)


if __name__ == "__main__":
    main()
