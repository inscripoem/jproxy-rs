# jproxy-rs Parity Milestone

Goal: reach 1:1 functional parity with the Java jproxy before any
simplification or feature removal.

## Baseline (main branch)

The baseline contains the core proxy plus the synchronization needed to keep
long-term search quality from degrading:

- Torznab proxy and title/rule rewriting (12/12 live parity with Java).
- Bounded concurrency, retry/backoff, and upstream error mapping.
- Sonarr/Radarr title and rule synchronization.
- TMDB title synchronization.
- On-demand synchronization when a lookup misses.
- GitHub author-directory rule source.
- Cache and system-config parity for the baseline features.
- Resource target: 30-45 MB RSS.

### Baseline implementation notes

- Title sync runs hourly and rule sync daily from process start, approximating
  Java's `:00`/`:30` and `00:15`/`01:45` cron entries. `SYNC_INTERVAL` (default
  3 minutes) gates background and on-demand title sync, not rule refresh.
- Configuration precedence is environment > `system_config` > default for
  `sonarrUrl`, `sonarrApikey`, `radarrUrl`, `radarrApikey`, `tmdbUrl`,
  `tmdbApikey`, `sonarrLanguage1`, `sonarrLanguage2`, `ruleSyncAuthors`,
  `ruleLocation`, and `ruleLocationBackup`. Only these keys are selected;
  downloader credentials and `system_user` rows are never read or logged.
- Rules use Java's GitHub author-directory protocol (`author.json`,
  `sonarr@{author}.json`, `radarr@{author}.json`) with backup-location fallback
  and per-author error isolation. `ruleLocation`/`ruleLocationBackup` are
  environment/`system_config` extensions because Java stores them in
  `application-prod.yml` rather than the database.
- On-demand title sync mirrors `syncAndGetSonarrTitle`/`syncAndGetRadarrTitle`
  with a per-kind mutex, `ONDEMAND_SYNC_TIMEOUT_SECS`, and failure degradation
  to the normal proxy response. Java can propagate the sync failure.
- Title and formatting caches use `CACHE_EXPIRES` after-access; the indexer
  result cache uses `INDEXER_RESULT_CACHE_EXPIRES` after-create and is not
  evicted by sync. Title/rule/TMDB sync clears the reference-data caches.
- Measured release RSS with local mocks: 9.5 MB idle, 12.5 MB after one sync
  pass (360 Sonarr title rows, 3,000 Radarr title rows, 240 TMDB rows, 10 rule
  rows), 25.6 MB under 2,000 proxy queries at concurrency 16 with 500 distinct
  `q` values, and 28.0 MB with a full 1,000-entry result cache (2,000 distinct
  `q` values). Both load phases are below the 30-45 MB target on this synthetic
  corpus; see `README.md` for the method and caveats.
- `cargo test` covers 90 tests, including 14 local-mock synchronization tests.
  No test makes a live Sonarr/Radarr/TMDB/GitHub request.

## Extra features (`features/full-parity` worktree)

Features added only to match the original, kept out of the baseline until
reviewed:

- qBittorrent/Transmission login and token refresh.
- Sonarr/Radarr rename tasks (`RENAME_FILE`).
- Admin UI, login/session, and management REST APIs.
- Operator/agent CLI for admin credentials, `system_config`, one-shot sync,
  diagnostics, and version reporting.
- Unix-domain control channel serving the same Axum router, with token and
  peer-credential authentication, job ids, SSE progress, cancellation, rename
  dry-run, downloader login, and a CLI equivalent to the admin UI.
- Example/cache/system-config REST endpoints.
- Resource target: total below 100 MB RSS.

## Acceptance

- Feature checklist compared against the Java implementation.
- Live A/B for proxy queries, synchronization writes, rename behavior, and
  management APIs.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, and
  `cargo build --release`.
- RSS measured idle, after synchronization, and under representative query
  load.
- No secrets in logs, tests, or fixtures.
- A written report reviewed before merging extra features.

## Process

1. Commit the baseline on `main`.
2. Create `features/full-parity` as a separate git worktree.
3. Implement and measure the baseline synchronization first.
4. Implement extra features in the worktree, measure their additional cost,
   and report.
5. Merge only after explicit approval.
