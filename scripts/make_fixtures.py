#!/usr/bin/env python3
"""Generate the F-05 audio fixtures from the vendored corpus.

The twelve clips below are the pinned test fixture set (PLAN F-05, DECISIONS
D13 as amended by F-06). They are *derived* artifacts, so they are generated
locally and in CI, never committed (PLAN §8: no committed fixtures over 1 MB).
Only `fixtures/README.md` and the `.gitkeep` placeholder are tracked.

Each clip is decoded three ways, because the three consumers want different
things:

    fixtures/wav/<voice>__<phrase>.wav        48 kHz mono 16-bit PCM (listening,
                                              tools that only read WAV)
    fixtures/f32/<voice>__<phrase>.f32        48 kHz mono raw little-endian f32
                                              (numeric tests, no header)
    fixtures/reference/<voice>__<phrase>.f32  same, decoded by **libopus**
                                              (`ffmpeg -c:a libopus`)

The `reference/` tree is the only valid oracle. `ffmpeg -i` with no decoder
override uses FFmpeg's native Opus implementation, which is *not* the same
waveform as libopus and is not sample-aligned (D-01, `docs/spikes/decode.md`).

Clips are decoded whole: leading and trailing silence is preserved so P-04's
trim work has something to trim. Nothing is normalised, resampled or faded.

Sources of truth
----------------
- `web/registry.json` decides what exists, which phrase group a phrase
  belongs to, and what the expected `sha256` / `bytes` of each source clip is.
  Every chosen clip is hash-verified before it is decoded; a mismatch is fatal
  and names the command that repairs the corpus.
- `SELECTION` in this file decides which twelve clips are fixtures and *why*.
  The reason is copied into `fixtures/manifest.json`.

Idempotency
-----------
`fixtures/manifest.json` records the source hash, the decoder signature and
the hash of every generated file. A clip is rebuilt only if the source hash,
the decoder signature, or one of its outputs changed. A run with nothing to do
decodes nothing and writes nothing; `--force` rebuilds everything.

Exit codes: 0 success (including "nothing to do"), 1 any failure, 130 Ctrl-C.

Examples
--------
    python3 scripts/make_fixtures.py
    python3 scripts/make_fixtures.py --force
    python3 scripts/make_fixtures.py --out target/fixtures

Requires Python 3.8+ (standard library only) and `ffmpeg` built with libopus.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import wave
from pathlib import Path
from typing import Dict, List, NamedTuple, NoReturn, Optional, Tuple

SAMPLE_RATE = 48000
CHANNELS = 1
F32_BYTES_PER_SAMPLE = 4
SCHEMA_VERSION = 1
SCRIPT_NAME = "scripts/make_fixtures.py"

# Matches scripts/vendor_corpus.py's DEFAULT_UPSTREAM. The vendored registry
# stores paths relative to `web/`, so the upstream URL is reconstructed here
# rather than read from it. Nothing is fetched: this value is provenance only.
CORPUS_ORIGIN = "https://dg-pp.livive.net"

OUTPUT_DIRS = ("wav", "f32", "reference")

#: Shortest / longest acceptable fixture set coverage, enforced after decode.
SHORT_MAX_SECONDS = 0.50
MEDIAN_SECONDS = (0.90, 1.10)
LONG_MIN_SECONDS = 1.90


class Selection(NamedTuple):
    """One pinned fixture: which clip, and why it earned its slot."""

    voice: str
    phrase: str
    group: str
    why: str


#: The fixture set. Order is the table/deterministic manifest order.
#:
#: Coverage rules this satisfies (checked at runtime):
#:   - 12 voices, one clip each, per D13 as amended by F-06.
#:   - every phrase group appears at least once;
#:   - the set spans short (~0.4 s), median (~1 s) and long (>= 1.9 s) material;
#:   - the English pitch anchors (low 88 Hz, high 221 Hz) and the 248 Hz stress
#:     voice, the two languages with lexical F0 (zh, ja), the D7 language
#:     mismatch (hi reading English) and the weak-signal Spanish slot are all in.
SELECTION: Tuple[Selection, ...] = (
    Selection(
        "af_heart",
        "friends-now",
        "friendship",
        "default registry voice; ~1 s median-length baseline decode case",
    ),
    Selection(
        "af_aoede",
        "you-got-me",
        "win",
        "high F0 anchor (221 Hz): -12 st approaches N=2048's resolution limit",
    ),
    Selection(
        "bf_alice",
        "maybe-next-time",
        "close",
        "highest English F0 (248 Hz) with enough duration to track: -12 st stress",
    ),
    Selection(
        "am_onyx",
        "got-time",
        "play",
        "low F0 anchor (88 Hz) and longest curated clip (1.75 s): +12 st octave test",
    ),
    Selection(
        "am_eric",
        "good-game",
        "loss",
        "mid English male (162 Hz): unbiased A/B and preset calibration reference",
    ),
    Selection(
        "bf_emma",
        "nice-day",
        "ambient",
        "worst measured tail silence in the corpus (456 ms): P-04 trim case",
    ),
    Selection(
        "zf_xiaoni",
        "greet",
        "greeting",
        "shortest clip in the corpus (0.42 s): one-shot stress; zh monotone gate = 0",
    ),
    Selection(
        "zm_yunxi",
        "no-worries",
        "decline",
        "Mandarin male: the second half of D11's hard language gate",
    ),
    Selection(
        "jf_nezumi",
        "be-friends",
        "friendship",
        "cleanest voice in the corpus (15.5 dB HNR): lexical-pitch reference, ja gate",
    ),
    Selection(
        "jm_kumo",
        "watch-this",
        "play",
        "Japanese male at 86 Hz: low pitch plus lexical accent, ja gate",
    ),
    Selection(
        "hf_alpha",
        "good-to-see",
        "greeting",
        "longest curated clip (2.06 s); hi reading English is D7 regression-only",
    ),
    Selection(
        "ef_dora",
        "see-you",
        "close",
        "Spanish slot; lowest-HNR curated voice (7.9 dB): the weak-signal case",
    ),
)


class FixtureError(Exception):
    """A fatal, user-facing fixture problem."""


# --------------------------------------------------------------------------
# paths and small helpers
# --------------------------------------------------------------------------


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def resolve_dir(value: Optional[str], default: Path) -> Path:
    path = Path(value) if value else default
    if not path.is_absolute():
        path = (Path.cwd() / path).resolve()
    return path


def display(path: Path) -> str:
    """Repo-relative path when possible; absolute only for outside paths."""
    try:
        return path.resolve().relative_to(repo_root()).as_posix()
    except ValueError:
        return str(path)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def atomic_write(path: Path, data: bytes) -> None:
    """Write bytes next to `path`, fsync, then atomically rename over it."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=".tmp-", suffix=path.suffix, dir=str(path.parent))
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(tmp, 0o644)
        os.replace(tmp, path)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


