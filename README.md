# JProxy-RS

Repository and package name: `jproxy-rs`.

A Rust implementation of the Phase 1 jproxy proxy between Sonarr/Radarr and
Prowlarr/Jackett. It forwards Torznab requests, rewrites search queries using
the existing `sonarr_title`, `radarr_title`, `sonarr_rule`, and `radarr_rule`
tables, applies `MIN_COUNT` alternate-title retries, formats returned item
titles with the configured rules, and caches upstream XML responses. It also
serves the compiled Java admin UI and implements the admin REST API and
Java-compatible login/JWT session handling.

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
- Baseline `system_config` keys (`sonarrUrl`, `sonarrApikey`,
  `sonarrIndexerFormat`, `sonarrLanguage1`, `sonarrLanguage2`, `radarrUrl`,
  `radarrApikey`, `radarrIndexerFormat`, `jackettUrl`, `prowlarrUrl`, `tmdbUrl`,
  `tmdbApikey`, `cleanTitleRegex`, `ruleSyncAuthors`) with environment
  override, plus the downloader keys described below. Downloader credentials
  are read only to log in and are never logged.
- Live config validation on `/api/system/config/update` (Sonarr/Radarr health,
  TMDB, qBittorrent/Transmission login, Jackett/Prowlarr reachability, format
  tokens, and regex compilation) with Java-compatible `validStatus` codes.
- Shared refreshable configuration: updates and `system_config` cache clears
  apply immediately without a restart. Invalid rows fail strict
  `queryValueByKey` reads while proxy traffic degrades instead of panicking.
- Exact Java cron schedules in process-local time, plus the GitHub release
  version check.
- qBittorrent and Transmission login with scheduled session refresh. Both
  failures and successes are logged without credentials or session values.
- Sonarr/Radarr history rename tasks, including `RENAME_FILE` file renaming for
  qBittorrent and the Java-compatible Transmission `torrent-rename-path` flow.
- `GET /health` JSON endpoint.
- Admin REST API for Sonarr/Radarr rules, titles and examples, TMDB titles,
  rule testing, system configuration, system users and cache control.
- Java-compatible login with an HS256 JWT, `LOGIN_ENABLED`, `TOKEN_EXPIRES`,
  a global wrong-login limit, logout blacklisting, and `403` on protected admin
  routes. The Torznab routes and `/health` remain public.
- The compiled Vue admin UI from the Java project, served from `admin_dist/`
  with SPA fallback and MIT attribution.
- First-run `system_config` rows matching the upstream Liquibase seed and a
  first-run `system_user` row. A fresh database gets `jproxy` / `jproxy@2023`
  when neither `ADMIN_USERNAME` nor `ADMIN_PASSWORD` overrides it. An existing
  `system_user` table is never modified, so a migrated Java account such as
  `inscripoem` is preserved exactly.
- Operator CLI (`admin`, `config`, `sync`, `doctor`, `version`) that opens the
  configured SQLite database directly and works through `docker exec` or SSH.
- Multi-stage Dockerfile.
- Reference-data import from an existing `jproxy.db` (`jproxy-rs import`).
- End-to-end parity harness against recorded Torznab XML fixtures.
- Bounded upstream concurrency with exponential backoff, retry on 408/429/5xx,
  and meaningful downstream error mapping (`429`, `502`, `504`).

Not implemented (deferred Java behavior):

- Java's distributed/persistent caching and exact Caffeine eviction behaviour.

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
- `tests/admin.rs` — admin CRUD, login/JWT rejection, pagination, cache/config
  endpoints and static UI serving with synthetic users only;
- `tests/cli.rs` — `admin`, `config`, `sync`, `doctor`, and `version` against
  temporary databases and a local mock upstream;
- `tests/real_data.rs` — validation against data exported from a live jproxy.

The release profile sets `opt-level = "z"`, thin LTO, one codegen unit and
`strip = true`; the resulting binary is about 9.0 MB. Panic unwinding is left
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

