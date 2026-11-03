//! SQLite access for the admin API.
//!
//! These queries mirror the MyBatis-Plus behaviour in the Java service:
//! `update-strategy: NOT_NULL` (missing JSON fields preserve stored values),
//! `ORDER BY update_time DESC`, and page sizes capped at 1000.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::sqlite::SqliteConnection;
use sqlx::{Row, SqlitePool};

use crate::models::AppKind;

const MAX_PAGE_SIZE: i64 = 1000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub current: i64,
    pub page_size: i64,
    pub total: i64,
    pub list: Vec<T>,
}

fn default_current() -> i64 {
    1
}

fn default_page_size() -> i64 {
    10
}

/// Java/Spring binds an empty query parameter to `null` for `Integer` fields.
/// Accept the same shape so an unset UI filter does not return 400.
pub(crate) fn empty_string_as_none_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => value
            .parse::<i64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn page_bounds(current: i64, page_size: i64) -> (i64, i64, i64) {
    let current = current.max(1);
    let page_size = page_size.clamp(1, MAX_PAGE_SIZE);
    let offset = (current - 1).saturating_mul(page_size);
    (current, page_size, offset)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleQuery {
    #[serde(default = "default_current")]
    pub current: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub remark: Option<String>,
}

impl RuleQuery {
    pub fn bounds(&self) -> (i64, i64, i64) {
        page_bounds(self.current, self.page_size)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SonarrTitleQuery {
    #[serde(default = "default_current")]
    pub current: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none_i64")]
    pub tvdb_id: Option<i64>,
}

impl SonarrTitleQuery {
    pub fn bounds(&self) -> (i64, i64, i64) {
        page_bounds(self.current, self.page_size)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrTitleQuery {
    #[serde(default = "default_current")]
    pub current: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none_i64")]
    pub tmdb_id: Option<i64>,
}

impl RadarrTitleQuery {
    pub fn bounds(&self) -> (i64, i64, i64) {
        page_bounds(self.current, self.page_size)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExampleQuery {
    #[serde(default = "default_current")]
    pub current: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    #[serde(default)]
    pub original_text: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none_i64")]
    pub valid_status: Option<i64>,
}

impl ExampleQuery {
    pub fn bounds(&self) -> (i64, i64, i64) {
        page_bounds(self.current, self.page_size)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmdbTitleQuery {
    #[serde(default = "default_current")]
    pub current: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none_i64")]
    pub tvdb_id: Option<i64>,
}

impl TmdbTitleQuery {
    pub fn bounds(&self) -> (i64, i64, i64) {
        page_bounds(self.current, self.page_size)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuleRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExampleRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SonarrTitleRecord {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_id: Option<i64>,
    pub tvdb_id: i64,
    pub sno: i64,
    pub main_title: String,
    pub title: String,
    pub clean_title: String,
    pub season_number: i64,
    pub monitored: i64,
    pub valid_status: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RadarrTitleRecord {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movie_id: Option<i64>,
    pub tmdb_id: i64,
    pub sno: i64,
    pub main_title: String,
    pub title: String,
    pub clean_title: String,
    pub year: i64,
    pub monitored: i64,
    pub valid_status: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TmdbTitleRecord {
    pub id: i64,
    pub tvdb_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmdb_id: Option<i64>,
    pub language: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean_title: Option<String>,
    pub valid_status: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TmdbTitleSave {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub tvdb_id: Option<i64>,
    #[serde(default)]
    pub tmdb_id: Option<i64>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub valid_status: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SystemConfigRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: i64,
    pub username: String,
    pub password: Option<String>,
    pub role: Option<String>,
    pub valid_status: i64,
}

fn table(kind: AppKind, suffix: &str) -> &'static str {
    match kind {
        AppKind::Sonarr => match suffix {
            "rule" => "sonarr_rule",
            "title" => "sonarr_title",
            "example" => "sonarr_example",
            _ => unreachable!("unknown Sonarr admin table"),
        },
        AppKind::Radarr => match suffix {
            "rule" => "radarr_rule",
            "title" => "radarr_title",
            "example" => "radarr_example",
            _ => unreachable!("unknown Radarr admin table"),
        },
    }
}

fn like(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"))
}

fn rule_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RuleRecord> {
    Ok(RuleRecord {
        id: row.try_get("id")?,
        token: row.try_get("token")?,
        priority: row.try_get("priority")?,
        regex: row.try_get("regex")?,
        replacement: row.try_get("replacement")?,
        offset: row.try_get("offset")?,
        example: row.try_get("example")?,
        remark: row.try_get("remark")?,
        author: row.try_get("author")?,
        valid_status: row.try_get("valid_status")?,
        create_time: row.try_get("create_time")?,
        update_time: row.try_get("update_time")?,
    })
}

fn rule_export_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RuleRecord> {
    Ok(RuleRecord {
        id: row.try_get("id")?,
        token: row.try_get("token")?,
        priority: row.try_get("priority")?,
        regex: row.try_get("regex")?,
        replacement: row.try_get("replacement")?,
        offset: row.try_get("offset")?,
        example: row.try_get("example")?,
        remark: row.try_get("remark")?,
        author: row.try_get("author")?,
        valid_status: None,
        create_time: None,
        update_time: None,
    })
}

pub async fn query_rules(
    pool: &SqlitePool,
    kind: AppKind,
    query: &RuleQuery,
) -> Result<Page<RuleRecord>> {
    let table = table(kind, "rule");
    let token = like(&query.token);
    let remark = like(&query.remark);

    let mut where_sql = String::from(" WHERE 1 = 1");
    if token.is_some() {
        where_sql.push_str(" AND token LIKE ?");
    }
    if remark.is_some() {
        where_sql.push_str(" AND remark LIKE ?");
    }

    let count_sql = format!("SELECT COUNT(*) AS count FROM {table}{where_sql}");
    let mut count_query = sqlx::query(sqlx::AssertSqlSafe(count_sql));
    if let Some(token) = &token {
        count_query = count_query.bind(token);
    }
    if let Some(remark) = &remark {
        count_query = count_query.bind(remark);
    }
    let total: i64 = count_query.fetch_one(pool).await?.try_get("count")?;

    let (mut current, page_size, mut offset) = query.bounds();
    // MyBatis-Plus `overflow: true` restarts at page one when the requested
    // page is past the end.
    if total > 0 && offset >= total {
        current = 1;
        offset = 0;
    }

    let list_sql = format!(
        "SELECT id, token, priority, regex, replacement, offset, example, remark, author, \
         valid_status, create_time, update_time FROM {table}{where_sql} \
         ORDER BY update_time DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query(sqlx::AssertSqlSafe(list_sql));
    if let Some(token) = &token {
        list_query = list_query.bind(token);
    }
    if let Some(remark) = &remark {
        list_query = list_query.bind(remark);
    }
    let rows = list_query
        .bind(page_size)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let list = rows
        .iter()
        .map(rule_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Page {
        current,
        page_size,
        total,
        list,
    })
}

pub async fn query_rules_all(pool: &SqlitePool, kind: AppKind) -> Result<Vec<RuleRecord>> {
    let table = table(kind, "rule");
    let sql = format!(
        "SELECT id, token, priority, regex, replacement, offset, example, remark, author \
         FROM {table} ORDER BY update_time DESC"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?;
    rows.iter().map(rule_export_from_row).collect()
}

pub async fn query_rules_by_ids(
    pool: &SqlitePool,
    kind: AppKind,
    ids: &[String],
) -> Result<Vec<RuleRecord>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let table = table(kind, "rule");
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, token, priority, regex, replacement, offset, example, remark, author, \
         valid_status, create_time, update_time FROM {table} WHERE id IN ({placeholders})"
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in ids {
        query = query.bind(id);
    }
    let rows = query.fetch_all(pool).await?;
    rows.iter().map(rule_from_row).collect()
}

pub async fn rule_tokens(pool: &SqlitePool, kind: AppKind) -> Result<Vec<String>> {
    let table = table(kind, "rule");
    let sql = format!("SELECT token FROM {table} GROUP BY token");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|row| row.try_get::<String, _>("token").map_err(Into::into))
        .collect()
}

async fn rule_exists(connection: &mut SqliteConnection, table: &str, id: &str) -> Result<bool> {
    let sql = format!("SELECT 1 FROM {table} WHERE id = ?");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *connection)
        .await?;
    Ok(row.is_some())
}

async fn upsert_rule_on(
    connection: &mut SqliteConnection,
    table: &str,
    record: &RuleRecord,
) -> Result<()> {
    let id = record
        .id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .context("rule id is required for persistence")?;
    if rule_exists(connection, table, &id).await? {
        let sql = format!(
            "UPDATE {table} SET token = COALESCE(?, token), priority = COALESCE(?, priority), \
             regex = COALESCE(?, regex), replacement = COALESCE(?, replacement), \
             offset = COALESCE(?, offset), example = COALESCE(?, example), \
             remark = COALESCE(?, remark), author = COALESCE(?, author), \
             valid_status = COALESCE(?, valid_status), \
             update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime') WHERE id = ?"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&record.token)
            .bind(record.priority)
            .bind(&record.regex)
            .bind(&record.replacement)
            .bind(record.offset)
            .bind(&record.example)
            .bind(&record.remark)
            .bind(&record.author)
            .bind(record.valid_status)
            .bind(&id)
            .execute(&mut *connection)
            .await?;
    } else {
        let sql = format!(
            "INSERT INTO {table} \
             (id, token, priority, regex, replacement, offset, example, remark, author, valid_status) \
             VALUES (?, ?, COALESCE(?, 1000), ?, COALESCE(?, ''), COALESCE(?, 0), \
             COALESCE(?, ''), ?, ?, COALESCE(?, 1))"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&id)
            .bind(&record.token)
            .bind(record.priority)
            .bind(&record.regex)
            .bind(&record.replacement)
            .bind(record.offset)
            .bind(&record.example)
            .bind(&record.remark)
            .bind(&record.author)
            .bind(record.valid_status)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

pub async fn upsert_rule(pool: &SqlitePool, kind: AppKind, record: &RuleRecord) -> Result<()> {
    let table = table(kind, "rule");
    let mut connection = pool.acquire().await?;
    upsert_rule_on(&mut connection, table, record).await
}

pub async fn upsert_rules(pool: &SqlitePool, kind: AppKind, records: &[RuleRecord]) -> Result<()> {
    let table = table(kind, "rule");
    let mut transaction = pool.begin().await?;
    for record in records {
        upsert_rule_on(&mut transaction, table, record).await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn delete_rules(pool: &SqlitePool, kind: AppKind, ids: &[String]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let table = table(kind, "rule");
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("DELETE FROM {table} WHERE id IN ({placeholders})");
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in ids {
        query = query.bind(id);
    }
    Ok(query.execute(pool).await?.rows_affected())
}

pub async fn set_rules_status(
    pool: &SqlitePool,
    kind: AppKind,
    ids: &[String],
    status: i64,
) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let table = table(kind, "rule");
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "UPDATE {table} SET valid_status = ?, \
         update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime') WHERE id IN ({placeholders})"
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(status);
    for id in ids {
        query = query.bind(id);
    }
    Ok(query.execute(pool).await?.rows_affected())
}

fn sonarr_title_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<SonarrTitleRecord> {
    Ok(SonarrTitleRecord {
        id: row.try_get("id")?,
        series_id: row.try_get("series_id")?,
        tvdb_id: row.try_get("tvdb_id")?,
        sno: row.try_get("sno")?,
        main_title: row.try_get("main_title")?,
        title: row.try_get("title")?,
        clean_title: row.try_get("clean_title")?,
        season_number: row.try_get("season_number")?,
        monitored: row.try_get("monitored")?,
        valid_status: row.try_get("valid_status")?,
        create_time: row.try_get("create_time")?,
        update_time: row.try_get("update_time")?,
    })
}

fn radarr_title_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RadarrTitleRecord> {
    Ok(RadarrTitleRecord {
        id: row.try_get("id")?,
        movie_id: row.try_get("movie_id")?,
        tmdb_id: row.try_get("tmdb_id")?,
        sno: row.try_get("sno")?,
        main_title: row.try_get("main_title")?,
        title: row.try_get("title")?,
        clean_title: row.try_get("clean_title")?,
        year: row.try_get("year")?,
        monitored: row.try_get("monitored")?,
        valid_status: row.try_get("valid_status")?,
        create_time: row.try_get("create_time")?,
        update_time: row.try_get("update_time")?,
    })
}

pub async fn query_sonarr_titles(
    pool: &SqlitePool,
    query: &SonarrTitleQuery,
) -> Result<Page<SonarrTitleRecord>> {
    let title = like(&query.title);
    let mut where_sql = String::from(" WHERE 1 = 1");
    if title.is_some() {
        where_sql.push_str(" AND title LIKE ?");
    }
    if query.tvdb_id.is_some() {
        where_sql.push_str(" AND tvdb_id = ?");
    }

    let count_sql = format!("SELECT COUNT(*) AS count FROM sonarr_title{where_sql}");
    let mut count_query = sqlx::query(sqlx::AssertSqlSafe(count_sql));
    if let Some(title) = &title {
        count_query = count_query.bind(title);
    }
    if let Some(tvdb_id) = query.tvdb_id {
        count_query = count_query.bind(tvdb_id);
    }
    let total: i64 = count_query.fetch_one(pool).await?.try_get("count")?;

    let (mut current, page_size, mut offset) = query.bounds();
    if total > 0 && offset >= total {
        current = 1;
        offset = 0;
    }

    let list_sql = format!(
        "SELECT id, series_id, tvdb_id, sno, main_title, title, clean_title, season_number, \
         monitored, valid_status, create_time, update_time FROM sonarr_title{where_sql} \
         ORDER BY update_time DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query(sqlx::AssertSqlSafe(list_sql));
    if let Some(title) = &title {
        list_query = list_query.bind(title);
    }
    if let Some(tvdb_id) = query.tvdb_id {
        list_query = list_query.bind(tvdb_id);
    }
    let rows = list_query
        .bind(page_size)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let list = rows
        .iter()
        .map(sonarr_title_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page {
        current,
        page_size,
        total,
        list,
    })
}

pub async fn query_radarr_titles(
    pool: &SqlitePool,
    query: &RadarrTitleQuery,
) -> Result<Page<RadarrTitleRecord>> {
    let title = like(&query.title);
    let mut where_sql = String::from(" WHERE 1 = 1");
    if title.is_some() {
        where_sql.push_str(" AND title LIKE ?");
    }
    if query.tmdb_id.is_some() {
        where_sql.push_str(" AND tmdb_id = ?");
    }

    let count_sql = format!("SELECT COUNT(*) AS count FROM radarr_title{where_sql}");
    let mut count_query = sqlx::query(sqlx::AssertSqlSafe(count_sql));
    if let Some(title) = &title {
        count_query = count_query.bind(title);
    }
    if let Some(tmdb_id) = query.tmdb_id {
        count_query = count_query.bind(tmdb_id);
    }
    let total: i64 = count_query.fetch_one(pool).await?.try_get("count")?;

    let (mut current, page_size, mut offset) = query.bounds();
    if total > 0 && offset >= total {
        current = 1;
        offset = 0;
    }

    let list_sql = format!(
        "SELECT id, movie_id, tmdb_id, sno, main_title, title, clean_title, year, monitored, \
         valid_status, create_time, update_time FROM radarr_title{where_sql} \
         ORDER BY update_time DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query(sqlx::AssertSqlSafe(list_sql));
    if let Some(title) = &title {
        list_query = list_query.bind(title);
    }
    if let Some(tmdb_id) = query.tmdb_id {
        list_query = list_query.bind(tmdb_id);
    }
    let rows = list_query
        .bind(page_size)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let list = rows
        .iter()
        .map(radarr_title_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page {
        current,
        page_size,
        total,
        list,
    })
}

pub async fn delete_titles(pool: &SqlitePool, kind: AppKind, ids: &[i64]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let table = table(kind, "title");
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("DELETE FROM {table} WHERE id IN ({placeholders})");
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in ids {
        query = query.bind(id);
    }
    Ok(query.execute(pool).await?.rows_affected())
}

fn example_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ExampleRecord> {
    Ok(ExampleRecord {
        hash: row.try_get("hash")?,
        original_text: row.try_get("original_text")?,
        format_text: row.try_get("format_text")?,
        valid_status: row.try_get("valid_status")?,
        create_time: row.try_get("create_time")?,
        update_time: row.try_get("update_time")?,
    })
}

pub async fn query_examples_page(
    pool: &SqlitePool,
    kind: AppKind,
    original_text: &Option<String>,
    mut offset: i64,
    limit: i64,
) -> Result<(i64, i64, Vec<ExampleRecord>)> {
    let table = table(kind, "example");
    let original = like(original_text);
    let where_sql = if original.is_some() {
        " WHERE original_text LIKE ?"
    } else {
        ""
    };

    let count_sql = format!("SELECT COUNT(*) AS count FROM {table}{where_sql}");
    let mut count_query = sqlx::query(sqlx::AssertSqlSafe(count_sql));
    if let Some(original) = &original {
        count_query = count_query.bind(original);
    }
    let total: i64 = count_query.fetch_one(pool).await?.try_get("count")?;

    // MyBatis-Plus runs with `overflow: true`: a page past the end restarts at
    // page one, including its offset.
    if total > 0 && offset >= total {
        offset = 0;
    }

    let list_sql = format!(
        "SELECT hash, original_text, format_text, valid_status, create_time, update_time \
         FROM {table}{where_sql} ORDER BY update_time DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query(sqlx::AssertSqlSafe(list_sql));
    if let Some(original) = &original {
        list_query = list_query.bind(original);
    }
    let rows = list_query.bind(limit).bind(offset).fetch_all(pool).await?;
    let list = rows
        .iter()
        .map(example_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((total, offset, list))
}

pub async fn query_examples_all(
    pool: &SqlitePool,
    kind: AppKind,
    original_text: &Option<String>,
) -> Result<Vec<ExampleRecord>> {
    let table = table(kind, "example");
    let original = like(original_text);
    let where_sql = if original.is_some() {
        " WHERE original_text LIKE ?"
    } else {
        ""
    };
    let sql = format!(
        "SELECT hash, original_text, format_text, valid_status, create_time, update_time \
         FROM {table}{where_sql} ORDER BY update_time DESC"
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    if let Some(original) = &original {
        query = query.bind(original);
    }
    let rows = query.fetch_all(pool).await?;
    rows.iter().map(example_from_row).collect()
}

pub async fn upsert_examples(
    pool: &SqlitePool,
    kind: AppKind,
    examples: &[(String, String)],
) -> Result<()> {
    let table = table(kind, "example");
    let sql = format!(
        "INSERT INTO {table} (hash, original_text, valid_status) VALUES (?, ?, 1) \
         ON CONFLICT(hash) DO UPDATE SET original_text = excluded.original_text, \
         update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime')"
    );
    let mut transaction = pool.begin().await?;
    for (hash, original_text) in examples {
        sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
            .bind(hash)
            .bind(original_text)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn delete_examples(pool: &SqlitePool, kind: AppKind, hashes: &[String]) -> Result<u64> {
    if hashes.is_empty() {
        return Ok(0);
    }
    let table = table(kind, "example");
    let placeholders = vec!["?"; hashes.len()].join(",");
    let sql = format!("DELETE FROM {table} WHERE hash IN ({placeholders})");
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for hash in hashes {
        query = query.bind(hash);
    }
    Ok(query.execute(pool).await?.rows_affected())
}

fn tmdb_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<TmdbTitleRecord> {
    Ok(TmdbTitleRecord {
        id: row.try_get("id")?,
        tvdb_id: row.try_get("tvdb_id")?,
        tmdb_id: row.try_get("tmdb_id")?,
        language: row.try_get("language")?,
        title: row.try_get("title")?,
        clean_title: None,
        valid_status: row.try_get("valid_status")?,
        create_time: row.try_get("create_time")?,
        update_time: row.try_get("update_time")?,
    })
}

pub async fn query_tmdb_titles(
    pool: &SqlitePool,
    query: &TmdbTitleQuery,
) -> Result<Page<TmdbTitleRecord>> {
    let title = like(&query.title);
    let mut where_sql = String::from(" WHERE 1 = 1");
    if title.is_some() {
        where_sql.push_str(" AND title LIKE ?");
    }
    if query.tvdb_id.is_some() {
        where_sql.push_str(" AND tvdb_id = ?");
    }

    let count_sql = format!("SELECT COUNT(*) AS count FROM tmdb_title{where_sql}");
    let mut count_query = sqlx::query(sqlx::AssertSqlSafe(count_sql));
    if let Some(title) = &title {
        count_query = count_query.bind(title);
    }
    if let Some(tvdb_id) = query.tvdb_id {
        count_query = count_query.bind(tvdb_id);
    }
    let total: i64 = count_query.fetch_one(pool).await?.try_get("count")?;

    let (mut current, page_size, mut offset) = query.bounds();
    if total > 0 && offset >= total {
        current = 1;
        offset = 0;
    }

    let list_sql = format!(
        "SELECT id, tvdb_id, tmdb_id, language, title, valid_status, create_time, update_time \
         FROM tmdb_title{where_sql} ORDER BY update_time DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query(sqlx::AssertSqlSafe(list_sql));
    if let Some(title) = &title {
        list_query = list_query.bind(title);
    }
    if let Some(tvdb_id) = query.tvdb_id {
        list_query = list_query.bind(tvdb_id);
    }
    let rows = list_query
        .bind(page_size)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let list = rows
        .iter()
        .map(tmdb_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page {
        current,
        page_size,
        total,
        list,
    })
}

pub async fn first_tmdb_id_by_tvdb(pool: &SqlitePool, tvdb_id: i64) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT tmdb_id FROM tmdb_title WHERE tvdb_id = ? ORDER BY id LIMIT 1",
    )
    .bind(tvdb_id)
    .fetch_optional(pool)
    .await?
    .flatten())
}

pub async fn save_tmdb_title(pool: &SqlitePool, title: &TmdbTitleSave) -> Result<()> {
    if let Some(id) = title.id {
        let sql = "UPDATE tmdb_title SET tvdb_id = COALESCE(?, tvdb_id), \
                   tmdb_id = COALESCE(?, tmdb_id), language = COALESCE(?, language), \
                   title = COALESCE(?, title), valid_status = COALESCE(?, valid_status), \
                   update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime') WHERE id = ?";
        sqlx::query(sql)
            .bind(title.tvdb_id)
            .bind(title.tmdb_id)
            .bind(&title.language)
            .bind(&title.title)
            .bind(title.valid_status)
            .bind(id)
            .execute(pool)
            .await?;
    } else {
        let tvdb_id = title.tvdb_id.context("tvdbId is required")?;
        let language = title.language.clone().context("language is required")?;
        let title_text = title.title.clone().context("title is required")?;
        sqlx::query(
            "INSERT INTO tmdb_title (tvdb_id, tmdb_id, language, title, valid_status) \
             VALUES (?, ?, ?, ?, COALESCE(?, 1))",
        )
        .bind(tvdb_id)
        .bind(title.tmdb_id)
        .bind(language)
        .bind(title_text)
        .bind(title.valid_status)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn delete_tmdb_titles(pool: &SqlitePool, ids: &[i64]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("DELETE FROM tmdb_title WHERE id IN ({placeholders})");
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in ids {
        query = query.bind(id);
    }
    Ok(query.execute(pool).await?.rows_affected())
}

pub async fn query_system_config(pool: &SqlitePool) -> Result<Vec<SystemConfigRecord>> {
    let rows = sqlx::query(
        "SELECT id, \"key\", value, valid_status, create_time, update_time \
         FROM system_config ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(SystemConfigRecord {
                id: row.try_get("id")?,
                key: row.try_get("key")?,
                value: row.try_get("value")?,
                valid_status: row.try_get("valid_status")?,
                create_time: row.try_get("create_time")?,
                update_time: row.try_get("update_time")?,
            })
        })
        .collect()
}

pub async fn update_system_config(pool: &SqlitePool, configs: &[SystemConfigRecord]) -> Result<()> {
    let mut transaction = pool.begin().await?;
    for config in configs {
        if let Some(id) = config.id {
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM system_config WHERE id = ?")
                .bind(id)
                .fetch_one(&mut *transaction)
                .await?;
            if exists == 0 {
                // MyBatis-Plus `saveOrUpdateBatch` inserts when `selectById`
                // finds no row, even when the entity already carries an id.
                let key = config
                    .key
                    .as_deref()
                    .context("system config key is required")?;
                sqlx::query(
                    "INSERT INTO system_config (id, \"key\", value, valid_status) \
                     VALUES (?, ?, ?, COALESCE(?, 1))",
                )
                .bind(id)
                .bind(key)
                .bind(&config.value)
                .bind(config.valid_status)
                .execute(&mut *transaction)
                .await?;
                continue;
            }
            sqlx::query(
                "UPDATE system_config SET \"key\" = COALESCE(?, \"key\"), \
                 value = COALESCE(?, value), valid_status = COALESCE(?, valid_status), \
                 update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime') WHERE id = ?",
            )
            .bind(&config.key)
            .bind(&config.value)
            .bind(config.valid_status)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        } else {
            let key = config
                .key
                .as_deref()
                .context("system config key is required")?;
            sqlx::query(
                "INSERT INTO system_config (\"key\", value, valid_status) \
                 VALUES (?, ?, COALESCE(?, 1))",
            )
            .bind(key)
            .bind(&config.value)
            .bind(config.valid_status)
            .execute(&mut *transaction)
            .await?;
        }
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn find_user_by_username(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<UserRecord>> {
    let row = sqlx::query(
        "SELECT id, username, password, role, valid_status FROM system_user \
         WHERE username = ? ORDER BY id LIMIT 1",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok::<_, sqlx::Error>(UserRecord {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            password: row.try_get("password")?,
            role: row.try_get("role")?,
            valid_status: row.try_get("valid_status")?,
        })
    })
    .transpose()
    .map_err(Into::into)
}

pub async fn update_user(
    pool: &SqlitePool,
    id: i64,
    username: &str,
    password_hash: &str,
    role: Option<&str>,
    valid_status: Option<i64>,
) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE system_user SET username = ?, password = ?, role = COALESCE(?, role), \
         valid_status = COALESCE(?, valid_status), \
         update_time = DATETIME(CURRENT_TIMESTAMP, 'localtime') WHERE id = ?",
    )
    .bind(username)
    .bind(password_hash)
    .bind(role)
    .bind(valid_status)
    .bind(id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        bail!("system user {id} does not exist");
    }
    Ok(result.rows_affected())
}
