# Parity Report

Milestone: jproxy-rs 1:1 parity, baseline and downloader/rename feature phases.

## Commits

- `8b976fa` — `jproxy-rs: baseline core proxy parity` (core proxy, 12/12 live
  query parity).
- `6f5bedb` — `jproxy-rs: baseline sync parity` (synchronization layer).
- Feature phase — branch `features/full-parity`, commit message
  `jproxy-rs: downloader login and rename parity` (not pushed).

Baseline branch: `main`. Feature worktree started at `e73d373`.

## Implemented baseline

- Sonarr/Radarr title synchronization, hourly from process start.
- Sonarr and Radarr rule synchronization, daily from process start.
- TMDB title synchronization for Sonarr `sno = 0` rows missing TMDB titles,
  using both configured languages.
- On-demand title synchronization on lookup misses, gated by
  `SYNC_INTERVAL`, serialized per kind, bounded by a timeout, degrading to the
  normal proxy response on failure.
- Java author-directory rule source: `author.json`,
  `sonarr@{author}.json`, `radarr@{author}.json`, backup-location retry, and
  per-author failure isolation.
- Title/formatting caches with `CACHE_EXPIRES` after-access; indexer-result
  cache with `INDEXER_RESULT_CACHE_EXPIRES` after-create.
- Configuration precedence: environment > `system_config` > default, limited
  to the baseline whitelist. Downloader credentials and `system_user` are not
  read.
- Java-compatible upsert semantics (no stale-row deletion, nullable insert
  defaults, update preservation).
- Radarr unknown-title year-less retry now matches Java (added after the
  subagent report).

## Verification

