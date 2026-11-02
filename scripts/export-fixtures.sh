#!/usr/bin/env bash
# Export the five non-secret jproxy reference tables from the live NAS database
# into a local SQLite fixture. The source database is opened read-only and only
# the tables listed in TABLES are ever read. system_config, system_user and any
# credential material are never touched.
#
# Usage:
#   scripts/export-fixtures.sh [ssh-target] [remote-db] [output-file]
#
# Defaults:
#   ssh-target   root@tower.local
#   remote-db    /mnt/user/appdata/servarr/jproxy/jproxy.db
#   output-file  fixtures/jproxy-live.db
set -euo pipefail

SSH_TARGET="${1:-root@tower.local}"
REMOTE_DB="${2:-/mnt/user/appdata/servarr/jproxy/jproxy.db}"
OUTPUT="${3:-fixtures/jproxy-live.db}"

TABLES=(sonarr_title radarr_title sonarr_rule radarr_rule tmdb_title)

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

mkdir -p "$(dirname "$OUTPUT")"
rm -f "$OUTPUT"

# Local schema so INSERT statements land in a database with identical columns.
sqlite3 "$OUTPUT" < migrations/0001_schema.sql

for table in "${TABLES[@]}"; do
  ssh -o BatchMode=yes "$SSH_TARGET" \
    "sqlite3 -readonly '$REMOTE_DB' '.mode insert $table' 'SELECT * FROM $table;'" \
    | sqlite3 "$OUTPUT"
  printf 'exported %-14s %s rows\n' "$table" \
    "$(sqlite3 "$OUTPUT" "SELECT count(*) FROM $table;")"
done

# The fixture intentionally carries no system_user/system_config tables.
sqlite3 "$OUTPUT" "DROP TABLE IF EXISTS system_user; DROP TABLE IF EXISTS system_config;"

printf 'fixture written to %s\n' "$OUTPUT"