The service creates missing tables and the upstream first-run rows on startup.
An empty `system_user` table gets `jproxy` / `jproxy@2023` unless
`ADMIN_USERNAME`/`ADMIN_PASSWORD` override the defaults. An imported database
keeps every existing user row, including a migrated account whose username is
not `jproxy`; the seed never resets or overwrites it. `LOGIN_ENABLED` defaults
to `true`, so the admin UI and API require login while the Torznab routes and
`/health` stay public. When the default credential is still stored and
`LISTEN_ADDR` is not a loopback address, startup logs one warning that the
default admin credential should be changed before public exposure; the warning
contains no username, password, or hash.

At runtime the service reads only the whitelisted keys described under
Configuration and verifies admin logins against `system_user`; downloader
credentials are held in memory and never logged.

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

## Agent and operator CLI

The binary serves the same Axum router on two listeners:

- the TCP Web UI listener (`LISTEN_ADDR`) with the existing login/JWT rules;
- a Unix-domain control socket for the CLI, authenticated by a per-process
  control token and Linux peer credentials.

There is no gRPC, D-Bus, or file-watch control plane. Online CLI commands are
thin HTTP clients over the socket and call the same admin handlers as the Web
UI; they never edit the database behind a running server.

```sh
# Defaults: /tmp/jproxy/control.sock, /tmp/jproxy/runtime.json, /tmp/jproxy/token
jproxy-rs status
jproxy-rs healthcheck
jproxy-rs version

# system_config. Credentials are read from stdin or a file, never argv.
jproxy-rs config list
jproxy-rs config get sonarrUrl
jproxy-rs config get sonarrApikey --reveal
printf '%s\n' "$VALUE" | jproxy-rs config set sonarrApikey --stdin
jproxy-rs config set cleanTitleRegex --file ./clean-title-regex.txt

# Rules, titles, examples, and TMDB titles (all admin-UI equivalents).
jproxy-rs rules sonarr list
jproxy-rs rules sonarr get <id>
jproxy-rs rules sonarr add --file ./rule.json
jproxy-rs rules sonarr update <id> --file ./rule.json
jproxy-rs rules sonarr remove <id> --yes
jproxy-rs rules sonarr enable <id>
jproxy-rs rules sonarr disable <id>
jproxy-rs rules sonarr export --file ./sonarr-rules.json
jproxy-rs rules sonarr import --file ./sonarr-rules.json
jproxy-rs titles sonarr list
jproxy-rs titles sonarr remove <id> --yes
jproxy-rs titles sonarr sync
jproxy-rs examples sonarr list
printf '%s\n' "Synthetic Show S01E01" | jproxy-rs examples sonarr add --stdin
jproxy-rs examples sonarr remove <hash> --yes
jproxy-rs tmdb list
jproxy-rs tmdb add --file ./tmdb-title.json
jproxy-rs tmdb remove <id> --yes
jproxy-rs tmdb sync

# Cache and long-running jobs. Jobs return an id, stream progress over SSE, and
# can be cancelled.
jproxy-rs cache list
jproxy-rs cache clear indexer_result --yes
jproxy-rs sync --kind sonarr|radarr|tmdb|rules|all [--no-wait]
jproxy-rs rename --kind sonarr|radarr [--dry-run] [--no-wait]
jproxy-rs login --client qbittorrent|transmission [--no-wait]
jproxy-rs jobs list
jproxy-rs jobs show <id>
jproxy-rs jobs watch <id>
jproxy-rs jobs cancel <id>

# Diagnostics.
jproxy-rs doctor

# Offline recovery. `admin` always edits DATABASE_PATH directly and refuses to
# run while a live control socket is present unless --offline is explicit.
printf '%s\n' "$ADMIN_PASSWORD" | jproxy-rs admin set-password --stdin
printf '%s\n' "$ADMIN_PASSWORD" | jproxy-rs admin set-password --username inscripoem --stdin
printf '%s\n' "$ADMIN_PASSWORD" | jproxy-rs admin reset-password --stdin
```

Global options apply to every command: `--socket <path>`, `--token-file
<path>`, `--runtime-file <path>`, `--timeout <seconds>`, `--json`,
`--no-color`, and `--offline`. Without `--socket`, the CLI reads
`runtime.json` (default `/tmp/jproxy/runtime.json`, override with
`CONTROL_RUNTIME_FILE`) and the token file it names. Results go to stdout;
progress and logs go to stderr. Exit codes are `0` success, `2` usage, `3`
connection failure, `4` authentication failure, `5` server error, and `6`
CLI/server version mismatch.

