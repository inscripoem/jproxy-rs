use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::app::AppState;
use crate::db;
use crate::models::{AppKind, MatchedTitle};
use crate::text::clean_title;

static RADARR_FOLDER_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" \(\d{4}\)$").expect("static regex"));

/// Java schedules titles hourly and rules daily. The ticker starts at process
/// start rather than at the Java cron minutes (:00/:30 and 00:15/01:45).
const TITLE_SYNC_PERIOD: Duration = Duration::from_secs(60 * 60);
const RULE_SYNC_PERIOD: Duration = Duration::from_secs(60 * 60 * 24);

const SONARR_TITLE_UPSERT: &str = "\
INSERT INTO sonarr_title \
(id, tvdb_id, series_id, sno, main_title, title, clean_title, season_number, monitored, valid_status, update_time) \
VALUES (?, ?, ?, ?, ?, ?, ?, COALESCE(?, 1), ?, 1, DATETIME(CURRENT_TIMESTAMP, 'localtime')) \
ON CONFLICT(id) DO UPDATE SET \
tvdb_id = excluded.tvdb_id, series_id = excluded.series_id, sno = excluded.sno, \
main_title = excluded.main_title, title = excluded.title, clean_title = excluded.clean_title, \
season_number = COALESCE(?, season_number), monitored = excluded.monitored, \
update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime')";

const RADARR_TITLE_UPSERT: &str = "\
INSERT INTO radarr_title \
(id, tmdb_id, movie_id, sno, main_title, title, clean_title, year, monitored, valid_status, update_time) \
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, DATETIME(CURRENT_TIMESTAMP, 'localtime')) \
ON CONFLICT(id) DO UPDATE SET \
tmdb_id = excluded.tmdb_id, movie_id = excluded.movie_id, sno = excluded.sno, \
main_title = excluded.main_title, title = excluded.title, clean_title = excluded.clean_title, \
year = excluded.year, monitored = excluded.monitored, \
update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime')";

/// Java's MyBatis update strategy is NOT_NULL: missing JSON fields preserve
/// existing values, and `validStatus = 1` is nulled before save so a locally
/// disabled rule stays disabled.
const RULE_UPSERT_TEMPLATE: &str = "\
INSERT INTO {table} \
(id, token, priority, regex, replacement, offset, example, remark, author, valid_status, update_time) \
VALUES (?, ?, COALESCE(?, 1000), ?, COALESCE(?, ''), COALESCE(?, 0), COALESCE(?, ''), ?, ?, COALESCE(?, 1), \
DATETIME(CURRENT_TIMESTAMP, 'localtime')) \
ON CONFLICT(id) DO UPDATE SET \
token = excluded.token, \
priority = COALESCE(?, priority), \
regex = excluded.regex, \
replacement = COALESCE(?, replacement), \
offset = COALESCE(?, offset), \
example = COALESCE(?, example), \
remark = COALESCE(?, remark), \
author = COALESCE(?, author), \
valid_status = CASE WHEN excluded.valid_status = 1 THEN valid_status ELSE excluded.valid_status END, \
update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime')";

#[derive(Debug, Clone)]
struct SonarrTitleRow {
    id: i64,
    tvdb_id: i64,
    series_id: Option<i64>,
    sno: i64,
    main_title: String,
    title: String,
    clean_title: String,
    season_number: Option<i64>,
    monitored: i64,
}

#[derive(Debug, Clone)]
struct RadarrTitleRow {
    id: i64,
    tmdb_id: i64,
    movie_id: Option<i64>,
    sno: i64,
    main_title: String,
    title: String,
    clean_title: String,
    year: i64,
    monitored: i64,
}

#[derive(Debug, Deserialize)]
struct RuleSyncRecord {
    id: String,
    token: String,
    #[serde(default)]
    priority: Option<i64>,
    regex: String,
    #[serde(default)]
    replacement: Option<String>,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    example: Option<String>,
    #[serde(default)]
    remark: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default, alias = "validStatus")]
    valid_status: Option<i64>,
}

