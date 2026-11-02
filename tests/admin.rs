//! Admin API, authentication and static UI integration tests.
//!
//! All credentials and users are synthetic. No test contacts a live service.

mod common;

use std::io::Write;

use common::{HttpResult, TestEnv, get, get_auth, post_json, post_json_auth, request};
use jproxy_rs::auth::md5_upper;
use serde_json::{Value, json};

const USER: &str = "tester";
const PASSWORD: &str = "synthetic-password";
const PRIMARY_RULE_ID: &str = "00000000000000000000000000000000";

fn auth_config(env: &TestEnv, login_enabled: bool) -> jproxy_rs::config::Config {
    let mut config = env.config();
    config.login_enabled = login_enabled;
    config.sync_enabled = false;
    config
}

async fn seed_user(env: &TestEnv) {
    let submitted = md5_upper(PASSWORD);
    let stored = md5_upper(&submitted);
    env.exec(&format!(
        "INSERT INTO system_user (id, username, password, role) \
         VALUES (10, '{USER}', '{stored}', 'admin')"
    ))
    .await;
}

async fn login(router: &axum::Router, username: &str, submitted: &str) -> HttpResult {
    let body = json!({"username": username, "password": submitted}).to_string();
    post_json(router, "/api/system/user/login", &body).await
}

async fn seed_rule(env: &TestEnv, kind: &str, id: &str, status: i64) {
    env.exec(&format!(
        "INSERT INTO {kind}_rule (id, token, priority, regex, replacement, offset, example, \
         remark, author, valid_status) VALUES ('{id}', 'title', 900, '.*', 'x', 0, 'example', \
         'remark', 'author', {status})"
    ))
    .await;
}

#[tokio::test]
async fn admin_routes_require_auth_while_health_and_proxy_stay_open() {
    let env = TestEnv::start().await;
    let router = env.router_with(auth_config(&env, true)).await;

    assert_eq!(get(&router, "/health").await.status, 200);
    assert_eq!(
        get(&router, "/sonarr/prowlarr/1/api?t=tvsearch&q=Show")
            .await
            .status,
        200
    );
    assert_eq!(get(&router, "/api/system/config/query").await.status, 403);
    // The login-enabled probe is explicitly excluded by the Java interceptor.
    let enabled = get(&router, "/api/system/user/isLoginEnabled").await;
    assert_eq!(enabled.status, 200);
    assert_eq!(enabled.body, "true");
}

#[tokio::test]
async fn login_disabled_issues_an_anonymous_token() {
    let env = TestEnv::start().await;
    let router = env.router_with(auth_config(&env, false)).await;

    let response = login(&router, "Anonymous", "Anonymous").await;
    assert_eq!(response.status, 200);
    assert!(!response.body.is_empty());

    let config = get_auth(&router, "/api/system/config/query", &response.body).await;
    assert_eq!(config.status, 200);
    let rows: Vec<Value> = serde_json::from_str(&config.body).unwrap();
    assert!(rows.iter().any(|row| row["key"] == "sonarrUrl"));
}

#[tokio::test]
async fn wrong_login_limit_is_global_and_blocks_after_ten() {
    let env = TestEnv::start().await;
    seed_user(&env).await;
    let router = env.router_with(auth_config(&env, true)).await;

    // A correct login before the limit succeeds.
    assert_eq!(login(&router, USER, &md5_upper(PASSWORD)).await.status, 200);

    for _ in 0..10 {
        let response = login(&router, USER, "wrong-hash").await;
        assert_eq!(response.status, 400);
        assert!(response.body.contains("Username or password"));
    }
    // The eleventh attempt is still evaluated and increments the counter.
    assert_eq!(login(&router, USER, "wrong-hash").await.status, 400);
    // The next attempt is blocked before credential checking, even if correct.
    let blocked = login(&router, USER, &md5_upper(PASSWORD)).await;
    assert_eq!(blocked.status, 400);
    assert!(blocked.body.contains("Wrong too many times"));
}

