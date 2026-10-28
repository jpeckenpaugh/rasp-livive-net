#!/usr/bin/env bash
# Decode the 12 D-01 clips with the feature-selected backend, compare to ffmpeg.
# usage: ./run_native.sh <backend-tag> [clip-dir] [iters]
set -euo pipefail

TAG=${1:?backend tag}
CLIP_DIR=${2:-${RASP_SPIKE_DIR:-/tmp/rasp-spike}}
ITERS=${3:-20}
OUT="$CLIP_DIR/out/$TAG"
mkdir -p "$OUT"
cd "$(dirname "$0")"

case "$TAG" in
  opus-pure) EXPECT=opus-pure ;;
  ruopus) EXPECT=ruopus ;;
  adapter-opus-rs) EXPECT=symphonia-adapter-opus-rs ;;
  oporus) EXPECT=oporus ;;
  *) EXPECT= ;;
esac
ACTUAL=$(./target/release/decode "$CLIP_DIR/af_heart/hello.weba" --iters 1 \
  | sed -n 's/^backend=\([^ ]*\).*/\1/p')
if [ -n "$EXPECT" ] && [ "$ACTUAL" != "$EXPECT" ]; then
  echo "error: target/release/decode is built as '$ACTUAL', expected '$EXPECT'" >&2
  echo "build: cargo build --release --no-default-features --features backend-$TAG" >&2
  exit 1
fi

CLIPS=(
  af_heart/hello af_heart/see-you am_eric/good-luck am_fenrir/lets-play
  bf_emma/catch-you-later af_sarah/you-got-me pm_alex/nice-day
  zf_xiaoni/greet zm_yunxi/good-game jf_nezumi/see-you
  if_sara/hello hf_alpha/greet
)

for spec in "${CLIPS[@]}"; do
  v=${spec%%/*}
  p=${spec##*/}
  ./target/release/decode "$CLIP_DIR/$v/$p.weba" "$OUT/${v}_${p}.f32" --iters "$ITERS" \
    | tee "$OUT/${v}_${p}.timing"
  printf '  vs libopus: '
  python3 compare.py "$CLIP_DIR/$v/$p.libopus.f32" "$OUT/${v}_${p}.f32" --lag 3
  printf '  vs ffmpeg-native (not sample-aligned; see README): '
  python3 compare.py "$CLIP_DIR/$v/$p.ffmpeg.f32" "$OUT/${v}_${p}.f32" --lag 3
done