/// Shared synchronization state.
///
/// The title gate mirrors Java's `syncIntervalCache`: the timestamp is written
/// before the HTTP attempt, so a failed sync still blocks retries for one
/// interval. Title locks mirror the Java `synchronized` on-demand path.
pub struct SyncState {
    title_gates: [Mutex<Option<Instant>>; 2],
    tmdb_gate: Mutex<Option<Instant>>,
    title_locks: [Mutex<()>; 2],
}

impl SyncState {
    pub fn new() -> Self {
        Self {
            title_gates: [Mutex::new(None), Mutex::new(None)],
            tmdb_gate: Mutex::new(None),
            title_locks: [Mutex::new(()), Mutex::new(())],
        }
    }

    fn title_gate(&self, kind: AppKind) -> &Mutex<Option<Instant>> {
        &self.title_gates[kind.index()]
    }

    fn title_lock(&self, kind: AppKind) -> &Mutex<()> {
        &self.title_locks[kind.index()]
    }

    pub async fn clear_tmdb_gate(&self) {
        *self.tmdb_gate.lock().await = None;
    }

    /// Java clears `sonarr_title_sync_interval` / `radarr_title_sync_interval`
    /// when a manual sync fails or when system configuration is updated.
    pub async fn clear_title_gate(&self, kind: AppKind) {
        *self.title_gate(kind).lock().await = None;
    }

    /// Used by the cache clear-all endpoint.
    pub async fn clear_all_gates(&self) {
        for kind in [AppKind::Sonarr, AppKind::Radarr] {
            self.clear_title_gate(kind).await;
        }
        self.clear_tmdb_gate().await;
    }
}

impl Default for SyncState {
    fn default() -> Self {
        Self::new()
    }
}

/// Start the background synchronization loops.
///
/// Titles run hourly and rules daily, approximating the Java cron schedule from
/// process start. `SYNC_ENABLED=false` or a zero `SYNC_INTERVAL` disables both
/// background and on-demand synchronization.
pub fn spawn(state: AppState) {
    if !state.config.sync_enabled {
        info!("background sync disabled");
        return;
    }

    let title_state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TITLE_SYNC_PERIOD);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            for kind in [AppKind::Sonarr, AppKind::Radarr] {
                // Java skips the TMDB step when the Sonarr title sync throws.
                let title_result = sync_titles_once(&title_state, kind).await;
                if let Err(error) = &title_result {
                    warn!(%kind, %error, "title sync failed");
                }
                if kind == AppKind::Sonarr
                    && title_result.is_ok()
                    && let Err(error) = sync_tmdb_once(&title_state).await
                {
                    warn!(%error, "TMDB title sync failed");
                }
            }
        }
    });

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RULE_SYNC_PERIOD);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            for kind in [AppKind::Sonarr, AppKind::Radarr] {
                if let Err(error) = sync_rules_once(&state, kind).await {
                    warn!(%kind, %error, "rule sync failed");
                }
            }
        }
    });
}