#[tokio::test]
async fn login_info_update_and_logout_flow() {
    let env = TestEnv::start().await;
    seed_user(&env).await;
    let router = env.router_with(auth_config(&env, true)).await;

    let token = login(&router, USER, &md5_upper(PASSWORD)).await.body;
    let info = get_auth(&router, "/api/system/user/info", &token).await;
    assert_eq!(info.status, 200);
    let info: Value = serde_json::from_str(&info.body).unwrap();
    assert_eq!(info["username"], USER);
    assert_eq!(info["password"], "******");
    assert_eq!(info["validStatus"], 1);

    // The UI posts the MD5 of the new password; the service hashes once more.
    let new_submitted = md5_upper("new-synthetic-password");
    let update = post_json_auth(
        &router,
        "/api/system/user/update",
        &token,
        &json!({"username": "tester2", "password": new_submitted}).to_string(),
    )
    .await;
    assert_eq!(update.status, 200);
    assert_eq!(login(&router, "tester2", &new_submitted).await.status, 200);
    assert_eq!(
        login(&router, "tester2", &md5_upper(PASSWORD)).await.status,
        400
    );

    let token = login(&router, "tester2", &new_submitted).await.body;
    assert_eq!(
        request(
            &router,
            axum::http::Method::POST,
            "/api/system/user/logout",
            &[("authorization", &format!("Bearer {token}"))],
            "",
        )
        .await
        .status,
        200
    );
    assert_eq!(
        get_auth(&router, "/api/system/user/info", &token)
            .await
            .status,
        403
    );
}

#[tokio::test]
async fn rule_crud_supports_query_status_export_and_primary_protection() {
    let env = TestEnv::start().await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let create = post_json_auth(
        &router,
        "/api/sonarr/rule/save",
        &token,
        &json!({
            "token": "title",
            "priority": 900,
            "regex": ".*",
            "replacement": "value",
            "offset": 0,
            "example": "example",
            "remark": "remark",
            "author": "author"
        })
        .to_string(),
    )
    .await;
    assert_eq!(create.status, 200);

    let page = get_auth(&router, "/api/sonarr/rule/query?token=title", &token).await;
    assert_eq!(page.status, 200);
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["current"], 1);
    assert_eq!(page["pageSize"], 10);
    assert_eq!(page["total"], 1);
    assert_eq!(page["list"][0]["validStatus"], 1);
    let id = page["list"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 32);

    assert_eq!(
        post_json_auth(
            &router,
            "/api/sonarr/rule/disable",
            &token,
            &json!([id]).to_string()
        )
        .await
        .status,
        200
    );
    let page = get_auth(&router, "/api/sonarr/rule/query?token=title", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["list"][0]["validStatus"], 0);

    assert_eq!(
        post_json_auth(
            &router,
            "/api/sonarr/rule/enable",
            &token,
            &json!([id]).to_string()
        )
        .await
        .status,
        200
    );

    let tokens = get_auth(&router, "/api/sonarr/rule/token/list", &token).await;
    let tokens: Vec<String> = serde_json::from_str(&tokens.body).unwrap();
    assert_eq!(tokens, vec!["title"]);

    let export = post_json_auth(
        &router,
        "/api/sonarr/rule/export",
        &token,
        &json!([]).to_string(),
    )
    .await;
    let export: Vec<Value> = serde_json::from_str(&export.body).unwrap();
    assert_eq!(export.len(), 1);
    assert!(export[0].get("validStatus").is_none());
    assert_eq!(export[0]["author"], "author");

    let export = post_json_auth(
        &router,
        "/api/sonarr/rule/export",
        &token,
        &json!([id]).to_string(),
    )
    .await;
    let export: Vec<Value> = serde_json::from_str(&export.body).unwrap();
    assert_eq!(export[0]["validStatus"], 1);

    // Primary-rule modification is rejected by save, disable and remove.
    for path in ["save", "disable", "remove"] {
        let body = if path == "save" {
            json!({
                "id": PRIMARY_RULE_ID,
                "token": "title",
                "priority": 900,
                "regex": ".*",
                "replacement": "x",
                "offset": 0,
                "example": "example",
                "remark": "remark",
                "author": "author"
            })
        } else {
            json!([PRIMARY_RULE_ID])
        };
        let response = post_json_auth(
            &router,
            &format!("/api/sonarr/rule/{path}"),
            &token,
            &body.to_string(),
        )
        .await;
        assert_eq!(response.status, 400, "path {path}");
        assert!(response.body.contains("primary rule"));
    }

    assert_eq!(
        post_json_auth(
            &router,
            "/api/sonarr/rule/remove",
            &token,
            &json!([id]).to_string()
        )
        .await
        .status,
        200
    );
    let page = get_auth(&router, "/api/sonarr/rule/query", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 0);
}

