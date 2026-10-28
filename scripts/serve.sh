#!/usr/bin/env bash
#
# D-04: serve `web/` for the smoke page (and, later, the demo app).
#
# The prescribed command is `python3 -m http.server 8080 --directory web`.
# Python's mimetypes table must report `.wasm` as `application/wasm`; when it
# does not (some Python builds predate the entry), browsers log a MIME warning
# and `WebAssembly.instantiateStreaming` falls back to an ArrayBuffer copy.
# This script checks first and, if needed, starts an equivalent server with
# the type registered — never serves the wasm with a wrong type silently.
#
# Usage: bash scripts/serve.sh          # http://127.0.0.1:8080/
#        PORT=8081 bash scripts/serve.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PORT="${PORT:-8080}"
WEB_DIR="web"

if [[ ! -f "$WEB_DIR/smoke.html" ]]; then
  echo "error: $WEB_DIR/smoke.html is missing; run bash scripts/build_wasm.sh first." >&2
  exit 1
fi

WASM_MIME="$(python3 - <<'PY'
import mimetypes

print(mimetypes.guess_type("probe.wasm")[0] or "")
PY
)"

if [[ "$WASM_MIME" == "application/wasm" ]]; then
  echo "Serving $WEB_DIR/ at http://127.0.0.1:${PORT}/ (wasm MIME: ${WASM_MIME})"
  echo "Smoke page:  http://127.0.0.1:${PORT}/smoke.html"
  exec python3 -m http.server "$PORT" --directory "$WEB_DIR"
fi

echo "warning: this Python does not map .wasm -> application/wasm (got '${WASM_MIME:-none}')." >&2
echo "         Starting an equivalent server with the type registered." >&2
echo "         Permanent fix: upgrade Python (>= 3.11 registers .wasm)." >&2
echo "Serving $WEB_DIR/ at http://127.0.0.1:${PORT}/ (wasm MIME forced to application/wasm)"
echo "Smoke page:  http://127.0.0.1:${PORT}/smoke.html"

exec python3 - "$PORT" "$WEB_DIR" <<'PY'
import functools
import http.server
import mimetypes
import sys

mimetypes.add_type("application/wasm", ".wasm")
mimetypes.add_type("text/javascript", ".js")
mimetypes.add_type("text/html", ".html")

port = int(sys.argv[1])
directory = sys.argv[2]
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)
http.server.ThreadingHTTPServer(("", port), handler).serve_forever()
PY
