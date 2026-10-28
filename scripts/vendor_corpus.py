#!/usr/bin/env python3
"""Vendor the dg-pp voice corpus into `web/` and verify it offline.

This script is the only supported way to (re)build `web/audio/` and
`web/registry.json`. It uses the Python 3 standard library only, so a clean
checkout needs nothing beyond `python3`.

Modes
-----
default            Reuse valid local clips, download missing/invalid ones,
                   verify every byte, then assert the registry's
                   `build.totals`. Idempotent: a second run downloads zero
                   clips.
--check            Offline integrity check of the committed corpus against
                   `web/registry.json` (alias: `--verify-only`). Exits 1 and
                   prints a precise diff on any drift. This is the CI entry
                   point.
--refresh-registry Re-fetch the upstream registry, rewrite `path` fields to
                   the local layout, and re-vendor. The registry is written
                   only after every clip it references has been downloaded
                   and verified.
--compare-upstream Fetch the upstream registry and report drift against the
                   committed snapshot without touching local files.

Examples
--------
    python3 scripts/vendor_corpus.py
    python3 scripts/vendor_corpus.py --check
    python3 scripts/vendor_corpus.py --refresh-registry
    python3 scripts/vendor_corpus.py --compare-upstream

Concurrency is capped at 8 by design (PLAN K2 / D6): be polite to the origin.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

DEFAULT_UPSTREAM = "https://dg-pp.livive.net"
USER_AGENT = "rasp-corpus-vendor/1.0 (+https://rasp.livive.net)"
MAX_WORKERS = 8
MAX_ATTEMPTS = 3
HTTP_TIMEOUT = 30.0
REQUIRED_CLIP_FIELDS = ("path", "bytes", "durationMs", "sha256", "phonemes", "clipKey")
TOTAL_KEYS = ("clips", "voices", "phrases", "bytes")
SAFE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SHA256_HEX = re.compile(r"^[0-9a-f]{64}$")
LIST_LIMIT = 40


class CorpusError(Exception):
    """A fatal, user-facing corpus problem."""


class FetchError(CorpusError):
    """A clip or registry could not be fetched or did not match its manifest."""


# --------------------------------------------------------------------------
# paths and small helpers
# --------------------------------------------------------------------------


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def clip_relpath(voice_id: str, phrase_id: str) -> str:
    """Local path of a clip, relative to the `web/` document root."""
    return "audio/{}/{}.weba".format(voice_id, phrase_id)


def format_bytes(count: int) -> str:
    return "{:,}".format(count)


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def is_safe_id(value: object) -> bool:
    return isinstance(value, str) and bool(SAFE_ID.match(value))


def atomic_write(path: Path, data: bytes, prefix: str) -> None:
    """Write `data` next to `path`, fsync, then atomically rename over it."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=prefix, suffix=".tmp", dir=str(path.parent))
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(tmp, 0o644)
        os.replace(tmp, path)
    except BaseException:
        try:
            tmp.unlink()
        except FileNotFoundError:
            pass
        raise


# --------------------------------------------------------------------------
# HTTP with bounded retries
# --------------------------------------------------------------------------


def http_get(url: str) -> bytes:
    request = urllib.request.Request(
        url,
        headers={"User-Agent": USER_AGENT, "Accept": "*/*"},
    )
    with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT) as response:
        return response.read()