async fn acquire_gate(gate: &Mutex<Option<Instant>>, interval: Duration) -> bool {
    if interval.is_zero() {
        return false;
    }
    let mut last = gate.lock().await;
    if last.is_some_and(|instant| instant.elapsed() < interval) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

pub async fn sync_titles_once(state: &AppState, kind: AppKind) -> Result<Option<usize>> {
    if !state.config.sync_enabled {
        return Ok(None);
    }
    let (base, api_key) = title_endpoint(state, kind);
    let (Some(base), Some(api_key)) = (base, api_key) else {
        return Ok(None);
    };
    if !acquire_gate(state.sync.title_gate(kind), state.config.sync_interval).await {
        return Ok(None);
    }

    let endpoint = match kind {
        AppKind::Sonarr => format!("{}/api/v3/series", base.trim_end_matches('/')),
        AppKind::Radarr => format!("{}/api/v3/movie", base.trim_end_matches('/')),
    };
    let response = state
        .upstream
        .http_client()
        .get(endpoint)
        .query(&[("apikey", api_key)])
        .send()
        .await
        .map_err(redact_reqwest)?;
    let status = response.status();
    if !status.is_success() {
        bail!("{} API returned HTTP {status}", kind);
    }
    let payload = response.json::<Value>().await.map_err(redact_reqwest)?;

    let count = match kind {
        AppKind::Sonarr => {
            let rows = parse_sonarr_titles(&payload, &state.clean_title_regex)?;
            upsert_sonarr_titles(&state.pool, &rows).await?
        }
        AppKind::Radarr => {
            let rows = parse_radarr_titles(&payload, &state.clean_title_regex)?;
            upsert_radarr_titles(&state.pool, &rows).await?
        }
    };
    state.clear_reference_caches().await;
    info!(%kind, count, "synced titles");
    Ok(Some(count))
}

pub async fn sync_rules_once(state: &AppState, kind: AppKind) -> Result<Option<usize>> {
    if !state.config.sync_enabled {
        return Ok(None);
    }
    let locations = state.config.rule_locations();
    if locations.is_empty() {
        return Ok(None);
    }
    let configured_authors = state.config.rule_sync_authors();
    if configured_authors.trim().is_empty() {
        return Ok(None);
    }

    let authors = if configured_authors == "ALL" {
        fetch_author_list(state, &locations).await?
    } else {
        configured_authors
            .split(',')
            .map(str::trim)
            .filter(|author| !author.is_empty())
            .map(str::to_owned)
            .collect()
    };
    if authors.is_empty() {
        return Ok(None);
    }

    let mut total = 0;
    let mut any_succeeded = false;
    for author in authors {
        match sync_rules_for_author(state, kind, &locations, &author).await {
            Ok(count) => {
                any_succeeded = true;
                total += count;
                info!(%kind, %author, count, "synced rules");
            }
            Err(error) => {
                warn!(%kind, %author, %error, "rule sync failed for author");
            }
        }
    }
    if any_succeeded {
        state.clear_reference_caches().await;
    }
    Ok(Some(total))
}

pub async fn sync_tmdb_once(state: &AppState) -> Result<Option<usize>> {
    if !state.config.sync_enabled {
        return Ok(None);
    }
    let Some(api_key) = state
        .config
        .tmdb_api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };
    let tvdb_ids = db::list_tvdb_ids_needing_tmdb(&state.pool).await?;
    if tvdb_ids.is_empty() {
        return Ok(None);
    }
    if !acquire_gate(&state.sync.tmdb_gate, state.config.sync_interval).await {
        return Ok(None);
    }

    let base = state.config.tmdb_url().trim_end_matches('/').to_owned();
    let languages = state.config.sonarr_languages();
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for tvdb_id in tvdb_ids {
        if !seen.insert(tvdb_id) {
            continue;
        }
        for language in &languages {
            match fetch_tmdb_title(state, &base, api_key, tvdb_id, language).await {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => {}
                Err(error) => {
                    warn!(tvdb_id, %language, %error, "TMDB title lookup failed");
                }
            }
        }
    }
    if rows.is_empty() {
        return Ok(Some(0));
    }
    let count = db::insert_tmdb_titles(&state.pool, &rows).await?;
    state.clear_reference_caches().await;
    info!(count, "synced TMDB titles");
    Ok(Some(count))
}

/// On-demand synchronization used when a proxy query misses the title tables.
///
/// The Java path is `synchronized` and may propagate an HTTP failure. This
/// implementation bounds the work with a timeout and returns `None` on failure so
/// the caller continues with its normal proxy behavior instead of returning 500.
pub async fn sync_and_get_title(
    state: &AppState,
    kind: AppKind,
    title: &str,
) -> Option<MatchedTitle> {
    let timeout = state.config.on_demand_sync_timeout;
    match tokio::time::timeout(timeout, sync_and_get_title_inner(state, kind, title)).await {
        Ok(result) => result,
        Err(_) => {
            warn!(%kind, "on-demand sync timed out");
            None
        }
    }
}

