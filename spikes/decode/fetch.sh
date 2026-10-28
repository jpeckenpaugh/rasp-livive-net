#!/usr/bin/env bash
# Fetch registry + the 12 D-01 clips, verify sha256, produce ffmpeg f32 reference.
# usage: ./fetch.sh [dest]   (default /tmp/rasp-spike)
set -euo pipefail

DEST=${1:-${RASP_SPIKE_DIR:-/tmp/rasp-spike}}
mkdir -p "$DEST"
cd "$DEST"

CLIPS=(
  af_heart/hello af_heart/see-you am_eric/good-luck am_fenrir/lets-play
  bf_emma/catch-you-later af_sarah/you-got-me pm_alex/nice-day
  zf_xiaoni/greet zm_yunxi/good-game jf_nezumi/see-you
  if_sara/hello hf_alpha/greet
)

curl -fsSL -o registry.json https://dg-pp.livive.net/registry.json
for spec in "${CLIPS[@]}"; do
  v=${spec%%/*}
  p=${spec##*/}
  mkdir -p "$v"
  curl -fsSL -o "$v/$p.weba" "https://dg-pp.livive.net/audio/$v/$p.weba"
done

python3 - "$DEST/registry.json" "${CLIPS[@]}" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1]).parent
reg = json.loads((root / "registry.json").read_text())
bad = 0
for spec in sys.argv[2:]:
    v, p = spec.split("/")
    e = reg["files"][v][p]
    data = (root / v / f"{p}.weba").read_bytes()
    ok = len(data) == e["bytes"] and hashlib.sha256(data).hexdigest() == e["sha256"]
    bad += not ok
    print(f"{spec:26} {'OK' if ok else 'MISMATCH'} {len(data)} bytes dur={e['durationMs']}ms")
raise SystemExit(1 if bad else 0)
PY

for spec in "${CLIPS[@]}"; do
  v=${spec%%/*}
  p=${spec##*/}
  # libopus is the reference implementation; ffmpeg's *native* Opus decoder is
  # not sample-aligned with it (see docs/spikes/decode.md).
  ffmpeg -v error -y -c:a libopus -i "$v/$p.weba" -ac 1 -ar 48000 -f f32le "$v/$p.libopus.f32"
  ffmpeg -v error -y -i "$v/$p.weba" -ac 1 -ar 48000 -f f32le "$v/$p.ffmpeg.f32"
done
echo "fetched + verified ${#CLIPS[@]} clips into $DEST"
