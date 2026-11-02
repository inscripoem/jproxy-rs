# jproxy-rs

A Rust implementation of the Phase 1 jproxy proxy between Sonarr/Radarr and
Prowlarr/Jackett. It forwards Torznab requests, rewrites search queries using
the existing `sonarr_title`, `radarr_title`, `sonarr_rule`, and `radarr_rule`
tables, applies `MIN_COUNT` alternate-title retries, formats returned item
titles with the configured rules, and caches upstream XML responses.

Phase 1 intentionally has **no admin UI and no authentication**. Do not expose
it to an untrusted network.

## Status

Implemented:

- Four Torznab proxy routes for Sonarr/Radarr through Prowlarr/Jackett.
- Query parameter pass-through, including duplicate parameters and the Torznab
  `apikey`.
- Title lookup in SQLite using the Java clean-title algorithm.
- Rule application in `priority ASC` order, with `{cleanTitle}` title matching,
  token formatting, year checks, and XML-escaped title replacement.
- Sonarr alternate-title attempts from `tmdb_title` and a final main-language
  fallback with season/episode removed.
- `MIN_COUNT` retry loop with page merging, limit trimming, and downstream
  offset/limit adjustment.
- In-memory upstream result cache with TTL
  `INDEXER_RESULT_CACHE_EXPIRES`, bounded entry count, and API-key-free keys.
- SQLite schema compatible with the upstream jproxy database.
- Periodically synchronized Sonarr/Radarr titles and optional JSON rule feeds.
- `GET /health` JSON endpoint.
- Multi-stage Dockerfile.
- Reference-data import from an existing `jproxy.db` (`jproxy-rs import`).
- End-to-end parity harness against recorded Torznab XML fixtures.
- Bounded upstream concurrency with exponential backoff, retry on 408/429/5xx,
  and meaningful downstream error mapping (`429`, `502`, `504`).

Not implemented (deferred Java behavior):

- Admin UI, login, users, sessions, and `LOGIN_ENABLED` enforcement.
- File renaming for qBittorrent/Transmission and `RENAME_FILE` behavior.
- Downloader login tasks and downloader API proxying.
- TMDB API synchronization. Existing `tmdb_title` rows are used, but this
  service does not fetch new ones.
- On-demand title synchronization when a query misses the local database.
- The GitHub author-directory rule source. Direct JSON rule URLs are supported
  instead.
- Example-table management and the Java cache/system-config REST APIs.
- The original Java Spring/Caffeine cache sizes and admin cache controls.

## Build and test

The project includes a Nix devShell. The machine's global Rust installation is
not used by the project commands.