async fn sync_and_get_title_inner(
    state: &AppState,
    kind: AppKind,
    title: &str,
) -> Option<MatchedTitle> {
    if !state.config.sync_enabled || state.config.sync_interval.is_zero() {
        return None;
    }
    if !state.config.title_sync_configured(kind) {
        return None;
    }

    let _guard = state.sync.title_lock(kind).lock().await;
    match db::find_title(&state.pool, kind, title, &state.clean_title_regex).await {
        Ok(Some(matched)) => return Some(matched),
        Ok(None) => {}
        Err(error) => {
            warn!(%kind, %error, "title re-query before on-demand sync failed");
            return None;
        }
    }

    if let Err(error) = sync_titles_once(state, kind).await {
        warn!(%kind, %error, "on-demand title sync failed");
    }
    if kind == AppKind::Sonarr {
        state.sync.clear_tmdb_gate().await;
        if let Err(error) = sync_tmdb_once(state).await {
            warn!(%error, "on-demand TMDB sync failed");
        }
    }

    match db::find_title(&state.pool, kind, title, &state.clean_title_regex).await {
        Ok(matched) => matched,
        Err(error) => {
            warn!(%kind, %error, "title re-query after on-demand sync failed");
            None
        }
    }
}

fn title_endpoint(state: &AppState, kind: AppKind) -> (Option<&str>, Option<&str>) {
    match kind {
        AppKind::Sonarr => (
            state.config.sonarr_url.as_deref(),
            state.config.sonarr_api_key.as_deref(),
        ),
        AppKind::Radarr => (
            state.config.radarr_url.as_deref(),
            state.config.radarr_api_key.as_deref(),
        ),
    }
}

async fn fetch_author_list(state: &AppState, locations: &[String]) -> Result<Vec<String>> {
    let body = fetch_rule_source(state, locations, "author.json").await?;
    let authors: Vec<String> =
        serde_json::from_str(&body).context("parse rule author list JSON")?;
    Ok(authors
        .into_iter()
        .map(|author| author.trim().to_owned())
        .filter(|author| !author.is_empty())
        .collect())
}

async fn sync_rules_for_author(
    state: &AppState,
    kind: AppKind,
    locations: &[String],
    author: &str,
) -> Result<usize> {
    if !valid_author(author) {
        bail!("invalid rule author name");
    }
    let path = format!("{}@{}.json", kind.as_str(), author);
    let body = fetch_rule_source(state, locations, &path).await?;
    let records: Vec<RuleSyncRecord> =
        serde_json::from_str(&body).with_context(|| format!("parse {kind} rule JSON"))?;
    upsert_rules(&state.pool, kind, &records).await
}

async fn fetch_rule_source(state: &AppState, locations: &[String], path: &str) -> Result<String> {
    let mut last_error = None;
    for base in locations {
        let url = format!("{}/{}", base.trim_end_matches('/'), path);
        match state.upstream.http_client().get(url).send().await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    last_error = Some(format!("rule source returned HTTP {status}"));
                    continue;
                }
                return response.text().await.map_err(redact_reqwest);
            }
            Err(error) => {
                last_error = Some(format!("HTTP request failed: {}", error.without_url()));
            }
        }
    }
    bail!(
        "{}",
        last_error.unwrap_or_else(|| "no rule location configured".to_owned())
    )
}

async fn fetch_tmdb_title(
    state: &AppState,
    base: &str,
    api_key: &str,
    tvdb_id: i64,
    language: &str,
) -> Result<Option<db::TmdbTitleRow>> {
    let url = format!("{base}/3/find/{tvdb_id}");
    let response = state
        .upstream
        .http_client()
        .get(url)
        .query(&[
            ("api_key", api_key),
            ("language", language),
            ("external_source", "tvdb_id"),
        ])
        .send()
        .await
        .map_err(redact_reqwest)?;
    let status = response.status();
    if !status.is_success() {
        bail!("TMDB API returned HTTP {status}");
    }
    let payload = response.json::<Value>().await.map_err(redact_reqwest)?;
    let Some(first) = payload
        .get("tv_results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
    else {
        return Ok(None);
    };
    let Some(title) = first
        .get("name")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(db::TmdbTitleRow {
        tvdb_id,
        tmdb_id: first.get("id").and_then(Value::as_i64),
        language: language.to_owned(),
        title: title.to_owned(),
    }))
}