#[tokio::test]
async fn rule_save_validates_regex_and_required_fields() {
    let env = TestEnv::start().await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let invalid = post_json_auth(
        &router,
        "/api/sonarr/rule/save",
        &token,
        &json!({
            "token": "title",
            "priority": 1,
            "regex": "(",
            "offset": 0,
            "example": "e",
            "remark": "r",
            "author": "a"
        })
        .to_string(),
    )
    .await;
    assert_eq!(invalid.status, 500);

    let missing = post_json_auth(
        &router,
        "/api/sonarr/rule/save",
        &token,
        &json!({"token": "title"}).to_string(),
    )
    .await;
    assert_eq!(missing.status, 400);
}

#[tokio::test]
async fn rule_import_does_not_reenable_locally_disabled_rules() {
    let env = TestEnv::start().await;
    seed_rule(&env, "sonarr", "disabled-rule", 0).await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let imported = json!([
        {
            "id": "disabled-rule",
            "token": "title",
            "priority": 900,
            "regex": ".*",
            "replacement": "x",
            "offset": 0,
            "example": "example",
            "remark": "remark",
            "author": "author",
            "validStatus": 1
        },
        {
            "id": "new-rule",
            "token": "group",
            "priority": 900,
            "regex": ".*",
            "replacement": "g",
            "offset": 0,
            "example": "example",
            "remark": "remark",
            "author": "author"
        }
    ])
    .to_string();
    let body = format!(
        "--BOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"rules.json\"\r\n\
         Content-Type: application/json\r\n\r\n{imported}\r\n--BOUND--\r\n"
    );
    let response = request(
        &router,
        axum::http::Method::POST,
        "/api/sonarr/rule/import",
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "multipart/form-data; boundary=BOUND"),
        ],
        &body,
    )
    .await;
    assert_eq!(response.status, 200);

    let page = get_auth(&router, "/api/sonarr/rule/query", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 2);
    let disabled = page["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "disabled-rule")
        .unwrap();
    assert_eq!(disabled["validStatus"], 0);
    let created = page["list"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "new-rule")
        .unwrap();
    assert_eq!(created["validStatus"], 1);
}

#[tokio::test]
async fn title_query_filters_paginates_and_removes() {
    let env = TestEnv::start().await;
    env.exec(
        "INSERT INTO sonarr_title (id, tvdb_id, sno, main_title, title, clean_title, \
         season_number, monitored) VALUES \
         (1, 100, 0, 'Main Show', 'Main Show', 'main show', -1, 1), \
         (2, 100, 1, 'Main Show', 'Main Show Slug', 'main show slug', -1, 1), \
         (3, 200, 0, 'Other Show', 'Other Show', 'other show', -1, 0)",
    )
    .await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let page = get_auth(
        &router,
        "/api/sonarr/title/query?title=Main&tvdbId=100&current=1&pageSize=1",
        &token,
    )
    .await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 2);
    assert_eq!(page["pageSize"], 1);
    assert_eq!(page["list"].as_array().unwrap().len(), 1);

    let remove = post_json_auth(
        &router,
        "/api/sonarr/title/remove",
        &token,
        &json!([1, 3]).to_string(),
    )
    .await;
    assert_eq!(remove.status, 200);
    let page = get_auth(&router, "/api/sonarr/title/query", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 1);
}

#[tokio::test]
async fn tmdb_save_reuses_tmdb_id_and_query_adds_clean_title() {
    let env = TestEnv::start().await;
    env.exec(
        "INSERT INTO tmdb_title (tvdb_id, tmdb_id, language, title) \
         VALUES (100, 555, 'zh-CN', '主标题')",
    )
    .await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let page = get_auth(&router, "/api/tmdb/title/query?tvdbId=100", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["list"][0]["cleanTitle"], "主标题");

    // Saving a second language without tmdbId copies the existing TMDB id.
    let save = post_json_auth(
        &router,
        "/api/tmdb/title/save",
        &token,
        &json!({"tvdbId": 100, "language": "zh-TW", "title": "主標題"}).to_string(),
    )
    .await;
    assert_eq!(save.status, 200);
    let page = get_auth(&router, "/api/tmdb/title/query?tvdbId=100", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 2);
    assert!(
        page["list"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["tmdbId"] == 555)
    );
}

#[tokio::test]
async fn rule_test_applies_regex_and_offset() {
    let env = TestEnv::start().await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let response = get_auth(
        &router,
        "/api/rule/test?regex=%5E%28.%2A%29%24&replacement=%5B%241%5D&offset=0\
         &example=First%0ASecond",
        &token,
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "[First]\n[Second]\n");

    let invalid = get_auth(
        &router,
        "/api/rule/test?regex=%28&replacement=x&example=y",
        &token,
    )
    .await;
    assert_eq!(invalid.status, 500);
}

