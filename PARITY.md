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

## Extra features (`features/full-parity` worktree)

Features added only to match the original, kept out of the baseline until
reviewed:

- qBittorrent/Transmission login and token refresh.
- Sonarr/Radarr rename tasks (`RENAME_FILE`).
- Admin UI, login/session, and management REST APIs.
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