`config`, `sync`, and `doctor` fall back to database-direct mode only when no
control socket exists; the fallback prints an stderr notice. A reachable socket
with a missing token, bad permission mode, or invalid token is an error, never
a silent switch to the database. `admin` is the only command that always runs
offline.

### Control socket security

- `CONTROL_SOCKET` defaults to `/tmp/jproxy/control.sock`, which works for the
  non-root container user. The parent directory is created with mode `0700`
  when absent; the socket, token file, `runtime.json`, and `control.lock` are
  mode `0600`. Set `CONTROL_SOCKET=/control/control.sock` in the container
  image, where `/control` is owned by UID 65532.
- A 32-byte random token is base64url-encoded into `<socket-dir>/token`.
  `runtime.json` records `socket`, `pid`, `version`, `token_file`, and
  `started_at`, never the token itself. Neither value is logged.
- On Linux the server verifies `SO_PEERCRED`: only the service UID and root are
  accepted. Other users are rejected before the token is checked. On other
  Unix systems `getpeereid` is used; if peer credentials are unavailable the
  request is rejected.
- `docker exec` defaults to the image user for entrypoint processes, so a plain
  `docker exec <container> jproxy-rs status` works. `docker exec -u <uid>` must
  use the service UID (65532 in the image) or `0`; any other UID fails the
  peer-credential check even with a valid token.
- Startup takes an exclusive `flock` on `<socket-dir>/control.lock`. An
  existing socket is unlinked only after a failed connect probe; a live
  listener is never removed. Graceful shutdown removes the socket, token, and
  runtime metadata. A crash leaves stale files, and the next start replaces
  them.
- Unix socket paths are limited to roughly 108 bytes on Linux. Longer
  `CONTROL_SOCKET` values are rejected before binding.
- The control token is accepted only on the Unix listener. On TCP it is not an
  auth bypass: `/api/control/**` requires the UDS principal, and admin routes
  still require a JWT.

In a container:

```sh
docker exec <container> jproxy-rs status
docker exec <container> jproxy-rs sync --kind sonarr
docker exec -u 65532 <container> jproxy-rs jobs list
```

`admin set-password` with no `--username` uses `ADMIN_USERNAME`, then the sole
existing account, then `jproxy`. This means a migrated database containing only
`inscripoem` is updated in place rather than gaining a second administrator.
`admin reset-password` updates the sole existing account. `config set` accepts
only the whitelisted `system_config` keys listed under Configuration and marks
the row valid. `config get`/`list` mask `sonarrApikey`, `radarrApikey`,
`tmdbApikey`, `qbittorrentPassword`, and `transmissionPassword` unless
`--reveal` is passed. Destructive commands (`rules remove`, `titles remove`,
`examples remove`, `tmdb remove`, `cache clear`) require `--yes`.

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
`tmdb_title` 20); `tests/real_data.rs` asserts them. Library titles are
replaced with synthetic placeholders during export; the only real titles
retained are the public works asserted by that test (`Jackie Chan Adventures`
and `The Fifth Element`, plus their TMDB alternates).

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
`sonarrUrl`, `sonarrApikey`, `sonarrIndexerFormat`, `sonarrLanguage1`,
`sonarrLanguage2`, `radarrUrl`, `radarrApikey`, `radarrIndexerFormat`,
`jackettUrl`, `prowlarrUrl`, `tmdbUrl`, `tmdbApikey`, `cleanTitleRegex`,
`ruleSyncAuthors`, `ruleLocation`, `ruleLocationBackup`,
`qbittorrentUrl`, `qbittorrentUsername`, `qbittorrentPassword`,
`transmissionUrl`, `transmissionUsername`, `transmissionPassword`. Downloader
credentials are held in memory, used only for login, and are never logged or
included in the startup summary. The admin login reads `system_user` only to
verify a submitted password; it never logs or returns stored hashes.