def http_get_with_retry(url: str, *, attempts: int = MAX_ATTEMPTS) -> bytes:
    """GET `url`, retrying transient network errors and 5xx/429 responses.

    404 and other 4xx responses fail immediately: they mean the manifest and
    the origin disagree, which a retry cannot fix. Backoff is 0.5 s, 1 s, ...
    """
    last_error: Exception = FetchError("could not fetch {}".format(url))
    for attempt in range(1, attempts + 1):
        try:
            return http_get(url)
        except urllib.error.HTTPError as error:
            if error.code == 404:
                raise FetchError("404 not found: {}".format(url)) from None
            transient = error.code == 429 or 500 <= error.code < 600
            last_error = FetchError(
                "HTTP {} from {}".format(error.code, url)
            )
            if not transient:
                raise last_error from None
            retry_after = error.headers.get("Retry-After") if error.headers else None
            delay = 0.5 * (2 ** (attempt - 1))
            if retry_after and retry_after.isdigit():
                delay = min(float(retry_after), 10.0)
        except (urllib.error.URLError, TimeoutError, ConnectionError, OSError) as error:
            last_error = FetchError(
                "network error fetching {}: {}".format(url, error)
            )
            delay = 0.5 * (2 ** (attempt - 1))

        if attempt < attempts:
            time.sleep(delay)
    raise last_error


# --------------------------------------------------------------------------
# registry loading, validation, rewriting
# --------------------------------------------------------------------------


def load_json(path: Path) -> dict:
    try:
        with path.open("r", encoding="utf-8") as handle:
            return json.load(handle)
    except FileNotFoundError:
        raise CorpusError("missing file: {}".format(path)) from None
    except (OSError, ValueError) as error:
        raise CorpusError("cannot read {}: {}".format(path, error)) from None


def registry_entries(registry: dict):
    """Yield (voice_id, phrase_id, clip_entry) in registry order."""
    files = registry.get("files")
    if not isinstance(files, dict):
        return
    for voice_id, clips in files.items():
        if not isinstance(clips, dict):
            continue
        for phrase_id, entry in clips.items():
            yield voice_id, phrase_id, entry


def rewrite_paths(registry: dict) -> dict:
    """Return a copy with every clip `path` set to the local relative path.

    Key order is preserved and no other field is touched.
    """
    local = json.loads(json.dumps(registry))
    for voice_id, phrase_id, entry in registry_entries(local):
        entry["path"] = clip_relpath(voice_id, phrase_id)
    return local


