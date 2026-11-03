# AGENTS.md

Instructions for AI coding agents working in this repository. Read `README.md`
for behavior and configuration details; this file defines workflow and
guardrails. When an explicit instruction from the operator conflicts with this
file, follow the operator.

## Project and goal

- `jproxy-rs` is a Rust rewrite of the Java jproxy (`LuckyPuppy514/jproxy`), a
  proxy between Sonarr/Radarr and Prowlarr/Jackett that rewrites Torznab
  queries and item titles.
- `main` contains the complete 0.1.0 implementation: core proxy, query, title
  and rule rewriting, caching, upstream error handling, Sonarr/Radarr/TMDB
  synchronization, downloader login, rename tasks, the admin UI/API and
  authentication, and the operator CLI over the control socket.
- The 0.1.0 milestone is 1:1 behavior parity with Java before any
  simplification. Do not remove, collapse, or "clean up" behavior, response
  shapes, or edge cases just because the Rust version is simpler. Match Java
  unless a deviation is already documented and pinned by a test.

## Worktree discipline

- Work in the repository root. Create a separate worktree only for a large
  isolated change, and only with an explicit request.
- Do not merge, rebase, cherry-pick, push, or publish without an explicit
  request from the operator.
- The Java reference source is external, not vendored. Use the checkout at
  `/tmp/jproxy-src.KSddZV` when present, otherwise the upstream GitHub
  repository. Do not copy Java source into this tree.
- `admin_dist/` contains the compiled upstream Vue admin UI copied from the
  Java distribution under the MIT license (`admin_dist/LICENSE.txt`). Treat it
  as read-only vendored output: do not edit, reformat, or regenerate it unless
  explicitly asked.

## Toolchain

All Rust commands must run inside the pinned Nix devShell. Do not rely on a
global `cargo`, `rustc`, `clippy`, or `rustfmt`.

```sh
nix develop --command cargo fmt --check
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo test
nix develop --command cargo build --release
```

- `.envrc` uses `use flake`; `direnv allow` once per machine is optional.
- `flake.lock` pins the Nix inputs. The shell provides `cargo`, `rustc`,
  `clippy`, `rustfmt`, `pkg-config`, and the `sqlite` CLI.
- Run commands from the repository root. Use `nix develop --command` for any
  tool that must come from the shell, including `sqlite3`.
- The release binary is `target/release/jproxy-rs`.

## Verification gate

A task is not done until all four commands above pass from a clean state, with
no `error:` output and no warnings. `cargo clippy` must include
`--all-targets -- -D warnings`; do not drop the flag or suppress lints to make
the gate pass.

- Run focused tests while iterating, then the full suite before declaring done.
- Do not delete tests, mark them ignored, or weaken assertions to pass.
- Behavior changes require a test that pins the new behavior and an update to
  `PARITY.md` or `PARITY_REPORT.md` when the parity record changes.

## Testing and mocks

- Unit tests live in `#[cfg(test)]` modules under `src/`.
- Integration suites:
  - `tests/parity.rs` — query rewriting, rule ordering, `MIN_COUNT`, item-title
    formatting against recorded Torznab XML.
  - `tests/sync.rs` — title, TMDB, rule, and on-demand synchronization.
  - `tests/admin.rs` — admin CRUD, login/JWT rejection, cache/config
    endpoints, pagination, static UI serving.
  - `tests/cli.rs` — offline `admin`/`config`/`sync`/`doctor`/`version`
    subcommands against temporary databases and local mocks.
  - `tests/control.rs` — UDS token/peer authentication, TCP isolation, stale
    socket handling, job/SSE/rename-dry-run/login flows, admin CRUD through the
    control token, and online CLI exit codes and command surface.
  - `tests/downloaders.rs` — qBittorrent/Transmission sessions and errors.
  - `tests/rename.rs` — Sonarr/Radarr history rename flows.
  - `tests/upstream_errors.rs` — retry, concurrency, and downstream status
    mapping.
  - `tests/real_data.rs` — assertions against the exported reference database.
