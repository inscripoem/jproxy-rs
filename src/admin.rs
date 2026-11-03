//! Admin HTTP API and compiled UI integration.
//!
//! Paths, methods, pagination parameters, and JSON fields follow the Java
//! controllers. All `/api/**` routes except login and `isLoginEnabled` require
//! a valid bearer token, matching `LoginInterceptor`.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Extension, Multipart, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::admin_db::{
    self, ExampleQuery, Page, RadarrTitleQuery, RuleQuery, RuleRecord, SonarrTitleQuery,
    SystemConfigRecord, TmdbTitleQuery, TmdbTitleSave,
};
use crate::app::AppState;
use crate::auth::{self, AuthState, AuthUser, LoginRequest, md5_upper, random_uuid};
use crate::formatting::FormattingData;
use crate::models::AppKind;
use crate::sync;

const PRIMARY_RULE_ID: &str = "00000000000000000000000000000000";

const MSG_LOGIN_WRONG_TOO_MANY: &str =
    "Wrong too many times, please try again after restart service";
const MSG_LOGIN_WRONG_USER: &str = "Username or password is wrong";
const MSG_SYNC_TOO_OFTEN: &str = "Sync too often, please clear cache or try again later";
const MSG_PRIMARY_RULE: &str = "Please don't modify the primary rule";
const MSG_EXAMPLE_MATCH_FAIL: &str = "Match Title Fail";

const MSG_LOGIN_WRONG_TOO_MANY_ZH: &str = "错误次数太多，请重启服务后再尝试";
const MSG_LOGIN_WRONG_USER_ZH: &str = "用户名或密码错误";
const MSG_SYNC_TOO_OFTEN_ZH: &str = "同步过于频繁，请清除缓存或稍后再试";
const MSG_PRIMARY_RULE_ZH: &str = "请勿修改主要规则";
const MSG_EXAMPLE_MATCH_FAIL_ZH: &str = "未匹配到标题";

pub fn router(state: AppState) -> Router<AppState> {
    let auth: std::sync::Arc<AuthState> = state.auth.clone();

    let protected = Router::new()
        // Sonarr / Radarr rules
        .route("/api/sonarr/rule/sync", post(sonarr_rule_sync))
        .route("/api/sonarr/rule/query", get(sonarr_rule_query))
        .route("/api/sonarr/rule/save", post(sonarr_rule_save))
        .route("/api/sonarr/rule/remove", post(sonarr_rule_remove))
        .route("/api/sonarr/rule/enable", post(sonarr_rule_enable))
        .route("/api/sonarr/rule/disable", post(sonarr_rule_disable))
        .route("/api/sonarr/rule/export", post(sonarr_rule_export))
        .route("/api/sonarr/rule/import", post(sonarr_rule_import))
        .route("/api/sonarr/rule/token/list", get(sonarr_rule_tokens))
        .route("/api/radarr/rule/sync", post(radarr_rule_sync))
        .route("/api/radarr/rule/query", get(radarr_rule_query))
        .route("/api/radarr/rule/save", post(radarr_rule_save))
        .route("/api/radarr/rule/remove", post(radarr_rule_remove))
        .route("/api/radarr/rule/enable", post(radarr_rule_enable))
        .route("/api/radarr/rule/disable", post(radarr_rule_disable))
        .route("/api/radarr/rule/export", post(radarr_rule_export))
        .route("/api/radarr/rule/import", post(radarr_rule_import))
        .route("/api/radarr/rule/token/list", get(radarr_rule_tokens))
        // Sonarr / Radarr titles
        .route("/api/sonarr/title/sync", post(sonarr_title_sync))
        .route("/api/sonarr/title/query", get(sonarr_title_query))
        .route("/api/sonarr/title/remove", post(sonarr_title_remove))
        .route("/api/radarr/title/sync", post(radarr_title_sync))
        .route("/api/radarr/title/query", get(radarr_title_query))
        .route("/api/radarr/title/remove", post(radarr_title_remove))
        // Sonarr / Radarr examples
        .route("/api/sonarr/example/save", post(sonarr_example_save))
        .route("/api/sonarr/example/query", get(sonarr_example_query))
        .route("/api/sonarr/example/remove", post(sonarr_example_remove))
        .route("/api/radarr/example/save", post(radarr_example_save))
        .route("/api/radarr/example/query", get(radarr_example_query))
        .route("/api/radarr/example/remove", post(radarr_example_remove))
        // Rule test, TMDB titles, system config/cache/user
        .route("/api/rule/test", get(rule_test))
        .route("/api/tmdb/title/sync", post(tmdb_title_sync))
        .route("/api/tmdb/title/query", get(tmdb_title_query))
        .route("/api/tmdb/title/remove", post(tmdb_title_remove))
        .route("/api/tmdb/title/save", post(tmdb_title_save))
        .route("/api/system/config/version", get(system_config_version))
        .route("/api/system/config/query", get(system_config_query))
        .route("/api/system/config/update", post(system_config_update))
        .route("/api/system/config/author/list", get(system_config_authors))
        .route("/api/system/cache/clearAll", post(system_cache_clear_all))
        .route("/api/system/cache/clear", post(system_cache_clear))
        .route("/api/system/user/info", get(system_user_info))
        .route("/api/system/user/update", post(system_user_update))
        .route("/api/system/user/logout", post(system_user_logout))
        // Java's interceptor protects every /api path, including unknown ones.
        .route("/api/{*rest}", any(reserved_not_found))
        .route_layer(middleware::from_fn_with_state(auth, auth::require_admin));

    protected
        .route("/api/system/user/login", post(system_user_login))
        .route(
            "/api/system/user/isLoginEnabled",
            get(system_user_is_login_enabled),
        )
}