def validate_registry(registry: dict, *, canonical_paths: bool) -> list:
    """Check registry structure and internal consistency. Returns problems."""
    problems = []
    if not isinstance(registry, dict):
        return ["registry root is not a JSON object"]

    build = registry.get("build")
    totals = build.get("totals") if isinstance(build, dict) else None
    if not isinstance(totals, dict):
        problems.append("build.totals is missing or not an object")
        totals = {}
    for key in TOTAL_KEYS:
        if not isinstance(totals.get(key), int):
            problems.append("build.totals.{} is missing or not an integer".format(key))

    voices = registry.get("voices")
    phrases = registry.get("phrases")
    voice_ids = set()
    phrase_ids = set()
    if not isinstance(voices, list):
        problems.append("voices is missing or not an array")
    else:
        for item in voices:
            voice_id = item.get("id") if isinstance(item, dict) else None
            if not is_safe_id(voice_id):
                problems.append("voices contains an invalid id: {!r}".format(voice_id))
            elif voice_id in voice_ids:
                problems.append("duplicate voice id: {}".format(voice_id))
            else:
                voice_ids.add(voice_id)
    if not isinstance(phrases, list):
        problems.append("phrases is missing or not an array")
    else:
        for item in phrases:
            phrase_id = item.get("id") if isinstance(item, dict) else None
            if not is_safe_id(phrase_id):
                problems.append("phrases contains an invalid id: {!r}".format(phrase_id))
            elif phrase_id in phrase_ids:
                problems.append("duplicate phrase id: {}".format(phrase_id))
            else:
                phrase_ids.add(phrase_id)

    files = registry.get("files")
    if not isinstance(files, dict):
        problems.append("files is missing or not an object")

    seen_clip_bytes = 0
    seen_clips = 0
    served_voices = set()
    served_phrases = set()
    for voice_id, phrase_id, entry in registry_entries(registry):
        seen_clips += 1
        served_voices.add(voice_id)
        served_phrases.add(phrase_id)
        where = "{}/{}".format(voice_id, phrase_id)
        if not is_safe_id(voice_id):
            problems.append("{}: unsafe voice id".format(where))
        if not is_safe_id(phrase_id):
            problems.append("{}: unsafe phrase id".format(where))
        if not isinstance(entry, dict):
            problems.append("{}: clip entry is not an object".format(where))
            continue
        for field in REQUIRED_CLIP_FIELDS:
            if field not in entry:
                problems.append("{}: missing field {!r}".format(where, field))
        size = entry.get("bytes")
        if not isinstance(size, int) or isinstance(size, bool) or size <= 0:
            problems.append("{}: invalid bytes {!r}".format(where, size))
        else:
            seen_clip_bytes += size
        duration = entry.get("durationMs")
        if not isinstance(duration, int) or isinstance(duration, bool) or duration <= 0:
            problems.append("{}: invalid durationMs {!r}".format(where, duration))
        digest = entry.get("sha256")
        if not isinstance(digest, str) or not SHA256_HEX.match(digest):
            problems.append("{}: invalid sha256 {!r}".format(where, digest))
        for field in ("phonemes", "clipKey"):
            value = entry.get(field)
            if field == "phonemes":
                if not isinstance(value, str):
                    problems.append("{}: invalid phonemes".format(where))
            elif not isinstance(value, str) or not value:
                problems.append("{}: invalid clipKey".format(where))
        path = entry.get("path")
        expected = clip_relpath(voice_id, phrase_id)
        if canonical_paths and path != expected:
            problems.append(
                "{}: path is {!r}, expected {!r}".format(where, path, expected)
            )

    if voice_ids and served_voices != voice_ids:
        missing = sorted(voice_ids - served_voices)
        extra = sorted(served_voices - voice_ids)
        if missing:
            problems.append("voices without clips: {}".format(", ".join(missing[:10])))
        if extra:
            problems.append("clip files for undeclared voices: {}".format(", ".join(extra[:10])))
    if phrase_ids and served_phrases != phrase_ids:
        missing = sorted(phrase_ids - served_phrases)
        if missing:
            problems.append("phrases without clips: {}".format(", ".join(missing[:10])))

    actual = {
        "clips": seen_clips,
        "voices": len(served_voices),
        "phrases": len(served_phrases),
        "bytes": seen_clip_bytes,
    }
    for key in TOTAL_KEYS:
        expected_value = totals.get(key)
        if isinstance(expected_value, int) and expected_value != actual[key]:
            problems.append(
                "build.totals.{key} says {want}, registry contains {got}".format(
                    key=key, want=expected_value, got=actual[key]
                )
            )
    for key in ("voices", "phrases"):
        declared = registry.get(key)
        if isinstance(declared, list) and totals.get(key) is not None:
            if len(declared) != totals[key]:
                problems.append(
                    "build.totals.{key} says {want}, registry declares {got} entries".format(
                        key=key, want=totals[key], got=len(declared)
                    )
                )
    return problems


def registry_totals(registry: dict) -> dict:
    entries = list(registry_entries(registry))
    return {
        "clips": len(entries),
        "voices": len({voice for voice, _, _ in entries}),
        "phrases": len({phrase for _, phrase, _ in entries}),
        "bytes": sum(
            entry.get("bytes", 0)
            for _, _, entry in entries
            if isinstance(entry, dict) and isinstance(entry.get("bytes"), int)
        ),
    }