fn redact_reqwest(error: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!("HTTP request failed: {}", error.without_url())
}

fn parse_sonarr_titles(payload: &Value, clean_regex: &Regex) -> Result<Vec<SonarrTitleRow>> {
    let series = payload
        .as_array()
        .context("Sonarr series response must be a JSON array")?;
    let mut rows = Vec::new();
    for item in series {
        let Some(tvdb_id) = item.get("tvdbId").and_then(Value::as_i64) else {
            continue;
        };
        let Some(main_title) = item.get("title").and_then(Value::as_str) else {
            continue;
        };
        let Some(series_id) = item.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let monitored = bool_i64(item.get("monitored"));
        let mut sno = 0;
        let Some(id) = derive_title_id(tvdb_id, sno) else {
            continue;
        };
        rows.push(SonarrTitleRow {
            id,
            tvdb_id,
            series_id: Some(series_id),
            sno,
            main_title: main_title.to_owned(),
            title: main_title.to_owned(),
            clean_title: clean_title(main_title, clean_regex),
            season_number: Some(-1),
            monitored,
        });
        sno += 1;

        if let Some(title_slug) = item
            .get("titleSlug")
            .and_then(Value::as_str)
            .filter(|slug| !slug.trim().is_empty())
            && let Some(id) = derive_title_id(tvdb_id, sno)
        {
            rows.push(SonarrTitleRow {
                id,
                tvdb_id,
                series_id: Some(series_id),
                sno,
                main_title: main_title.to_owned(),
                title: main_title.to_owned(),
                clean_title: clean_title(title_slug, clean_regex),
                season_number: Some(-1),
                monitored,
            });
            sno += 1;
        }

        if let Some(alternates) = item.get("alternateTitles").and_then(Value::as_array) {
            for alternate in alternates {
                let Some(title) = alternate.get("title").and_then(Value::as_str) else {
                    continue;
                };
                let Some(id) = derive_title_id(tvdb_id, sno) else {
                    continue;
                };
                rows.push(SonarrTitleRow {
                    id,
                    tvdb_id,
                    series_id: Some(series_id),
                    sno,
                    main_title: main_title.to_owned(),
                    title: title.to_owned(),
                    clean_title: clean_title(title, clean_regex),
                    season_number: alternate.get("sceneSeasonNumber").and_then(Value::as_i64),
                    monitored,
                });
                sno += 1;
            }
        }
    }
    Ok(rows)
}