- `tests/common/mod.rs` provides the in-process mock Torznab indexer, mock HTTP
  servers for Sonarr/Radarr/TMDB/downloaders, a temporary SQLite database, and
  the real router. Reuse it instead of adding network-dependent tests.
- Fixtures live under `fixtures/`: `fixtures/jproxy-live.db` is exported
  reference data with no `system_config` or `system_user` rows; library titles
  are replaced with synthetic placeholders except the public works pinned by
  `tests/real_data.rs`. `fixtures/torznab/*.xml` are synthetic Torznab
  responses.
- Never call a real Sonarr, Radarr, TMDB, GitHub, downloader, or
  `tower.local` service from a test. Use loopback mock servers. Use synthetic
  users, API keys, passwords, cookies, and session ids only.
- `tests/real_data.rs` asserts the live row counts recorded at export time.
  Change those counts only with fresh `SELECT count(*)` evidence and a note in
  `PARITY_REPORT.md`.

Regenerate the reference fixture with the read-only export script:

```sh
nix develop --command scripts/export-fixtures.sh [ssh-target] [remote-db] [output]
```

The script defaults are documented in its header. It opens the remote database
with `sqlite3 -readonly`, exports only the five reference tables, writes the
fixture locally, and drops `system_user`/`system_config` from the output. It
does not copy credentials or configuration values. Clean up temporary copies
when finished.

Run the local proxy against a Prowlarr/Jackett instance on the LAN:

```sh
export PROWLARR_URL=http://<prowlarr-host>:9696   # or JACKETT_URL
scripts/run-local.sh
```

`run-local.sh` builds through `nix develop` if needed and defaults to
`DATABASE_PATH=./local-dev.db`, `LISTEN_ADDR=127.0.0.1:9117`,
`SYNC_ENABLED=false`, and `RUST_LOG=jproxy_rs=info,tower_http=info`. It needs
`PROWLARR_URL` or `JACKETT_URL`; `PROWLARR_API_KEY` is optional and is used
only to print a smoke-test command, never written to disk or echoed.

Compare Java and Rust responses with:

```sh
export PROWLARR_API_KEY=...   # never echo, log, or commit this value
scripts/compare.sh
```

`compare.sh` requires `PROWLARR_API_KEY`; optional overrides are `JAVA_URL`
(default `http://127.0.0.1:8117`), `RUST_URL` (default
`http://127.0.0.1:9117`), `INDEXER_ID` (default `1`), and `QUERIES_FILE`.
Both proxies must point at the same upstream and the same database. The script
exits non-zero when any query differs. Without the key it prints instructions
and makes no live request.

Do not commit `.env`, `*.db`, database sidecars, or `local-dev.db`; they are
gitignored. Never force-add them.

## Architecture map

- `src/main.rs` — binary entry point, startup and shutdown of both the TCP and
  Unix-domain listeners, `import`, and subcommand dispatch.
- `src/cli.rs` — online CLI commands over the Unix control socket plus the
  explicit offline `admin`/`config` paths.
- `src/control.rs` — Unix socket lifecycle, token/runtime files, peer-credential
  checks, control middleware/endpoints, and the bounded job manager with SSE.
- `src/app.rs` — Axum router, shared `AppState`, proxy flow, health, static UI
  fallback, and upstream request orchestration.
- `src/admin.rs` — Java-compatible admin REST handlers, config validation, and
  cache controls.
- `src/admin_db.rs` — admin SQLite queries with MyBatis-compatible pagination
  and `NOT_NULL` update semantics.
- `src/auth.rs` — MD5 password contract, HS256 JWT, logout blacklist, login
  limit, and authentication middleware.
- `src/sync.rs` — Sonarr/Radarr title sync, TMDB title sync, rule sync, and
  on-demand sync.
- `src/config.rs` — environment > `system_config` > default resolution, the
  key whitelist, and the shared `ConfigStore` snapshot.
- `src/cache.rs` — bounded TTL cache with Java Caffeine-compatible expiry
  modes.
- `src/db.rs` — sqlx SQLite pool, schema and seed bootstrap, reference-data
  queries.
