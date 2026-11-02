//! Synchronization parity tests against local mock Sonarr/Radarr/TMDB/rule
//! endpoints. No test makes a live network request.

mod common;

use std::time::Duration;

use common::{EMPTY_FEED, MockResponse, TestEnv, get};
use jproxy_rs::app::AppState;
use jproxy_rs::models::AppKind;
use jproxy_rs::sync;

fn sync_config(env: &TestEnv) -> jproxy_rs::config::Config {
    let mut config = env.config();
    config.sync_enabled = true;
    config.sync_interval = Duration::from_secs(60);
    config.on_demand_sync_timeout = Duration::from_secs(5);
    config
}

fn sonarr_series() -> String {
    serde_json::json!([
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
    ])
    .to_string()
}

fn radarr_movie() -> String {
    serde_json::json!([
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
    ])
    .to_string()
}

fn sonarr_rule() -> String {
    serde_json::json!([
        {
            "id": "rule-alice",
            "token": "title",
            "priority": 999,
            "regex": ".*{cleanTitle}.*",
            "replacement": "{mainTitle}",
            "offset": 0,
            "example": "",
            "remark": "synthetic",
            "author": "alice",
            "validStatus": 1
        }
    ])
    .to_string()
}

#[tokio::test]
async fn sonarr_title_sync_upserts_java_rows() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::json(sonarr_series()));
    let mut config = sync_config(&env);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-sonarr-key".to_owned());
    let state = AppState::new(config).await.unwrap();

    let count = sync::sync_titles_once(&state, AppKind::Sonarr)
        .await
        .unwrap();
    assert_eq!(count, Some(3));

    let render: String = sqlx::query_scalar(
        "SELECT group_concat(id || ':' || sno || ':' || clean_title || ':' || season_number, '|') \
         FROM sonarr_title ORDER BY id",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        render,
        "1000:0:main show:-1|1001:1:main show:-1|1002:2:alternate show:2"
    );
    let series_id: i64 = sqlx::query_scalar("SELECT series_id FROM sonarr_title WHERE id = 1000")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(series_id, 7);
}

#[tokio::test]
async fn radarr_title_sync_upserts_java_rows() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/movie", MockResponse::json(radarr_movie()));
    let mut config = sync_config(&env);
    config.radarr_url = Some(env.mock.base_url());
    config.radarr_api_key = Some("synthetic-radarr-key".to_owned());
    let state = AppState::new(config).await.unwrap();

    let count = sync::sync_titles_once(&state, AppKind::Radarr)
        .await
        .unwrap();
    assert_eq!(count, Some(5));

    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM radarr_title ORDER BY id")
        .fetch_all(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        titles,
        vec![
            "Main Movie",
            "Main Movie",
            "Main Movie",
            "Original Movie",
            "Alt Movie"
        ]
    );
    let clean: String = sqlx::query_scalar("SELECT clean_title FROM radarr_title WHERE sno = 2")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(clean, "mainmovie");
}

