//! Import path for an existing jproxy SQLite database.
//!
//! The upstream Java service stores titles, rules and TMDB titles in a SQLite
//! file that already matches `migrations/0001_schema.sql`. These helpers copy
//! only the reference tables, so an operator can move data between databases
//! without ever touching `system_config`/`system_user`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use crate::db;

/// Non-secret tables copied by the importer, in dependency order.
pub const REFERENCE_TABLES: &[&str] = &[
    "sonarr_title",
    "radarr_title",
    "sonarr_rule",
    "radarr_rule",
    "tmdb_title",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// `(table, rows)` in the target database after the import.
    pub copied: Vec<(String, i64)>,
}

impl ImportReport {
    pub fn total(&self) -> i64 {
        self.copied.iter().map(|(_, count)| count).sum()
    }
}

/// Open a SQLite file read-only with a single connection.
pub async fn open_read_only(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .with_context(|| format!("open SQLite database {} read-only", path.display()))
}

/// Count rows in a table using a pooled connection.
pub async fn count_rows(pool: &SqlitePool, table: &str) -> Result<i64> {
    // `table` is always one of REFERENCE_TABLES, never user input.
    // `table` is always one of REFERENCE_TABLES, never user input.
    let row = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS count FROM {table}"
    )))
    .fetch_one(pool)
    .await
    .with_context(|| format!("count rows in {table}"))?;
    let count: i64 = sqlx::Row::try_get(&row, 0)?;
    Ok(count)
}

/// Copy the reference tables from `source` into `target`.
///
/// `target` is created with the project schema when missing. Existing reference
/// rows in `target` are replaced, so the result is a faithful copy of the
/// source reference data. Both databases must share the same reference-table
/// column layout; a mismatch is reported instead of silently reordering
/// columns.
pub async fn import_reference_data(source: &Path, target: &Path) -> Result<ImportReport> {
    let source = source
        .canonicalize()
        .with_context(|| format!("resolve source database {}", source.display()))?;
    let target = target.to_path_buf();
    if source == target {
        bail!("source and target databases must differ");
    }

    let source_pool = open_read_only(&source).await?;
    for table in REFERENCE_TABLES {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(&source_pool)
        .await?;
        if exists == 0 {
            bail!("source database {} has no {table} table", source.display());
        }
    }

    let target_pool = db::connect(&target).await?;
    let mut connection = target_pool.acquire().await?;

    sqlx::query("ATTACH DATABASE ?1 AS src")
        .bind(source.to_string_lossy().as_ref())
        .execute(&mut *connection)
        .await
        .context("attach source database")?;

    let result = copy_tables(&mut connection).await;

    // Always detach so a failed copy does not leave the source attached.
    let _ = sqlx::query("DETACH DATABASE src")
        .execute(&mut *connection)
        .await;
    result?;

    drop(connection);

    let mut copied = Vec::with_capacity(REFERENCE_TABLES.len());
    for table in REFERENCE_TABLES {
        copied.push(((*table).to_owned(), count_rows(&target_pool, table).await?));
    }
    Ok(ImportReport { copied })
}

async fn copy_tables(connection: &mut sqlx::SqliteConnection) -> Result<()> {
    for table in REFERENCE_TABLES {
        let main_columns = column_count(connection, table, "main").await?;
        let source_columns = column_count(connection, table, "src").await?;
        if main_columns != source_columns {
            bail!(
                "{table} column count differs: target has {main_columns}, source has {source_columns}"
            );
        }
        // Replacing the whole table keeps the import idempotent.
        // `table` is always one of REFERENCE_TABLES, never user input.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DELETE FROM main.{table}")))
            .execute(&mut *connection)
            .await
            .with_context(|| format!("clear target table {table}"))?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO main.{table} SELECT * FROM src.{table}"
        )))
        .execute(&mut *connection)
        .await
        .with_context(|| format!("copy table {table} from source"))?;
    }
    Ok(())
}

async fn column_count(
    connection: &mut sqlx::SqliteConnection,
    table: &str,
    schema: &str,
) -> Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS count FROM pragma_table_info(?1, ?2)")
        .bind(table)
        .bind(schema)
        .fetch_one(&mut *connection)
        .await
        .with_context(|| format!("inspect {schema}.{table} columns"))?;
    row.try_get("count").map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_source(path: &Path) {
        let pool = db::connect(path).await.unwrap();
        sqlx::query(
            "INSERT INTO sonarr_title \
             (id, tvdb_id, sno, main_title, title, clean_title, season_number, monitored) \
             VALUES (767110, 76711, 0, 'Jackie Chan Adventures', 'Jackie Chan Adventures', \
                     'jackie chan adventures', -1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO radarr_rule (id, token, priority, regex, replacement, offset) \
             VALUES ('rule-1', 'title', 999, '.*', '{mainTitle}', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tmdb_title (tvdb_id, language, title) VALUES (76711, 'zh-CN', '成龙历险记')",
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn imports_reference_tables_and_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        seed_source(&source).await;

        let report = import_reference_data(&source, &target).await.unwrap();
        let counts: std::collections::HashMap<_, _> = report.copied.iter().cloned().collect();
        assert_eq!(counts["sonarr_title"], 1);
        assert_eq!(counts["radarr_rule"], 1);
        assert_eq!(counts["tmdb_title"], 1);
        assert_eq!(report.total(), 3);

        let target_pool = db::connect(&target).await.unwrap();
        let title: String =
            sqlx::query_scalar("SELECT main_title FROM sonarr_title WHERE id = 767110")
                .fetch_one(&target_pool)
                .await
                .unwrap();
        assert_eq!(title, "Jackie Chan Adventures");
    }

    #[tokio::test]
    async fn import_is_idempotent_and_replaces_rows() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        seed_source(&source).await;

        import_reference_data(&source, &target).await.unwrap();
        import_reference_data(&source, &target).await.unwrap();
        let target_pool = db::connect(&target).await.unwrap();
        assert_eq!(count_rows(&target_pool, "sonarr_title").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn rejects_a_missing_source_table() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("empty.db");
        let target = dir.path().join("target.db");
        // Create an empty database with no reference tables at all.
        db::connect(&source).await.unwrap();
        sqlx::query("DROP TABLE sonarr_title")
            .execute(&db::connect(&source).await.unwrap())
            .await
            .unwrap();
        assert!(import_reference_data(&source, &target).await.is_err());
    }
}