fn is_zh(headers: &HeaderMap) -> bool {
    headers
        .get("accept-language")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("zh"))
}

fn message(key: &str, headers: &HeaderMap) -> &'static str {
    let zh = is_zh(headers);
    match key {
        "login.wrong.too.many.times" => {
            if zh {
                MSG_LOGIN_WRONG_TOO_MANY_ZH
            } else {
                MSG_LOGIN_WRONG_TOO_MANY
            }
        }
        "login.wrong.user" => {
            if zh {
                MSG_LOGIN_WRONG_USER_ZH
            } else {
                MSG_LOGIN_WRONG_USER
            }
        }
        "title.sync.too.often" => {
            if zh {
                MSG_SYNC_TOO_OFTEN_ZH
            } else {
                MSG_SYNC_TOO_OFTEN
            }
        }
        "rule.modify.primary.forbidden" => {
            if zh {
                MSG_PRIMARY_RULE_ZH
            } else {
                MSG_PRIMARY_RULE
            }
        }
        "example.match.title.fail" => {
            if zh {
                MSG_EXAMPLE_MATCH_FAIL_ZH
            } else {
                MSG_EXAMPLE_MATCH_FAIL
            }
        }
        _ => "",
    }
}

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    (status, body.into()).into_response()
}

fn empty_ok() -> Response {
    StatusCode::OK.into_response()
}

fn bad_request(message: impl Into<String>) -> Response {
    text(StatusCode::BAD_REQUEST, message)
}

fn internal_error(context: &str, error: anyhow::Error) -> Response {
    tracing::error!(%error, %context, "admin request failed");
    text(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
}

// ---------------------------------------------------------------------------
// System user
// ---------------------------------------------------------------------------

async fn system_user_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Response {
    if !state.auth.login_enabled() {
        return match state.auth.sign(&AuthUser::anonymous()) {
            Ok(token) => text(StatusCode::OK, token),
            Err(error) => internal_error("sign anonymous token", error),
        };
    }
    if request.username.trim().is_empty() {
        return bad_request("username: must not be blank");
    }
    if request.password.is_empty() {
        return bad_request("password: must not be blank");
    }
    if state.auth.login_blocked() {
        return bad_request(message("login.wrong.too.many.times", &headers));
    }

    let found = match admin_db::find_user_by_username(&state.pool, &request.username).await {
        Ok(found) => found,
        Err(error) => return internal_error("look up admin user", error),
    };
    let Some(user) = found else {
        state.auth.record_wrong_login();
        return bad_request(message("login.wrong.user", &headers));
    };
    if user.password.as_deref() != Some(md5_upper(&request.password).as_str()) {
        state.auth.record_wrong_login();
        return bad_request(message("login.wrong.user", &headers));
    }

    let claims = AuthUser {
        id: Some(user.id),
        username: Some(user.username),
        // Java embeds the submitted password (the MD5 digest sent by the UI)
        // in the token. It is never logged here.
        password: Some(request.password),
        role: user.role,
        valid_status: Some(user.valid_status),
    };
    match state.auth.sign(&claims) {
        Ok(token) => text(StatusCode::OK, token),
        Err(error) => internal_error("sign login token", error),
    }
}

async fn system_user_is_login_enabled(State(state): State<AppState>) -> Response {
    Json(state.auth.login_enabled()).into_response()
}

async fn system_user_info(Extension(user): Extension<AuthUser>) -> Response {
    let mut body = serde_json::Map::new();
    if let Some(id) = user.id {
        body.insert("id".to_owned(), Value::from(id));
    }
    if let Some(username) = user.username {
        body.insert("username".to_owned(), Value::from(username));
    }
    body.insert("password".to_owned(), Value::from("******"));
    if let Some(role) = user.role {
        body.insert("role".to_owned(), Value::from(role));
    }
    if let Some(valid_status) = user.valid_status {
        body.insert("validStatus".to_owned(), Value::from(valid_status));
    }
    Json(Value::Object(body)).into_response()
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UserUpdateRequest {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

async fn system_user_update(
    State(state): State<AppState>,
    Extension(current): Extension<AuthUser>,
    headers: HeaderMap,
    Json(request): Json<UserUpdateRequest>,
) -> Response {
    if !state.auth.login_enabled() {
        return text(
            StatusCode::UNAUTHORIZED,
            message("login.wrong.user", &headers),
        );
    }
    let Some(id) = current.id else {
        return text(
            StatusCode::UNAUTHORIZED,
            message("login.wrong.user", &headers),
        );
    };
    let username = request
        .username
        .filter(|value| !value.trim().is_empty())
        .or(current.username)
        .unwrap_or_default();
    let password = request
        .password
        .filter(|value| !value.is_empty())
        .or(current.password)
        .unwrap_or_default();
    let password_hash = md5_upper(&password);
    match admin_db::update_user(
        &state.pool,
        id,
        &username,
        &password_hash,
        current.role.as_deref(),
        current.valid_status,
    )
    .await
    {
        Ok(_) => empty_ok(),
        Err(error) => internal_error("update admin user", error),
    }
}

async fn system_user_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state
        .auth
        .logout(auth::authorization_header(&headers))
        .await;
    empty_ok()
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

async fn sonarr_rule_sync(State(state): State<AppState>) -> Response {
    rule_sync(&state, AppKind::Sonarr).await
}

async fn radarr_rule_sync(State(state): State<AppState>) -> Response {
    rule_sync(&state, AppKind::Radarr).await
}

async fn rule_sync(state: &AppState, kind: AppKind) -> Response {
    match sync::sync_rules_once(state, kind).await {
        // Java's rule sync always reports success, including when no authors
        // or locations are configured; per-author failures are isolated.
        Ok(_) => empty_ok(),
        Err(error) => internal_error("sync rules", error),
    }
}

async fn sonarr_rule_query(
    State(state): State<AppState>,
    Query(query): Query<RuleQuery>,
) -> Response {
    rule_query(&state, AppKind::Sonarr, &query).await
}

async fn radarr_rule_query(
    State(state): State<AppState>,
    Query(query): Query<RuleQuery>,
) -> Response {
    rule_query(&state, AppKind::Radarr, &query).await
}

async fn rule_query(state: &AppState, kind: AppKind, query: &RuleQuery) -> Response {
    match admin_db::query_rules(&state.pool, kind, query).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => internal_error("query rules", error),
    }
}

fn validate_rule(rule: &RuleRecord) -> Option<String> {
    let blanks = [
        ("token", rule.token.as_deref()),
        ("regex", rule.regex.as_deref()),
        ("example", rule.example.as_deref()),
        ("remark", rule.remark.as_deref()),
        ("author", rule.author.as_deref()),
    ];
    for (field, value) in blanks {
        if value.is_none_or(|value| value.trim().is_empty()) {
            return Some(format!("{field}: must not be blank"));
        }
    }
    if rule.priority.is_none() {
        return Some("priority: must not be null".to_owned());
    }
    if rule.offset.is_none() {
        return Some("offset: must not be null".to_owned());
    }
    None
}

async fn sonarr_rule_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut rule): Json<RuleRecord>,
) -> Response {
    rule_save(&state, AppKind::Sonarr, &headers, &mut rule).await
}

