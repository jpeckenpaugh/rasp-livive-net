#!/usr/bin/env python3
"""Build the HR-2 blind listening pack (PLAN U-12a).

The pack is a folder that a non-technical tester can open with a double click:

    <out>/index.html      self-contained page (item data inlined, no fetch)
    <out>/qa-data.json    the same data, for serving the generic web/qa.html over HTTP
    <out>/audio/aNN.wav   the six originals (one per voice, shared by four trials)
    <out>/audio/bNN.wav   the 24 processed renders

Nothing in a file name says which preset produced it: presets are base64 in the
inlined JSON and are decoded only for the CSV export and the end-of-round
summary. This is a soft blind (the data is on the tester's own disk) — it stops
the page from telling the tester, which is the point of the round.

Usage
-----
    python3 scripts/make_hr2_pack.py
    python3 scripts/make_hr2_pack.py --out ~/rasp-hr2 --seed 20261028
    python3 scripts/make_hr2_pack.py --voices am_onyx,af_heart --presets shift-up

Python 3.8+ standard library only. Builds `rasp` in release mode when the
binary is missing or older than its sources.
"""

import argparse
import base64
import hashlib
import json
import math
import subprocess
import sys
import wave
from array import array
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "rasp"
QA_HTML = ROOT / "web" / "qa.html"
QA_JS = ROOT / "web" / "qa.js"
CORPUS = ROOT / "web" / "audio"

SCHEMA = "rasp-hr2-pack/1"
DEFAULT_SEED = 20261028

# The fixed stimulus set (U-12a). The reason column is documented, not shipped.
STIMULI = [
    ("af_heart", "watch-this", "plosives + fricatives, high F0"),
    ("am_onyx", "got-time", "low anchor; shift-up is the known p90-failing cell (28.58 c)"),
    ("bf_alice", "good-luck", "British F, stress case"),
    ("af_aoede", "good-to-see", "high F0, formant stress"),
    ("zf_xiaoni", "greet", "Mandarin, shortest clip"),
    ("jf_nezumi", "be-friends", "Japanese, cleanest voice"),
]

# D12 preset ids. `natural` is deliberately not a trial: it is the A side.
PRESETS = ["shift-up", "shift-down", "bright", "deep"]

QUESTION_CODEBOOK = {
    "q1": {"1": "clearly same person", "5": "clearly different person"},
    "q2": {"1": "just pitch (thin/chipmunk)", "5": "convincingly bigger/smaller body"},
    "q3": {"1": "crisper", "2": "unchanged", "3": "clickier"},
    "q4": {"1": "no artifact", "2": "mild", "3": "obvious"},
    "q5": {"1": "yes, ship", "2": "maybe", "3": "no"},
}

MASK64 = (1 << 64) - 1


class SplitMix64:
    """Tiny deterministic PRNG.

    `random.shuffle` is not guaranteed identical across Python versions. The
    trial order must be reproducible from `--seed` alone, so this script owns
    the generator and the Fisher-Yates loop.
    """

    def __init__(self, seed):
        self.state = seed & MASK64

    def next_u64(self):
        self.state = (self.state + 0x9E3779B97F4A7C15) & MASK64
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK64
        return z ^ (z >> 31)

    def below(self, n):
        """Uniform integer in [0, n), rejection-sampled (no modulo bias)."""
        limit = (1 << 64) - ((1 << 64) % n)
        while True:
            value = self.next_u64()
            if value < limit:
                return value % n


def shuffle(items, seed):
    rng = SplitMix64(seed)
    for i in range(len(items) - 1, 0, -1):
        j = rng.below(i + 1)
        items[i], items[j] = items[j], items[i]
    return items


def order_digest(ordered):
    digest = hashlib.sha256()
    digest.update(b"rasp-hr2-order-v1\n")
    for trial in ordered:
        line = "{order}\t{trial}\t{voice}\t{phrase}\t{preset}\n".format(**trial)
        digest.update(line.encode("utf-8"))
    return digest.hexdigest()


def fail(message):
    print("error: " + message, file=sys.stderr)
    raise SystemExit(1)


def parse_selection(raw, allowed, label):
    if not raw:
        return list(allowed)
    chosen = []
    for chunk in raw:
        for value in chunk.split(","):
            value = value.strip()
            if value and value not in chosen:
                chosen.append(value)
    unknown = [value for value in chosen if value not in allowed]
    if unknown:
        fail(
            "unknown {}: {}. Available: {}".format(
                label, ", ".join(unknown), ", ".join(allowed)
            )
        )
    if not chosen:
        fail("no {} selected".format(label))
    return chosen