#[tokio::test]
async fn title_sync_gate_limits_attempts_to_one_per_interval() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::json(sonarr_series()));
    let mut config = sync_config(&env);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-key".to_owned());
    let state = AppState::new(config).await.unwrap();

    assert_eq!(
        sync::sync_titles_once(&state, AppKind::Sonarr)
            .await
            .unwrap(),
        Some(3)
    );
    assert_eq!(
        sync::sync_titles_once(&state, AppKind::Sonarr)
            .await
            .unwrap(),
        None
    );
    let calls = env
        .mock
        .recorded_paths()
        .into_iter()
        .filter(|path| path == "/api/v3/series")
        .count();
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn tmdb_sync_inserts_both_languages_once() {
    let env = TestEnv::start().await;
    env.mock.route_path(
        "/3/find/100",
        MockResponse::json(
            serde_json::json!({
                "tv_results": [{"id": 555, "name": "Main Show"}]
            })
            .to_string(),
        ),
    );
    env.exec(
        "INSERT INTO sonarr_title \
         (id, tvdb_id, sno, main_title, title, clean_title, season_number, monitored) \
         VALUES (1000, 100, 0, 'Main Show', 'Main Show', 'main show', -1, 1)",
    )
    .await;

    let mut config = sync_config(&env);
    config.tmdb_url = Some(env.mock.base_url());
    config.tmdb_api_key = Some("synthetic-tmdb-key".to_owned());
    config.sonarr_language_1 = Some("zh-CN".to_owned());
    config.sonarr_language_2 = Some("zh-TW".to_owned());
    let state = AppState::new(config).await.unwrap();

    let count = sync::sync_tmdb_once(&state).await.unwrap();
    assert_eq!(count, Some(2));
    let rows: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT tvdb_id, tmdb_id, language FROM tmdb_title ORDER BY language")
            .fetch_all(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            (100, 555, "zh-CN".to_owned()),
            (100, 555, "zh-TW".to_owned())
        ]
    );

    // The need-sync query excludes a tvdb id as soon as any translation exists.
    assert_eq!(sync::sync_tmdb_once(&state).await.unwrap(), None);
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tmdb_title")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stored, 2);

    let queries = env.mock.recorded_full_queries();
    assert!(
        queries
            .iter()
            .any(|query| query.contains("external_source=tvdb_id"))
    );
    assert!(queries.iter().any(|query| query.contains("language=zh-CN")));
    assert!(queries.iter().any(|query| query.contains("language=zh-TW")));
}

#[tokio::test]
async fn rule_sync_uses_author_directory_and_isolates_author_failures() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/author.json", MockResponse::json(r#"["alice","bob"]"#));
    env.mock
        .route_path("/sonarr@alice.json", MockResponse::json(sonarr_rule()));
    env.mock
        .route_path("/sonarr@bob.json", MockResponse::status(503));

    let mut config = sync_config(&env);
    config.rule_location = Some(env.mock.base_url());
    config.rule_location_backup = Some(String::new());
    config.rule_sync_authors = Some("ALL".to_owned());
    let state = AppState::new(config).await.unwrap();

    let count = sync::sync_rules_once(&state, AppKind::Sonarr)
        .await
        .unwrap();
    assert_eq!(count, Some(1));
    let stored: String =
        sqlx::query_scalar("SELECT author FROM sonarr_rule WHERE id = 'rule-alice'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(stored, "alice");
}

#[tokio::test]
async fn rule_sync_falls_back_to_the_backup_location() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/sonarr@alice.json", MockResponse::json(sonarr_rule()));

    let mut config = sync_config(&env);
    config.rule_location = Some("http://127.0.0.1:1".to_owned());
    config.rule_location_backup = Some(env.mock.base_url());
    config.rule_sync_authors = Some("alice".to_owned());
    let state = AppState::new(config).await.unwrap();

    let count = sync::sync_rules_once(&state, AppKind::Sonarr)
        .await
        .unwrap();
    assert_eq!(count, Some(1));
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sonarr_rule")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);
}

#[tokio::test]
async fn rule_sync_preserves_a_locally_disabled_rule() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/sonarr@alice.json", MockResponse::json(sonarr_rule()));
    env.exec(
        "INSERT INTO sonarr_rule (id, token, priority, regex, valid_status) \
         VALUES ('rule-alice', 'title', 10, 'old', 0)",
    )
    .await;

    let mut config = sync_config(&env);
    config.rule_location = Some(env.mock.base_url());
    config.rule_location_backup = Some(String::new());
    config.rule_sync_authors = Some("alice".to_owned());
    let state = AppState::new(config).await.unwrap();

    sync::sync_rules_once(&state, AppKind::Sonarr)
        .await
        .unwrap();
    let status: i64 =
        sqlx::query_scalar("SELECT valid_status FROM sonarr_rule WHERE id = 'rule-alice'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(status, 0);
}