async fn radarr_rule_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut rule): Json<RuleRecord>,
) -> Response {
    rule_save(&state, AppKind::Radarr, &headers, &mut rule).await
}

async fn rule_save(
    state: &AppState,
    kind: AppKind,
    headers: &HeaderMap,
    rule: &mut RuleRecord,
) -> Response {
    if let Some(problem) = validate_rule(rule) {
        return bad_request(problem);
    }
    let regex_text = rule.regex.as_deref().unwrap_or_default();
    let replacement = rule.replacement.as_deref().unwrap_or_default();
    let regex = match Regex::new(regex_text) {
        Ok(regex) => regex,
        Err(error) => return text(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    // Java applies the replacement to the pattern itself to surface invalid
    // group references before persisting.
    let _ = regex.replace_all(regex_text, replacement);

    let id = rule
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    match id {
        Some(id) if id == PRIMARY_RULE_ID => {
            return bad_request(message("rule.modify.primary.forbidden", headers));
        }
        Some(id) => rule.id = Some(id),
        None => match random_uuid() {
            Ok(id) => rule.id = Some(id),
            Err(error) => return internal_error("generate rule id", error),
        },
    }

    match admin_db::upsert_rule(&state.pool, kind, rule).await {
        Ok(()) => {
            state.formatting_cache.clear().await;
            empty_ok()
        }
        Err(error) => internal_error("save rule", error),
    }
}

async fn sonarr_rule_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_remove(&state, AppKind::Sonarr, &headers, &ids).await
}

async fn radarr_rule_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_remove(&state, AppKind::Radarr, &headers, &ids).await
}

async fn rule_remove(
    state: &AppState,
    kind: AppKind,
    headers: &HeaderMap,
    ids: &[String],
) -> Response {
    if ids.iter().any(|id| id == PRIMARY_RULE_ID) {
        return bad_request(message("rule.modify.primary.forbidden", headers));
    }
    match admin_db::delete_rules(&state.pool, kind, ids).await {
        Ok(_) => {
            state.formatting_cache.clear().await;
            empty_ok()
        }
        Err(error) => internal_error("remove rules", error),
    }
}

async fn sonarr_rule_enable(
    State(state): State<AppState>,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_status(&state, AppKind::Sonarr, &ids, 1).await
}

async fn radarr_rule_enable(
    State(state): State<AppState>,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_status(&state, AppKind::Radarr, &ids, 1).await
}

async fn sonarr_rule_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_disable(&state, AppKind::Sonarr, &headers, &ids).await
}

async fn radarr_rule_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_disable(&state, AppKind::Radarr, &headers, &ids).await
}