def json_bytes(document: dict) -> bytes:
    return (json.dumps(document, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def fail(message: str, fix: Optional[str] = None) -> NoReturn:
    raise FixtureError(message if not fix else "{}\nfix:   {}".format(message, fix))


# --------------------------------------------------------------------------
# inputs: registry + corpus
# --------------------------------------------------------------------------


REPAIR_HINT = (
    "python3 scripts/vendor_corpus.py\n"
    "       then: python3 scripts/vendor_corpus.py --check"
)


def load_registry(registry_path: Path) -> dict:
    if not registry_path.is_file():
        fail(
            "vendored corpus is missing: {} (PLAN D6 requires it)".format(
                display(registry_path)
            ),
            REPAIR_HINT,
        )
    try:
        with registry_path.open("r", encoding="utf-8") as handle:
            registry = json.load(handle)
    except (OSError, ValueError) as error:
        fail(
            "cannot read {}: {}".format(display(registry_path), error),
            REPAIR_HINT,
        )
    if not isinstance(registry, dict) or not isinstance(registry.get("files"), dict):
        fail(
            "{} has no `files` map; not a dg-pp registry".format(display(registry_path)),
            REPAIR_HINT,
        )
    return registry


def phrase_table(registry: dict) -> Dict[str, dict]:
    phrases = registry.get("phrases")
    if not isinstance(phrases, list):
        fail("registry has no `phrases` list", REPAIR_HINT)
    table = {}
    for phrase in phrases:
        if isinstance(phrase, dict) and isinstance(phrase.get("id"), str):
            table[phrase["id"]] = phrase
    return table


def voice_table(registry: dict) -> Dict[str, dict]:
    voices = registry.get("voices")
    if not isinstance(voices, list):
        fail("registry has no `voices` list", REPAIR_HINT)
    return {
        voice["id"]: voice
        for voice in voices
        if isinstance(voice, dict) and isinstance(voice.get("id"), str)
    }


def validate_selection(registry: dict) -> None:
    """Fail early if SELECTION no longer matches the pinned registry."""
    phrases = phrase_table(registry)
    voices = voice_table(registry)
    files = registry["files"]

    if len(SELECTION) != 12:
        fail("SELECTION must hold exactly 12 clips for F-05, found {}".format(len(SELECTION)))
    seen = set()
    for item in SELECTION:
        name = "{}__{}".format(item.voice, item.phrase)
        if name in seen:
            fail("SELECTION lists {} twice".format(name))
        seen.add(name)
        if item.voice not in voices:
            fail("SELECTION names unknown voice {}".format(item.voice), REPAIR_HINT)
        if item.phrase not in phrases:
            fail("SELECTION names unknown phrase {}".format(item.phrase), REPAIR_HINT)
        if item.phrase not in files.get(item.voice, {}):
            fail(
                "clip {} is not in the registry".format(name),
                REPAIR_HINT
                + "\n       (if this is a deliberate re-bake, update SELECTION in "
                + SCRIPT_NAME
                + ")",
            )
        actual_group = phrases[item.phrase].get("group")
        if actual_group != item.group:
            fail(
                "phrase {} is group {!r} in the registry, SELECTION says {!r}".format(
                    item.phrase, actual_group, item.group
                ),
                "review SELECTION in {} against the new registry".format(SCRIPT_NAME),
            )

    covered = {item.group for item in SELECTION}
    missing = sorted(set(registry.get("groups") or []) - covered)
    if missing:
        fail(
            "SELECTION does not cover phrase group(s): {}".format(", ".join(missing)),
            "add or swap a clip in {} so every group appears".format(SCRIPT_NAME),
        )


def verify_source(source: Path, entry: dict, label: str) -> None:
    """Check the source clip against the registry before spending ffmpeg on it."""
    if not source.is_file():
        fail(
            "missing corpus clip {}: {}".format(label, display(source)),
            REPAIR_HINT,
        )
    expected_bytes = entry.get("bytes")
    actual_bytes = source.stat().st_size
    if isinstance(expected_bytes, int) and actual_bytes != expected_bytes:
        fail(
            "size mismatch for {}: registry says {} B, on disk {} B".format(
                display(source), expected_bytes, actual_bytes
            ),
            REPAIR_HINT,
        )
    expected_sha = entry.get("sha256")
    actual_sha = sha256_file(source)
    if expected_sha != actual_sha:
        fail(
            "sha256 mismatch for {}:\n"
            "         registry: {}\n"
            "         on disk:  {}".format(display(source), expected_sha, actual_sha),
            REPAIR_HINT
            + "\n       (if the upstream bake changed, re-vendor deliberately with "
            + "`--refresh-registry`)",
        )


# --------------------------------------------------------------------------
# ffmpeg
# --------------------------------------------------------------------------


class Decoder(NamedTuple):
    """The ffmpeg/libopus pair that produced the fixtures."""

    path: str
    version: str

    @property
    def signature(self) -> str:
        return "ffmpeg {} + libopus".format(self.version)


def find_decoder() -> Decoder:
    executable = shutil.which("ffmpeg")
    if not executable:
        fail(
            "ffmpeg not found on PATH (fixtures are decoded with ffmpeg)",
            "macOS: brew install ffmpeg\n"
            "       Debian/Ubuntu: sudo apt-get install ffmpeg",
        )
    version = run_text([executable, "-hide_banner", "-version"], "ffmpeg -version")
    match = re.match(r"ffmpeg version (\S+)", version)
    decoder = Decoder(executable, match.group(1) if match else "unknown")

    decoders = run_text([executable, "-hide_banner", "-decoders"], "ffmpeg -decoders")
    if not any(line.split()[1:2] == ["libopus"] for line in decoders.splitlines()):
        fail(
            "this ffmpeg has no libopus decoder, so the reference fixtures "
            "cannot be produced ({} build)".format(decoder.signature),
            "install an ffmpeg built with --enable-libopus (macOS: brew install ffmpeg)",
        )
    return decoder


def run_text(command: List[str], what: str) -> str:
    try:
        proc = subprocess.run(
            command, capture_output=True, text=True, encoding="utf-8", errors="replace"
        )
    except OSError as error:
        fail("cannot run {}: {}".format(what, error))
    if proc.returncode != 0:
        stderr = (proc.stderr or "").strip()
        fail(
            "{} failed (exit {}){}".format(
                what, proc.returncode, "\n  " + stderr if stderr else ""
            ),
            "reproduce with: " + " ".join(command),
        )
    return proc.stdout


def decode(decoder: Decoder, source: Path, target: Path, mode: str) -> None:
    """Decode `source` to `target` atomically. `mode` is wav, f32 or reference."""
    target.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=".tmp-", suffix=target.suffix, dir=str(target.parent))
    os.close(fd)
    tmp = Path(tmp_name)

    command = [decoder.path, "-v", "error", "-nostdin", "-y"]
    if mode == "reference":
        command += ["-c:a", "libopus"]  # decoder override: must precede -i
    command += ["-i", str(source)]
    if mode == "wav":
        command += [
            "-map_metadata",
            "-1",
            "-fflags",
            "+bitexact",
            "-ac",
            str(CHANNELS),
            "-ar",
            str(SAMPLE_RATE),
            "-c:a",
            "pcm_s16le",
            "-f",
            "wav",
        ]
    else:
        command += ["-ac", str(CHANNELS), "-ar", str(SAMPLE_RATE), "-f", "f32le"]
    command.append(str(tmp))

    try:
        try:
            proc = subprocess.run(
                command, capture_output=True, text=True, encoding="utf-8", errors="replace"
            )
        except OSError as error:
            fail("cannot run ffmpeg: {}".format(error))
        if proc.returncode != 0:
            stderr = (proc.stderr or "").strip()
            fail(
                "ffmpeg failed to decode {} ({} mode, exit {}){}".format(
                    display(source), mode, proc.returncode,
                    "\n  " + stderr if stderr else "",
                ),
                "reproduce with: " + " ".join(command),
            )
        if tmp.stat().st_size == 0:
            fail(
                "ffmpeg produced an empty {} for {}".format(mode, display(source)),
                "reproduce with: " + " ".join(command),
            )
        with tmp.open("rb") as handle:
            os.fsync(handle.fileno())
        os.chmod(tmp, 0o644)
        os.replace(tmp, target)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


def wav_sample_count(path: Path) -> int:
    """Assert the WAV contract (48 kHz mono 16-bit) and return its frame count."""
    try:
        with wave.open(str(path), "rb") as handle:
            channels = handle.getnchannels()
            width = handle.getsampwidth()
            rate = handle.getframerate()
            frames = handle.getnframes()
    except (OSError, EOFError, wave.Error) as error:
        fail("cannot read generated WAV {}: {}".format(display(path), error))
    if (channels, width, rate) != (CHANNELS, 2, SAMPLE_RATE):
        fail(
            "generated WAV {} is {} ch / {}-bit / {} Hz, expected {} ch / 16-bit / {} Hz".format(
                display(path), channels, width * 8, rate, CHANNELS, SAMPLE_RATE
            ),
            "check the ffmpeg build and report this; the fixture contract is fixed",
        )
    return frames


# --------------------------------------------------------------------------
# manifest
# --------------------------------------------------------------------------


def clip_paths(out_root: Path, voice: str, phrase: str) -> Dict[str, Path]:
    name = "{}__{}".format(voice, phrase)
    return {
        "wav": out_root / "wav" / (name + ".wav"),
        "f32": out_root / "f32" / (name + ".f32"),
        "reference": out_root / "reference" / (name + ".f32"),
    }


def is_cached(entry: dict, source_sha: str, out_root: Path) -> bool:
    """True when this clip's outputs are present, unmodified and current."""
    if entry.get("source", {}).get("sha256") != source_sha:
        return False
    files = entry.get("files") or {}
    if set(files) != set(OUTPUT_DIRS):
        return False
    for record in files.values():
        path = out_root / record.get("path", "")
        if not path.is_file() or path.stat().st_size != record.get("bytes"):
            return False
        if sha256_file(path) != record.get("sha256"):
            return False
    return True


def build_manifest(
    registry: dict,
    decoder: Decoder,
    clips: List[dict],
) -> dict:
    build = registry.get("build") or {}
    return {
        "schemaVersion": SCHEMA_VERSION,
        "task": "F-05",
        "generatedBy": SCRIPT_NAME,
        "note": (
            "Generated, never committed (PLAN §8). Rebuild with "
            "`python3 scripts/make_fixtures.py`; `--force` regenerates."
        ),
        "audio": {
            "sampleRate": SAMPLE_RATE,
            "channels": CHANNELS,
            "wav": "48 kHz mono 16-bit PCM",
            "f32": "48 kHz mono raw little-endian float32, headerless",
            "reference": (
                "48 kHz mono raw little-endian float32 decoded by libopus "
                "(`ffmpeg -c:a libopus`); the only valid oracle (D-01)"
            ),
            "trimmed": False,
        },
        "decoder": {
            "tool": "ffmpeg",
            "version": decoder.version,
            "referenceDecoder": "libopus",
            "signature": decoder.signature,
        },
        "registry": {
            "path": "web/registry.json",
            "buildId": build.get("id"),
            "createdAt": build.get("createdAt"),
            "sourceHash": build.get("sourceHash"),
            "clipVersion": build.get("clipVersion"),
        },
        "paths": {
            "files": "relative to this manifest's directory",
            "sources": "relative to the repository's web/ directory",
        },
        "sourceCorpus": {
            "origin": CORPUS_ORIGIN,
            "webRoot": "web",
            "clipPattern": "audio/<voice>/<phrase>.weba",
            "note": (
                "Clip URLs are the vendor origin plus the registry path; the "
                "vendored registry stores only repo-relative paths."
            ),
        },
        "clips": clips,
    }


def describe_clip(
    item: Selection,
    entry: dict,
    phrase: dict,
    voice: dict,
    out_root: Path,
    files: Dict[str, Path],
    sample_count: int,
    output_sha: Dict[str, str],
) -> dict:
    name = "{}__{}".format(item.voice, item.phrase)
    source_rel = entry.get("path") or "audio/{}/{}.weba".format(item.voice, item.phrase)
    return {
        "name": name,
        "voice": item.voice,
        "phrase": item.phrase,
        "phraseGroup": item.group,
        "phraseText": phrase.get("text"),
        "language": voice.get("language"),
        "gender": voice.get("gender"),
        "reason": item.why,
        "source": {
            "path": source_rel,
            "url": "{}/{}".format(CORPUS_ORIGIN, source_rel),
            "bytes": entry.get("bytes"),
            "sha256": entry.get("sha256"),
            "registryDurationMs": entry.get("durationMs"),
            "phonemes": entry.get("phonemes"),
        },
        "sampleCount": sample_count,
        "sampleRate": SAMPLE_RATE,
        "channels": CHANNELS,
        "durationSeconds": round(sample_count / SAMPLE_RATE, 6),
        "files": {
            key: {
                "path": files[key].relative_to(out_root).as_posix(),
                "bytes": files[key].stat().st_size,
                "sha256": output_sha[key],
            }
            for key in OUTPUT_DIRS
        },
    }


# --------------------------------------------------------------------------
# build
# --------------------------------------------------------------------------


def make_fixtures(web_root: Path, out_root: Path, force: bool) -> int:
    registry_path = web_root / "registry.json"
    registry = load_registry(registry_path)
    validate_selection(registry)

    decoder = find_decoder()
    phrases = phrase_table(registry)
    voices = voice_table(registry)

    manifest_path = out_root / "manifest.json"
    cache = load_cache(manifest_path, decoder)
    rebuilt_reason = cache["reason"]
    cached_clips = cache["clips"]

    # Verify every input before decoding anything: a damaged corpus must not
    # leave a half-built fixture set behind.
    plan = []
    for item in SELECTION:
        name = "{}__{}".format(item.voice, item.phrase)
        entry = registry["files"][item.voice][item.phrase]
        source = web_root / (
            entry.get("path") or "audio/{}/{}.weba".format(item.voice, item.phrase)
        )
        verify_source(source, entry, name)
        plan.append((item, entry, source))

    rows: List[Tuple[str, str, str, float, int, str, str]] = []
    clips: List[dict] = []
    built = 0

    for item, entry, source in plan:
        name = "{}__{}".format(item.voice, item.phrase)
        files = clip_paths(out_root, item.voice, item.phrase)
        cached = (
            not force
            and rebuilt_reason is None
            and isinstance(cached_clips.get(name), dict)
            and is_cached(cached_clips[name], entry["sha256"], out_root)
        )

        if cached:
            record = cached_clips[name]
            sample_count = record["sampleCount"]
            state = "cached"
        else:
            decode(decoder, source, files["wav"], "wav")
            decode(decoder, source, files["f32"], "f32")
            decode(decoder, source, files["reference"], "reference")

            sample_count = wav_sample_count(files["wav"])
            f32_count = files["f32"].stat().st_size // F32_BYTES_PER_SAMPLE
            ref_count = files["reference"].stat().st_size // F32_BYTES_PER_SAMPLE
            if f32_count != sample_count or ref_count != sample_count:
                fail(
                    "decoders disagree on sample count for {}: wav {} / f32 {} / "
                    "libopus {}".format(name, sample_count, f32_count, ref_count),
                    "the fixtures must be sample-aligned; check that ffmpeg's "
                    "native and libopus decoders both honour pre_skip/DiscardPadding",
                )
            state = "built"
            built += 1

        output_sha = {key: sha256_file(files[key]) for key in OUTPUT_DIRS}
        record = describe_clip(
            item, entry, phrases[item.phrase], voices[item.voice],
            out_root, files, sample_count, output_sha,
        )
        clips.append(record)
        rows.append(
            (
                item.voice,
                item.phrase,
                item.group,
                sample_count / SAMPLE_RATE,
                files["f32"].stat().st_size,
                output_sha["f32"][:12],
                state,
            )
        )

    check_coverage(clips)
    manifest = build_manifest(registry, decoder, clips)
    data = json_bytes(manifest)
    manifest_changed = not manifest_path.is_file() or manifest_path.read_bytes() != data
    if manifest_changed:
        atomic_write(manifest_path, data)

    print_table(rows, out_root)
    print_summary(
        clips, built, len(rows) - built, manifest_path, manifest_changed,
        decoder, (registry.get("build") or {}).get("id"),
        "forced by --force" if force else rebuilt_reason,
    )
    return 0


def load_cache(manifest_path: Path, decoder: Decoder) -> dict:
    """Return previously generated clips plus why the cache is unusable, if so."""
    empty = {"clips": {}, "reason": "no manifest yet"}
    if not manifest_path.is_file():
        return empty
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print(
            "warning: ignoring unreadable {} ({})".format(
                display(manifest_path), error
            ),
            file=sys.stderr,
        )
        return {"clips": {}, "reason": "unreadable manifest"}
    if manifest.get("schemaVersion") != SCHEMA_VERSION:
        return {"clips": {}, "reason": "manifest schema changed"}
    current_decoder = (manifest.get("decoder") or {}).get("signature")
    if current_decoder != decoder.signature:
        return {
            "clips": {},
            "reason": "decoder changed ({} -> {})".format(
                current_decoder or "unknown", decoder.signature
            ),
        }
    clips = manifest.get("clips")
    if not isinstance(clips, list):
        return {"clips": {}, "reason": "manifest has no clips"}
    return {
        "clips": {
            clip["name"]: clip
            for clip in clips
            if isinstance(clip, dict) and isinstance(clip.get("name"), str)
        },
        "reason": None,
    }


def check_coverage(clips: List[dict]) -> None:
    """Enforce F-05's short/median/long and phrase-group coverage in the result."""
    durations = [clip["durationSeconds"] for clip in clips]
    groups = {clip["phraseGroup"] for clip in clips}
    problems = []
    if min(durations) > SHORT_MAX_SECONDS:
        problems.append(
            "no short clip (min {:.3f} s > {:.2f} s)".format(
                min(durations), SHORT_MAX_SECONDS
            )
        )
    if not any(MEDIAN_SECONDS[0] <= value <= MEDIAN_SECONDS[1] for value in durations):
        problems.append(
            "no median-length clip ({:.2f}-{:.2f} s)".format(*MEDIAN_SECONDS)
        )
    if max(durations) < LONG_MIN_SECONDS:
        problems.append(
            "no long clip (max {:.3f} s < {:.2f} s)".format(
                max(durations), LONG_MIN_SECONDS
            )
        )
    if len(clips) != 12:
        problems.append("{} clips, expected 12".format(len(clips)))
    if len({clip["voice"] for clip in clips}) != len(clips):
        problems.append("a voice appears more than once")
    if problems:
        fail(
            "fixture set no longer meets the F-05 coverage contract:\n  "
            + "\n  ".join(problems),
            "adjust SELECTION in {} (groups covered: {})".format(
                SCRIPT_NAME, ", ".join(sorted(groups))
            ),
        )


# --------------------------------------------------------------------------
# reporting
# --------------------------------------------------------------------------


def print_table(rows: List[Tuple[str, str, str, float, int, str, str]], out_root: Path) -> None:
    header = ("voice", "phrase", "group", "sec", "bytes", "sha256", "state")
    cells = [
        (
            row[0],
            row[1],
            row[2],
            "{:.3f}".format(row[3]),
            "{:,}".format(row[4]),
            row[5],
            row[6],
        )
        for row in rows
    ]
    widths = [
        max(len(header[column]), *(len(row[column]) for row in cells))
        for column in range(len(header))
    ]

    def line(row: Tuple[str, ...]) -> str:
        return "  ".join(
            cell.ljust(width) if index < 3 else cell.rjust(width)
            for index, (cell, width) in enumerate(zip(row, widths))
        ).rstrip()

    print("fixtures in {}".format(display(out_root)))
    print(line(header))
    print("  ".join("-" * width for width in widths))
    for row in cells:
        print(line(row))


def print_summary(
    clips: List[dict],
    built: int,
    cached: int,
    manifest_path: Path,
    manifest_changed: bool,
    decoder: Decoder,
    registry_build: Optional[str],
    rebuild_note: Optional[str],
) -> None:
    samples = sum(clip["sampleCount"] for clip in clips)
    seconds = samples / SAMPLE_RATE
    total_bytes = sum(
        record["bytes"] for clip in clips for record in clip["files"].values()
    )
    groups = sorted({clip["phraseGroup"] for clip in clips})
    languages = sorted({clip["language"] for clip in clips})

    print()
    print(
        "{} clips · {:,} samples · {:.2f} s · {:,} bytes · {} · groups {}".format(
            len(clips), samples, seconds, total_bytes,
            "/".join(languages), ",".join(groups),
        )
    )
    print(
        "decoder {} · registry build {}".format(
            decoder.signature, registry_build or "unknown"
        )
    )
    if rebuild_note:
        print("rebuilding everything: {}".format(rebuild_note))
    print("{} built · {} cached".format(built, cached))
    if built == 0:
        print("nothing to do; --force regenerates every file")
    print(
        "manifest {} · {}".format(
            display(manifest_path), "written" if manifest_changed else "unchanged"
        )
    )
    print("bytes column is the f32 payload; sha256 is the f32 digest (first 12 hex)")


# --------------------------------------------------------------------------
# cli
# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="make_fixtures.py",
        description=(
            "Generate the F-05 audio fixtures from the vendored corpus: "
            "12 WAV, 12 f32, 12 libopus reference f32 and a manifest."
        ),
        epilog=(
            "Fixtures are gitignored: they are regenerated locally and in CI. "
            "Re-running is a no-op unless an input, a decoder or an output changed."
        ),
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="rebuild every fixture even if the manifest says it is current",
    )
    parser.add_argument(
        "--root",
        default=None,
        help="web root holding registry.json and audio/ (default: <repo>/web)",
    )
    parser.add_argument(
        "--out",
        default=None,
        help="fixtures directory (default: <repo>/fixtures)",
    )
    return parser


def main(argv: Optional[List[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    web_root = resolve_dir(args.root, repo_root() / "web")
    out_root = resolve_dir(args.out, repo_root() / "fixtures")

    try:
        return make_fixtures(web_root, out_root, args.force)
    except FixtureError as error:
        sys.stdout.flush()
        print("error: {}".format(error), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\ninterrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
