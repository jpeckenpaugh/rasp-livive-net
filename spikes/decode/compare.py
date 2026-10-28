#!/usr/bin/env python3
"""Compare decoded f32 PCM against ffmpeg's reference.

usage: compare.py <ref.f32> <test.f32> [--lag N]
Prints JSON: lengths, sample delta, best lag, SNR dB over the overlap.
"""
import array
import json
import math
import sys


def load(path):
    a = array.array("f")
    with open(path, "rb") as f:
        a.fromfile(f, (lambda n: n)(__import__("os").path.getsize(path) // 4))
    return a


def snr(ref, test, lag):
    # positive lag: test is delayed by `lag` samples relative to ref
    if lag >= 0:
        a = ref[: len(ref) - lag]
        b = test[lag:]
    else:
        a = ref[-lag:]
        b = test[: len(test) + lag]
    n = min(len(a), len(b))
    sig = 0.0
    err = 0.0
    peak = 0.0
    for i in range(n):
        r = a[i]
        d = b[i]
        sig += r * r
        e = r - d
        err += e * e
        if abs(e) > peak:
            peak = abs(e)
    if err == 0.0:
        return 200.0, peak, n
    return 10.0 * math.log10(sig / err), peak, n


def main():
    ref = load(sys.argv[1])
    test = load(sys.argv[2])
    maxlag = 0
    if "--lag" in sys.argv:
        maxlag = int(sys.argv[sys.argv.index("--lag") + 1])
    best = None
    for lag in range(-maxlag, maxlag + 1):
        v, peak, n = snr(ref, test, lag)
        if best is None or v > best[0]:
            best = (v, peak, n, lag)
    v, peak, n, lag = best
    print(
        json.dumps(
            {
                "ref_samples": len(ref),
                "test_samples": len(test),
                "delta": len(test) - len(ref),
                "lag": lag,
                "overlap": n,
                "snr_db": round(v, 2),
                "max_abs_err": round(peak, 6),
            }
        )
    )


if __name__ == "__main__":
    main()