#[tokio::test]
async fn example_query_formats_known_and_unknown_titles() {
    let env = TestEnv::start().await;
    env.exec(
        "INSERT INTO sonarr_title (id, tvdb_id, sno, main_title, title, clean_title, \
         season_number, monitored) VALUES \
         (1, 100, 0, 'Main Show', 'Main Show', 'main show', -1, 1); \
         INSERT INTO sonarr_rule (id, token, priority, regex, replacement, offset, example, \
         remark, author, valid_status) VALUES \
         ('title-rule', 'title', 900, '.*(\\b|\\s){cleanTitle}(\\b|\\s).*', '{mainTitle}', 0, 'example', \
         'remark', 'author', 1); \
         UPDATE system_config SET value = '{title}' WHERE \"key\" = 'sonarrIndexerFormat'",
    )
    .await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let save = post_json_auth(
        &router,
        "/api/sonarr/example/save",
        &token,
        &json!({"originalText": "Main Show S01E01\nUnknown Show S01E01\n"}).to_string(),
    )
    .await;
    assert_eq!(save.status, 200);

    let page = get_auth(&router, "/api/sonarr/example/query", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 2);
    let rows = page["list"].as_array().unwrap();
    let known = rows
        .iter()
        .find(|row| row["originalText"] == "Main Show S01E01")
        .unwrap();
    assert_eq!(known["formatText"], "Main Show");
    assert_eq!(known["validStatus"], 1);
    let unknown = rows
        .iter()
        .find(|row| row["originalText"] == "Unknown Show S01E01")
        .unwrap();
    assert!(
        unknown["formatText"]
            .as_str()
            .unwrap()
            .contains("Match Title Fail")
    );

    let hashes: Vec<String> = rows
        .iter()
        .map(|row| row["hash"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        post_json_auth(
            &router,
            "/api/sonarr/example/remove",
            &token,
            &json!(hashes).to_string()
        )
        .await
        .status,
        200
    );
    let page = get_auth(&router, "/api/sonarr/example/query", &token).await;
    let page: Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(page["total"], 0);
}

#[tokio::test]
async fn system_config_update_persists_validity_and_normalizes_transmission() {
    let env = TestEnv::start().await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let query = get_auth(&router, "/api/system/config/query", &token).await;
    assert_eq!(query.status, 200);
    let mut rows: Vec<Value> = serde_json::from_str(&query.body).unwrap();
    for row in &mut rows {
        match row["key"].as_str().unwrap() {
            "sonarrUrl" => row["value"] = Value::from("http://sonarr.invalid"),
            "sonarrApikey" => row["value"] = Value::from(""),
            "transmissionUrl" => {
                row["value"] = Value::from("http://transmission.invalid/transmission/web/")
            }
            "cleanTitleRegex" => row["value"] = Value::from("("),
            _ => {}
        }
    }

    let update = post_json_auth(
        &router,
        "/api/system/config/update",
        &token,
        &serde_json::to_string(&rows).unwrap(),
    )
    .await;
    assert_eq!(update.status, 200);

    let query = get_auth(&router, "/api/system/config/query", &token).await;
    let rows: Vec<Value> = serde_json::from_str(&query.body).unwrap();
    let by_key = |key: &str| {
        rows.iter()
            .find(|row| row["key"] == key)
            .unwrap_or_else(|| panic!("missing config key {key}"))
    };
    assert_eq!(by_key("sonarrUrl")["validStatus"], 1);
    assert_eq!(by_key("sonarrApikey")["validStatus"], 0);
    assert_eq!(
        by_key("transmissionUrl")["value"],
        "http://transmission.invalid/transmission/rpc"
    );
    assert_eq!(by_key("cleanTitleRegex")["validStatus"], 0);

    let clear = post_json_auth(
        &router,
        "/api/system/cache/clear",
        &token,
        &json!({"cacheName": "indexer_result"}).to_string(),
    )
    .await;
    assert_eq!(clear.status, 200);
    let blank = post_json_auth(
        &router,
        "/api/system/cache/clear",
        &token,
        &json!({"cacheName": ""}).to_string(),
    )
    .await;
    assert_eq!(blank.status, 400);
    assert_eq!(
        post_json_auth(&router, "/api/system/cache/clearAll", &token, "")
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn author_list_uses_the_configured_rule_location() {
    let env = TestEnv::start().await;
    env.mock.route_path(
        "/author.json",
        common::MockResponse::json(r#"["alice","bob"]"#),
    );
    let mut config = auth_config(&env, false);
    config.rule_location = Some(env.mock.base_url());
    let router = env.router_with(config).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;

    let authors = get_auth(&router, "/api/system/config/author/list", &token).await;
    assert_eq!(authors.status, 200);
    let authors: Vec<String> = serde_json::from_str(&authors.body).unwrap();
    assert_eq!(authors, vec!["alice", "bob"]);
}

#[tokio::test]
async fn static_ui_serves_spa_and_does_not_shadow_reserved_paths() {
    let env = TestEnv::start().await;
    let ui = tempfile::tempdir().unwrap();
    std::fs::create_dir(ui.path().join("assets")).unwrap();
    std::fs::write(ui.path().join("index.html"), "<html>admin spa</html>").unwrap();
    let mut file = std::fs::File::create(ui.path().join("assets/app.js")).unwrap();
    writeln!(file, "console.log('admin');").unwrap();

    let mut config = auth_config(&env, false);
    config.admin_ui_dir = ui.path().to_path_buf();
    let router = env.router_with(config).await;

    let root = get(&router, "/").await;
    assert_eq!(root.status, 200);
    assert_eq!(root.body, "<html>admin spa</html>");
    // SPA fallback for client-side routes.
    let client_route = get(&router, "/some/client/route").await;
    assert_eq!(client_route.status, 200);
    assert_eq!(client_route.body, "<html>admin spa</html>");
    assert_eq!(get(&router, "/assets/app.js").await.status, 200);

    // Reserved prefixes never fall through to the SPA.
    assert_eq!(get(&router, "/health").await.status, 200);
    assert!(
        get(&router, "/health")
            .await
            .body
            .contains("\"status\":\"ok\"")
    );
    // Unknown `/api` paths are protected like Java's interceptor; ordinary
    // proxy prefixes stay public but never reach the SPA.
    assert_eq!(get(&router, "/api/does-not-exist").await.status, 403);
    let token = login(&router, "Anonymous", "Anonymous").await.body;
    assert_eq!(
        get_auth(&router, "/api/does-not-exist", &token)
            .await
            .status,
        404
    );
    assert_eq!(get(&router, "/sonarr/does-not-exist").await.status, 404);
    assert_eq!(get(&router, "/radarr/does-not-exist").await.status, 404);
}

#[tokio::test]
async fn first_run_seed_matches_the_upstream_default_account() {
    let env = TestEnv::start().await;
    let pool = env.pool().await;
    let password: String =
        sqlx::query_scalar("SELECT password FROM system_user WHERE username = 'jproxy'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(password, jproxy_rs::auth::DEFAULT_PASSWORD_HASH);
    // MD5(MD5("jproxy@2023")) is the published UI login for `jproxy@2023`.
    assert_eq!(
        md5_upper(&md5_upper("jproxy@2023")),
        jproxy_rs::auth::DEFAULT_PASSWORD_HASH
    );

    let config_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM system_config")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(config_count, 20);
}

#[tokio::test]
async fn sync_endpoints_report_java_compatible_statuses() {
    let env = TestEnv::start().await;
    // A Sonarr row needs TMDB, so the TMDB endpoint reaches the interval gate
    // instead of taking the Java "nothing to do" success path.
    env.exec(
        "INSERT INTO sonarr_title (id, tvdb_id, sno, main_title, title, clean_title, \
         season_number, monitored) VALUES (1, 100, 0, 'Show', 'Show', 'show', -1, 1)",
    )
    .await;
    let router = env.router_with(auth_config(&env, false)).await;
    let token = login(&router, "Anonymous", "Anonymous").await.body;
    let authorization = format!("Bearer {token}");
    let auth = [("authorization", authorization.as_str())];

    // Rule sync always reports success in Java, even when nothing runs.
    assert_eq!(
        request(
            &router,
            axum::http::Method::POST,
            "/api/sonarr/rule/sync",
            &auth,
            "",
        )
        .await
        .status,
        200
    );

    for kind in ["sonarr", "radarr", "tmdb"] {
        let path = if kind == "tmdb" {
            "/api/tmdb/title/sync".to_owned()
        } else {
            format!("/api/{kind}/title/sync")
        };
        let response = request(&router, axum::http::Method::POST, &path, &auth, "").await;
        assert_eq!(response.status, 400, "path {path}");
        assert!(response.body.contains("Sync too often"));
    }
}