def assert_totals(registry: dict, web_root: Path) -> None:
    """Fail unless build.totals matches both the manifest and the disk."""
    totals = registry.get("build", {}).get("totals", {})
    actual = registry_totals(registry)
    problems = []
    for key in TOTAL_KEYS:
        want = totals.get(key)
        if want != actual[key]:
            problems.append(
                "{}: build.totals says {!r}, registry contains {!r}".format(
                    key, want, actual[key]
                )
            )

    disk_bytes = 0
    disk_missing = 0
    for _, _, entry in registry_entries(registry):
        target = web_root / entry["path"]
        try:
            disk_bytes += target.stat().st_size
        except OSError:
            disk_missing += 1
    if disk_missing:
        problems.append("{} clips referenced by the registry are missing on disk".format(disk_missing))
    elif totals.get("bytes") != disk_bytes:
        problems.append(
            "bytes: build.totals says {!r}, {} files on disk total {!r}".format(
                totals.get("bytes"), actual["clips"], disk_bytes
            )
        )

    if problems:
        raise CorpusError(
            "registry totals do not match the corpus:\n  " + "\n  ".join(problems)
        )


# --------------------------------------------------------------------------
# offline verification
# --------------------------------------------------------------------------


class Drift:
    def __init__(self) -> None:
        self.missing = []
        self.wrong_size = []  # (relpath, expected, actual)
        self.wrong_hash = []  # (relpath, expected, actual)
        self.unreadable = []
        self.orphans = []
        self.path_problems = []

    def __bool__(self) -> bool:
        return bool(
            self.missing
            or self.wrong_size
            or self.wrong_hash
            or self.unreadable
            or self.orphans
            or self.path_problems
        )

    def count(self) -> int:
        return (
            len(self.missing)
            + len(self.wrong_size)
            + len(self.wrong_hash)
            + len(self.unreadable)
            + len(self.orphans)
            + len(self.path_problems)
        )

    def report(self, limit: int = LIST_LIMIT) -> str:
        lines = ["DRIFT DETECTED - {} problem(s)".format(self.count())]

        def section(title, items, render):
            if not items:
                return
            lines.append("  {}: {}".format(title, len(items)))
            for item in items[:limit]:
                lines.append("    " + render(item))
            if len(items) > limit:
                lines.append("    ... and {} more".format(len(items) - limit))

        section("missing files", self.missing, lambda rel: rel)
        section(
            "byte-count mismatch",
            self.wrong_size,
            lambda item: "{} (expected {} bytes, found {})".format(*item),
        )
        section(
            "sha256 mismatch",
            self.wrong_hash,
            lambda item: "{} (expected {}, found {})".format(*item),
        )
        section("unreadable files", self.unreadable, lambda rel: rel)
        section(
            "unexpected local clips",
            self.orphans,
            lambda rel: rel,
        )
        section(
            "non-canonical registry paths",
            self.path_problems,
            lambda item: "{}: {!r} should be {!r}".format(*item),
        )
        return "\n".join(lines)


def find_local_clips(web_root: Path) -> list:
    audio_root = web_root / "audio"
    if not audio_root.is_dir():
        return []
    clips = []
    for path in audio_root.rglob("*.weba"):
        if path.is_file():
            clips.append(path.relative_to(web_root).as_posix())
    return sorted(clips)


def verify_clip(path: Path, entry: dict) -> str:
    """Return 'ok', 'missing', 'size', 'hash', or 'unreadable'."""
    try:
        stat = path.stat()
        if stat.st_size != entry["bytes"]:
            return "size"
        if sha256_file(path) != entry["sha256"]:
            return "hash"
    except FileNotFoundError:
        return "missing"
    except OSError:
        return "unreadable"
    return "ok"


def verify_offline(registry: dict, web_root: Path, *, include_orphans: bool = True) -> Drift:
    drift = Drift()
    expected = set()
    for voice_id, phrase_id, entry in registry_entries(registry):
        rel = entry["path"]
        canonical = clip_relpath(voice_id, phrase_id)
        if rel != canonical:
            drift.path_problems.append((clip_relpath(voice_id, phrase_id), rel, canonical))
            rel = canonical
        expected.add(rel)
        path = web_root / rel
        status = verify_clip(path, entry)
        if status == "missing":
            drift.missing.append(rel)
        elif status == "size":
            drift.wrong_size.append((rel, entry["bytes"], path.stat().st_size))
        elif status == "hash":
            drift.wrong_hash.append((rel, entry["sha256"], sha256_file(path)))
        elif status == "unreadable":
            drift.unreadable.append(rel)

    drift.orphans = (
        [rel for rel in find_local_clips(web_root) if rel not in expected]
        if include_orphans
        else []
    )
    return drift