fn parse_radarr_titles(payload: &Value, clean_regex: &Regex) -> Result<Vec<RadarrTitleRow>> {
    let movies = payload
        .as_array()
        .context("Radarr movie response must be a JSON array")?;
    let mut rows = Vec::new();
    for item in movies {
        let Some(tmdb_id) = item.get("tmdbId").and_then(Value::as_i64) else {
            continue;
        };
        let Some(main_title) = item.get("title").and_then(Value::as_str) else {
            continue;
        };
        let Some(year) = item.get("year").and_then(Value::as_i64) else {
            continue;
        };
        let movie_id = item.get("id").and_then(Value::as_i64);
        let monitored = bool_i64(item.get("monitored"));
        let folder_title = item
            .get("path")
            .and_then(Value::as_str)
            .map(radarr_folder_title)
            .filter(|title| !title.trim().is_empty());
        let radarr_clean_title = item
            .get("cleanTitle")
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .map(str::to_owned);
        let mut sno = 0;

        let Some(id) = derive_title_id(tmdb_id, sno) else {
            continue;
        };
        rows.push(RadarrTitleRow {
            id,
            tmdb_id,
            movie_id,
            sno,
            main_title: main_title.to_owned(),
            title: main_title.to_owned(),
            clean_title: clean_title(main_title, clean_regex),
            year,
            monitored,
        });
        sno += 1;

        if let Some(folder_title) = &folder_title {
            if let Some(id) = derive_title_id(tmdb_id, sno) {
                rows.push(RadarrTitleRow {
                    id,
                    tmdb_id,
                    movie_id,
                    sno,
                    main_title: main_title.to_owned(),
                    title: folder_title.clone(),
                    clean_title: clean_title(folder_title, clean_regex),
                    year,
                    monitored,
                });
                sno += 1;
            }
            if let Some(id) = derive_title_id(tmdb_id, sno) {
                rows.push(RadarrTitleRow {
                    id,
                    tmdb_id,
                    movie_id,
                    sno,
                    main_title: main_title.to_owned(),
                    title: folder_title.clone(),
                    clean_title: radarr_clean_title
                        .clone()
                        .unwrap_or_else(|| clean_title(folder_title, clean_regex)),
                    year,
                    monitored,
                });
                sno += 1;
            }
        }

        if let Some(original_title) = item
            .get("originalTitle")
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            && let Some(id) = derive_title_id(tmdb_id, sno)
        {
            rows.push(RadarrTitleRow {
                id,
                tmdb_id,
                movie_id,
                sno,
                main_title: main_title.to_owned(),
                title: original_title.to_owned(),
                clean_title: clean_title(original_title, clean_regex),
                year,
                monitored,
            });
            sno += 1;
        }

        if let Some(alternates) = item.get("alternateTitles").and_then(Value::as_array) {
            for alternate in alternates {
                let Some(title) = alternate.get("title").and_then(Value::as_str) else {
                    continue;
                };
                let Some(id) = derive_title_id(tmdb_id, sno) else {
                    continue;
                };
                rows.push(RadarrTitleRow {
                    id,
                    tmdb_id,
                    movie_id,
                    sno,
                    main_title: main_title.to_owned(),
                    title: title.to_owned(),
                    clean_title: clean_title(title, clean_regex),
                    year,
                    monitored,
                });
                sno += 1;
            }
        }
    }
    Ok(rows)
}

fn radarr_folder_title(path: &str) -> String {
    let folder = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    RADARR_FOLDER_YEAR.replace(folder, "").into_owned()
}

fn derive_title_id(external_id: i64, sno: i64) -> Option<i64> {
    if external_id <= 0 || sno < 0 {
        return None;
    }
    format!("{external_id}{sno}").parse().ok()
}

fn bool_i64(value: Option<&Value>) -> i64 {
    i64::from(value.and_then(Value::as_bool).unwrap_or(false))
}

fn valid_author(author: &str) -> bool {
    !author.is_empty()
        && author
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-' | '.'))
}

