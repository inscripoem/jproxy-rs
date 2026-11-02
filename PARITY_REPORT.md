# Baseline Parity Report

Milestone: jproxy-rs 1:1 parity, baseline phase.

## Commits

- `8b976fa` — `jproxy-rs: baseline core proxy parity` (core proxy, 12/12 live
  query parity).
- `6f5bedb` — `jproxy-rs: baseline sync parity` (synchronization layer).

Branch: `main`. Working tree clean. Nothing pushed.

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

## Not covered by this baseline

- Live synchronization A/B against the real Sonarr/Radarr/TMDB/GitHub
  endpoints. All synchronization tests use local mocks and synthetic values.
- Exact cron alignment and Java's admin cache controls.
- Extra features: qBittorrent/Transmission login, rename tasks, admin UI/API,
  login/session, example/cache/system-config REST endpoints.

## Review questions

1. Accept the baseline resource profile (9.5-28.0 MB) and the documented
   deviations?
2. Allow the `features/full-parity` worktree to fast-forward from `8b976fa` to
   `6f5bedb`, then start the extra-feature implementation?
3. Do a live read-only synchronization check against the real Sonarr/Radarr/
   TMDB/GitHub endpoints before or after the extra features?