# --------------------------------------------------------------------------
# download
# --------------------------------------------------------------------------


def fetch_clip(url: str, entry: dict, destination: Path) -> None:
    """Download one clip, verify it, and atomically land it at `destination`."""
    data = http_get_with_retry(url)
    if len(data) != entry["bytes"]:
        raise FetchError(
            "{}: byte-count mismatch (expected {}, got {})".format(
                destination.name, entry["bytes"], len(data)
            )
        )
    actual = sha256_bytes(data)
    if actual != entry["sha256"]:
        raise FetchError(
            "{}: sha256 mismatch (expected {}, got {})".format(
                destination.name, entry["sha256"], actual
            )
        )
    atomic_write(destination, data, prefix=".clip-")


def source_url(upstream: str, source_path: str) -> str:
    """Resolve a manifest path against the origin, refusing nothing remote."""
    if source_path.startswith(("http://", "https://")):
        return source_path
    return "{}/{}".format(upstream, source_path.lstrip("/"))


def plan_work(registry: dict, web_root: Path, upstream: str, source_paths: dict):
    """Split clips into already-valid and to-download lists."""
    reused = 0
    pending = []  # (relpath, url, entry)
    for voice_id, phrase_id, entry in registry_entries(registry):
        rel = entry["path"]
        destination = web_root / rel
        if verify_clip(destination, entry) == "ok":
            reused += 1
            continue
        source_rel = source_paths.get((voice_id, phrase_id), rel)
        pending.append((rel, source_url(upstream, source_rel), entry))
    return reused, pending


