use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::config::SystemConfigValues;
use crate::models::{AppKind, MatchedTitle, Rule, TitleCandidate};
use crate::text::{clean_title, remove_season};

const SCHEMA: &str = include_str!("../migrations/0001_schema.sql");
const ADMIN_SEED: &str = include_str!("../migrations/0002_admin_seed.sql");

pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .with_context(|| format!("open SQLite database {}", path.display()))?;
    ensure_schema(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
pub async fn connect_memory() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new().filename(":memory:");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .context("open in-memory SQLite database")?;
    ensure_schema(&pool).await?;
    Ok(pool)
}

pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(SCHEMA)
        .execute(pool)
        .await
        .context("initialize SQLite schema")?;
    // Matches the Java Liquibase first-run rows. The seed is keyed by
    // username/config key, so an imported upstream database is never
    // overwritten and its own user/settings are preserved.
    sqlx::raw_sql(ADMIN_SEED)
        .execute(pool)
        .await
        .context("initialize admin seed rows")?;
    Ok(())
}

pub async fn healthcheck(pool: &SqlitePool) -> Result<()> {
    let _: i64 = sqlx::query_scalar("SELECT 1").fetch_one(pool).await?;
    Ok(())
}

/// Load the whitelisted `system_config` rows.
///
/// Downloader credentials are selected only so the login tasks can use them.
/// Values are kept in memory and are not logged by the caller. `system_user`
/// rows are never selected.
pub async fn load_system_config_values(pool: &SqlitePool) -> Result<SystemConfigValues> {
    let keys = crate::config::Config::system_config_keys();
    let placeholders = vec!["?"; keys.len()].join(",");
    let sql = format!(
        "SELECT \"key\", value FROM system_config \
         WHERE valid_status = 1 AND \"key\" IN ({placeholders})"
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for key in keys {
        query = query.bind(*key);
    }
    let rows = query.fetch_all(pool).await?;
    let mut values = HashMap::new();
    for row in rows {
        let key: String = row.try_get("key")?;
        if let Some(value) = row.try_get::<Option<String>, _>("value")? {
            values.insert(key, value);
        }
    }
    Ok(SystemConfigValues::new(values))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdbTitleRow {
    pub tvdb_id: i64,
    pub tmdb_id: Option<i64>,
    pub language: String,
    pub title: String,
}

/// Java `SonarrTitleMapper.selectNeedSyncTmdbTitle`.
pub async fn list_tvdb_ids_needing_tmdb(pool: &SqlitePool) -> Result<Vec<i64>> {
    let rows = sqlx::query(
        "SELECT DISTINCT st.tvdb_id \
         FROM sonarr_title st LEFT JOIN tmdb_title tt ON st.tvdb_id = tt.tvdb_id \
         WHERE st.sno = 0 AND tt.tvdb_id IS NULL \
         ORDER BY st.tvdb_id",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get::<i64, _>("tvdb_id").map_err(Into::into))
        .collect()
}

pub async fn insert_tmdb_titles(pool: &SqlitePool, rows: &[TmdbTitleRow]) -> Result<usize> {
    let mut transaction = pool.begin().await?;
    for row in rows {
        sqlx::query(
            "INSERT INTO tmdb_title (tvdb_id, tmdb_id, language, title) VALUES (?, ?, ?, ?)",
        )
        .bind(row.tvdb_id)
        .bind(row.tmdb_id)
        .bind(&row.language)
        .bind(&row.title)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(rows.len())
}

pub async fn find_title(
    pool: &SqlitePool,
    kind: AppKind,
    query_title: &str,
    clean_regex: &Regex,
) -> Result<Option<MatchedTitle>> {
    let mut lookup = query_title.to_owned();
    let clean = clean_title(&lookup, clean_regex);
    let mut matched = query_title_by_clean(pool, kind, &clean).await?;

    if matched.is_none() && kind == AppKind::Sonarr {
        let without_season = remove_season(&lookup);
        if without_season != lookup {
            lookup = without_season;
            let clean = clean_title(&lookup, clean_regex);
            matched = query_title_by_clean(pool, kind, &clean).await?;
        }
    }

    Ok(matched.map(|mut row| {
        row.title = lookup;
        row
    }))
}

async fn query_title_by_clean(
    pool: &SqlitePool,
    kind: AppKind,
    clean: &str,
) -> Result<Option<MatchedTitle>> {
    match kind {
        AppKind::Sonarr => {
            let row = sqlx::query(
                "SELECT id, tvdb_id, sno, main_title, title, clean_title, season_number, monitored \
                 FROM sonarr_title WHERE clean_title = ? LIMIT 1",
            )
            .bind(clean)
            .fetch_optional(pool)
            .await?;
            row.map(|row| {
                Ok::<_, sqlx::Error>(MatchedTitle {
                    id: row.try_get("id")?,
                    tvdb_id: Some(row.try_get("tvdb_id")?),
                    tmdb_id: None,
                    sno: row.try_get("sno")?,
                    main_title: row.try_get("main_title")?,
                    title: row.try_get("title")?,
                    clean_title: row.try_get("clean_title")?,
                    season_number: row.try_get("season_number")?,
                    year: None,
                    monitored: row.try_get::<i64, _>("monitored")? != 0,
                })
            })
            .transpose()
            .map_err(Into::into)
        }
        AppKind::Radarr => {
            let row = sqlx::query(
                "SELECT id, tmdb_id, sno, main_title, title, clean_title, year, monitored \
                 FROM radarr_title WHERE clean_title = ? LIMIT 1",
            )
            .bind(clean)
            .fetch_optional(pool)
            .await?;
            row.map(|row| {
                Ok::<_, sqlx::Error>(MatchedTitle {
                    id: row.try_get("id")?,
                    tvdb_id: None,
                    tmdb_id: Some(row.try_get("tmdb_id")?),
                    sno: row.try_get("sno")?,
                    main_title: row.try_get("main_title")?,
                    title: row.try_get("title")?,
                    clean_title: row.try_get("clean_title")?,
                    season_number: -1,
                    year: Some(row.try_get("year")?),
                    monitored: row.try_get::<i64, _>("monitored")? != 0,
                })
            })
            .transpose()
            .map_err(Into::into)
        }
    }
}

pub async fn query_tmdb_titles(pool: &SqlitePool, tvdb_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT title FROM tmdb_title WHERE tvdb_id = ? GROUP BY title ORDER BY id ASC",
    )
    .bind(tvdb_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get::<String, _>("title").map_err(Into::into))
        .collect()
}

pub async fn load_rules(pool: &SqlitePool, kind: AppKind) -> Result<Vec<Rule>> {
    let sql = match kind {
        AppKind::Sonarr => {
            "SELECT id, token, priority, regex, replacement, offset FROM sonarr_rule \
             WHERE valid_status = 1 ORDER BY token, priority ASC, id ASC"
        }
        AppKind::Radarr => {
            "SELECT id, token, priority, regex, replacement, offset FROM radarr_rule \
             WHERE valid_status = 1 ORDER BY token, priority ASC, id ASC"
        }
    };
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| {
            Ok::<_, sqlx::Error>(Rule {
                id: row.try_get("id")?,
                token: row.try_get("token")?,
                priority: row.try_get("priority")?,
                regex: row.try_get("regex")?,
                replacement: row.try_get("replacement")?,
                offset: row.try_get("offset")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub async fn load_title_candidates(
    pool: &SqlitePool,
    kind: AppKind,
) -> Result<Vec<TitleCandidate>> {
    match kind {
        AppKind::Sonarr => load_sonarr_candidates(pool).await,
        AppKind::Radarr => load_radarr_candidates(pool).await,
    }
}

async fn load_sonarr_candidates(pool: &SqlitePool) -> Result<Vec<TitleCandidate>> {
    let mut rows = Vec::new();

    let sonarr_rows = sqlx::query(
        "SELECT main_title, title, clean_title, season_number, monitored \
         FROM sonarr_title GROUP BY clean_title",
    )
    .fetch_all(pool)
    .await?;
    for row in sonarr_rows {
        rows.push(TitleCandidate {
            main_title: row.try_get("main_title")?,
            title: row.try_get("title")?,
            clean_title: Some(row.try_get("clean_title")?),
            season_number: row.try_get("season_number")?,
            year: None,
            monitored: row.try_get::<i64, _>("monitored")? != 0,
        });
    }

    let tmdb_rows = sqlx::query(
        "SELECT st.main_title, tt.title, NULL AS clean_title, -1 AS season_number, st.monitored \
         FROM sonarr_title st LEFT JOIN tmdb_title tt ON st.tvdb_id = tt.tvdb_id \
         WHERE st.sno = 0 AND tt.title IS NOT NULL GROUP BY tt.title",
    )
    .fetch_all(pool)
    .await?;
    for row in tmdb_rows {
        rows.push(TitleCandidate {
            main_title: row.try_get("main_title")?,
            title: row.try_get("title")?,
            clean_title: None,
            season_number: -1,
            year: None,
            monitored: row.try_get::<i64, _>("monitored")? != 0,
        });
    }

    // Java's UNION removes identical tuples before the outer ordering.
    let mut seen = std::collections::HashSet::new();
    rows.retain(|candidate| {
        seen.insert((
            candidate.main_title.clone(),
            candidate.title.clone(),
            candidate.clean_title.clone(),
            candidate.season_number,
            candidate.monitored,
        ))
    });
    sort_candidates(&mut rows);
    Ok(rows)
}

async fn load_radarr_candidates(pool: &SqlitePool) -> Result<Vec<TitleCandidate>> {
    let rows = sqlx::query(
        "SELECT main_title, title, clean_title, monitored, year \
         FROM radarr_title GROUP BY clean_title",
    )
    .fetch_all(pool)
    .await?;
    let mut candidates = rows
        .into_iter()
        .map(|row| {
            Ok::<_, sqlx::Error>(TitleCandidate {
                main_title: row.try_get("main_title")?,
                title: row.try_get("title")?,
                clean_title: Some(row.try_get("clean_title")?),
                season_number: -1,
                year: Some(row.try_get("year")?),
                monitored: row.try_get::<i64, _>("monitored")? != 0,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    sort_candidates(&mut candidates);
    Ok(candidates)
}

fn sort_candidates(candidates: &mut [TitleCandidate]) {
    candidates.sort_by(|left, right| {
        right
            .monitored
            .cmp(&left.monitored)
            .then_with(|| right.title.chars().count().cmp(&left.title.chars().count()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex() -> Regex {
        Regex::new(crate::config::DEFAULT_CLEAN_TITLE_REGEX).unwrap()
    }

    #[tokio::test]
    async fn finds_title_and_uses_query_text_like_java() {
        let pool = connect_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO sonarr_title \
             (id, tvdb_id, sno, main_title, title, clean_title, season_number, monitored) \
             VALUES (1, 42, 0, 'Main Show', 'Main Show', 'main show', -1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let matched = find_title(&pool, AppKind::Sonarr, "Main Show S02", &regex())
            .await
            .unwrap()
            .expect("title should match after removeSeason");
        assert_eq!(matched.title, "Main Show");
        assert_eq!(matched.main_title, "Main Show");
    }

    #[tokio::test]
    async fn rules_are_loaded_in_priority_order() {
        let pool = connect_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO sonarr_rule (id, token, priority, regex, replacement, valid_status) \
             VALUES ('b', 'title', 20, 'x', 'second', 1), \
                    ('a', 'title', 10, 'x', 'first', 1), \
                    ('c', 'title', 5, 'x', 'disabled', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let rules = load_rules(&pool, AppKind::Sonarr).await.unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].id, "a");
        assert_eq!(rules[1].id, "b");
    }

    #[tokio::test]
    async fn system_config_loader_reads_only_whitelisted_keys() {
        let pool = connect_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO system_config (\"key\", value, valid_status) VALUES \
             ('sonarrUrl', 'http://synthetic.invalid', 1), \
             ('sonarrApikey', 'synthetic-key', 1), \
             ('ruleSyncAuthors', 'alice', 1), \
             ('qbittorrentPassword', 'synthetic-downloader-password', 1), \
             ('unlistedKey', 'must-not-be-read', 1), \
             ('tmdbApikey', 'ignored-invalid-row', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let values = load_system_config_values(&pool).await.unwrap();
        assert_eq!(values.get("sonarrUrl"), Some("http://synthetic.invalid"));
        assert_eq!(values.get("sonarrApikey"), Some("synthetic-key"));
        assert_eq!(values.get("ruleSyncAuthors"), Some("alice"));
        assert_eq!(
            values.get("qbittorrentPassword"),
            Some("synthetic-downloader-password")
        );
        assert_eq!(values.get("unlistedKey"), None);
        assert_eq!(values.get("tmdbApikey"), None);
    }

    #[tokio::test]
    async fn tmdb_need_list_ignores_rows_with_any_translation() {
        let pool = connect_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO sonarr_title \
             (id, tvdb_id, sno, main_title, title, clean_title, season_number, monitored) \
             VALUES (10, 1, 0, 'One', 'One', 'one', -1, 1), \
                    (11, 1, 1, 'One', 'One', 'one', -1, 1), \
                    (20, 2, 0, 'Two', 'Two', 'two', -1, 1); \
             INSERT INTO tmdb_title (tvdb_id, language, title) VALUES (2, 'zh-CN', '二')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let ids = list_tvdb_ids_needing_tmdb(&pool).await.unwrap();
        assert_eq!(ids, vec![1]);
    }
}
