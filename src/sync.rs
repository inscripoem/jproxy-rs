use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::app::AppState;
use crate::models::AppKind;
use crate::text::clean_title;

static RADARR_FOLDER_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" \(\d{4}\)$").expect("static regex"));

const SONARR_TITLE_UPSERT: &str = "\
INSERT INTO sonarr_title \
(id, tvdb_id, series_id, sno, main_title, title, clean_title, season_number, monitored, valid_status, update_time) \
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, DATETIME(CURRENT_TIMESTAMP, 'localtime')) \
ON CONFLICT(id) DO UPDATE SET \
tvdb_id = excluded.tvdb_id, series_id = excluded.series_id, sno = excluded.sno, \
main_title = excluded.main_title, title = excluded.title, clean_title = excluded.clean_title, \
season_number = excluded.season_number, monitored = excluded.monitored, \
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

#[derive(Debug, Clone)]
struct SonarrTitleRow {
    id: i64,
    tvdb_id: i64,
    series_id: Option<i64>,
    sno: i64,
    main_title: String,
    title: String,
    clean_title: String,
    season_number: i64,
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
    #[serde(default = "default_priority")]
    priority: i64,
    regex: String,
    #[serde(default)]
    replacement: String,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    example: String,
    #[serde(default)]
    remark: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default = "default_valid_status", alias = "validStatus")]
    valid_status: i64,
}

fn default_priority() -> i64 {
    1000
}

fn default_valid_status() -> i64 {
    1
}

/// Start the periodic title/rule synchronization loop.
///
/// The task is disabled by `SYNC_ENABLED=false`, a zero `SYNC_INTERVAL`, or a
/// missing Sonarr/Radarr URL or API key. The first tick runs immediately.
pub fn spawn(state: AppState) {
    if !state.config.sync_enabled {
        info!("background sync disabled");
        return;
    }
    let interval = state.config.sync_interval;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(error) = sync_once(&state).await {
                warn!(%error, "background sync cycle failed");
            }
        }
    });
}

async fn sync_once(state: &AppState) -> Result<()> {
    let mut reference_data_changed = false;

    for kind in [AppKind::Sonarr, AppKind::Radarr] {
        if state.config.sync_enabled {
            match sync_titles(state, kind).await {
                Ok(Some(count)) => {
                    reference_data_changed = true;
                    info!(%kind, count, "synced titles");
                }
                Ok(None) => {}
                Err(error) => warn!(%kind, %error, "title sync failed"),
            }
            match sync_rules(state, kind).await {
                Ok(Some(count)) => {
                    reference_data_changed = true;
                    info!(%kind, count, "synced rules");
                }
                Ok(None) => {}
                Err(error) => warn!(%kind, %error, "rule sync failed"),
            }
        }
    }

    if reference_data_changed {
        state.formatting_cache.clear().await;
    }
    Ok(())
}

async fn sync_titles(state: &AppState, kind: AppKind) -> Result<Option<usize>> {
    let (base, api_key) = match kind {
        AppKind::Sonarr => (
            state.config.sonarr_url.as_deref(),
            state.config.sonarr_api_key.as_deref(),
        ),
        AppKind::Radarr => (
            state.config.radarr_url.as_deref(),
            state.config.radarr_api_key.as_deref(),
        ),
    };
    let (Some(base), Some(api_key)) = (base, api_key) else {
        return Ok(None);
    };

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

    match kind {
        AppKind::Sonarr => {
            let rows = parse_sonarr_titles(&payload, &state.clean_title_regex)?;
            let count = upsert_sonarr_titles(&state.pool, &rows).await?;
            Ok(Some(count))
        }
        AppKind::Radarr => {
            let rows = parse_radarr_titles(&payload, &state.clean_title_regex)?;
            let count = upsert_radarr_titles(&state.pool, &rows).await?;
            Ok(Some(count))
        }
    }
}

async fn sync_rules(state: &AppState, kind: AppKind) -> Result<Option<usize>> {
    let source = match kind {
        AppKind::Sonarr => state.config.sonarr_rules_url.as_deref(),
        AppKind::Radarr => state.config.radarr_rules_url.as_deref(),
    };
    let Some(source) = source else {
        return Ok(None);
    };
    let response = state
        .upstream
        .http_client()
        .get(source)
        .send()
        .await
        .map_err(redact_reqwest)?;
    let status = response.status();
    if !status.is_success() {
        bail!("rule source returned HTTP {status}");
    }
    let payload = response.json::<Value>().await.map_err(redact_reqwest)?;
    let records: Vec<RuleSyncRecord> =
        serde_json::from_value(payload).with_context(|| format!("parse {kind} rule JSON"))?;
    let count = upsert_rules(&state.pool, kind, &records).await?;
    Ok(Some(count))
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
            season_number: -1,
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
                season_number: -1,
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
                    season_number: alternate
                        .get("sceneSeasonNumber")
                        .and_then(Value::as_i64)
                        .unwrap_or(1),
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
    let sql = match kind {
        AppKind::Sonarr => {
            "INSERT INTO sonarr_rule \
             (id, token, priority, regex, replacement, offset, example, remark, author, valid_status, update_time) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, DATETIME(CURRENT_TIMESTAMP, 'localtime')) \
             ON CONFLICT(id) DO UPDATE SET token = excluded.token, priority = excluded.priority, \
             regex = excluded.regex, replacement = excluded.replacement, offset = excluded.offset, \
             example = excluded.example, remark = excluded.remark, author = excluded.author, \
             valid_status = CASE WHEN excluded.valid_status = 1 THEN valid_status ELSE excluded.valid_status END, \
             update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime')"
        }
        AppKind::Radarr => {
            "INSERT INTO radarr_rule \
             (id, token, priority, regex, replacement, offset, example, remark, author, valid_status, update_time) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, DATETIME(CURRENT_TIMESTAMP, 'localtime')) \
             ON CONFLICT(id) DO UPDATE SET token = excluded.token, priority = excluded.priority, \
             regex = excluded.regex, replacement = excluded.replacement, offset = excluded.offset, \
             example = excluded.example, remark = excluded.remark, author = excluded.author, \
             valid_status = CASE WHEN excluded.valid_status = 1 THEN valid_status ELSE excluded.valid_status END, \
             update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime')"
        }
    };
    let mut transaction = pool.begin().await?;
    for record in records {
        sqlx::query(sql)
            .bind(&record.id)
            .bind(&record.token)
            .bind(record.priority)
            .bind(&record.regex)
            .bind(&record.replacement)
            .bind(record.offset)
            .bind(&record.example)
            .bind(&record.remark)
            .bind(&record.author)
            .bind(record.valid_status)
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
        assert_eq!(rows[0].season_number, -1);
        assert_eq!(rows[0].monitored, 1);
        assert_eq!(rows[1].id, 1001);
        assert_eq!(rows[1].clean_title, "main show");
        assert_eq!(rows[2].id, 1002);
        assert_eq!(rows[2].title, "Alternate Show");
        assert_eq!(rows[2].season_number, 2);

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
}
