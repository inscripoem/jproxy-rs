#!/usr/bin/env bash
# Export the five non-secret jproxy reference tables from the live NAS database
# into a local SQLite fixture. The source database is opened read-only and only
# the tables listed in TABLES are ever read. system_config, system_user and any
# credential material are never touched.
#
# Library-specific titles are replaced with synthetic placeholders before the
# fixture is written. Row counts and identifiers are preserved; only the public
# works asserted by tests/real_data.rs and their TMDB alternates are retained.
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

# Replace library titles with synthetic placeholders. The pinned rows are the
# public works used by tests/real_data.rs; every other title is private data.
sqlite3 "$OUTPUT" <<'SQL'
PRAGMA secure_delete = ON;

UPDATE sonarr_title
   SET main_title = 'Series ' || id,
       title = 'Series ' || id,
       clean_title = 'series ' || id
 WHERE main_title <> 'Jackie Chan Adventures';

UPDATE radarr_title
   SET main_title = 'Movie ' || id,
       title = 'Movie ' || id,
       clean_title = 'movie ' || id
 WHERE main_title <> 'The Fifth Element';

UPDATE tmdb_title
   SET title = 'Alternate ' || id
 WHERE tvdb_id <> 76711;

VACUUM;
SQL

printf 'library titles anonymized; row counts and identifiers preserved\n'
printf 'fixture written to %s\n' "$OUTPUT"