async fn rule_disable(
    state: &AppState,
    kind: AppKind,
    headers: &HeaderMap,
    ids: &[String],
) -> Response {
    if ids.iter().any(|id| id == PRIMARY_RULE_ID) {
        return bad_request(message("rule.modify.primary.forbidden", headers));
    }
    rule_status(state, kind, ids, 0).await
}

async fn rule_status(state: &AppState, kind: AppKind, ids: &[String], status: i64) -> Response {
    match admin_db::set_rules_status(&state.pool, kind, ids, status).await {
        Ok(_) => {
            state.formatting_cache.clear().await;
            empty_ok()
        }
        Err(error) => internal_error("change rule status", error),
    }
}

async fn sonarr_rule_export(
    State(state): State<AppState>,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_export(&state, AppKind::Sonarr, &ids).await
}

async fn radarr_rule_export(
    State(state): State<AppState>,
    Json(ids): Json<Vec<String>>,
) -> Response {
    rule_export(&state, AppKind::Radarr, &ids).await
}

async fn rule_export(state: &AppState, kind: AppKind, ids: &[String]) -> Response {
    let result = if ids.is_empty() {
        admin_db::query_rules_all(&state.pool, kind).await
    } else {
        admin_db::query_rules_by_ids(&state.pool, kind, ids).await
    };
    match result {
        Ok(rules) => Json(rules).into_response(),
        Err(error) => internal_error("export rules", error),
    }
}

async fn sonarr_rule_import(State(state): State<AppState>, multipart: Multipart) -> Response {
    rule_import(&state, AppKind::Sonarr, multipart).await
}

async fn radarr_rule_import(State(state): State<AppState>, multipart: Multipart) -> Response {
    rule_import(&state, AppKind::Radarr, multipart).await
}

async fn rule_import(state: &AppState, kind: AppKind, mut multipart: Multipart) -> Response {
    let content = match multipart.next_field().await {
        Ok(Some(field)) => match field.bytes().await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => return internal_error("read import upload", error.into()),
        },
        Ok(None) => return bad_request("file is required"),
        Err(error) => return internal_error("parse import upload", error.into()),
    };
    let mut rules: Vec<RuleRecord> = match serde_json::from_str(&content) {
        Ok(rules) => rules,
        Err(error) => return text(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    for rule in &mut rules {
        if rule.valid_status == Some(1) {
            // Java nulls `validStatus = 1` so imports never re-enable a rule
            // that an operator disabled locally.
            rule.valid_status = None;
        }
        if rule.id.as_deref().is_none_or(|id| id.trim().is_empty()) {
            match random_uuid() {
                Ok(id) => rule.id = Some(id),
                Err(error) => return internal_error("generate imported rule id", error),
            }
        }
    }
    match admin_db::upsert_rules(&state.pool, kind, &rules).await {
        Ok(()) => {
            state.formatting_cache.clear().await;
            empty_ok()
        }
        Err(error) => internal_error("import rules", error),
    }
}

async fn sonarr_rule_tokens(State(state): State<AppState>) -> Response {
    rule_tokens(&state, AppKind::Sonarr).await
}

async fn radarr_rule_tokens(State(state): State<AppState>) -> Response {
    rule_tokens(&state, AppKind::Radarr).await
}

async fn rule_tokens(state: &AppState, kind: AppKind) -> Response {
    match admin_db::rule_tokens(&state.pool, kind).await {
        Ok(tokens) => Json(tokens).into_response(),
        Err(error) => internal_error("list rule tokens", error),
    }
}

// ---------------------------------------------------------------------------
// Titles
// ---------------------------------------------------------------------------

async fn sonarr_title_sync(State(state): State<AppState>) -> Response {
    title_sync(&state, AppKind::Sonarr).await
}

async fn radarr_title_sync(State(state): State<AppState>) -> Response {
    title_sync(&state, AppKind::Radarr).await
}

async fn title_sync(state: &AppState, kind: AppKind) -> Response {
    match sync::sync_titles_once(state, kind).await {
        Ok(Some(_)) => empty_ok(),
        Ok(None) => bad_request(MSG_SYNC_TOO_OFTEN),
        Err(error) => {
            state.sync.clear_title_gate(kind).await;
            internal_error("sync titles", error)
        }
    }
}

async fn sonarr_title_query(
    State(state): State<AppState>,
    Query(query): Query<SonarrTitleQuery>,
) -> Response {
    match admin_db::query_sonarr_titles(&state.pool, &query).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => internal_error("query Sonarr titles", error),
    }
}

async fn radarr_title_query(
    State(state): State<AppState>,
    Query(query): Query<RadarrTitleQuery>,
) -> Response {
    match admin_db::query_radarr_titles(&state.pool, &query).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => internal_error("query Radarr titles", error),
    }
}

async fn sonarr_title_remove(State(state): State<AppState>, Json(ids): Json<Vec<i64>>) -> Response {
    title_remove(&state, AppKind::Sonarr, &ids).await
}

async fn radarr_title_remove(State(state): State<AppState>, Json(ids): Json<Vec<i64>>) -> Response {
    title_remove(&state, AppKind::Radarr, &ids).await
}