Configuration is resolved into a shared snapshot at startup and re-resolved
after every successful `/api/system/config/update` or
`/api/system/cache/clear` with `cacheName=system_config`. New URLs, API keys,
languages, rule locations, indexer formats, clean-title regex, and downloader
credentials therefore apply without a restart. Environment values remain
overrides. A read of a row whose `validStatus` is `INVALID` fails with a
Java-compatible `SystemConfigException` equivalent; background tasks skip or
log that failure instead of using the value. A `system_config` row is never
used for a key that the environment supplied, even when the values are equal.

All other settings remain environment-only.

| Variable | Default | Purpose |
| --- | --- | --- |
| `LISTEN_ADDR` | `0.0.0.0:8117` | HTTP listen socket. |
| `CONTROL_SOCKET` | `/tmp/jproxy/control.sock` | Unix-domain socket for the CLI control channel. The parent directory is created `0700`; the socket and metadata are `0600`. |
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
| `SONARR_RENAME_FALLBACK_MINS` | `30` | Sonarr history lookback window in minutes. |
| `RADARR_RENAME_FALLBACK_MINS` | `30` | Radarr history lookback window in minutes. |
| `RENAME_FILE` | `true` | Also rename files inside a qBittorrent torrent after the torrent rename succeeds. Transmission has no file-level rename. |
| `LOGIN_ENABLED` | `true` | Require credentials for `/api/**` admin routes, matching Java's production default. When false, `/api/system/user/login` returns an Anonymous JWT; the UI still needs that token for admin calls. |
| `TOKEN_EXPIRES` | `10080` | Admin JWT lifetime in minutes (seven days). |
| `ADMIN_USERNAME` | `jproxy` | Username for the first-run admin row only. Ignored when `system_user` is non-empty. |
| `ADMIN_PASSWORD` | `jproxy@2023` | Plaintext password for the first-run admin row only, stored as uppercase MD5 of its MD5. Ignored when `system_user` is non-empty. Never logged. |
| `ADMIN_UI_DIR` | `admin_dist` | Directory containing the compiled admin UI (`index.html` and `assets/`). Missing files return 404; it is never fetched at runtime. |
| `VERSION_CHECK_URL` | `https://api.github.com/repos/inscripoem/jproxy-rs/releases/latest` | Release endpoint used by `/api/system/config/version`; override points tests at a local mock. |
| `REQUEST_TIMEOUT_SECS` | `30` | Upstream and sync HTTP timeout. |
| `PROBE_CONNECT_TIMEOUT_SECS` | `6` | Connect timeout for live configuration probes (`/api/system/config/update`, `doctor`). |
| `PROBE_READ_TIMEOUT_SECS` | `12` | Whole-request timeout for live configuration probes. |
| `DOWNLOADER_REQUEST_TIMEOUT_SECS` | `30` | Timeout for qBittorrent and Transmission login, rename, and file calls. |
| `RESULT_CACHE_MAX_ENTRIES` | `1000` | Maximum cached Torznab responses. |
| `UPSTREAM_MAX_CONCURRENCY` | `8` | Maximum simultaneous upstream indexer requests. |
| `UPSTREAM_MAX_RETRIES` | `2` | Retries for 408/429/5xx and transport failures. |
| `UPSTREAM_RETRY_BASE_MS` | `250` | Base delay for exponential backoff. |
| `UPSTREAM_RETRY_MAX_MS` | `4000` | Maximum delay per retry; caps `Retry-After`. |

`SYNC_INTERVAL=0` or `SYNC_ENABLED=false` disables both background and
on-demand synchronization. A missing Sonarr/Radarr URL or API key skips that
kind. A missing Prowlarr/Jackett URL makes the related routes return HTTP 503.

## Admin UI, API and authentication

The Vue admin UI in `admin_dist/` is rebuilt from the MIT-licensed upstream
`LuckyPuppy514/jproxy-admin` source with a small branding patch; see
`admin-ui/README.md` and `scripts/build-admin-ui.sh`. It ships together with
`admin_dist/LICENSE.txt` and `admin_dist/NOTICE` and is served from disk at
`/` with SPA fallback for client-side routes. `ADMIN_UI_DIR` selects the
directory; the UI is never downloaded at runtime. Unknown `/api/**` paths,
`/health`, and the Torznab proxy prefixes never fall through to the SPA.

All admin endpoints use the Java paths, methods, camelCase JSON fields, and
`current`/`pageSize` pagination:

| Base path | Operations |
| --- | --- |
| `/api/sonarr/rule`, `/api/radarr/rule` | `POST sync`, `GET query`, `POST save`, `POST remove`, `POST enable`, `POST disable`, `POST export`, `POST import`, `GET token/list` |
| `/api/sonarr/title`, `/api/radarr/title` | `POST sync`, `GET query`, `POST remove` |
| `/api/sonarr/example`, `/api/radarr/example` | `POST save`, `GET query`, `POST remove` |
| `/api/rule/test` | `GET` with `regex`, `replacement`, `offset`, `example` |
| `/api/tmdb/title` | `POST sync`, `GET query`, `POST remove`, `POST save` |
| `/api/system/config` | `GET version`, `GET query`, `POST update`, `GET author/list` |
| `/api/system/user` | `POST login`, `POST logout`, `GET info`, `POST update`, `GET isLoginEnabled` |
| `/api/system/cache` | `POST clear`, `POST clearAll` |
| `/api/control` | UDS-only: `GET status`, `POST reload`, `POST sync`, `POST rename`, `POST login`, `GET/POST jobs`, `GET jobs/{id}`, `GET jobs/{id}/events` (SSE), `POST jobs/{id}/cancel` |

Authentication follows the Java controller and interceptor:

- Every `/api/**` route requires `Authorization: Bearer <jwt>` except
  `/api/system/user/login` and `/api/system/user/isLoginEnabled`; missing or
  invalid tokens return `403`. The Torznab proxy routes and `/health` stay
  public.
- `LOGIN_ENABLED` defaults to `true`, matching Java's production configuration.
  With `LOGIN_ENABLED=false`, `login` returns an Anonymous JWT; user `update`
  then returns `401`. The UI still needs the token for admin calls.
- First-run admin policy:
  - A database with no `system_user` rows gets `jproxy` / `jproxy@2023`,
    enabled, matching Java. `ADMIN_USERNAME` and `ADMIN_PASSWORD` override
    those defaults and are ignored when any user already exists.
  - The stored value is the uppercase MD5 of the uppercase MD5 of the typed
    password, the format Java uses when the compiled UI hashes the value before
    POSTing it. Direct API clients must POST the uppercase MD5 of the typed
    password, not the plaintext.
  - Existing `system_user` rows are preserved exactly. A migrated Java database
    whose account is `inscripoem` is never renamed, reset, or duplicated.
  - There is no setup wizard, setup token, secret file, or forced first-login
    change.
  - If the default credential is still stored and the service is bound to a
    non-loopback address, startup logs one warning that it should be changed
    before public exposure. The warning never contains the password or hash.
- JWTs are HS256 with a random per-process secret. Claims are `id`, `username`,
  `password`, `role`, `valid_status`, `exp`, and `iat`; `TOKEN_EXPIRES`
  (minutes) controls expiry. Tokens become invalid after a process restart.
  Logout blacklists the raw `Authorization` header. More than ten wrong logins
  block login until restart, as in Java.
- `/api/control/**` is reachable only through the Unix listener with the
  control token (`X-Control-Token` or `Authorization: Bearer <token>`). A
  valid Web UI JWT on TCP receives `403` for these paths, and the control token
  on TCP never authenticates an admin route.
- `/api/system/config/update` reproduces Java's live validation:
  - Sonarr/Radarr: `GET {url}/api/v3/health?apikey=...` with
    `PROBE_CONNECT_TIMEOUT_SECS` (default 6) and `PROBE_READ_TIMEOUT_SECS`
    (default 12). A 401 or an `Unauthorized` body marks only the URL valid; a
    connection failure marks both invalid.
  - Indexer formats: Sonarr requires `{title}`, `{season}`, `{episode}`;
    Radarr requires `{title}`, `{year}`.
  - qBittorrent and Transmission: check the URL, normalize the Transmission
    JSON-RPC path, then log in with the submitted credentials. URL, username,
    and password each receive their own `validStatus`.
  - Jackett and Prowlarr: any HTTP response counts as reachable; a connection
    failure does not.
  - TMDB: `GET {url}/3/movie/550?api_key=...`; an invalid key or 401 marks only
    the URL valid.
  - `cleanTitleRegex` is compiled before it is stored.
  The rows are persisted with Java's upsert semantics, then the shared
  configuration snapshot and dependent caches are refreshed immediately.
  URLs, API keys, usernames, passwords, and tokens are never logged.