def ensure_binary(rebuild):
    need = rebuild or not BIN.exists()
    if not need:
        binary_mtime = BIN.stat().st_mtime
        sources = []
        for pattern in ("crates/*/src/**/*.rs", "crates/*/Cargo.toml"):
            sources.extend(ROOT.glob(pattern))
        sources.extend([ROOT / "Cargo.toml", ROOT / "Cargo.lock"])
        newest = max((path.stat().st_mtime for path in sources if path.exists()), default=0.0)
        need = newest > binary_mtime
    if not need:
        return
    print("building rasp (cargo build -p rasp-cli --release)…")
    proc = subprocess.run(
        ["cargo", "build", "-p", "rasp-cli", "--release"],
        cwd=str(ROOT),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if proc.returncode != 0:
        print(proc.stdout, file=sys.stderr)
        fail("cargo build failed")
    if not BIN.exists():
        fail("cargo build succeeded but {} is missing".format(BIN))


def run_json(args):
    proc = subprocess.run(args, cwd=str(ROOT), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if proc.returncode != 0:
        fail(
            "{} failed (exit {})\n{}".format(
                " ".join(args[:2]), proc.returncode, proc.stderr.strip()
            )
        )
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        fail("could not parse rasp output: {}\n{}".format(exc, proc.stdout[:400]))


def wav_stats(path):
    """Frames, sample peak and RMS measured from the written file."""
    with wave.open(str(path), "rb") as handle:
        channels = handle.getnchannels()
        rate = handle.getframerate()
        width = handle.getsampwidth()
        frames = handle.getnframes()
        raw = handle.readframes(frames)
    if channels != 1 or rate != 48000 or width != 2:
        fail(
            "{}: expected 48000 Hz mono 16-bit, got {} Hz / {} ch / {} bytes".format(
                path, rate, channels, width
            )
        )
    samples = array("h")
    samples.frombytes(raw)
    if sys.byteorder == "big":
        samples.byteswap()
    count = len(samples)
    if count == 0:
        fail("{}: empty WAV".format(path))
    peak = max(abs(value) for value in samples) / 32768.0
    rms = math.sqrt(sum(value * value for value in samples) / count) / 32768.0
    return {"frames": frames, "peak": peak, "rms": rms, "rate": rate}


def db(value):
    return 20.0 * math.log10(value) if value > 0 else float("-inf")


def render_pack(out_dir, seed, voices, presets, fmt):
    if out_dir.resolve() == ROOT.resolve() or (out_dir / ".git").exists():
        fail("refusing to write a pack into a repository root: {}".format(out_dir))

    audio_dir = out_dir / "audio"
    audio_dir.mkdir(parents=True, exist_ok=True)
    for stale in audio_dir.glob("*.wav"):
        stale.unlink()

    voice_index = {voice: index + 1 for index, (voice, _, _) in enumerate(STIMULI)}

    # Fixed grid, stable trial ids independent of the shuffle.
    grid = []
    number = 0
    for voice, phrase, reason in STIMULI:
        if voice not in voices:
            continue
        for preset in PRESETS:
            if preset not in presets:
                continue
            number += 1
            grid.append(
                {
                    "trial": "t{:02d}".format(number),
                    "voice": voice,
                    "phrase": phrase,
                    "preset": preset,
                    "reason": reason,
                }
            )
    if not grid:
        fail("the selected voices and presets produced no trials")

    print("RASP HR-2 blind pack")
    print("  seed        {}".format(seed))
    print("  presets     {}".format(", ".join(presets)))
    print("  voices      {}".format(", ".join(voices)))
    print("  output      {}".format(out_dir))
    print("")

    originals = {}
    print("Rendering originals ({})".format(len(voices)))
    for voice, phrase, _ in STIMULI:
        if voice not in voices:
            continue
        source = CORPUS / voice / (phrase + ".weba")
        if not source.exists():
            fail("missing corpus clip: {}".format(source))
        target = audio_dir / "a{:02d}.wav".format(voice_index[voice])
        decoded = run_json(
            [
                str(BIN),
                "decode",
                "--in",
                str(source),
                "--format",
                fmt,
                "--out",
                str(target),
                "--json",
            ]
        )
        stats = wav_stats(target)
        if decoded.get("samples") != stats["frames"]:
            fail(
                "{}: decode reported {} samples, WAV has {}".format(
                    target, decoded.get("samples"), stats["frames"]
                )
            )
        originals[voice] = {
            "path": target,
            "name": target.name,
            "frames": stats["frames"],
            "rms": stats["rms"],
            "peak": stats["peak"],
            "source": source,
            "phrase": phrase,
        }
        print(
            "  a{:02d}.wav  {:<10} {:<14} {:>7} frames  {:>6.2f} dBFS peak".format(
                voice_index[voice],
                voice,
                phrase,
                stats["frames"],
                db(stats["peak"]),
            )
        )

    ordered = shuffle([dict(trial) for trial in grid], seed)
    for position, trial in enumerate(ordered, start=1):
        trial["order"] = position

    print("")
    print("Rendering processed trials ({})".format(len(ordered)))
    rows = []
    worst_tp = float("-inf")
    worst_delta = 0.0
    errors = []
    for trial in ordered:
        source = CORPUS / trial["voice"] / (trial["phrase"] + ".weba")
        target = audio_dir / "b{}.wav".format(trial["trial"][1:])
        rendered = run_json(
            [
                str(BIN),
                "render",
                "--in",
                str(source),
                "--preset",
                trial["preset"],
                "--format",
                fmt,
                "--loudness",
                "on",
                "--out",
                str(target),
                "--json",
            ]
        )
        stats = wav_stats(target)
        original = originals[trial["voice"]]
        disk_delta = abs(db(stats["rms"]) - db(original["rms"]))
        reported_delta = abs(float(rendered.get("levelDeltaDb", 0.0)))
        true_peak = float(rendered.get("truePeakDbtp", 0.0))

        if stats["frames"] != original["frames"]:
            errors.append(
                "{}: {} samples, source has {} (trim must stay off)".format(
                    target.name, stats["frames"], original["frames"]
                )
            )
        if rendered.get("samplesOut") != stats["frames"]:
            errors.append("{}: rasp samplesOut disagrees with the WAV".format(target.name))
        if rendered.get("sourceSamples") != original["frames"]:
            errors.append("{}: sourceSamples disagrees with the decoded original".format(target.name))
        if true_peak > -1.0:
            errors.append("{}: true peak {:+.3f} dBTP is above -1 dBTP".format(target.name, true_peak))
        if reported_delta > 1.0:
            errors.append("{}: level delta {:.3f} dB is above 1 dB".format(target.name, reported_delta))
        if disk_delta > 1.0:
            errors.append("{}: measured level delta {:.3f} dB is above 1 dB".format(target.name, disk_delta))

        worst_tp = max(worst_tp, true_peak)
        worst_delta = max(worst_delta, reported_delta, disk_delta)
        rows.append(
            {
                "trial": trial["trial"],
                "order": trial["order"],
                "voice": trial["voice"],
                "phrase": trial["phrase"],
                "preset": trial["preset"],
                "frames": stats["frames"],
                "true_peak": true_peak,
                "level_delta": reported_delta,
                "disk_delta": disk_delta,
                "bytes": target.stat().st_size,
                "a": "audio/" + original["name"],
                "b": "audio/" + target.name,
            }
        )

    if errors:
        print("")
        print("VERIFICATION FAILED", file=sys.stderr)
        for line in errors:
            print("  " + line, file=sys.stderr)
        raise SystemExit(1)

    digest = order_digest(ordered)
    payload = {
        "schema": SCHEMA,
        "generator": "scripts/make_hr2_pack.py",
        "createdUtc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "seed": seed,
        "orderSha256": digest,
        "format": fmt,
        "sampleRate": 48000,
        "codebook": QUESTION_CODEBOOK,
        "trials": [
            {
                "trial": row["trial"],
                "order": row["order"],
                "voice": row["voice"],
                "phrase": row["phrase"],
                "a": row["a"],
                "b": row["b"],
                "p": base64.b64encode(row["preset"].encode("ascii")).decode("ascii"),
            }
            for row in rows
        ],
    }

    index_html = build_index(payload)
    data_json = json.dumps(payload, indent=2, ensure_ascii=False) + "\n"
    (out_dir / "index.html").write_text(index_html, encoding="utf-8")
    (out_dir / "qa-data.json").write_text(data_json, encoding="utf-8")

    wav_files = sorted(audio_dir.glob("*.wav"))
    payload_bytes = len(index_html.encode("utf-8")) + len(data_json.encode("utf-8"))
    total_bytes = sum(path.stat().st_size for path in wav_files) + payload_bytes

    print("")
    print("Verification")
    print("  sample count   {}/{} renders equal their source (no trim)".format(len(rows), len(rows)))
    print("  true peak      worst {:+.2f} dBTP; all <= -1.0 dBTP".format(worst_tp))
    print("  level delta    worst {:.3f} dB; all <= 1.0 dB".format(worst_delta))
    print("  wav format     48000 Hz, mono, 16-bit")
    print("  order sha256   {}".format(digest))
    print("")
    print("Pack contents (presentation order)")
    print(
        "  {:<5} {:<6} {:<10} {:<14} {:<10} {:>7} {:>9} {:>7} {:>9}".format(
            "order", "trial", "voice", "phrase", "preset", "frames", "dBTP", "delta", "bytes"
        )
    )
    for row in rows:
        print(
            "  {:<5} {:<6} {:<10} {:<14} {:<10} {:>7} {:>+9.2f} {:>+7.3f} {:>9}".format(
                row["order"],
                row["trial"],
                row["voice"],
                row["phrase"],
                row["preset"],
                row["frames"],
                row["true_peak"],
                row["level_delta"],
                row["bytes"],
            )
        )
    print("")
    print(
        "  files  {} ({} WAV + index.html + qa-data.json)".format(
            len(wav_files) + 2, len(wav_files)
        )
    )
    print("  total  {:,} bytes ({:.1f} MiB)".format(total_bytes, total_bytes / (1024 * 1024)))
    print("")
    print("Open:")
    print("  file://{}".format((out_dir / "index.html").resolve()))
    print("Or serve it over HTTP (needed if a browser blocks local files):")
    print("  python3 -m http.server 8091 --directory {}".format(out_dir))


def build_index(payload):
    html = QA_HTML.read_text(encoding="utf-8")
    script = QA_JS.read_text(encoding="utf-8")
    if "</script" in script.lower():
        fail("web/qa.js contains '</script' and cannot be inlined safely")

    data_json = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    # A JSON string can never contain a literal "</script>" from our data, but
    # escape the sequence anyway so the inline script cannot be closed early.
    data_json = data_json.replace("</", "<\\/")

    data_tag = '<script id="qa-data" type="application/json">null</script>'
    script_tag = '<script src="qa.js"></script>'
    if data_tag not in html:
        fail("web/qa.html is missing the inline-data placeholder")
    if script_tag not in html:
        fail("web/qa.html is missing the qa.js script tag")

    html = html.replace(
        data_tag,
        '<script id="qa-data" type="application/json">' + data_json + "</script>",
    )
    html = html.replace(script_tag, "<script>\n" + script + "\n</script>")
    return html


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Build the HR-2 blind listening pack (PLAN U-12a).",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--out",
        default="~/rasp-hr2",
        help="output directory (created; existing *.wav under <out>/audio is replaced)",
    )
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED, help="trial-order seed")
    parser.add_argument(
        "--voices",
        action="append",
        default=None,
        help="comma-separated subset of the fixed stimulus voices (repeatable)",
    )
    parser.add_argument(
        "--presets",
        action="append",
        default=None,
        help="comma-separated subset of: " + ", ".join(PRESETS) + " (repeatable)",
    )
    parser.add_argument(
        "--format",
        choices=["wav16"],
        default="wav16",
        help="audio format written into audio/",
    )
    parser.add_argument("--rebuild", action="store_true", help="force a release rebuild of rasp")
    args = parser.parse_args(argv)

    raw_presets = [value.strip() for chunk in (args.presets or []) for value in chunk.split(",")]
    if "natural" in raw_presets:
        fail("'natural' is the A side, not a trial; choose from " + ", ".join(PRESETS))
    voices = parse_selection(args.voices, [voice for voice, _, _ in STIMULI], "voice")
    presets = parse_selection(args.presets, PRESETS, "preset")

    out_dir = Path(args.out).expanduser().resolve()
    ensure_binary(args.rebuild)
    render_pack(out_dir, args.seed, voices, presets, args.format)


if __name__ == "__main__":
    main()