async fn title_remove(state: &AppState, kind: AppKind, ids: &[i64]) -> Response {
    match admin_db::delete_titles(&state.pool, kind, ids).await {
        Ok(_) => {
            state.clear_reference_caches().await;
            empty_ok()
        }
        Err(error) => internal_error("remove titles", error),
    }
}

// ---------------------------------------------------------------------------
// Examples
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExampleSaveRequest {
    #[serde(default)]
    original_text: Option<String>,
}

async fn sonarr_example_save(
    State(state): State<AppState>,
    Json(request): Json<ExampleSaveRequest>,
) -> Response {
    example_save(&state, AppKind::Sonarr, request).await
}

async fn radarr_example_save(
    State(state): State<AppState>,
    Json(request): Json<ExampleSaveRequest>,
) -> Response {
    example_save(&state, AppKind::Radarr, request).await
}

async fn example_save(state: &AppState, kind: AppKind, request: ExampleSaveRequest) -> Response {
    let Some(original_text) = request
        .original_text
        .filter(|value| !value.trim().is_empty())
    else {
        return bad_request("originalText: must not be blank");
    };
    let lines = split_java_lines(&original_text);
    let examples: Vec<(String, String)> = lines
        .into_iter()
        .map(|line| (md5_upper(&line), line))
        .collect();
    match admin_db::upsert_examples(&state.pool, kind, &examples).await {
        Ok(()) => empty_ok(),
        Err(error) => internal_error("save examples", error),
    }
}

/// `String.split("\\n")` drops trailing empty strings but keeps interior ones.
fn split_java_lines(value: &str) -> Vec<String> {
    let mut lines: Vec<String> = value.split('\n').map(str::to_owned).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

async fn sonarr_example_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ExampleQuery>,
) -> Response {
    example_query(&state, AppKind::Sonarr, &headers, &query).await
}

async fn radarr_example_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ExampleQuery>,
) -> Response {
    example_query(&state, AppKind::Radarr, &headers, &query).await
}

async fn example_query(
    state: &AppState,
    kind: AppKind,
    headers: &HeaderMap,
    query: &ExampleQuery,
) -> Response {
    let (current, page_size, offset) = query.bounds();
    let mut examples = if query.valid_status.is_none() {
        match admin_db::query_examples_page(
            &state.pool,
            kind,
            &query.original_text,
            offset,
            page_size,
        )
        .await
        {
            Ok((total, used_offset, rows)) => {
                let rows = match format_examples(state, kind, headers, rows).await {
                    Ok(rows) => rows,
                    Err(error) => return internal_error("format examples", error),
                };
                // MyBatis-Plus runs with `overflow: true`, so a page past the
                // end restarts at page one.
                let current = if total > 0 && used_offset != offset {
                    1
                } else {
                    current
                };
                return Json(Page {
                    current,
                    page_size,
                    total,
                    list: rows,
                })
                .into_response();
            }
            Err(error) => return internal_error("query examples", error),
        }
    } else {
        match admin_db::query_examples_all(&state.pool, kind, &query.original_text).await {
            Ok(rows) => rows,
            Err(error) => return internal_error("query examples", error),
        }
    };

    examples = match format_examples(state, kind, headers, examples).await {
        Ok(rows) => rows,
        Err(error) => return internal_error("format examples", error),
    };
    let wanted = query.valid_status.unwrap_or(1);
    examples.retain(|example| example.valid_status == Some(wanted));
    let total = examples.len() as i64;
    let start = offset.min(total);
    let end = (offset + page_size).min(total);
    let list = examples
        .into_iter()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect();
    Json(Page {
        current,
        page_size,
        total,
        list,
    })
    .into_response()
}

async fn format_examples(
    state: &AppState,
    kind: AppKind,
    headers: &HeaderMap,
    mut examples: Vec<admin_db::ExampleRecord>,
) -> Result<Vec<admin_db::ExampleRecord>> {
    if examples.is_empty() {
        return Ok(examples);
    }
    let format_key = match kind {
        AppKind::Sonarr => "sonarrIndexerFormat",
        AppKind::Radarr => "radarrIndexerFormat",
    };
    let snapshot = state.snapshot();
    let format = snapshot
        .query_value_by_key(format_key)
        .map(str::to_owned)
        .map_err(anyhow::Error::from)?;
    let clean_source = snapshot
        .query_value_by_key("cleanTitleRegex")
        .map(str::to_owned)
        .map_err(anyhow::Error::from)?;
    let clean_regex = Regex::new(&clean_source).map_err(anyhow::Error::from)?;
    let data = FormattingData::load_example(&state.pool, kind, &format, clean_regex).await?;
    let fail_text = message("example.match.title.fail", headers);

    for example in &mut examples {
        let Some(original) = example.original_text.clone() else {
            continue;
        };
        let formatted = data.format_example_text(&original, fail_text);
        example.valid_status = Some(if formatted == original { 0 } else { 1 });
        example.format_text = Some(formatted);
    }
    Ok(examples)
}

async fn sonarr_example_remove(
    State(state): State<AppState>,
    Json(hashes): Json<Vec<String>>,
) -> Response {
    example_remove(&state, AppKind::Sonarr, &hashes).await
}

async fn radarr_example_remove(
    State(state): State<AppState>,
    Json(hashes): Json<Vec<String>>,
) -> Response {
    example_remove(&state, AppKind::Radarr, &hashes).await
}

