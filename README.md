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
- Scheduled Sonarr/Radarr title sync with Java-compatible upserts, plus TMDB
  title sync for Sonarr rows with no translation.
- On-demand Sonarr/Radarr title sync when a query misses the local database,
  bounded by a timeout and degrading to normal proxy behavior on failure.
- Rule sync using the Java GitHub author-directory protocol: `author.json`,
  `sonarr@{author}.json`, and `radarr@{author}.json`, with backup-location
  fallback and per-author error isolation.
- Baseline `system_config` keys (`sonarrUrl`, `sonarrApikey`, `radarrUrl`,
  `radarrApikey`, `tmdbUrl`, `tmdbApikey`, `sonarrLanguage1`,
  `sonarrLanguage2`, `ruleSyncAuthors`) with environment override, plus the
  downloader keys described below. Downloader credentials are read only to log
  in and are never logged.
- qBittorrent and Transmission login with scheduled session refresh. Both
  failures and successes are logged without credentials or session values.
- Sonarr/Radarr history rename tasks, including `RENAME_FILE` file renaming for
  qBittorrent and the Java-compatible Transmission `torrent-rename-path` flow.
- `GET /health` JSON endpoint.
- Multi-stage Dockerfile.
- Reference-data import from an existing `jproxy.db` (`jproxy-rs import`).
- End-to-end parity harness against recorded Torznab XML fixtures.
- Bounded upstream concurrency with exponential backoff, retry on 408/429/5xx,
  and meaningful downstream error mapping (`429`, `502`, `504`).

Not implemented (deferred Java behavior):

- Admin UI, login, users, sessions, and `LOGIN_ENABLED` enforcement.
- Downloader API proxying (the `/sonarr/qbittorrent`, `/sonarr/transmission`,
  and Radarr equivalents of the Java Charon routes). Login and rename use the
  downloader APIs directly.
- Example-table management and the Java cache/system-config REST APIs.
- Java's admin cache controls and distributed/persistent caching.

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
- `tests/sync.rs` — title, TMDB, rule and on-demand synchronization against
  local mock Sonarr/Radarr/TMDB/rule endpoints;
- `tests/upstream_errors.rs` — retry, concurrency and error-mapping tests;
- `tests/real_data.rs` — validation against data exported from a live jproxy.