- `/api/system/config/version` queries
  `https://api.github.com/repos/inscripoem/jproxy-rs/releases/latest` (override
  with `VERSION_CHECK_URL`) with a bounded timeout, strips one leading `v` from
  `tag_name`, compares it with this crate's `CARGO_PKG_VERSION` (`0.1.0`), and
  appends ` 🚨` on a mismatch. A missing release, non-success status, timeout,
  or parse failure returns the local version unchanged.
- `/api/system/config/query` runs one Sonarr rename pass before returning the
  rows, matching the Java controller.
- Pagination is strict and non-destructive:
  - `current`/`pageSize` below 1, or non-numeric values, return HTTP 400.
  - A valid page beyond the end returns HTTP 200 with the requested
    `current`/`pageSize`, the real `total`, and an empty `list`, for both
    filtered and unfiltered queries. It never resets to page one and never
    reproduces Java's `subList` exception.
  - `pageSize` above 1000 is capped, matching the MyBatis limit.

## Background schedule

Schedules use the exact Java cron expressions in process-local time. A cron
entry does not fire at process start, so the first run is the next matching
wall-clock boundary.

| Task | Java cron | Fire times |
| --- | --- | --- |
| Sonarr title sync + TMDB sync | `0 0 0/1 * * ?` | Hourly at `:00`. |
| Radarr title sync | `0 30 0/1 * * ?` | Hourly at `:30`. |
| Sonarr rule sync | `0 15 0 * * ?` | Daily at `00:15`. |
| Radarr rule sync | `0 45 1 * * ?` | Daily at `01:45`. |
| Sonarr rename | `0/30 * * * * ?` | Seconds `0` and `30` of every minute. |
| Radarr rename | `0 0/1 * * * ?` | Second `0` of every minute. |
| Downloader login | `0 */30 * * * ?` | Startup, then `:00` and `:30` of every hour. |

The downloader login is the only task with a startup run, matching Java's
`CommandLineRunner`. The `SYNC_INTERVAL` gate still limits Sonarr/Radarr title
synchronization (background and on-demand) to one attempt per interval; it does
not gate rule synchronization. `SYNC_INTERVAL` defaults to 3, matching Java's
Caffeine gate. Clock jumps skip to the next matching boundary rather than
replaying missed runs.

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

Strict-parity profile, measured with the release binary, a temporary SQLite
database, one local Python mock for probes and Torznab responses,
`SYNC_ENABLED=false`, `MIN_COUNT=0`, and synthetic credentials. RSS is `VmRSS`,
five samples per phase and the median of three independent runs.

| Phase | Workload | RSS |
| --- | --- | --- |
| Idle | No requests | 10.5 MB (10,672–10,756 KB) |
| After config probes with mocks | Sonarr/Radarr/TMDB health, qBittorrent/Transmission login, Jackett/Prowlarr checks | 11.6 MB (11,428–11,832 KB) |
| Under proxy load | 2,000 Torznab queries, concurrency 16, 2,000 distinct `q` values | 16.6 MB (16,652–17,224 KB) |

The application logs contained none of the synthetic keys, passwords, cookies,
or session ids. No real credential, live service, or NAS path was used.

### Admin UI/API profile

Method: release binary, temporary SQLite database, `admin_dist/` served from
disk, local mock Torznab upstream, `SYNC_ENABLED=false`, `MIN_COUNT=0`,
`LOGIN_ENABLED=true`, and a synthetic admin user. The database was seeded with
200 Sonarr titles, 200 Radarr titles, 200 rules, 100 examples, and 100 TMDB
rows. Five `VmRSS` samples were taken 0.3 seconds apart per phase. The admin
phase covered login, user info, config query, rule/title/example/TMDB queries,
a rule save, the UI root and a JS asset, and cache clear-all. The load phase
used 2,000 Torznab queries with concurrency 16 and 500 distinct `q` values.
No live service or real credential was used.