async fn example_remove(state: &AppState, kind: AppKind, hashes: &[String]) -> Response {
    match admin_db::delete_examples(&state.pool, kind, hashes).await {
        Ok(_) => empty_ok(),
        Err(error) => internal_error("remove examples", error),
    }
}

// ---------------------------------------------------------------------------
// Rule test
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleTestQuery {
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    replacement: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::admin_db::empty_string_as_none_i64"
    )]
    offset: Option<i64>,
    #[serde(default)]
    example: Option<String>,
}

async fn rule_test(Query(query): Query<RuleTestQuery>) -> Response {
    let (Some(regex_text), Some(replacement), Some(example)) = (
        query.regex.filter(|value| !value.trim().is_empty()),
        query.replacement.filter(|value| !value.trim().is_empty()),
        query.example.filter(|value| !value.trim().is_empty()),
    ) else {
        return bad_request("regex, replacement and example must not be blank");
    };
    let regex = match Regex::new(&regex_text) {
        Ok(regex) => regex,
        Err(error) => return text(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let mut output = String::new();
    for line in split_java_lines(&example) {
        let value = regex.replace_all(&line, replacement.as_str()).into_owned();
        output.push_str(&crate::text::execute_offset(
            &value,
            query.offset.unwrap_or(0),
        ));
        output.push('\n');
    }
    text(StatusCode::OK, output)
}

// ---------------------------------------------------------------------------
// TMDB titles
// ---------------------------------------------------------------------------

async fn tmdb_title_sync(State(state): State<AppState>) -> Response {
    // Java returns success without touching the interval gate when the
    // need-sync query is empty.
    if matches!(
        crate::db::list_tvdb_ids_needing_tmdb(&state.pool).await,
        Ok(ids) if ids.is_empty()
    ) {
        return empty_ok();
    }
    match sync::sync_tmdb_once(&state).await {
        Ok(Some(_)) => empty_ok(),
        Ok(None) => bad_request(MSG_SYNC_TOO_OFTEN),
        Err(error) => {
            state.sync.clear_tmdb_gate().await;
            internal_error("sync TMDB titles", error)
        }
    }
}

async fn tmdb_title_query(
    State(state): State<AppState>,
    Query(query): Query<TmdbTitleQuery>,
) -> Response {
    let page = match admin_db::query_tmdb_titles(&state.pool, &query).await {
        Ok(page) => page,
        Err(error) => return internal_error("query TMDB titles", error),
    };
    let snapshot = state.snapshot();
    let clean_source = match snapshot.query_value_by_key("cleanTitleRegex") {
        Ok(value) => value.to_owned(),
        Err(error) => return internal_error("clean title regex", anyhow::Error::from(error)),
    };
    let clean_regex = match Regex::new(&clean_source) {
        Ok(regex) => regex,
        Err(error) => return internal_error("compile clean title regex", error.into()),
    };
    let list = page
        .list
        .into_iter()
        .map(|mut title| {
            title.clean_title = Some(crate::text::clean_title(&title.title, &clean_regex));
            title
        })
        .collect();
    Json(Page {
        current: page.current,
        page_size: page.page_size,
        total: page.total,
        list,
    })
    .into_response()
}

async fn tmdb_title_remove(State(state): State<AppState>, Json(ids): Json<Vec<i64>>) -> Response {
    match admin_db::delete_tmdb_titles(&state.pool, &ids).await {
        Ok(_) => {
            state.clear_reference_caches().await;
            empty_ok()
        }
        Err(error) => internal_error("remove TMDB titles", error),
    }
}

async fn tmdb_title_save(
    State(state): State<AppState>,
    Json(mut title): Json<TmdbTitleSave>,
) -> Response {
    if title.tmdb_id.is_none()
        && let Some(tvdb_id) = title.tvdb_id
    {
        match admin_db::first_tmdb_id_by_tvdb(&state.pool, tvdb_id).await {
            Ok(Some(tmdb_id)) => title.tmdb_id = Some(tmdb_id),
            Ok(None) => {}
            Err(error) => return internal_error("look up TMDB title", error),
        }
    }
    match admin_db::save_tmdb_title(&state.pool, &title).await {
        Ok(()) => {
            state.clear_reference_caches().await;
            empty_ok()
        }
        Err(error) => internal_error("save TMDB title", error),
    }
}

// ---------------------------------------------------------------------------
// System config and cache
// ---------------------------------------------------------------------------

async fn system_config_version(State(state): State<AppState>) -> Response {
    let local = env!("CARGO_PKG_VERSION");
    let url = state.snapshot().version_check_url.clone();
    match latest_release_version(&state, &url).await {
        Some(latest) if latest != local => text(StatusCode::OK, format!("{local} 🚨")),
        _ => text(StatusCode::OK, local),
    }
}

/// Java `SystemConfigController.version`: read `tag_name`, strip `v`, compare
/// with the local version. Any failure returns the local version.
async fn latest_release_version(state: &AppState, url: &str) -> Option<String> {
    let timeout = state.snapshot().request_timeout.min(Duration::from_secs(5));
    let request = state
        .upstream
        .http_client()
        .get(url)
        .header("accept", "application/vnd.github+json")
        .send();
    match tokio::time::timeout(timeout, request).await {
        Ok(Ok(response)) if response.status().is_success() => {
            match response.json::<Value>().await {
                Ok(payload) => payload
                    .get("tag_name")
                    .and_then(Value::as_str)
                    .map(|tag| tag.replace('v', "")),
                Err(error) => {
                    tracing::debug!(
                        error = %error.without_url(),
                        "parse latest release response failed"
                    );
                    None
                }
            }
        }
        Ok(Ok(response)) => {
            tracing::debug!(status = %response.status(), "latest release query failed");
            None
        }
        Ok(Err(error)) => {
            tracing::debug!(
                error = %error.without_url(),
                "latest release query failed"
            );
            None
        }
        Err(_) => {
            tracing::debug!("latest release query timed out");
            None
        }
    }
}

async fn system_config_query(State(state): State<AppState>) -> Response {
    // Java's controller runs the Sonarr rename task before returning config.
    if let Err(error) = crate::rename::sonarr_rename_once(&state).await {
        tracing::debug!(%error, "config query rename pass failed");
    }
    match admin_db::query_system_config(&state.pool).await {
        Ok(configs) => Json(configs).into_response(),
        Err(error) => internal_error("query system config", error),
    }
}

fn valid_code(flag: bool) -> i64 {
    if flag { 1 } else { 0 }
}

/// Java normalizes a non-blank Transmission URL by removing one trailing
/// slash, stripping `/transmission/web`, and appending `/transmission/rpc`.
fn normalize_transmission_url(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_owned();
    }
    let mut url = value.strip_suffix('/').unwrap_or(value).to_owned();
    if !url.ends_with("/transmission/rpc") {
        url = url.replace("/transmission/web", "") + "/transmission/rpc";
    }
    url
}

/// Java `SystemConfigServiceImpl.updateSystemConfig`.
async fn apply_live_validation(configs: &mut [SystemConfigRecord], state: &AppState) {
    let positions: HashMap<String, usize> = configs
        .iter()
        .enumerate()
        .filter_map(|(index, config)| config.key.clone().map(|key| (key, index)))
        .collect();
    let value = |configs: &[SystemConfigRecord], key: &str| -> Option<String> {
        positions
            .get(key)
            .and_then(|index| configs[*index].value.clone())
    };
    let set = |configs: &mut [SystemConfigRecord], key: &str, status: i64| {
        if let Some(index) = positions.get(key) {
            configs[*index].valid_status = Some(status);
        }
    };

    let sonarr = state
        .prober
        .check_sonarr_or_radarr(
            value(configs, "sonarrUrl").as_deref(),
            value(configs, "sonarrApikey").as_deref(),
        )
        .await;
    set(configs, "sonarrUrl", valid_code(sonarr.url_valid()));
    set(configs, "sonarrApikey", valid_code(sonarr.api_key_valid()));

    if let Some(format) = value(configs, "sonarrIndexerFormat") {
        set(
            configs,
            "sonarrIndexerFormat",
            valid_code(crate::probe::check_sonarr_indexer_format(Some(&format))),
        );
    }

    let radarr = state
        .prober
        .check_sonarr_or_radarr(
            value(configs, "radarrUrl").as_deref(),
            value(configs, "radarrApikey").as_deref(),
        )
        .await;
    set(configs, "radarrUrl", valid_code(radarr.url_valid()));
    set(configs, "radarrApikey", valid_code(radarr.api_key_valid()));

    if let Some(format) = value(configs, "radarrIndexerFormat") {
        set(
            configs,
            "radarrIndexerFormat",
            valid_code(crate::probe::check_radarr_indexer_format(Some(&format))),
        );
    }

    let qbittorrent_url = value(configs, "qbittorrentUrl");
    let qbittorrent_url_ok = state.prober.check_url(qbittorrent_url.as_deref()).await;
    set(configs, "qbittorrentUrl", valid_code(qbittorrent_url_ok));
    set(configs, "qbittorrentUsername", 0);
    set(configs, "qbittorrentPassword", 0);
    if qbittorrent_url_ok && let Some(url) = qbittorrent_url.as_deref() {
        let username = value(configs, "qbittorrentUsername").unwrap_or_default();
        let password = value(configs, "qbittorrentPassword").unwrap_or_default();
        let logged_in = state
            .downloaders
            .qbittorrent
            .login_with(url, &username, &password)
            .await;
        set(configs, "qbittorrentUsername", valid_code(logged_in));
        set(configs, "qbittorrentPassword", valid_code(logged_in));
    }

    if let Some(index) = positions.get("transmissionUrl")
        && let Some(url) = configs[*index].value.clone()
    {
        configs[*index].value = Some(normalize_transmission_url(&url));
    }
    let transmission_url = value(configs, "transmissionUrl");
    let transmission_url_ok = state.prober.check_url(transmission_url.as_deref()).await;
    set(configs, "transmissionUrl", valid_code(transmission_url_ok));
    set(configs, "transmissionUsername", 0);
    set(configs, "transmissionPassword", 0);
    if transmission_url_ok && let Some(url) = transmission_url.as_deref() {
        let username = value(configs, "transmissionUsername").unwrap_or_default();
        let password = value(configs, "transmissionPassword").unwrap_or_default();
        let logged_in = state
            .downloaders
            .transmission
            .login_with(url, &username, &password)
            .await;
        set(configs, "transmissionUsername", valid_code(logged_in));
        set(configs, "transmissionPassword", valid_code(logged_in));
    }

    for key in ["jackettUrl", "prowlarrUrl"] {
        if let Some(url) = value(configs, key) {
            set(
                configs,
                key,
                valid_code(state.prober.check_url(Some(&url)).await),
            );
        }
    }

    let tmdb = state
        .prober
        .check_tmdb(
            value(configs, "tmdbUrl").as_deref(),
            value(configs, "tmdbApikey").as_deref(),
        )
        .await;
    set(configs, "tmdbUrl", valid_code(tmdb.url_valid()));
    set(configs, "tmdbApikey", valid_code(tmdb.api_key_valid()));

    if let Some(regex) = value(configs, "cleanTitleRegex") {
        set(
            configs,
            "cleanTitleRegex",
            valid_code(crate::probe::check_regex(Some(&regex))),
        );
    }
}

async fn system_config_update(
    State(state): State<AppState>,
    Json(mut configs): Json<Vec<SystemConfigRecord>>,
) -> Response {
    apply_live_validation(&mut configs, &state).await;
    if let Err(error) = admin_db::update_system_config(&state.pool, &configs).await {
        return internal_error("update system config", error);
    }
    // Java evicts the SYSTEM_CONFIG cache and the three title-sync interval
    // caches. `refresh_config` also drops formatting/title caches so the next
    // request sees the new formats, languages, URLs and credentials.
    if let Err(error) = state.refresh_config().await {
        return internal_error("refresh system config", error);
    }
    empty_ok()
}

async fn system_config_authors(State(state): State<AppState>) -> Response {
    Json(fetch_author_list(&state).await).into_response()
}

async fn fetch_author_list(state: &AppState) -> Vec<String> {
    const FALLBACK: &str = "LuckyPuppy514";
    let snapshot = state.snapshot();
    let Some(location) = snapshot.rule_locations().into_iter().next() else {
        return vec![FALLBACK.to_owned()];
    };
    let url = format!("{}/author.json", location.trim_end_matches('/'));
    let request = state.upstream.http_client().get(url).send();
    let timeout = snapshot.request_timeout.min(Duration::from_secs(5));
    match tokio::time::timeout(timeout, request).await {
        Ok(Ok(response)) if response.status().is_success() => {
            match response.json::<Vec<String>>().await {
                Ok(authors) if !authors.is_empty() => authors,
                Ok(_) => vec![FALLBACK.to_owned()],
                Err(error) => {
                    tracing::error!(%error, "parse author list failed");
                    vec![FALLBACK.to_owned()]
                }
            }
        }
        Ok(Ok(response)) => {
            tracing::error!(status = %response.status(), "fetch author list failed");
            vec![FALLBACK.to_owned()]
        }
        Ok(Err(error)) => {
            tracing::error!(%error, "fetch author list failed");
            vec![FALLBACK.to_owned()]
        }
        Err(_) => {
            tracing::error!("fetch author list timed out");
            vec![FALLBACK.to_owned()]
        }
    }
}

async fn system_cache_clear_all(State(state): State<AppState>) -> Response {
    state.result_cache.clear().await;
    state.formatting_cache.clear().await;
    state.title_cache.clear().await;
    state.sync.clear_all_gates().await;
    // Java `clearAll` also clears the token blacklist cache.
    state.auth.clear_blacklist().await;
    empty_ok()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheClearRequest {
    #[serde(default)]
    cache_name: Option<String>,
}

async fn system_cache_clear(
    State(state): State<AppState>,
    Json(request): Json<CacheClearRequest>,
) -> Response {
    let Some(name) = request.cache_name.filter(|name| !name.trim().is_empty()) else {
        return bad_request("cacheName: must not be blank");
    };
    match name.as_str() {
        "indexer_result" => state.result_cache.clear().await,
        "token_black_list" => state.auth.clear_blacklist().await,
        "system_config" => {
            // Java clears the SYSTEM_CONFIG cache, so the next read reloads
            // the database. Re-resolve now and drop dependent caches.
            if let Err(error) = state.refresh_config().await {
                return internal_error("refresh system config", error);
            }
        }
        "sonarr_title_sync_interval" => {
            state.sync.clear_title_gate(AppKind::Sonarr).await;
        }
        "radarr_title_sync_interval" => {
            state.sync.clear_title_gate(AppKind::Radarr).await;
        }
        "tmdb_title_sync_interval" => state.sync.clear_tmdb_gate().await,
        "sonarr_rule"
        | "radarr_rule"
        | "sonarr_search_title"
        | "radarr_search_title"
        | "indexer_search_offset"
        | "sonarr_result_title"
        | "radarr_result_title" => {
            state.formatting_cache.clear().await;
            state.title_cache.clear().await;
        }
        _ => {}
    }
    empty_ok()
}

/// Reserved API paths must return 404 instead of the SPA index page.
pub async fn reserved_not_found() -> Response {
    (StatusCode::NOT_FOUND, "").into_response()
}
