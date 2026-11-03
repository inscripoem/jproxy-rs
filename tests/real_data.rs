//! Parity against real data exported from the live jproxy database.
//!
//! `fixtures/jproxy-live.db` was produced by `scripts/export-fixtures.sh`,
//! which copies only the five reference tables with `sqlite3 -readonly`. The
//! expected counts below were read from the live database with
//! `SELECT count(*)` at export time.
//!
//! Library titles are replaced by synthetic placeholders during export. Only
//! the public works asserted here, and their TMDB alternates, retain their real
//! titles.

use std::path::Path;

use jproxy_rs::config::DEFAULT_CLEAN_TITLE_REGEX;
use jproxy_rs::db;
use jproxy_rs::import;
use jproxy_rs::models::AppKind;
use regex::Regex;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/jproxy-live.db");

/// Live `SELECT count(*)` values, in the order returned by the export script.
const LIVE_COUNTS: &[(&str, i64)] = &[
    ("sonarr_title", 46),
    ("radarr_title", 1535),
    ("sonarr_rule", 90),
    ("radarr_rule", 38),
    ("tmdb_title", 20),
];

#[tokio::test]
async fn exports_and_imports_the_live_reference_counts() {
    assert!(Path::new(FIXTURE).exists(), "missing fixture {FIXTURE}");

    let pool = import::open_read_only(Path::new(FIXTURE)).await.unwrap();
    for (table, expected) in LIVE_COUNTS {
        let count = import::count_rows(&pool, table).await.unwrap();
        assert_eq!(
            count, *expected,
            "{table} row count differs from the live DB"
        );
    }

    // The fixture carries no system tables, proving only reference data was
    // exported.
    let secret_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('system_config', 'system_user')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(secret_tables, 0);

    // Round-trip through the importer into a fresh database.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("imported.db");
    let report = import::import_reference_data(Path::new(FIXTURE), &target)
        .await
        .unwrap();
    for (table, expected) in LIVE_COUNTS {
        let (_, count) = report
            .copied
            .iter()
            .find(|(name, _)| name == table)
            .unwrap_or_else(|| panic!("{table} missing from import report"));
        assert_eq!(*count, *expected);
    }
}

#[tokio::test]
async fn fixture_library_titles_are_anonymized() {
    let pool = import::open_read_only(Path::new(FIXTURE)).await.unwrap();

    let sonarr_unexpected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sonarr_title \
         WHERE main_title <> 'Jackie Chan Adventures' \
           AND main_title NOT GLOB 'Series [0-9]*'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        sonarr_unexpected, 0,
        "fixture contains a non-anonymized Sonarr title"
    );

    let radarr_unexpected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM radarr_title \
         WHERE main_title <> 'The Fifth Element' \
           AND main_title NOT GLOB 'Movie [0-9]*'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        radarr_unexpected, 0,
        "fixture contains a non-anonymized Radarr title"
    );

    let tmdb_unexpected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tmdb_title \
         WHERE tvdb_id <> 76711 \
           AND title NOT GLOB 'Alternate [0-9]*'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        tmdb_unexpected, 0,
        "fixture contains a non-anonymized TMDB title"
    );
}

#[tokio::test]
async fn service_opens_the_exported_database_directly() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    std::fs::copy(FIXTURE, &database).unwrap();

    // Opening through the same schema path the service uses must be
    // non-destructive and must recreate the (empty) system tables.
    let pool = db::connect(&database).await.unwrap();
    let regex = Regex::new(DEFAULT_CLEAN_TITLE_REGEX).unwrap();

    let sonarr = db::find_title(&pool, AppKind::Sonarr, "Jackie Chan Adventures", &regex)
        .await
        .unwrap()
        .expect("Jackie Chan Adventures should be present in the live fixture");
    assert_eq!(sonarr.main_title, "Jackie Chan Adventures");
    assert_eq!(sonarr.sno, 0);

    // The router strips the year before lookup (Java `removeYear`); the raw
    // query still would not match, which is exactly why that step exists.
    assert!(
        db::find_title(&pool, AppKind::Radarr, "The Fifth Element 1997", &regex)
            .await
            .unwrap()
            .is_none()
    );
    let lookup = jproxy_rs::text::remove_year("The Fifth Element 1997");
    let radarr = db::find_title(&pool, AppKind::Radarr, &lookup, &regex)
        .await
        .unwrap()
        .expect("The Fifth Element should match after year removal");
    assert_eq!(radarr.main_title, "The Fifth Element");

    // TMDB alternates for a live series are readable.
    let tmdb = db::query_tmdb_titles(&pool, 76711).await.unwrap();
    assert!(
        tmdb.iter().any(|title| title == "成龙历险记"),
        "expected the zh-CN alternate, got {tmdb:?}"
    );
}

#[tokio::test]
async fn live_rules_compile_and_run_over_real_titles() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    std::fs::copy(FIXTURE, &database).unwrap();
    let pool = db::connect(&database).await.unwrap();

    let sonarr_rules = db::load_rules(&pool, AppKind::Sonarr).await.unwrap();
    assert_eq!(sonarr_rules.len(), 90);
    let radarr_rules = db::load_rules(&pool, AppKind::Radarr).await.unwrap();
    assert_eq!(radarr_rules.len(), 38);

    // Every enabled rule must compile under the Rust regex engine (the bundle
    // is documented as free of backreferences/look-around).
    for rule in sonarr_rules.iter().chain(radarr_rules.iter()) {
        if rule.regex.contains("{cleanTitle}") {
            continue;
        }
        regex::Regex::new(&rule.regex)
            .unwrap_or_else(|error| panic!("rule {} failed to compile: {error}", rule.id));
    }
}