| Phase | Workload | RSS |
| --- | --- | --- |
| Idle, admin UI enabled | Seeded database, no admin or proxy requests | 10,468 KB (10.2 MB) |
| After representative admin calls | Login, CRUD queries, formatting preview, static UI, cache clear | 13,184 KB (12.9 MB) |
| Under proxy load | 2,000 queries, concurrency 16, 500 distinct `q` values | 20,532 KB (20.1 MB) |

The samples were stable within each phase (`VmRSS` did not move during the
sampling window). Values are current RSS, not peak `VmHWM`, with SQLite page
cache and allocator retention included.

## SQLite compatibility

The bundled `sqlx` SQLite build uses its own SQLite library. The schema in
`migrations/0001_schema.sql` mirrors the final upstream Liquibase migrations,
including the later `series_id` and `movie_id` columns.
`migrations/0002_admin_seed.sql` adds the upstream first-run `system_config`
rows with `INSERT OR IGNORE`. The first-run admin user is inserted separately,
only when `system_user` is empty, so an existing account (for example
`inscripoem` in a migrated Java database) is never overwritten and the default
`jproxy` account is not added alongside it.
There is no Liquibase metadata table in this project, so an existing `jproxy.db`
can be opened directly, but run it against a backup first. The service reads
only the whitelisted `system_config` keys and verifies admin logins against
`system_user`; it never writes downloader credentials back to the database.

## Docker

The Dockerfile builds the release binary in a Rust stage and copies the binary
and `admin_dist/` into a distroless runtime image. The image sets
`ADMIN_UI_DIR=/app/admin_dist`.

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

### Published image

Images are published to GitHub Container Registry by
`.github/workflows/container.yml` when a `v*` tag is pushed, or when the
workflow is run manually. A stable `vX.Y.Z` tag publishes `X.Y.Z`, `X.Y`, and
`latest`; the commit is also tagged `sha-<short>`.

```sh
docker pull ghcr.io/inscripoem/jproxy-rs:latest
```

For a Compose deployment, pin the tag and pull explicitly when updating:

```sh
cd /path/to/stack
docker compose pull jproxy
docker compose up -d jproxy
```

The `latest` tag is mutable. Compose does not pull it in the background; with
`pull_policy: always` it is refreshed on the next `docker compose up`.

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
- **Sync schedule.** Titles and rules use Java's exact cron expressions in
  process-local time: `:00` Sonarr titles, `:30` Radarr titles, `00:15` Sonarr
  rules, and `01:45` Radarr rules. The three-minute `SYNC_INTERVAL` gate and
  the Java-compatible upserts are preserved.
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
- **Rename schedule.** Sonarr (`0/30 * * * * ?`) and Radarr
  (`0 0/1 * * * ?`) use the exact Java cron expressions in process-local time;
  neither fires at process start. The history fallback windows are 30 minutes
  for both, as in `application-prod.yml`; `SONARR_RENAME_FALLBACK_MINS` and
  `RADARR_RENAME_FALLBACK_MINS` are environment extensions because Java stores
  these timings in `application-prod.yml`.
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
- **Admin password contract.** The Java UI hashes the typed password with MD5
  before login, and the service hashes the submitted value again. This
  implementation preserves that contract, so non-UI API clients must send the
  uppercase MD5 of the password. Passwords, password hashes, tokens, and
  `system_config` values are never logged.
- **Admin JWT secret lifetime.** Java generates a random secret per process, so
  tokens die on restart; this implementation does the same. The logout
  blacklist is process-local and is cleared by `cache/clearAll`, matching
  Java's cache clear-all.
- **Config validation uses live probes.** `/api/system/config/update` checks
  Sonarr/Radarr health, TMDB, qBittorrent and Transmission login, and
  Jackett/Prowlarr reachability with the same status codes and timeouts as
  Java's `CheckUtil`, then compiles the clean-title regex and checks format
  tokens locally. Probe traffic uses only the submitted URLs; failure messages
  never include the URL, key, username, password, cookie, or session id.
- **Config hot reload.** Successful updates and `cache/clear` for
  `system_config` replace a shared snapshot, so proxy formatting and sync tasks
  use new values without restart. Java achieves the same through its
  `SYSTEM_CONFIG` cache. Rows marked `INVALID` fail strict reads; callers skip
  or degrade rather than panicking.