- `src/import.rs` — reference-table import for `jproxy-rs import --from ... --to ...`.
- `src/query.rs` — query-parameter editing and cache-key construction.
- `src/rules.rs` — priority-ordered compiled formatting rules.
- `src/search.rs` — alternate-title plan and `MIN_COUNT` retry executor.
- `src/torznab.rs` — item counting, page merge/trim, and XML item-title
  rewriting.
- `src/upstream.rs` — reqwest/rustls upstream client, retry/backoff, bounded
  concurrency, and error mapping.
- `src/downloader.rs` — qBittorrent/Transmission login, sessions, and API
  calls.
- `src/rename.rs` — Sonarr/Radarr history rename tasks and their schedules.
- `src/schedule.rs` — exact Java cron next-run calculation and wall-clock
  loops.
- `src/probe.rs` — Java `CheckUtil` live configuration probes.
- Supporting modules: `src/formatting.rs` (item title formatting),
  `src/text.rs` (Java `FormatUtil` ports), `src/models.rs` (shared types).
- `migrations/` — Java-compatible schema (`0001_schema.sql`) and first-run
  `system_config` seed (`0002_admin_seed.sql`). The first-run admin row is
  inserted by `db::seed_first_run_admin` only when `system_user` is empty.
- `admin_dist/` — compiled upstream admin UI and MIT license, served read-only
  from disk.

## Configuration and secrets

- Precedence is **environment > `system_config` > built-in default** for the
  whitelisted keys. Everything else is environment-only.
- The whitelist is `src/config.rs::SYSTEM_CONFIG_KEYS`: Sonarr/Radarr URL, API
  key, indexer format, and languages; Jackett/Prowlarr URL; TMDB URL and API
  key; `cleanTitleRegex`; `ruleSyncAuthors`; rule locations; and
  qBittorrent/Transmission URL, username, and password. `system_user` is read
  only to verify admin login. Downloader credentials stay in memory.
- `CONTROL_SOCKET` is environment-only. It defaults to
  `/tmp/jproxy/control.sock`; token and `runtime.json` live beside it. The
  socket, token, and metadata are `0600`, the directory is created `0700`, and
  Linux peer credentials must report the service UID or root. Never log the
  control token or socket payloads.
- Configuration is a shared snapshot. A successful
  `/api/system/config/update` or `/api/system/cache/clear` with
  `cacheName=system_config` re-reads the database and applies new values
  without a restart. Environment values continue to override rows. Rows with
  an invalid `validStatus` fail strict reads; callers must degrade or skip,
  not panic.
- `SYNC_ENABLED` defaults to `true`; `SYNC_INTERVAL=0` also disables
  background and on-demand title sync (it does not gate rule sync).
  `LOGIN_ENABLED` defaults to `true`, matching Java's production default; admin
  API calls require a bearer token, which login returns as an Anonymous JWT
  only when login is explicitly disabled. Relevant timeouts:
  `REQUEST_TIMEOUT_SECS=30`, `ONDEMAND_SYNC_TIMEOUT_SECS=30`,
  `PROBE_CONNECT_TIMEOUT_SECS=6`, `PROBE_READ_TIMEOUT_SECS=12`,
  `DOWNLOADER_REQUEST_TIMEOUT_SECS=30`, and `TOKEN_EXPIRES=10080` minutes.
- Never log, print, echo, or commit API keys, passwords, password values or
  hashes, cookies, session ids, bearer tokens, `Authorization` headers, or real
  `system_config` values. Tests and fixtures must use synthetic values.
- `/api/system/config/update` performs live probes against the submitted URLs.
  Do not point it at production services, the NAS, or a real downloader in
  tests or casual runs.
- `/api/system/config/version` reports `CARGO_PKG_VERSION` and checks
  `VERSION_CHECK_URL`, which defaults to
  `https://api.github.com/repos/inscripoem/jproxy-rs/releases/latest`. A
  missing release or failed request returns the local version. Do not create
  releases or point the check at the Java repository.
- Do not put real credentials in `.env`, test code, fixtures, or shell history.
  Prefer an operator-exported environment variable passed directly to a
  command.

### Default admin credentials (decided)

