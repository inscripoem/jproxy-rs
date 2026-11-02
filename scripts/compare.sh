#!/usr/bin/env bash
# Compare the Java jproxy and the Rust rewrite for the same Torznab queries.
#
# The script never embeds or prints API keys: the key is read from
# PROWLARR_API_KEY at runtime and passed to curl via --data-urlencode.
#
# Required:
#   PROWLARR_API_KEY   Torznab API key (Prowlarr/Jackett)
# Optional:
#   JAVA_URL      default http://127.0.0.1:8117  (Java jproxy)
#   RUST_URL      default http://127.0.0.1:9117  (jproxy-rs)
#   INDEXER_ID    default 1
#   QUERIES_FILE  one query per line; defaults to a built-in multi-language set
#
# The two proxies must point at the same Prowlarr/Jackett and use the same
# database. Live requests run only when PROWLARR_API_KEY is already exported.
set -euo pipefail

JAVA_URL="${JAVA_URL:-http://127.0.0.1:8117}"
RUST_URL="${RUST_URL:-http://127.0.0.1:9117}"
INDEXER_ID="${INDEXER_ID:-1}"

if [[ -z "${PROWLARR_API_KEY:-}" ]]; then
  cat >&2 <<'EOF'
PROWLARR_API_KEY is not set, so no live requests are made.

To run the comparison:
  1. start the Java jproxy (e.g. on 127.0.0.1:8117)
  2. start jproxy-rs (scripts/run-local.sh, e.g. on 127.0.0.1:9117)
  3. export PROWLARR_API_KEY=...   # never committed, never printed
  4. scripts/compare.sh
EOF
  exit 0
fi

queries=()
if [[ -n "${QUERIES_FILE:-}" ]]; then
  while IFS= read -r line; do
    [[ -z "$line" || "$line" == \#* ]] && continue
    queries+=("$line")
  done < "$QUERIES_FILE"
else
  # Representative Chinese, Japanese, anime, movie and TV queries. Adjust to
  # match your library for meaningful results.
  queries=(
    "成龙历险记"
    "葬送的芙莉莲"
    "Kill la Kill"
    "Jackie Chan Adventures"
    "The Fifth Element 1997"
    "Rick and Morty"
  )
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

# Extract item titles, dropping the channel title, sorted for stable diffing.
titles_of() {
  { grep -o '<title>[^<]*</title>' "$1" 2>/dev/null || true; } \
    | sed -e 's#^<title>##' -e 's#</title>$##' \
    | tail -n +2 \
    | LC_ALL=C sort
}

fetch() {
  local base="$1" query="$2" out="$3"
  local t=tvsearch
  [ "$kind" = "radarr" ] && t=movie
  local code
  code="$(curl -s -G "$base/$kind/prowlarr/$INDEXER_ID/api" \
    --data-urlencode "t=$t" \
    --data-urlencode "q=$query" \
    --data-urlencode "limit=100" \
    --data-urlencode "offset=0" \
    --data-urlencode "apikey=$PROWLARR_API_KEY" \
    -o "$out" -w '%{http_code}' 2>/dev/null)" || code="000"
  printf '%s' "$code"
}

differences=0
for kind in sonarr radarr; do
  printf '\n=== %s ===\n' "$kind"
  for query in "${queries[@]}"; do
    java_body="$workdir/java.xml"
    rust_body="$workdir/rust.xml"
    java_status="$(fetch "$JAVA_URL" "$query" "$java_body")"
    rust_status="$(fetch "$RUST_URL" "$query" "$rust_body")"

    titles_of "$java_body" > "$workdir/java.titles"
    titles_of "$rust_body" > "$workdir/rust.titles"
    java_count="$(wc -l < "$workdir/java.titles" | tr -d ' ')"
    rust_count="$(wc -l < "$workdir/rust.titles" | tr -d ' ')"

    if [[ "$java_status" == "$rust_status" && "$java_count" == "$rust_count" ]] \
       && diff -q "$workdir/java.titles" "$workdir/rust.titles" >/dev/null; then
      printf '  OK   %-32s java=%s/%s rust=%s/%s\n' \
        "$query" "$java_status" "$java_count" "$rust_status" "$rust_count"
    else
      differences=$((differences + 1))
      printf '  DIFF %-32s java=%s/%s rust=%s/%s\n' \
        "$query" "$java_status" "$java_count" "$rust_status" "$rust_count"
      diff -u "$workdir/java.titles" "$workdir/rust.titles" \
        | sed -e 's/^/       /' || true
    fi
  done
done

if [[ "$differences" -eq 0 ]]; then
  echo "all queries match"
else
  printf '%d queries differ\n' "$differences"
  exit 1
fi