The release profile sets `opt-level = "z"`, thin LTO, one codegen unit and
`strip = true`; the resulting binary is about 8.5 MB. Panic unwinding is left
enabled so a handler panic cannot abort the whole process. See
[Resource use](#resource-use) for measured RSS.

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
keys, or any other upstream `system_config` values. At runtime it reads only the
whitelisted baseline keys described under Configuration; downloader credentials
and `system_user` rows are never selected.

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

Precedence is **environment > `system_config` > built-in default** for the
keys listed below. Only the following `system_config` keys are read:
`sonarrUrl`, `sonarrApikey`, `radarrUrl`, `radarrApikey`, `tmdbUrl`,
`tmdbApikey`, `sonarrLanguage1`, `sonarrLanguage2`, `ruleSyncAuthors`,
`ruleLocation`, `ruleLocationBackup`, `qbittorrentUrl`, `qbittorrentUsername`,
`qbittorrentPassword`, `transmissionUrl`, `transmissionUsername`,
`transmissionPassword`. Downloader credentials are held in memory, used only
for login, and are never logged or included in the startup summary;
`system_user` rows are never selected. Values are resolved once at startup;
restart the process after changing `system_config` or the environment.

All other settings remain environment-only.

| Variable | Default | Purpose |
| --- | --- | --- |
| `LISTEN_ADDR` | `0.0.0.0:8117` | HTTP listen socket. |
| `DATABASE_PATH` | `./jproxy.db` | SQLite file. Missing tables are created. |
| `PROWLARR_URL` | unset | Prowlarr base URL. |
| `JACKETT_URL` | unset | Jackett base URL. |
| `SONARR_URL` | `system_config.sonarrUrl` | Sonarr base URL for title sync. |
| `SONARR_API_KEY` | `system_config.sonarrApikey` | Sonarr API key for sync. Never logged. |
| `RADARR_URL` | `system_config.radarrUrl` | Radarr base URL for title sync. |
| `RADARR_API_KEY` | `system_config.radarrApikey` | Radarr API key for sync. Never logged. |
| `TMDB_URL` | `https://api.themoviedb.org` | TMDB base URL. |
| `TMDB_API_KEY` | `system_config.tmdbApikey` | TMDB API key. Never logged. |
| `SONARR_LANGUAGE_1` | `zh-CN` | First TMDB title language. |
| `SONARR_LANGUAGE_2` | `zh-TW` | Second TMDB title language. |
| `RULE_SYNC_AUTHORS` | `system_config.ruleSyncAuthors` (`ALL`) | Comma-separated authors or `ALL`. Blank disables rule sync. |
| `RULE_LOCATION` | Java GitHub rule directory | Base URL for `author.json` and rule files. Blank disables rule sync. |
| `RULE_LOCATION_BACKUP` | Java backup mirror | Second base URL tried when the primary fails. Blank disables the backup. |
| `CLEAN_TITLE_REGEX` | final upstream migration value | Clean-title punctuation regex. |
| `SONARR_INDEXER_FORMAT` | `{title} {season}{episode} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}` | Sonarr item format. |
| `RADARR_INDEXER_FORMAT` | `{title} {year} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}` | Radarr item format. |
| `MIN_COUNT` | `6` | Stop alternate-title retries at this count. `0` disables retries. |
| `CACHE_EXPIRES` | `4320` | Minutes for title, rule, and formatting caches (after-access). |
| `INDEXER_RESULT_CACHE_EXPIRES` | `15` | Minutes for upstream Torznab XML results (after-create). |
| `CACHE_MAX_ENTRIES` | `1000` | Maximum title-cache entries. |
| `SYNC_INTERVAL` | `3` | Minutes for the title/on-demand sync gate. `0` disables sync. |
| `SYNC_ENABLED` | `true` | Set `false` to disable background and on-demand sync. |
| `ONDEMAND_SYNC_TIMEOUT_SECS` | `30` | Upper bound for one on-demand sync attempt. |
| `QBITTORRENT_URL` | `system_config.qbittorrentUrl` | qBittorrent base URL. The client appends `/api/v2/...`. |
| `QBITTORRENT_USERNAME` | `system_config.qbittorrentUsername` | qBittorrent username. Never logged. |
| `QBITTORRENT_PASSWORD` | `system_config.qbittorrentPassword` | qBittorrent password (blank allowed). Never logged. |
| `TRANSMISSION_URL` | `system_config.transmissionUrl` | Full Transmission JSON-RPC URL, normally `http://host:9091/transmission/rpc`. |
| `TRANSMISSION_USERNAME` | `system_config.transmissionUsername` | Transmission username. Never logged. |
| `TRANSMISSION_PASSWORD` | `system_config.transmissionPassword` | Transmission password (blank allowed). Never logged. |
| `DOWNLOADER_LOGIN_INTERVAL_SECS` | `1800` | Seconds between scheduled re-logins (Java runs every 30 minutes). `0` keeps the startup login only. |
| `SONARR_RENAME_INTERVAL_SECS` | `30` | Seconds between Sonarr rename passes (Java cron `0/30 * * * * ?`). `0` disables the task. |
| `RADARR_RENAME_INTERVAL_SECS` | `60` | Seconds between Radarr rename passes (Java cron `0 0/1 * * * ?`). `0` disables the task. |
| `SONARR_RENAME_FALLBACK_MINS` | `30` | Sonarr history lookback window in minutes. |
| `RADARR_RENAME_FALLBACK_MINS` | `30` | Radarr history lookback window in minutes. |
| `RENAME_FILE` | `true` | Also rename files inside a qBittorrent torrent after the torrent rename succeeds. Transmission has no file-level rename. |
| `LOGIN_ENABLED` | `false` | Parsed for deployment compatibility; this service has no auth. |
| `REQUEST_TIMEOUT_SECS` | `30` | Upstream and sync HTTP timeout. |
| `RESULT_CACHE_MAX_ENTRIES` | `1000` | Maximum cached Torznab responses. |
| `UPSTREAM_MAX_CONCURRENCY` | `8` | Maximum simultaneous upstream indexer requests. |
| `UPSTREAM_MAX_RETRIES` | `2` | Retries for 408/429/5xx and transport failures. |
| `UPSTREAM_RETRY_BASE_MS` | `250` | Base delay for exponential backoff. |
| `UPSTREAM_RETRY_MAX_MS` | `4000` | Maximum delay per retry; caps `Retry-After`. |

`SYNC_INTERVAL=0` or `SYNC_ENABLED=false` disables both background and
on-demand synchronization. A missing Sonarr/Radarr URL or API key skips that
kind. A missing Prowlarr/Jackett URL makes the related routes return HTTP 503.

### Background schedule

Titles synchronize hourly and rules daily, approximating the Java cron schedule
from process start rather than at Java's fixed minutes. The first tick runs
immediately. The `SYNC_INTERVAL` gate limits title synchronization (background
and on-demand) to one attempt per interval; it does not make rules refresh every
three minutes. `SYNC_INTERVAL` defaults to 3, matching Java's Caffeine gate.

Downloader login runs once at startup, like Java's `CommandLineRunner`, then
every `DOWNLOADER_LOGIN_INTERVAL_SECS` (30 minutes by default). Sonarr rename
runs every 30 seconds and Radarr rename every minute, matching the Java cron
entries; like a cron entry, neither rename task fires at process start. All
three schedules start counting from process start rather than at fixed wall
clock seconds.

### Downloader login

The service logs in to every downloader that has a URL, username, and password
configured. Credentials come from the environment first, then `system_config`,
and remain in memory only.

- **qBittorrent**: `POST {QBITTORRENT_URL}/api/v2/auth/login` with a form body.
  A 2xx response with a non-blank `Set-Cookie` header establishes the session.
  The cookie is sent verbatim as the `Cookie` header on later calls. The login
  response body is not inspected, matching Java.
- **Transmission**: `POST {TRANSMISSION_URL}` with a JSON-RPC `session-get` and
  HTTP Basic authentication. A `409 Conflict` carrying
  `X-Transmission-Session-Id` establishes the session. Any later 409 triggers
  one re-login and one retry.

A login failure is non-fatal: it is logged at debug level with no credential or
session value, and the next scheduled pass retries. If the process starts while
a downloader is unavailable, proxy traffic is unaffected.

### Rename tasks

Both tasks follow Java's history flow:

1. Return immediately unless at least one downloader is logged in.
2. Fetch `GET {sonarr|radarr}/api/v3/history/since?eventType=1&date=...&apikey=...`
   with `date` set to now minus the configured fallback window in UTC
   (`yyyy-MM-dd'T'HH:mm:ss.SSS'Z'`).
3. Read `sourceTitle`, `downloadId` (lowercased when non-blank), and
   `data.torrentInfoHash`/`data.downloadClient`. Sonarr deduplicates records by
   torrent hash; Radarr processes every record.
4. Send the rename to Transmission when `downloadClient` is `Transmission`
   (case-insensitive); otherwise use qBittorrent.
5. When `RENAME_FILE=true` and the qBittorrent torrent rename succeeded, list
   the torrent files (`/api/v2/torrents/files`) and rename each one through
   `/api/v2/torrents/renameFile`.

Sonarr file names are formatted from the existing `season` and `episode` rules
(priority order, offset applied) with the Java token fallback; a file whose name
does not resolve to `SxxExx`/`Exx` keeps its original name. Radarr file names
become the source title. Subtitle extensions are numbered `.1.srt`, `.2.srt`,
and so on, as in Java. A failure for one history item or one file is logged and
isolated; remaining items still run. The tasks never delete media or touch the
filesystem directly.

Transmission renames preserve the existing extension, replace `:` with `_`, and
skip the RPC when the name is unchanged, the torrent name is unavailable, or
the name equals the torrent hash. Transmission exposes no file-level rename in
Java, so `RENAME_FILE` applies only to qBittorrent.

### Rule synchronization

Sonarr and Radarr do not expose jproxy formatting rules. The service follows
Java's GitHub author-directory protocol:

1. Read `ruleSyncAuthors`. `ALL` fetches `{RULE_LOCATION}/author.json` and uses
   its JSON string array; otherwise the value is split on commas.
2. For each author and kind, fetch `{RULE_LOCATION}/sonarr@{author}.json` or
   `{RULE_LOCATION}/radarr@{author}.json`.
3. If the primary location fails, retry the same path under
   `RULE_LOCATION_BACKUP`.
4. Upsert by `id`. Remote `validStatus: 1` is treated as "do not change" so a
   locally disabled rule stays disabled; `validStatus: 0` disables it. Missing
   rule fields preserve existing values.

A failure for one author is logged and skipped; the remaining authors still
sync. A blank `RULE_SYNC_AUTHORS` or `RULE_LOCATION` disables rule sync.

### TMDB title synchronization

After a Sonarr title sync, the service selects `sonarr_title.sno = 0` rows whose
`tvdb_id` has no `tmdb_title` row. For each id and each configured language it
calls `{TMDB_URL}/3/find/{tvdbId}?api_key=...&language=...&external_source=tvdb_id`
and inserts the first `tv_results` entry. The API key is read from the
environment or `system_config.tmdbApikey` and is never logged. Tests use a local
mock; no test makes a live TMDB request.

### On-demand synchronization

When `find_title` misses, the proxy calls the same gated title sync as the
background task, then refreshes TMDB for Sonarr (matching
`syncAndGetSonarrTitle`). A per-kind mutex serializes concurrent misses. The
whole path is bounded by `ONDEMAND_SYNC_TIMEOUT_SECS`; a sync failure is logged
and the request continues with its normal proxy behavior instead of returning
500. The resolved lookup, including a miss, is cached like Java's synthetic
search-title result and is cleared by the next successful sync.

## Query behavior

For a request with a non-blank `q`:

1. `q` has a trailing `" 00"` removed, matching Java's absolute-episode cleanup.
2. Sonarr strips a trailing numeric episode token before title lookup; Radarr
   strips a trailing ` (19|20)xx` year token before lookup. The upstream query
   keeps the original `q` in both cases.
3. The clean title is looked up in `sonarr_title`/`radarr_title`; Sonarr also
   retries after removing a trailing `" Sxx"` season token. On a miss, the
   service attempts a gated on-demand title sync (and TMDB for Sonarr) before
   continuing.
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

- The indexer-result cache stores final XML bodies and content types for
  successful responses containing `<channel>`. It uses
  `INDEXER_RESULT_CACHE_EXPIRES` with Caffeine's `expireAfterCreate` semantics,
  matching Java's `indexerResultCache`, and is **not** evicted when rules or
  titles are synced.
- Cache keys include the app kind, provider, indexer id, and normalized query
  parameters, but exclude `apikey`.
- The title lookup cache uses `CACHE_EXPIRES` with `expireAfterAccess`
  semantics, matching the Java `sonarr_search_title`/`radarr_search_title`
  caches. It stores the resolved lookup, including the synthetic miss result
  Java caches, and is cleared by the next successful reference-data sync.
- Formatting rules and title candidates use `CACHE_EXPIRES` with
  `expireAfterAccess`, matching the Java `*_result_title` and `*_rule` caches.
- Both reference-data caches are cleared after a successful title, TMDB, or rule
  sync.
- All caches are process-local and bounded. Eviction is not LRU-identical to
  Caffeine; Java's admin cache controls are not part of this baseline.

## Resource use

Measured with the release binary, a temporary SQLite database, and local mock
Sonarr/Radarr/TMDB/rule/Torznab endpoints. RSS is `VmRSS` from
`/proc/<pid>/status`, sampled three times per phase; the first three rows are
the median of three independent runs, with the observed range in parentheses.
The full-cache row is one run.

| Phase | Workload | RSS |
| --- | --- | --- |
| Idle | `SYNC_ENABLED=false`, no requests | 9.5 MB (9,636–9,772 KB) |
| After one sync pass | 120 series (360 title rows), 600 movies (3,000 title rows), 240 TMDB rows, 5+5 rules | 12.5 MB (12,720–13,252 KB) |
| Under proxy load | 2,000 Torznab queries, 16 concurrent, 500 distinct `q` values | 25.6 MB (26,116–26,336 KB) |
| Under proxy load, full cache | 2,000 Torznab queries, 16 concurrent, 2,000 distinct `q` values, one run | 28.0 MB (28,692 KB) |

Both load phases are below the 30–45 MB baseline target on this synthetic
corpus. The full-cache row exercises the 1,000-entry result-cache capacity;
larger upstream feeds, a larger database, or a different allocator can raise
the number. The numbers are current `VmRSS`, not peak `VmHWM`; SQLite page cache
and allocator retention are included. The mocks and data are local and
synthetic; no live service or credential was used.

Downloader login and rename add one qBittorrent session, one Transmission
session, and the rename workers. Measured with the release binary, a temporary
SQLite database, and local mock Sonarr/qBittorrent/Torznab endpoints. Both
downloaders were configured; the measurement used synthetic credentials and the
Sonarr rename interval was shortened to 10 seconds so a pass could be observed.
RSS is `VmRSS`, median of three independent runs, five samples per run.

| Phase | Workload | RSS |
| --- | --- | --- |
| Idle with downloaders configured | Initial qBittorrent login complete, no rename pass yet, no queries | 9.8 MB (10,028–10,300 KB) |
| After one login/rename pass | qBittorrent login, Sonarr history, torrent rename, one file rename | 10.3 MB (10,572–10,848 KB) |
| Under proxy load | 2,000 Torznab queries, 16 concurrent, 500 distinct `q` values, 500 cached results | 13.9 MB (14,140–14,596 KB) |

The measured log contained none of the synthetic password, cookie, or API key
values. The feature profile remains below both the 30–45 MB baseline target and
the 100 MB full-parity target.

## SQLite compatibility

The bundled `sqlx` SQLite build uses its own SQLite library. The schema in
`migrations/0001_schema.sql` mirrors the final upstream Liquibase migrations,
including the later `series_id` and `movie_id` columns. There is no Liquibase
metadata table in this project, so an existing `jproxy.db` can be opened
directly, but run it against a backup first. The service reads only the
whitelisted `system_config` keys, never `system_user`, and never writes
downloader credentials back to the database.

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
- **Sync schedule.** Titles run hourly and rules daily from process start. Java
  uses cron at `:00`/`:30` and `00:15`/`01:45` local time. The three-minute
  `SYNC_INTERVAL` gate and the Java-compatible upserts are preserved.
- **On-demand sync.** Java's synchronized path can propagate an HTTP failure.
  This implementation bounds the attempt with `ONDEMAND_SYNC_TIMEOUT_SECS`,
  serializes concurrent misses per kind, logs failures, and continues with the
  normal proxy response instead of returning 500.
- **Rule source.** The Java author-directory protocol is used. `ruleLocation`
  and `ruleLocationBackup` are accepted as `system_config` keys (and
  `RULE_LOCATION`/`RULE_LOCATION_BACKUP` environment variables) because Java
  keeps these in `application-prod.yml` rather than `system_config`. Author
  names are restricted to ASCII letters, digits, `_`, `-`, and `.` so a remote
  author list cannot alter the rule URL path. Java checks the primary location
  once at startup and switches the global rule location to the backup; this
  implementation retries the backup for each fetch.
- **Rename schedule.** Sonarr (30 s) and Radarr (60 s) intervals match the Java
  cron expressions, but the ticker counts from process start instead of the
  wall-clock second. The history fallback windows are 30 minutes for both, as
  in `application-prod.yml`. `DOWNLOADER_LOGIN_INTERVAL_SECS`,
  `SONARR_RENAME_INTERVAL_SECS`, `RADARR_RENAME_INTERVAL_SECS`, and the two
  fallback variables are environment extensions because Java stores these
  timings in `application-prod.yml`.
- **Downloader configuration.** qBittorrent is configured as a base URL; the
  client appends `/api/v2/...`. Transmission is configured with the full
  JSON-RPC URL, as in Java. Credentials now come from the same whitelist as
  other `system_config` keys, with environment override.
- **Downloader logging.** Java logs the qBittorrent cookie at debug level. This
  implementation never logs cookies, session ids, passwords, or API keys.
  Login failures are debug-level and non-fatal.
- **Per-item rename errors.** A missing source title, an unavailable torrent
  name, or a downloader HTTP error is isolated to that history item. Java
  raises inside the item loop and catches the same class of failures; the
  difference is that Rust skips blank titles instead of attempting a null
  rename.
- **qBittorrent responses.** Non-2xx rename/renameFile responses are errors for
  that item, matching RestTemplate's exception behavior. The login path only
  requires a 2xx status and a non-blank `Set-Cookie`, ignoring the body.
- **Transmission responses.** Only HTTP status is checked; the JSON-RPC
  `result` field is ignored, matching Java. A 409 triggers one re-login and one
  retry; a second failure is isolated to the item.
- **File rename scope.** File renames run only after a successful qBittorrent
  torrent rename, only when `RENAME_FILE=true`, and use the Java path
  construction, extension matching, subtitle numbering, and Sonarr token
  fallback. Radarr uses the source title. Transmission has no file rename. No
  media is deleted or moved outside the downloader.
- **Configuration precedence.** Environment values override `system_config`;
  built-in defaults fill the rest. Only the whitelisted keys are read, and no
  value is logged.
- **Rule fields.** Missing JSON fields preserve existing values, matching
  MyBatis `update-strategy=NOT_NULL`. The Java-compatible `validStatus=1`
  "do not change" behavior is preserved.
- **TMDB errors.** A failed TMDB lookup for one id/language is logged and the
  remaining ids still sync; Java aborts the whole transaction. Successful rows
  are committed together.
- **Radarr clean title.** If Radarr omits `cleanTitle`, the service derives it
  from the folder title. Java would attempt to write a null clean title.
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
src/downloader.rs   qBittorrent/Transmission login, sessions and API calls
src/import.rs       reference-table import path (`jproxy-rs import`)
src/rename.rs       Sonarr/Radarr history rename tasks and schedules
src/sync.rs         Sonarr/Radarr title sync, TMDB sync, rule sync, on-demand
src/upstream.rs     reqwest/rustls upstream client, retry and concurrency
src/query.rs        lossless-enough query parameter editing and cache keys
src/text.rs         Java FormatUtil ports
migrations/         Java-compatible SQLite schema
fixtures/           exported reference DB and synthetic Torznab XML
tests/              parity, sync, downloader, rename and upstream-error tests
scripts/            fixture export, local run and Java/Rust comparison
```
