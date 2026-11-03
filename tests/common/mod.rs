//! Shared helpers for the integration test harness.
//!
//! Provides an in-process mock Torznab upstream plus a temporary jproxy
//! database so the real Axum router, SQLite queries, search planner and XML
//! formatter can be exercised end to end without any live service.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use jproxy_rs::app::{AppState, router as app_router};
use jproxy_rs::config::{
    Config, DEFAULT_CLEAN_TITLE_REGEX, DEFAULT_RADARR_FORMAT, DEFAULT_SONARR_FORMAT,
};
use jproxy_rs::db;
use jproxy_rs::query::QueryParams;
use jproxy_rs::upstream::RetryPolicy;
use sqlx::SqlitePool;
use tempfile::{TempDir, tempdir};
use tokio::net::TcpListener;
use tower::ServiceExt;

/// A Torznab feed with a channel but no items.
pub const EMPTY_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>Fixture Indexer</title>
  </channel>
</rss>
"#;

#[derive(Clone, Debug)]
pub struct MockResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
    pub delay: Duration,
    pub retry_after: Option<String>,
}

impl MockResponse {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "application/xml; charset=utf-8".to_owned(),
            delay: Duration::ZERO,
            retry_after: None,
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::ok(String::new())
        }
    }

    pub fn json(body: impl Into<String>) -> Self {
        Self {
            content_type: "application/json".to_owned(),
            ..Self::ok(body)
        }
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn with_retry_after(mut self, value: &str) -> Self {
        self.retry_after = Some(value.to_owned());
        self
    }
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: String,
    pub q: Option<String>,
    /// The raw query string exactly as received upstream (minus the leading `?`).
    pub query: String,
}

#[derive(Default)]
struct Plan {
    by_query: HashMap<String, MockResponse>,
    by_path: HashMap<String, MockResponse>,
    /// When non-empty, consumed in order and overrides `by_path`/`by_query`.
    script: Vec<MockResponse>,
    position: usize,
    fallback: Option<MockResponse>,
}

struct Inner {
    log: Mutex<Vec<Recorded>>,
    plan: Mutex<Plan>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

#[derive(Clone)]
struct MockState {
    inner: Arc<Inner>,
}

/// An in-process HTTP server that answers every path as a Torznab indexer.
pub struct MockIndexer {
    pub addr: SocketAddr,
    state: MockState,
}

impl MockIndexer {
    pub async fn start() -> Self {
        let state = MockState {
            inner: Arc::new(Inner {
                log: Mutex::new(Vec::new()),
                plan: Mutex::new(Plan::default()),
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
            }),
        };
        let app = Router::new().fallback(handle).with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, state }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Return `response` whenever the decoded `q` parameter equals `q`.
    pub fn route(&self, q: &str, response: MockResponse) {
        self.state
            .inner
            .plan
            .lock()
            .unwrap()
            .by_query
            .insert(q.to_owned(), response);
    }

    /// Return `response` whenever the request path matches exactly.
    pub fn route_path(&self, path: &str, response: MockResponse) {
        self.state
            .inner
            .plan
            .lock()
            .unwrap()
            .by_path
            .insert(path.to_owned(), response);
    }

    /// Response used when no `q` route matches.
    pub fn fallback(&self, response: MockResponse) {
        self.state.inner.plan.lock().unwrap().fallback = Some(response);
    }

    /// Answer requests in order, independent of `q`.
    pub fn script(&self, responses: Vec<MockResponse>) {
        let mut plan = self.state.inner.plan.lock().unwrap();
        plan.script = responses;
        plan.position = 0;
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.state.inner.log.lock().unwrap().clone()
    }

    /// Decoded `q` values in arrival order.
    pub fn recorded_queries(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .filter_map(|record| record.q)
            .collect()
    }

    /// Full raw query strings in arrival order.
    pub fn recorded_full_queries(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .map(|record| record.query)
            .collect()
    }

    /// Request paths in arrival order.
    pub fn recorded_paths(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .map(|record| record.path)
            .collect()
    }

    pub fn request_count(&self) -> usize {
        self.state.inner.log.lock().unwrap().len()
    }

    pub fn max_concurrency(&self) -> usize {
        self.state.inner.max_in_flight.load(Ordering::SeqCst)
    }

    pub fn reset_log(&self) {
        self.state.inner.log.lock().unwrap().clear();
        self.state.inner.max_in_flight.store(0, Ordering::SeqCst);
    }
}

/// A canned response for `MockHttp`.
#[derive(Clone, Debug)]
pub struct MockHttpReply {
    pub status: u16,
    pub body: String,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
}

impl MockHttpReply {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: String::new(),
            content_type: "text/plain; charset=utf-8".to_owned(),
            headers: Vec::new(),
        }
    }

    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "text/plain; charset=utf-8".to_owned(),
            headers: Vec::new(),
        }
    }

    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "application/json".to_owned(),
            headers: Vec::new(),
        }
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