- `cargo fmt --check`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo test`: **91 passed, 0 failed** — 58 unit, 10 parity, 3 real-data,
  14 sync, 6 upstream-error.
- `cargo build --release`: pass; binary 8.5 MB.

## Resource measurements

Method: release binary, temporary SQLite database, local mock
Sonarr/Radarr/TMDB/rule/Torznab endpoints; `VmRSS` from
`/proc/<pid>/status`; idle and post-sync are medians of three runs.

| Phase | Workload | RSS |
|---|---|---|
| Idle | sync disabled, no requests | 9.5 MB |
| After one sync pass | 360 Sonarr title rows, 3,000 Radarr rows, 240 TMDB rows, 10 rules | 12.5 MB |
| Proxy load | 2,000 queries, concurrency 16, 500 distinct `q` | 25.6 MB |
| Proxy load, full cache | 2,000 distinct `q`, one run | 28.0 MB |

Baseline target was 30-45 MB RSS; measured values are below it. Java jproxy
currently uses about 737 MB. Caveats: `VmRSS`, not peak `VmHWM`; SQLite page
cache and allocator retention included; corpus is synthetic and smaller than a
live library in some dimensions.

## Live read-only sync check (2026-11-03)

Method: a local SQLite backup of the live jproxy database was used; the release
binary ran locally for 240 seconds with `SYNC_ENABLED=true`, with
`SONARR_URL`/`RADARR_URL` overridden to the LAN addresses. API keys and TMDB
settings came from the copied `system_config` at runtime. No `/mnt/user` writes
and no NAS service changes were made; the copy was deleted afterwards.

Results:

- Sonarr titles synced: 46.
- Radarr titles synced: 1,402 source rows.
- Sonarr rules synced: 90 across authors `LuckyPuppy514` (85), `grc` (1),
  `haha` (4).
- Radarr rules synced: 38 from `LuckyPuppy514`; `grc`/`haha` fetches failed and
  were skipped per-author, as designed.
- TMDB rows: no new rows; the existing 20 rows satisfied the need-sync query.
- DB counts after the run: 46 / 1535 / 20 / 90 / 38 / 20 (titles, rules, config),
  unchanged because the sync is upsert-only.
- The process was stopped by the 240-second bound; this is expected for a
  long-running server.

## Deviations from Java

- Hourly/daily sync starts from process start rather than Java's fixed
  `:00`/`:30` and `00:15`/`01:45` cron times.
- Rule backup location is retried per fetch; Java selects it globally at
  startup.
- TMDB per-item failures are isolated; Java aborts the transaction.
- Missing Sonarr `titleSlug`/alternate fields are skipped; missing Radarr
  `cleanTitle` is derived from the folder title. Java would fail on nulls.
- Rule author names are restricted to a conservative character set before URL
  construction.
- Configuration is resolved at startup; restart to apply changes.
- On-demand sync has a timeout and degrades; Java can propagate the failure.

## Feature phase: downloader login and rename

Implemented on `features/full-parity`:

- qBittorrent login via `POST /api/v2/auth/login` with a form body, a
  `Set-Cookie` session, verbatim `Cookie` reuse, and torrent
  `rename`/`files`/`renameFile` calls.
- Transmission login via JSON-RPC `session-get`, HTTP Basic auth, the
  `409 Conflict` + `X-Transmission-Session-Id` handshake, one re-login and retry
  on later 409 responses, and `torrent-get`/`torrent-rename-path`.
- Startup login plus scheduled re-login every 30 minutes
  (`DOWNLOADER_LOGIN_INTERVAL_SECS`). Login failures are debug-level,
  non-fatal, and retried on the next pass.
- Sonarr rename every 30 seconds and Radarr rename every minute
  (`SONARR_RENAME_INTERVAL_SECS`/`RADARR_RENAME_INTERVAL_SECS`), from process
  start, with the first cron tick skipped. History lookback is 30 minutes for
  both (`*_RENAME_FALLBACK_MINS`).
- Sonarr/Radarr `history/since?eventType=1` parsing, `downloadId` lowercasing,
  `data.torrentInfoHash` fallback, download-client selection, Sonarr hash
  deduplication, and per-item failure isolation.
- `RENAME_FILE=true`: qBittorrent torrent file renaming with the Java path
  construction, video/subtitle extension matching, subtitle numbering, and
  Sonarr `season`/`episode` rule formatting. Radarr file names use the source
  title. Transmission file renaming remains a no-op, as in Java.
- Downloader keys added to the configuration whitelist:
  `qbittorrentUrl`, `qbittorrentUsername`, `qbittorrentPassword`,
  `transmissionUrl`, `transmissionUsername`, `transmissionPassword`. Precedence
  is environment > `system_config` > default. Values are never logged.
- No media is deleted or moved by the service; only downloader rename APIs are
  called.

### Verification

- `nix develop --command cargo fmt --check`: pass.
- `nix develop --command cargo clippy --all-targets -- -D warnings`: pass.
- `nix develop --command cargo test`: **111 passed, 0 failed** — 66 unit, 10
  parity, 3 real-data, 14 sync, 5 downloader, 7 rename, 6 upstream-error.
- `nix develop --command cargo build --release`: pass; binary 8.3 MB.
- All downloader/rename tests use local mock HTTP servers and synthetic
  credentials. No test contacts a real downloader, Sonarr, Radarr, NAS, or the
  network.

### Resource measurements

Method: release binary, temporary SQLite database, local mock
Sonarr/qBittorrent/Torznab endpoints, `VmRSS` from `/proc/<pid>/status`, median
of three independent runs with five samples per phase. The Sonarr rename
interval was shortened to 10 seconds so a pass could be observed; the
2,000-query load used concurrency 16 and 500 distinct `q` values.

| Phase | Workload | RSS |
| --- | --- | --- |
| Idle with downloaders configured | Initial qBittorrent login, no rename pass, no queries | 9.8 MB (10,028–10,300 KB) |
| After one login/rename pass | qBittorrent login, Sonarr history, torrent rename, one file rename | 10.3 MB (10,572–10,848 KB) |
| Under proxy load | 2,000 queries, 16 concurrent, 500 distinct `q`, 500 cached results | 13.9 MB (14,140–14,596 KB) |

The measured application log contained zero occurrences of the synthetic
password, cookie, or API-key values. The feature profile stays below the 30–45
MB baseline target and the 100 MB full-parity target.

### Deviations and assumptions for this phase

- Login and rename tickers count from process start rather than from fixed
  wall-clock cron seconds.
- Timing variables are environment extensions because Java keeps them in
  `application-prod.yml` rather than `system_config`.
- qBittorrent is configured as a base URL; Transmission as the full JSON-RPC
  URL.
- Java debug-logs the qBittorrent cookie; this implementation never logs
  cookies, session ids, passwords, or API keys.
- Blank history source titles are skipped and counted as per-item errors instead
  of attempting a null rename.
- Non-2xx qBittorrent rename responses fail that item, matching RestTemplate's
  exception behavior. Transmission only checks HTTP status and ignores the
  JSON-RPC `result`, matching Java.
- A history item addressed to Transmission while only qBittorrent is logged in
  fails in isolation; Java would likewise fail inside the item loop.
- No live A/B was performed against a real downloader or NAS. The feature tests
  are mock-only by design; activation requires a user-run deployment check.

## Not covered by this baseline

- Full live A/B for synchronization writes. A read-only live sync check was
  performed (see above); the remaining synchronization tests use local mocks and
  synthetic values.
- Exact cron alignment and Java's admin cache controls.
- Remaining extra features: admin UI/API, login/session, example/cache/
  system-config REST endpoints, and downloader API proxying. Downloader login
  and rename tasks are implemented above.

## Review questions

1. Accept the baseline resource profile (9.5-28.0 MB) and the documented
   deviations?
2. Allow the `features/full-parity` worktree to fast-forward from `8b976fa` to
   `6f5bedb`, then start the extra-feature implementation?
3. Do a live read-only synchronization check against the real Sonarr/Radarr/
   TMDB/GitHub endpoints before or after the extra features?
4. Accept the downloader/rename feature profile (9.8-14.0 MB RSS) and the
   documented mock-only verification?