#[tokio::test]
async fn blank_rule_configuration_disables_rule_sync() {
    let env = TestEnv::start().await;
    let mut config = sync_config(&env);
    config.rule_location = Some(env.mock.base_url());
    config.rule_sync_authors = Some(String::new());
    let state = AppState::new(config).await.unwrap();

    assert_eq!(
        sync::sync_rules_once(&state, AppKind::Sonarr)
            .await
            .unwrap(),
        None
    );
    assert!(env.mock.recorded_paths().is_empty());
}

#[tokio::test]
async fn system_config_values_are_used_when_environment_is_absent() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::json(sonarr_series()));
    env.exec(&format!(
        "INSERT INTO system_config (\"key\", value, valid_status) VALUES \
         ('sonarrUrl', '{}', 1), \
         ('sonarrApikey', 'synthetic-db-key', 1)",
        env.mock.base_url()
    ))
    .await;

    let config = sync_config(&env);
    assert!(config.sonarr_url.is_none());
    let state = AppState::new(config).await.unwrap();
    assert_eq!(state.config.sonarr_url, Some(env.mock.base_url()));
    assert!(state.config.sonarr_api_key.is_some());

    assert_eq!(
        sync::sync_titles_once(&state, AppKind::Sonarr)
            .await
            .unwrap(),
        Some(3)
    );
}

#[tokio::test]
async fn explicit_environment_values_override_system_config() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::json(sonarr_series()));
    env.exec(
        "INSERT INTO system_config (\"key\", value, valid_status) VALUES \
         ('sonarrUrl', 'http://127.0.0.1:1', 1), \
         ('sonarrApikey', 'ignored-db-key', 1)",
    )
    .await;

    let mut config = sync_config(&env);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-env-key".to_owned());
    let state = AppState::new(config).await.unwrap();

    assert_eq!(
        sync::sync_titles_once(&state, AppKind::Sonarr)
            .await
            .unwrap(),
        Some(3)
    );
}

#[tokio::test]
async fn on_demand_sync_populates_a_missing_title() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::json(sonarr_series()));
    env.mock.fallback(MockResponse::ok(EMPTY_FEED));

    let mut config = sync_config(&env);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-key".to_owned());
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Main+Show&limit=100",
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(
        env.mock
            .recorded_paths()
            .iter()
            .any(|path| path == "/api/v3/series")
    );

    let pool = env.pool().await;
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sonarr_title")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 3);
}

#[tokio::test]
async fn on_demand_sync_failure_does_not_become_a_500() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::status(503));
    env.mock.fallback(MockResponse::ok(EMPTY_FEED));

    let mut config = sync_config(&env);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-key".to_owned());
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Unknown+Show&limit=100",
    )
    .await;
    assert_eq!(response.status, 200);
    assert_ne!(response.status, 500);
}

#[tokio::test]
async fn cached_miss_does_not_repeat_the_on_demand_sync() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::status(503));
    env.mock.fallback(MockResponse::ok(EMPTY_FEED));

    let mut config = sync_config(&env);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-key".to_owned());
    let router = env.router_with(config).await;

    for page in 1..=3 {
        let response = get(
            &router,
            &format!("/sonarr/prowlarr/1/api?t=tvsearch&q=Unknown+Show&limit=100&page={page}"),
        )
        .await;
        assert_eq!(response.status, 200);
    }

    let sync_calls = env
        .mock
        .recorded_paths()
        .into_iter()
        .filter(|path| path == "/api/v3/series")
        .count();
    assert_eq!(sync_calls, 1);
}

#[tokio::test]
async fn sync_disabled_skips_on_demand_lookups() {
    let env = TestEnv::start().await;
    env.mock
        .route_path("/api/v3/series", MockResponse::json(sonarr_series()));
    env.mock.fallback(MockResponse::ok(EMPTY_FEED));

    let mut config = env.config();
    config.sync_enabled = false;
    config.sync_interval = Duration::from_secs(60);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-key".to_owned());
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Main+Show&limit=100",
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(
        !env.mock
            .recorded_paths()
            .iter()
            .any(|path| path == "/api/v3/series")
    );
}