- A fresh database with no `system_user` rows receives `jproxy` /
  `jproxy@2023`, enabled, matching Java. `ADMIN_USERNAME` and `ADMIN_PASSWORD`
  override the defaults for that first row only.
- The stored value is uppercase `MD5(MD5(password))`, matching the Java
  UI-plus-service double hash.
- Existing `system_user` rows are preserved exactly. Never overwrite, rename,
  reset, or duplicate a migrated account; the live Java database uses
  `inscripoem`, not `jproxy`.
- There is no setup wizard, setup token, secret file, or forced first-login
  change.
- The admin UI/API require login by default and stay LAN-accessible. The
  Torznab routes and `/health` remain public.
- Startup logs one warning when the default credential is still stored and the
  service binds to a non-loopback address. Never log the password, hash, or
  environment value.
- Operator recovery uses `jproxy-rs admin set-password --stdin` or
  `admin reset-password --stdin`; both open the database directly. See
  `README.md` for the full CLI.

### Pagination and CLI invariants

- Malformed `current`/`pageSize` values (zero, negative, non-numeric) return
  HTTP 400.
- A valid out-of-range page returns HTTP 200 with the requested coordinates,
  the real total, and an empty list. Do not reintroduce the MyBatis page-one
  restart or Java's `subList` exception.
- CLI subcommands must never start the HTTP server. Online commands speak to
  the Unix control socket and reuse the admin handlers; they must not edit the
  database of a running process. `admin` is always an explicit offline
  database operation and refuses to run while a live control socket exists
  unless `--offline` is passed. `config`, `sync`, and `doctor` may fall back to
  direct database access only when no control socket exists, with a stderr
  notice; a reachable socket with a bad token or permission mode is an error.
  Every command exits non-zero on failure, masks secret `system_config` values
  unless `--reveal` is passed, and requires `--yes` for destructive operations.

## Git and scope

- Do not commit, push, merge, rebase, reset, or create tags unless explicitly
  requested.
- Commit messages use `scope: description` with a lowercase description and no
  trailing period, for example `rename: isolate per-item downloader errors`.
- Stage only the files changed by the task with `git add <file>`. Never use
  `git add -A`, `git add .`, or interactive staging. Keep one file's changes in
  one commit.
- Do not touch `/home/inscripoem/dev/jproxy-rs`, `/home/inscripoem/nix-config`,
  or `/mnt/user`. Do not deploy, publish images, or restart services.
- No force operations. No history rewriting.

## NAS constraints

- `tower.local` runs the Java jproxy that serves as the reference
  implementation. Do not restart, stop, reconfigure, or otherwise modify it.
- Do not write under `/mnt/user`, including test databases, logs, fixtures,
  and temporary files.
- A read-only export is allowed only through `scripts/export-fixtures.sh`,
  which uses `sqlite3 -readonly` on the remote database. Keep any temporary
  copy outside the NAS and delete it when finished.
- Any NAS write requires a timestamped backup and explicit operator approval
  before the change. Do not perform one on your own judgment.
- Run local proxies with a local or temporary `DATABASE_PATH`. Never point
  `DATABASE_PATH` or an import target at `/mnt/user`.

## Parity documentation

- `PARITY.md` defines scope: baseline versus extra features, acceptance
  criteria, and the merge process.
- `PARITY_REPORT.md` is the chronological evidence record: implemented surface,
  verification results, resource measurements, deviations, remaining gaps, and
  review questions.
- `PARITY_REPORT.md` contains earlier phases followed by the latest strict
  Java-source alignment phase. The latest phase supersedes earlier deviations
  it explicitly resolves; check the whole document before treating an old
  deviation as current.
- When behavior changes, update the relevant parity document with the commit,
  test evidence, and any new or resolved deviation. Never silently remove a
  recorded deviation. If a deviation is intentional, pin it in a test with a
  comment and keep it documented.
- Do not claim a live A/B, NAS verification, or resource measurement that was
  not actually performed. Label mock-based results as such.
- The Rust `regex` crate lacks backreferences and look-around. Unsupported
  rules are skipped with a warning; document the limitation rather than
  silently changing match semantics.
