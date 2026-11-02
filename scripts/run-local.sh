#!/usr/bin/env bash
# Run the Rust proxy locally against a real Prowlarr/Jackett on the LAN.
#
# The proxy forwards the `apikey` it receives, so it needs no key to start.
# The key is only required by the example curl printed at the end, where it is
# read from the environment and never written to disk or echoed.
#
# Required:
#   PROWLARR_URL   e.g. http://prowlarr.lan:9696   (or JACKETT_URL)
# Optional:
#   PROWLARR_API_KEY   Prowlarr/Jackett Torznab API key (for the example curl)
#   DATABASE_PATH      defaults to ./local-dev.db
#   LISTEN_ADDR        defaults to 127.0.0.1:9117
#   RUST_LOG           defaults to jproxy_rs=info,tower_http=info
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ -z "${PROWLARR_URL:-}" && -z "${JACKETT_URL:-}" ]]; then
  echo "set PROWLARR_URL (or JACKETT_URL) before running this script" >&2
  exit 1
fi

export DATABASE_PATH="${DATABASE_PATH:-$ROOT_DIR/local-dev.db}"
export LISTEN_ADDR="${LISTEN_ADDR:-127.0.0.1:9117}"
export SYNC_ENABLED="${SYNC_ENABLED:-false}"
export RUST_LOG="${RUST_LOG:-jproxy_rs=info,tower_http=info}"

if [[ "${1:-}" == "--build" || ! -x target/release/jproxy-rs ]]; then
  nix develop --command cargo build --release
fi

echo "starting jproxy-rs on ${LISTEN_ADDR} (database ${DATABASE_PATH})"
echo "configure Sonarr/Radarr with:"
echo "  http://${LISTEN_ADDR}/sonarr/prowlarr/<indexer_id>/api"
echo "  http://${LISTEN_ADDR}/radarr/prowlarr/<indexer_id>/api"
if [[ -n "${PROWLARR_API_KEY:-}" ]]; then
  echo "smoke test (key read from \$PROWLARR_API_KEY):"
  echo "  curl -fsS 'http://${LISTEN_ADDR}/sonarr/prowlarr/1/api?t=caps&apikey='\"\$PROWLARR_API_KEY\""
else
  echo "set PROWLARR_API_KEY to print a ready-to-run smoke test command"
fi

exec ./target/release/jproxy-rs