- **Admin version check.** `/api/system/config/version` calls this project's
  GitHub `releases/latest` endpoint
  (`inscripoem/jproxy-rs`), with a bounded timeout. It compares the tag with
  `CARGO_PKG_VERSION` (`0.1.0`). A repository with no release, a failed request,
  or an unparseable response returns the local version without the marker.
  `VERSION_CHECK_URL` exists so tests use a local mock.
- **Admin credential policy.** A fresh database receives the documented Java
  default `jproxy` / `jproxy@2023`, or the `ADMIN_USERNAME`/`ADMIN_PASSWORD`
  pair when provided. Existing users are never modified, and there is no setup
  wizard, setup token, or forced first-login change. Startup warns once when
  the default hash is still stored and the listener is non-loopback.
- **Pagination edge.** A valid page beyond the end returns HTTP 200 with the
  requested coordinates and an empty list. Malformed `current`/`pageSize`
  values return HTTP 400. Java's MyBatis path restarts some pages at one and
  its filtered example path can throw `IndexOutOfBoundsException` from
  `subList`; both behaviours are deliberately not reproduced.
- **Bounded timeouts.** Probe timeouts are `PROBE_CONNECT_TIMEOUT_SECS` (6) and
  `PROBE_READ_TIMEOUT_SECS` (12); downloader calls use
  `DOWNLOADER_REQUEST_TIMEOUT_SECS` (30). Java's RestTemplate can wait far
  longer. Retry/backoff for indexer requests remains bounded.
- **Admin UI delivery.** The UI is rebuilt from the pinned upstream Vue
  source (MIT, see `admin_dist/LICENSE.txt`, `admin_dist/NOTICE` and
  `admin-ui/README.md`) by `scripts/build-admin-ui.sh`, then vendored and
  served from disk. It is not downloaded at runtime. Unknown `/api/**` paths
  require authentication and return `403` without a token, then `404`,
  matching the Java interceptor.
- **Charon downloader routes.** Java v3.1.0 (commit `d3480bd`, 2023-04-11)
  removed the Charon reverse proxy and replaced it with direct qBittorrent
  rename. The current Java build has no Charon dependency or route
  registration; only unused `CHARON_*` and downloader filter path constants
  remain in `Common.java`.
  Login and rename here call the downloader APIs directly. The legacy
  `/sonarr|radarr/qbittorrent|transmission` paths are absent in both
  implementations, so this is not a parity gap.

## Project layout

```text
src/app.rs          Axum routes, proxy flow, health, static UI fallback
src/admin.rs        Java-compatible admin REST handlers and validation
src/admin_db.rs     admin SQLite queries and page types
src/auth.rs         MD5 password contract, HS256 JWT, blacklist, middleware
src/cli.rs          admin/config/sync/doctor/version subcommands
src/search.rs       alternate-title plan and MIN_COUNT retry executor
src/rules.rs        priority-ordered compiled rules
src/formatting.rs   Java-compatible item title formatting
src/torznab.rs      item counting, page merge/trim, XML title rewrite
src/cache.rs        bounded TTL cache
src/db.rs           sqlx SQLite queries and schema setup
src/downloader.rs   qBittorrent/Transmission login, sessions and API calls
src/import.rs       reference-table import path (`jproxy-rs import`)
src/probe.rs        Java CheckUtil live config probes
src/schedule.rs     exact Java cron next-run calculation and wall-clock loops
src/rename.rs       Sonarr/Radarr history rename tasks and schedules
src/sync.rs         Sonarr/Radarr title sync, TMDB sync, rule sync, on-demand
src/upstream.rs     reqwest/rustls upstream client, retry and concurrency
src/query.rs        lossless-enough query parameter editing and cache keys
src/text.rs         Java FormatUtil ports
migrations/         Java-compatible SQLite schema and first-run config seed
admin_dist/         compiled admin UI rebuilt from the upstream Vue source
fixtures/           exported reference DB and synthetic Torznab XML
tests/              parity, sync, downloader, rename, admin, CLI and error tests
scripts/            fixture export, local run and Java/Rust comparison
```

## License

MIT. The admin UI under `admin_dist/` is derived from the upstream
`LuckyPuppy514/jproxy-admin` project and carries its MIT license in
`admin_dist/LICENSE.txt`.