async fn upsert_sonarr_titles(pool: &SqlitePool, rows: &[SonarrTitleRow]) -> Result<usize> {
    let mut transaction = pool.begin().await?;
    for row in rows {
        sqlx::query(SONARR_TITLE_UPSERT)
            .bind(row.id)
            .bind(row.tvdb_id)
            .bind(row.series_id)
            .bind(row.sno)
            .bind(&row.main_title)
            .bind(&row.title)
            .bind(&row.clean_title)
            .bind(row.season_number)
            .bind(row.monitored)
            .bind(row.season_number)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(rows.len())
}

async fn upsert_radarr_titles(pool: &SqlitePool, rows: &[RadarrTitleRow]) -> Result<usize> {
    let mut transaction = pool.begin().await?;
    for row in rows {
        sqlx::query(RADARR_TITLE_UPSERT)
            .bind(row.id)
            .bind(row.tmdb_id)
            .bind(row.movie_id)
            .bind(row.sno)
            .bind(&row.main_title)
            .bind(&row.title)
            .bind(&row.clean_title)
            .bind(row.year)
            .bind(row.monitored)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(rows.len())
}

async fn upsert_rules(
    pool: &SqlitePool,
    kind: AppKind,
    records: &[RuleSyncRecord],
) -> Result<usize> {
    let sql = RULE_UPSERT_TEMPLATE.replace("{table}", kind.rule_table());
    let mut transaction = pool.begin().await?;
    for record in records {
        // Java nulls validStatus == 1 before saving so the local value is kept.
        let valid_status = record.valid_status.filter(|value| *value != 1);
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(&record.id)
            .bind(&record.token)
            .bind(record.priority)
            .bind(&record.regex)
            .bind(&record.replacement)
            .bind(record.offset)
            .bind(&record.example)
            .bind(&record.remark)
            .bind(&record.author)
            .bind(valid_status)
            .bind(record.priority)
            .bind(&record.replacement)
            .bind(record.offset)
            .bind(&record.example)
            .bind(&record.remark)
            .bind(&record.author)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(records.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use sqlx::Row;

    fn regex() -> Regex {
        Regex::new(crate::config::DEFAULT_CLEAN_TITLE_REGEX).unwrap()
    }

    #[tokio::test]
    async fn sonarr_payload_maps_main_slug_and_alternate_rows() {
        let payload = serde_json::json!([
            {
                "id": 7,
                "tvdbId": 100,
                "title": "Main Show",
                "titleSlug": "main-show",
                "monitored": true,
                "alternateTitles": [
                    {"title": "Alternate Show", "sceneSeasonNumber": 2}
                ]
            }
        ]);
        let rows = parse_sonarr_titles(&payload, &regex()).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, 1000);
        assert_eq!(rows[0].season_number, Some(-1));
        assert_eq!(rows[0].monitored, 1);
        assert_eq!(rows[1].id, 1001);
        assert_eq!(rows[1].title, "Main Show");
        assert_eq!(rows[1].clean_title, "main show");
        assert_eq!(rows[2].id, 1002);
        assert_eq!(rows[2].title, "Alternate Show");
        assert_eq!(rows[2].season_number, Some(2));

        let pool = db::connect_memory().await.unwrap();
        let count = upsert_sonarr_titles(&pool, &rows).await.unwrap();
        assert_eq!(count, 3);
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sonarr_title")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 3);
    }

    #[tokio::test]
    async fn sonarr_null_scene_season_uses_default_and_preserves_existing() {
        let payload = serde_json::json!([
            {
                "id": 7,
                "tvdbId": 100,
                "title": "Main Show",
                "titleSlug": "main-show",
                "alternateTitles": [{"title": "Alternate Show"}]
            }
        ]);
        let rows = parse_sonarr_titles(&payload, &regex()).unwrap();
        assert_eq!(rows[2].season_number, None);

        let pool = db::connect_memory().await.unwrap();
        upsert_sonarr_titles(&pool, &rows).await.unwrap();
        let season: i64 =
            sqlx::query_scalar("SELECT season_number FROM sonarr_title WHERE id = 1002")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(season, 1);

        sqlx::query("UPDATE sonarr_title SET season_number = 4 WHERE id = 1002")
            .execute(&pool)
            .await
            .unwrap();
        upsert_sonarr_titles(&pool, &rows).await.unwrap();
        let season: i64 =
            sqlx::query_scalar("SELECT season_number FROM sonarr_title WHERE id = 1002")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(season, 4);
    }

    #[tokio::test]
    async fn radarr_payload_maps_folder_clean_and_original_titles() {
        let payload = serde_json::json!([
            {
                "id": 9,
                "tmdbId": 200,
                "title": "Main Movie",
                "originalTitle": "Original Movie",
                "cleanTitle": "mainmovie",
                "path": "/movies/Main Movie (2024)",
                "year": 2024,
                "monitored": false,
                "alternateTitles": [{"title": "Alt Movie"}]
            }
        ]);
        let rows = parse_radarr_titles(&payload, &regex()).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].id, 2000);
        assert_eq!(rows[1].title, "Main Movie");
        assert_eq!(rows[2].clean_title, "mainmovie");
        assert_eq!(rows[3].title, "Original Movie");
        assert_eq!(rows[4].title, "Alt Movie");

        let pool = db::connect_memory().await.unwrap();
        upsert_radarr_titles(&pool, &rows).await.unwrap();
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM radarr_title")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 5);
    }

    #[tokio::test]
    async fn rule_records_are_upserted() {
        let payload = serde_json::json!([
            {
                "id": "rule-1",
                "token": "title",
                "priority": 999,
                "regex": ".*{cleanTitle}.*",
                "replacement": "{mainTitle}",
                "validStatus": 1
            }
        ]);
        let records: Vec<RuleSyncRecord> = serde_json::from_value(payload).unwrap();
        let pool = db::connect_memory().await.unwrap();
        assert_eq!(
            upsert_rules(&pool, AppKind::Sonarr, &records)
                .await
                .unwrap(),
            1
        );
        let stored: String =
            sqlx::query_scalar("SELECT replacement FROM sonarr_rule WHERE id = 'rule-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, "{mainTitle}");
    }

    #[tokio::test]
    async fn remote_valid_status_one_preserves_local_disabled_rule() {
        let payload = serde_json::json!([
            {
                "id": "rule-1",
                "token": "title",
                "regex": ".*",
                "validStatus": 1
            }
        ]);
        let records: Vec<RuleSyncRecord> = serde_json::from_value(payload).unwrap();
        let pool = db::connect_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO sonarr_rule (id, token, priority, regex, valid_status) \
             VALUES ('rule-1', 'title', 10, 'old', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        upsert_rules(&pool, AppKind::Sonarr, &records)
            .await
            .unwrap();
        let stored: i64 =
            sqlx::query_scalar("SELECT valid_status FROM sonarr_rule WHERE id = 'rule-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, 0);

        let payload = serde_json::json!([
            {
                "id": "rule-1",
                "token": "title",
                "regex": ".*",
                "validStatus": 0
            }
        ]);
        let records: Vec<RuleSyncRecord> = serde_json::from_value(payload).unwrap();
        upsert_rules(&pool, AppKind::Sonarr, &records)
            .await
            .unwrap();
        let stored: i64 =
            sqlx::query_scalar("SELECT valid_status FROM sonarr_rule WHERE id = 'rule-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn missing_rule_fields_preserve_existing_values() {
        let pool = db::connect_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO sonarr_rule \
             (id, token, priority, regex, replacement, offset, example, remark, author, valid_status) \
             VALUES ('rule-1', 'title', 42, 'old', 'keep', 3, 'example', 'remark', 'author', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let payload = serde_json::json!([
            {"id": "rule-1", "token": "title", "regex": "new"}
        ]);
        let records: Vec<RuleSyncRecord> = serde_json::from_value(payload).unwrap();
        upsert_rules(&pool, AppKind::Sonarr, &records)
            .await
            .unwrap();
        let row = sqlx::query(
            "SELECT priority, replacement, offset, example, remark, author, regex \
             FROM sonarr_rule WHERE id = 'rule-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("priority"), 42);
        assert_eq!(row.get::<String, _>("replacement"), "keep");
        assert_eq!(row.get::<i64, _>("offset"), 3);
        assert_eq!(row.get::<String, _>("example"), "example");
        assert_eq!(row.get::<String, _>("remark"), "remark");
        assert_eq!(row.get::<String, _>("author"), "author");
        assert_eq!(row.get::<String, _>("regex"), "new");
    }

    #[test]
    fn author_names_reject_path_characters() {
        assert!(valid_author("LuckyPuppy514"));
        assert!(valid_author("grc"));
        assert!(!valid_author("../etc/passwd"));
        assert!(!valid_author("a/b"));
        assert!(!valid_author(""));
    }

    #[tokio::test]
    async fn title_gate_allows_one_attempt_per_interval() {
        let gate = Mutex::new(None);
        assert!(acquire_gate(&gate, Duration::from_secs(60)).await);
        assert!(!acquire_gate(&gate, Duration::from_secs(60)).await);
        *gate.lock().await = None;
        assert!(acquire_gate(&gate, Duration::from_secs(60)).await);
        assert!(!acquire_gate(&gate, Duration::ZERO).await);
    }
}