def run_downloads(pending: list, web_root: Path, jobs: int) -> None:
    """Download with bounded concurrency. Fails fast and cancels the queue."""
    total = len(pending)
    if total == 0:
        return

    interactive = sys.stdout.isatty()
    started = time.monotonic()
    last_print = 0.0
    done = 0
    failure = None

    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {}
        for rel, url, entry in pending:
            future = pool.submit(fetch_clip, url, entry, web_root / rel)
            futures[future] = rel

        for future in concurrent.futures.as_completed(futures):
            try:
                future.result()
            except Exception as error:  # noqa: BLE001 - surfaced verbatim below
                failure = (futures[future], error)
                for other in futures:
                    other.cancel()
                break
            done += 1
            now = time.monotonic()
            if interactive and now - last_print >= 0.5:
                sys.stdout.write(
                    "\r  downloaded {}/{} clips ({:.1f}s)".format(
                        done, total, now - started
                    )
                )
                sys.stdout.flush()
                last_print = now
            elif not interactive and (
                done == total or done % max(1, total // 4) == 0
            ):
                print(
                    "  downloaded {}/{} clips ({:.1f}s)".format(
                        done, total, now - started
                    )
                )

    if interactive:
        sys.stdout.write("\r" + " " * 72 + "\r")
        sys.stdout.flush()

    if failure is not None:
        rel, error = failure
        raise CorpusError("failed to vendor {}: {}".format(rel, error))


# --------------------------------------------------------------------------
# registry diffing
# --------------------------------------------------------------------------


def diff_registries(old: dict, new: dict, limit: int = LIST_LIMIT) -> tuple:
    """Return (summary_lines, changed: bool) comparing two registries."""
    old_build = old.get("build", {}).get("id")
    new_build = new.get("build", {}).get("id")
    old_clips = {
        (voice, phrase): entry for voice, phrase, entry in registry_entries(old)
    }
    new_clips = {
        (voice, phrase): entry for voice, phrase, entry in registry_entries(new)
    }
    added = sorted(set(new_clips) - set(old_clips))
    removed = sorted(set(old_clips) - set(new_clips))
    changed = []
    for key in sorted(set(old_clips) & set(new_clips)):
        before, after = old_clips[key], new_clips[key]
        if (
            before.get("sha256") != after.get("sha256")
            or before.get("bytes") != after.get("bytes")
            or before.get("durationMs") != after.get("durationMs")
        ):
            changed.append(key)

    lines = []
    if old_build != new_build:
        lines.append("build id: {} -> {}".format(old_build, new_build))
    if added:
        lines.append("added clips: {} ({})".format(len(added), ", ".join(
            "{}/{}".format(*key) for key in added[:limit]
        )))
    if removed:
        lines.append("removed clips: {} ({})".format(len(removed), ", ".join(
            "{}/{}".format(*key) for key in removed[:limit]
        )))
    if changed:
        lines.append("changed clips: {} ({})".format(len(changed), ", ".join(
            "{}/{}".format(*key) for key in changed[:limit]
        )))
    has_diff = bool(old_build != new_build or added or removed or changed)
    return lines, has_diff


# --------------------------------------------------------------------------
# modes
# --------------------------------------------------------------------------


def resolve_web_root(value: str) -> Path:
    root = Path(value) if value else repo_root() / "web"
    if not root.is_absolute():
        root = (Path.cwd() / root).resolve()
    return root


def fetch_upstream_registry(upstream: str) -> dict:
    url = "{}/registry.json".format(upstream)
    try:
        data = http_get_with_retry(url)
    except CorpusError as error:
        raise CorpusError("cannot fetch upstream registry: {}".format(error)) from None
    try:
        registry = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as error:
        raise CorpusError("upstream registry is not valid UTF-8 JSON: {}".format(error)) from None
    problems = validate_registry(registry, canonical_paths=False)
    if problems:
        raise CorpusError(
            "upstream registry is invalid:\n  " + "\n  ".join(problems)
        )
    return registry


def print_headline(prefix: str, registry: dict) -> None:
    totals = registry["build"]["totals"]
    print(
        "{}: build {} - {} voices, {} phrases, {} clips, {} bytes".format(
            prefix,
            registry["build"].get("id", "?"),
            totals["voices"],
            totals["phrases"],
            totals["clips"],
            format_bytes(totals["bytes"]),
        )
    )


def print_summary(totals: dict, reused: int, downloaded: int, elapsed: float) -> None:
    rows = [
        ("voices", "{:,}".format(totals["voices"])),
        ("phrases", "{:,}".format(totals["phrases"])),
        ("clips", "{:,}".format(totals["clips"])),
        ("bytes", format_bytes(totals["bytes"])),
        ("reused", "{:,}".format(reused)),
        ("downloaded", "{:,}".format(downloaded)),
        ("elapsed", "{:.1f}s".format(elapsed)),
    ]
    width = max(len(label) for label, _ in rows)
    print("summary:")
    for label, value in rows:
        print("  {:<{width}}  {}".format(label, value, width=width))


def mode_check(registry: dict, web_root: Path, upstream: str, compare: bool) -> int:
    started = time.monotonic()
    print_headline("check", registry)
    drift = verify_offline(registry, web_root)
    failures = []

    try:
        assert_totals(registry, web_root)
    except CorpusError as error:
        failures.append(str(error))

    if drift:
        failures.append(drift.report())

    if compare:
        print("check: comparing against {}...".format(upstream))
        try:
            remote = fetch_upstream_registry(upstream)
        except CorpusError as error:
            failures.append(str(error))
        else:
            lines, changed = diff_registries(registry, remote)
            if changed:
                failures.append(
                    "upstream registry has drifted from the committed snapshot:\n  "
                    + "\n  ".join(lines)
                )
            else:
                print("check: upstream build {} matches the committed snapshot".format(
                    remote["build"].get("id", "?")
                ))

    if failures:
        sys.stdout.flush()
        print("\n".join(failures), file=sys.stderr)
        print("check: FAILED ({:.1f}s)".format(time.monotonic() - started), file=sys.stderr)
        return 1

    totals = registry_totals(registry)
    print(
        "check: ok - {} clips, {} bytes verified offline ({:.1f}s)".format(
            format_bytes(totals["clips"]),
            format_bytes(totals["bytes"]),
            time.monotonic() - started,
        )
    )
    return 0


def mode_vendor(
    registry: dict,
    web_root: Path,
    upstream: str,
    jobs: int,
    *,
    registry_is_new: bool,
    source_paths: dict,
    prune: bool,
) -> int:
    started = time.monotonic()
    print_headline("vendor", registry)
    if registry_is_new:
        print("vendor: registry will be written to {}".format(
            relative_display(web_root / "registry.json")
        ))

    reused, pending = plan_work(registry, web_root, upstream, source_paths)
    print(
        "vendor: {} clips already valid, {} to download".format(
            reused, len(pending)
        )
    )
    if pending:
        print("vendor: downloading with {} workers".format(jobs))
        run_downloads(pending, web_root, jobs)

    downloaded = len(pending)
    print("vendor: verifying every clip against the registry...")
    drift = verify_offline(registry, web_root, include_orphans=False)
    if drift:
        sys.stdout.flush()
        print(drift.report(), file=sys.stderr)
        raise CorpusError("corpus failed post-download verification")

    orphans = find_local_clips(web_root)
    expected = {entry["path"] for _, _, entry in registry_entries(registry)}
    orphans = [rel for rel in orphans if rel not in expected]
    if orphans and prune:
        for rel in orphans:
            (web_root / rel).unlink()
        print("vendor: pruned {} stale clip(s)".format(len(orphans)))
    elif orphans:
        print(
            "warning: {} unexpected .weba file(s) under web/audio/ "
            "(use --prune to remove)".format(len(orphans)),
            file=sys.stderr,
        )

    assert_totals(registry, web_root)

    if registry_is_new:
        payload = (
            json.dumps(registry, indent=2, ensure_ascii=False) + "\n"
        ).encode("utf-8")
        atomic_write(web_root / "registry.json", payload, prefix=".registry-")
        print("vendor: wrote {}".format(relative_display(web_root / "registry.json")))

    totals = registry_totals(registry)
    print_summary(totals, reused, downloaded, time.monotonic() - started)
    print("vendor: corpus ok")
    return 0


def mode_compare(registry: dict, upstream: str) -> int:
    print_headline("compare", registry)
    remote = fetch_upstream_registry(upstream)
    lines, changed = diff_registries(registry, remote)
    if not changed:
        print("compare: upstream build {} matches the committed snapshot".format(
            remote["build"].get("id", "?")
        ))
        return 0
    print("compare: upstream has drifted from the committed snapshot", file=sys.stderr)
    for line in lines:
        print("  " + line, file=sys.stderr)
    print(
        "compare: re-run `python3 scripts/vendor_corpus.py --refresh-registry` "
        "and review the registry diff",
        file=sys.stderr,
    )
    return 1


def relative_display(path: Path) -> str:
    try:
        return path.resolve().relative_to(repo_root()).as_posix()
    except ValueError:
        return path.as_posix()


def positive_workers(value: str) -> int:
    try:
        workers = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError("must be an integer") from None
    if not 1 <= workers <= MAX_WORKERS:
        raise argparse.ArgumentTypeError(
            "must be between 1 and {} (be polite to the origin)".format(MAX_WORKERS)
        )
    return workers


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="vendor_corpus.py",
        description="Vendor and verify the dg-pp voice corpus under web/.",
        epilog=(
            "Run without flags to download missing/invalid clips. "
            "`--check` is the offline CI entry point."
        ),
    )
    parser.add_argument(
        "--check",
        "--verify-only",
        dest="check",
        action="store_true",
        help="verify local clips against web/registry.json offline (CI mode)",
    )
    parser.add_argument(
        "--refresh-registry",
        action="store_true",
        help="re-fetch the upstream registry and re-vendor (human-driven)",
    )
    parser.add_argument(
        "--compare-upstream",
        action="store_true",
        help="report upstream registry drift without modifying local files",
    )
    parser.add_argument(
        "--jobs",
        type=positive_workers,
        default=MAX_WORKERS,
        help="download workers, 1-{} (default: {})".format(MAX_WORKERS, MAX_WORKERS),
    )
    parser.add_argument(
        "--upstream",
        default=DEFAULT_UPSTREAM,
        help="origin base URL (default: {})".format(DEFAULT_UPSTREAM),
    )
    parser.add_argument(
        "--root",
        default=None,
        help="web root to read/write (default: <repo>/web)",
    )
    parser.add_argument(
        "--prune",
        action="store_true",
        help="delete .weba files under web/audio/ that the registry does not list",
    )
    return parser


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    upstream = args.upstream.rstrip("/")
    web_root = resolve_web_root(args.root)
    registry_path = web_root / "registry.json"

    if args.check and args.refresh_registry:
        parser.error("--check cannot be combined with --refresh-registry")
    if args.refresh_registry and args.compare_upstream:
        parser.error("--refresh-registry already fetches upstream; drop --compare-upstream")
    if args.check and args.prune:
        parser.error("--prune only applies when vendoring")
    if args.prune and args.compare_upstream:
        parser.error("--prune only applies when vendoring")

    try:
        if args.check:
            registry = load_json(registry_path)
            problems = validate_registry(registry, canonical_paths=True)
            if problems:
                raise CorpusError(
                    "local registry is invalid:\n  " + "\n  ".join(problems)
                )
            return mode_check(registry, web_root, upstream, args.compare_upstream)

        if args.compare_upstream:
            registry = load_json(registry_path)
            problems = validate_registry(registry, canonical_paths=True)
            if problems:
                raise CorpusError(
                    "local registry is invalid:\n  " + "\n  ".join(problems)
                )
            return mode_compare(registry, upstream)

        source_paths = {}
        registry_is_new = False
        old_registry = None
        if registry_path.is_file() and not args.refresh_registry:
            registry = load_json(registry_path)
            problems = validate_registry(registry, canonical_paths=True)
            if problems:
                raise CorpusError(
                    "local registry is invalid:\n  " + "\n  ".join(problems)
                    + "\n  run --refresh-registry to rebuild it from upstream"
                )
        else:
            if registry_path.is_file():
                old_registry = load_json(registry_path)
            print("vendor: fetching upstream registry from {}/registry.json".format(
                upstream
            ))
            remote = fetch_upstream_registry(upstream)
            registry = rewrite_paths(remote)
            for voice_id, phrase_id, entry in registry_entries(remote):
                source_paths[(voice_id, phrase_id)] = entry["path"]
            problems = validate_registry(registry, canonical_paths=True)
            if problems:
                raise CorpusError(
                    "rewritten registry is invalid:\n  " + "\n  ".join(problems)
                )
            registry_is_new = True
            if old_registry is not None:
                lines, changed = diff_registries(old_registry, registry)
                if changed:
                    print("vendor: upstream registry changed since the last bake:")
                    for line in lines:
                        print("  " + line)
                else:
                    print("vendor: upstream registry is unchanged")

        return mode_vendor(
            registry,
            web_root,
            upstream,
            args.jobs,
            registry_is_new=registry_is_new,
            source_paths=source_paths,
            prune=args.prune,
        )
    except CorpusError as error:
        sys.stdout.flush()
        print("error: {}".format(error), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\ninterrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