#[derive(Clone, Debug)]
pub struct MockHttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: String,
}

impl MockHttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Decode an `application/x-www-form-urlencoded` request body.
    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    pub fn form_value(&self, name: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find_map(|(key, value)| (key == name).then_some(value))
    }
}

#[derive(Default)]
struct HttpPlan {
    routes: HashMap<(String, String), Vec<MockHttpReply>>,
}

struct HttpInner {
    log: Mutex<Vec<MockHttpRequest>>,
    plan: Mutex<HttpPlan>,
}

#[derive(Clone)]
struct MockHttpState {
    inner: Arc<HttpInner>,
}

/// A generic in-process HTTP mock for downloader and Sonarr/Radarr APIs.
///
/// Routes match on method and exact path. A single reply repeats; a sequence
/// is consumed in order.
pub struct MockHttp {
    pub addr: SocketAddr,
    state: MockHttpState,
}

impl MockHttp {
    pub async fn start() -> Self {
        let state = MockHttpState {
            inner: Arc::new(HttpInner {
                log: Mutex::new(Vec::new()),
                plan: Mutex::new(HttpPlan::default()),
            }),
        };
        let app = Router::new()
            .fallback(handle_http)
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, state }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn route(&self, method: &str, path: &str, reply: MockHttpReply) {
        self.state
            .inner
            .plan
            .lock()
            .unwrap()
            .routes
            .insert((method.to_owned(), path.to_owned()), vec![reply]);
    }

    pub fn route_sequence(&self, method: &str, path: &str, replies: Vec<MockHttpReply>) {
        assert!(!replies.is_empty(), "a route sequence needs one reply");
        self.state
            .inner
            .plan
            .lock()
            .unwrap()
            .routes
            .insert((method.to_owned(), path.to_owned()), replies);
    }

    pub fn requests(&self) -> Vec<MockHttpRequest> {
        self.state.inner.log.lock().unwrap().clone()
    }

    pub fn requests_for(&self, method: &str, path: &str) -> Vec<MockHttpRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.method == method && request.path == path)
            .collect()
    }

    pub fn request_count(&self) -> usize {
        self.state.inner.log.lock().unwrap().len()
    }
}

async fn handle_http(State(state): State<MockHttpState>, request: Request<Body>) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let headers = request.headers().clone();
    let body = request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    state.inner.log.lock().unwrap().push(MockHttpRequest {
        method: method.clone(),
        path: path.clone(),
        query,
        headers,
        body,
    });

    let reply = {
        let mut plan = state.inner.plan.lock().unwrap();
        match plan.routes.get_mut(&(method, path)) {
            Some(replies) if replies.len() > 1 => replies.remove(0),
            Some(replies) => replies[0].clone(),
            None => MockHttpReply::status(404),
        }
    };

    let status = StatusCode::from_u16(reply.status).unwrap();
    let mut result = (status, reply.body).into_response();
    if let Ok(value) = HeaderValue::from_str(&reply.content_type) {
        result.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    for (name, value) in reply.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            result.headers_mut().insert(name, value);
        }
    }
    result
}