```sh
direnv allow            # once, for this machine; do not put this in scripts
nix develop             # or enter through direnv after allow
cargo build             # debug
cargo build --release   # release binary at target/release/jproxy-rs
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Every command in this repository is expected to run inside the devShell. The
flake provides `cargo`, `rustc`, `clippy`, `rustfmt`, `pkg-config`, and the
`sqlite` command line tool. `flake.lock` pins the Nix inputs.

The test suite includes:

- `src/**` unit tests;
- `tests/parity.rs` — the behaviour parity harness (SQLite + mock Torznab
  upstream, no live service);
- `tests/upstream_errors.rs` — retry, concurrency and error-mapping tests;
- `tests/real_data.rs` — validation against data exported from a live jproxy.

The release profile sets `opt-level = "z"`, thin LTO, one codegen unit and
`strip = true`; the resulting binary is about 8.5 MB and idles at roughly
9.5 MB RSS. Panic unwinding is left enabled so a handler panic cannot abort the
whole process.

## Run

Minimal Prowlarr-only deployment:

```sh
export PROWLARR_URL=http://127.0.0.1:9696
export DATABASE_PATH=/var/lib/jproxy/jproxy.db
export LISTEN_ADDR=0.0.0.0:8117
export SYNC_ENABLED=false
target/release/jproxy-rs
```

The service creates missing tables on startup. It does not copy users, API
keys, or any other upstream `system_config` values.

Torznab URLs to configure in Sonarr/Radarr:

```text
http://jproxy:8117/sonarr/prowlarr/{indexer_id}/api
http://jproxy:8117/radarr/prowlarr/{indexer_id}/api
http://jproxy:8117/sonarr/jackett/{indexer_id}/api
http://jproxy:8117/radarr/jackett/{indexer_id}/api
```

All query parameters are forwarded. For Prowlarr, the upstream path is
`{PROWLARR_URL}/{indexer_id}/api`. For Jackett, the route is translated to the
real Torznab endpoint
`{JACKETT_URL}/api/v2.0/indexers/{indexer_id}/results/torznab/api`.

Health:

```sh
curl -fsS http://127.0.0.1:8117/health
{"status":"ok","version":"0.1.0","database":"ok","result_cache_entries":0}
```

## Real data import

The reference tables in an existing Java jproxy database use exactly the schema
in `migrations/0001_schema.sql`, so the service can open that file directly.
To copy only the non-secret reference tables into a fresh database:

```sh
target/release/jproxy-rs import --from /path/to/jproxy.db --to ./jproxy.db
```

The importer copies `sonarr_title`, `radarr_title`, `sonarr_rule`,
`radarr_rule` and `tmdb_title` inside one transaction, replacing the target
tables so repeated imports are idempotent. It never reads or writes
`system_config` or `system_user`. The source is opened read-only and the
importer refuses to run when the two paths are identical.

`scripts/export-fixtures.sh` produced the checked-in fixture
`fixtures/jproxy-live.db` from the live NAS database using
`sqlite3 -readonly`:

```sh
nix develop --command scripts/export-fixtures.sh [user@host] [remote.db] [output]
```

The fixture contains no `system_config`/`system_user` tables. Its row counts
match the live `SELECT count(*)` values recorded at export time
(`sonarr_title` 46, `radarr_title` 1535, `sonarr_rule` 90, `radarr_rule` 38,
`tmdb_title` 20); `tests/real_data.rs` asserts them.

## Parity harness and local integration

`tests/parity.rs` starts an in-process mock Torznab indexer, seeds a temporary
SQLite database and drives the real router, so query rewriting, rule ordering,
`MIN_COUNT` handling and item-title rewriting can be compared against the Java
behaviour without any network access. Fixtures live under `fixtures/torznab/`
and are synthetic; none contains a credential or upstream URL.

To exercise the Rust proxy against a real Prowlarr/Jackett on the LAN:

```sh
export PROWLARR_URL=http://prowlarr.lan:9696
scripts/run-local.sh            # builds if needed, listens on 127.0.0.1:9117
```

The proxy forwards the `apikey` it receives, so it needs no key to start. The
script prints a smoke-test command that reads `PROWLARR_API_KEY` from the
environment; the key is never written to disk or echoed.

To compare the Java jproxy and the Rust rewrite for the same queries:

```sh
export PROWLARR_API_KEY=...     # required; never committed or printed
scripts/compare.sh              # JAVA_URL/RUST_URL/INDEXER_ID/QUERIES_FILE override defaults
```

Both proxies must point at the same Prowlarr/Jackett and use the same database.
`compare.sh` reports the HTTP status, item count and a diff of the item titles
for each query, and exits non-zero when any query differs. Without
`PROWLARR_API_KEY` it prints instructions and makes no live request.

## Configuration

Configuration is environment-only in Phase 1. Existing `system_config` values
are not read; keep secrets in the process environment or a service manager's
secret store.

| Variable | Default | Purpose |
| --- | --- | --- |
| `LISTEN_ADDR` | `0.0.0.0:8117` | HTTP listen socket. |
| `DATABASE_PATH` | `./jproxy.db` | SQLite file. Missing tables are created. |
| `PROWLARR_URL` | unset | Prowlarr base URL. |
| `JACKETT_URL` | unset | Jackett base URL. |
| `SONARR_URL` | unset | Sonarr base URL for title sync. |
| `SONARR_API_KEY` | unset | Sonarr API key for sync. Never logged. |
| `RADARR_URL` | unset | Radarr base URL for title sync. |
| `RADARR_API_KEY` | unset | Radarr API key for sync. Never logged. |
| `SONARR_RULES_URL` | unset | Optional direct URL to a Sonarr rule JSON array. |
| `RADARR_RULES_URL` | unset | Optional direct URL to a Radarr rule JSON array. |
| `CLEAN_TITLE_REGEX` | final upstream migration value | Clean-title punctuation regex. |
| `SONARR_INDEXER_FORMAT` | `{title} {season}{episode} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}` | Sonarr item format. |
| `RADARR_INDEXER_FORMAT` | `{title} {year} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}` | Radarr item format. |
| `MIN_COUNT` | `6` | Stop alternate-title retries at this count. `0` disables retries. |
| `CACHE_EXPIRES` | `4320` | Minutes for cached formatting rules and title candidates. |
| `INDEXER_RESULT_CACHE_EXPIRES` | `15` | Minutes for upstream Torznab XML results. |
| `SYNC_INTERVAL` | `3` | Minutes between background sync cycles. `0` disables sync. |
| `SYNC_ENABLED` | `true` | Set `false` to disable the background task completely. |
| `RENAME_FILE` | `true` | Parsed for deployment compatibility; Phase 1 ignores it. |
| `LOGIN_ENABLED` | `false` | Parsed for deployment compatibility; Phase 1 has no auth. |
| `REQUEST_TIMEOUT_SECS` | `30` | Upstream and sync HTTP timeout. |
| `RESULT_CACHE_MAX_ENTRIES` | `1000` | Maximum cached Torznab responses. |
| `UPSTREAM_MAX_CONCURRENCY` | `8` | Maximum simultaneous upstream indexer requests. |
| `UPSTREAM_MAX_RETRIES` | `2` | Retries for 408/429/5xx and transport failures. |
| `UPSTREAM_RETRY_BASE_MS` | `250` | Base delay for exponential backoff. |
| `UPSTREAM_RETRY_MAX_MS` | `4000` | Maximum delay per retry; caps `Retry-After`. |

`SYNC_INTERVAL=0`, `SYNC_ENABLED=false`, or a missing Sonarr/Radarr URL or API
key disables the corresponding sync. A missing Prowlarr/Jackett URL makes the
related routes return HTTP 503.

### Rule synchronization

Sonarr and Radarr APIs expose series/movie metadata, not jproxy formatting
rules. Phase 1 therefore syncs titles from `/api/v3/series` and `/api/v3/movie`,
and optionally syncs rules from `SONARR_RULES_URL` / `RADARR_RULES_URL`. The
rule source must return the upstream rule JSON array shape:

```json
[
  {
    "id": "RULE-ID",
    "token": "title",
    "priority": 999,
    "regex": ".*(\\b|\\s){cleanTitle}(\\b|\\s).*",
    "replacement": "{mainTitle}",
    "offset": 0
  }
]
```

Rule rows are upserted by `id`. Title rows are upserted by the Java-compatible
`{external_id}{sno}` integer key. Sync does not delete rows that disappeared
upstream, matching Java's `saveOrUpdateBatch` behavior.

## Query behavior

For a request with a non-blank `q`:

1. `q` has a trailing `" 00"` removed, matching Java's absolute-episode cleanup.
2. Sonarr strips a trailing numeric episode token before title lookup; Radarr
   strips a trailing ` (19|20)xx` year token before lookup. The upstream query
   keeps the original `q` in both cases.
3. The clean title is looked up in `sonarr_title`/`radarr_title`; Sonarr also
   retries after removing a trailing `" Sxx"` season token.
4. The first upstream request preserves the original query except for the
   trailing `" 00"` cleanup.
5. If the merged result count is below `MIN_COUNT`, TMDB alternate titles are
   tried for Sonarr main/slug rows. Radarr tries the title without its year and
   may try the main title for folder/clean-title rows.
6. If still below `MIN_COUNT`, the main-language title from the matched row is
   tried. For Sonarr, season/episode text is stripped from the fallback title and
   the `season`/`ep` query parameters are removed.

Requests without a non-blank `q` (for example capabilities requests) are
proxied once without search rewriting.

`MIN_COUNT=0` returns after the first upstream request. This is a deliberate
simplification of the Java loop, which still tried additional titles but
skipped the final stripped fallback.

## Upstream error handling

Upstream requests are bounded by `UPSTREAM_MAX_CONCURRENCY` and retried with
exponential backoff plus jitter on `408`, `429`, any `5xx`, timeouts and
transport failures. A `Retry-After` header (seconds) overrides the computed
delay, capped by `UPSTREAM_RETRY_MAX_MS`. The URL is built once, so retries
reuse the incoming Torznab query unchanged.

Failures are mapped instead of becoming generic 500s:

| Upstream result | Downstream status |
| --- | --- |
| `429` | `429` (original `Retry-After` forwarded) |
| Any `5xx` / transport failure | `502` |
| Timeout | `504` |
| Missing provider URL | `503` |
| Non-retryable 4xx | `502` (single attempt) |

For multi-title searches, a failure after at least one page has been merged
returns the partial results with HTTP 200; a failure on the first attempt
propagates the mapped status. Java aborts the whole request on the first
upstream error.

## Caching

- The result cache stores final XML bodies and content types for successful
  responses containing `<channel>`.
- Cache keys include the app kind, provider, indexer id, and normalized query
  parameters, but exclude `apikey`.
- Cache entries are process-local and are not evicted when rules or titles are
  synced. Lower `INDEXER_RESULT_CACHE_EXPIRES` when rule changes must take
  effect quickly.
- Formatting rules/title candidates are cached separately for `CACHE_EXPIRES`
  and are cleared after a successful background sync.

## SQLite compatibility

The bundled `sqlx` SQLite build uses its own SQLite library. The schema in
`migrations/0001_schema.sql` mirrors the final upstream Liquibase migrations,
including the later `series_id` and `movie_id` columns. There is no Liquibase
metadata table in this project, so an existing `jproxy.db` can be opened
directly, but run it against a backup first. The service never reads
`system_config` or `system_user` rows.

## Docker

The Dockerfile builds the release binary in a Rust stage and copies only the
binary into a distroless runtime image.

```sh
docker build -t jproxy-rs .
docker run --rm -p 8117:8117 \
  -v jproxy-data:/data \
  -e PROWLARR_URL=http://prowlarr:9696 \
  -e DATABASE_PATH=/data/jproxy.db \
  -e SYNC_ENABLED=false \
  jproxy-rs
```

`SYNC_ENABLED=false` is recommended when the container has no route to a
Sonarr/Radarr instance. Supply API keys through your container runtime's secret
mechanism rather than committing them.

## Notable assumptions and deviations

- **Rule regex flavor.** Rules compile with Rust's `regex` crate, which does not
  implement backreferences or look-around. The upstream bundled rule set was
  checked and contains neither; such rules are skipped with a warning if added.
- **Rules are formatting rules.** Search alternates come from title tables and
  TMDB rows; `*_rule` rows select/format an item's output title, as in Java.
- **Search-loop shape (`MIN_COUNT`).** Java always walks the whole title list
  (query title, every TMDB alternate, then the first alternate repeated) and
  uses `MIN_COUNT` only to decide whether to strip season/episode on the last
  entry. This implementation stops as soon as the merged count reaches
  `MIN_COUNT`, which is the documented intent ("append the primary-language
  title while the count is below the value") and avoids over-fetching. As a
  result the Rust proxy may send fewer upstream requests than Java for the same
  query; the first alternate-title queries are identical.
- **Fallback title.** Java's final entry is the first TMDB title repeated, with
  season/episode stripped from the query when below `MIN_COUNT`. This
  implementation uses the matched row's `main_title` with season/episode text
  removed and the `season`/`ep` parameters cleared. The final query can differ;
  tests pin the divergence.
- **Radarr alternate title.** For folder/slug rows (sno 1/2) Java re-queries
  the main row's lowercase `clean_title` twice. This implementation queries the
  proper-cased `main_title` once (duplicates removed).
- **Radarr year handling.** The year suffix is stripped before the SQLite lookup
  and for the year-less retry, matching Java's net effect.
- **Rule tie-break.** Java orders rules by `priority ASC` only. This
  implementation adds a stable `id ASC` tie-break so equal priorities are
  deterministic; the bundled rule set has unique priorities.
- **No on-demand sync.** A query miss does not trigger a live Sonarr/Radarr sync.
  The background task is the only updater.
- **Rule source.** Sonarr/Radarr do not expose formatting rules, so optional
  direct JSON URLs replace Java's GitHub author-directory feed.
- **Existing DB configuration.** Environment variables are authoritative.
  `system_config` values from an existing database are intentionally ignored.
- **Cache scope.** Java's Caffeine caches are replaced with a small process-local
  TTL cache. There is no distributed or persistent cache.
- **Response formatting always re-encodes changed titles.** Unchanged item
  titles, all other XML, and the XML declaration are preserved. A response is
  returned byte-for-byte when it has no applicable item title changes.

## Project layout

```text
src/app.rs          Axum routes, proxy flow, health
src/search.rs       alternate-title plan and MIN_COUNT retry executor
src/rules.rs        priority-ordered compiled rules
src/formatting.rs   Java-compatible item title formatting
src/torznab.rs      item counting, page merge/trim, XML title rewrite
src/cache.rs        bounded TTL cache
src/db.rs           sqlx SQLite queries and schema setup
src/import.rs       reference-table import path (`jproxy-rs import`)
src/sync.rs         Sonarr/Radarr title sync and optional rule sync
src/upstream.rs     reqwest/rustls upstream client, retry and concurrency
src/query.rs        lossless-enough query parameter editing and cache keys
src/text.rs         Java FormatUtil ports
migrations/         Java-compatible SQLite schema
fixtures/           exported reference DB and synthetic Torznab XML
tests/              parity, upstream-error and real-data integration tests
scripts/            fixture export, local run and Java/Rust comparison
```