async fn handle(State(state): State<MockState>, uri: Uri, RawQuery(query): RawQuery) -> Response {
    let query = query.unwrap_or_default();
    let path = uri.path().to_owned();
    let current = state.inner.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state
        .inner
        .max_in_flight
        .fetch_max(current, Ordering::SeqCst);

    let q = QueryParams::parse(&query).get("q").map(str::to_owned);
    state.inner.log.lock().unwrap().push(Recorded {
        path: path.clone(),
        q: q.clone(),
        query: query.clone(),
    });

    let response = {
        let mut plan = state.inner.plan.lock().unwrap();
        let scripted = if plan.script.is_empty() {
            None
        } else {
            let index = plan.position.min(plan.script.len() - 1);
            plan.position += 1;
            Some(plan.script[index].clone())
        };
        scripted
            .or_else(|| plan.by_path.get(&path).cloned())
            .or_else(|| plan.by_query.get(q.as_deref()?).cloned())
            .or_else(|| plan.fallback.clone())
            .unwrap_or_else(|| MockResponse::ok(EMPTY_FEED))
    };

    if !response.delay.is_zero() {
        tokio::time::sleep(response.delay).await;
    }
    state.inner.in_flight.fetch_sub(1, Ordering::SeqCst);

    let status = StatusCode::from_u16(response.status).unwrap();
    let mut result = (status, response.body).into_response();
    if let Ok(value) = HeaderValue::from_str(&response.content_type) {
        result.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    if let Some(retry_after) = response.retry_after
        && let Ok(value) = HeaderValue::from_str(&retry_after)
    {
        result.headers_mut().insert(header::RETRY_AFTER, value);
    }
    result
}

/// A temporary database plus a mock upstream, wired into a `Config`.
pub struct TestEnv {
    _dir: TempDir,
    pub db_path: PathBuf,
    pub mock: MockIndexer,
}

impl TestEnv {
    pub async fn start() -> Self {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("jproxy.db");
        let mock = MockIndexer::start().await;
        Self {
            _dir: dir,
            db_path,
            mock,
        }
    }

    pub async fn pool(&self) -> SqlitePool {
        db::connect(&self.db_path).await.unwrap()
    }

    /// Execute one or more SQL statements against the test database.
    pub async fn exec(&self, sql: &str) {
        let pool = self.pool().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    pub fn config(&self) -> Config {
        Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            database_path: self.db_path.clone(),
            prowlarr_url: Some(self.mock.base_url()),
            jackett_url: None,
            sonarr_url: None,
            sonarr_api_key: None,
            radarr_url: None,
            radarr_api_key: None,
            tmdb_url: None,
            tmdb_api_key: None,
            sonarr_language_1: None,
            sonarr_language_2: None,
            rule_location: None,
            rule_location_backup: None,
            rule_sync_authors: None,
            qbittorrent_url: None,
            qbittorrent_username: None,
            qbittorrent_password: None,
            transmission_url: None,
            transmission_username: None,
            transmission_password: None,
            clean_title_regex: DEFAULT_CLEAN_TITLE_REGEX.to_owned(),
            sonarr_indexer_format: DEFAULT_SONARR_FORMAT.to_owned(),
            radarr_indexer_format: DEFAULT_RADARR_FORMAT.to_owned(),
            min_count: 6,
            cache_expires: Duration::from_secs(60),
            sync_interval: Duration::ZERO,
            indexer_result_cache_expires: Duration::from_secs(15),
            sonarr_rename_fallback: Duration::from_secs(1800),
            radarr_rename_fallback: Duration::from_secs(1800),
            rename_file: false,
            token_expires: Duration::from_secs(10080 * 60),
            version_check_url: format!(
                "{}/repos/LuckyPuppy514/jproxy/releases/latest",
                self.mock.base_url()
            ),
            admin_ui_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("admin_dist"),
            login_enabled: false,
            sync_enabled: false,
            request_timeout: Duration::from_secs(5),
            on_demand_sync_timeout: Duration::from_secs(5),
            result_cache_max_entries: 100,
            cache_max_entries: 100,
            upstream_max_concurrency: 8,
            upstream_retry: RetryPolicy {
                max_retries: 2,
                base_delay: Duration::from_millis(5),
                max_delay: Duration::from_millis(50),
            },
        }
    }

    pub async fn router_with(&self, config: Config) -> Router {
        app_router(AppState::new(config).await.unwrap())
    }

    pub async fn router(&self) -> Router {
        self.router_with(self.config()).await
    }
}

pub struct HttpResult {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl HttpResult {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    pub fn item_count(&self) -> usize {
        jproxy_rs::torznab::count_items(&self.body)
    }

    pub fn item_titles(&self) -> Vec<String> {
        let mut titles = Vec::new();
        let mut cursor = 0;
        while let Some(start) = self.body[cursor..].find("<title>") {
            let start = cursor + start + "<title>".len();
            let Some(end) = self.body[start..].find("</title>") else {
                break;
            };
            titles.push(self.body[start..start + end].to_owned());
            cursor = start + end;
        }
        // The first <title> is the channel title, not an item.
        titles.into_iter().skip(1).collect()
    }
}

/// Issue a GET request through the router.
pub async fn get(router: &Router, uri: &str) -> HttpResult {
    request(router, Method::GET, uri, &[], "").await
}

/// Issue a request with explicit headers and body through the router.
pub async fn request(
    router: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> HttpResult {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body.to_owned())).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    HttpResult {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Issue an authenticated GET request.
pub async fn get_auth(router: &Router, uri: &str, token: &str) -> HttpResult {
    request(
        router,
        Method::GET,
        uri,
        &[("authorization", &format!("Bearer {token}"))],
        "",
    )
    .await
}

/// Issue an authenticated JSON POST request.
pub async fn post_json_auth(router: &Router, uri: &str, token: &str, body: &str) -> HttpResult {
    request(
        router,
        Method::POST,
        uri,
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/json"),
        ],
        body,
    )
    .await
}

/// Issue an unauthenticated JSON POST request.
pub async fn post_json(router: &Router, uri: &str, body: &str) -> HttpResult {
    request(
        router,
        Method::POST,
        uri,
        &[("content-type", "application/json")],
        body,
    )
    .await
}
