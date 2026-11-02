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
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
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
    pub q: Option<String>,
    /// The raw query string exactly as received upstream (minus the leading `?`).
    pub query: String,
}

#[derive(Default)]
struct Plan {
    by_query: HashMap<String, MockResponse>,
    /// When non-empty, consumed in order and overrides `by_query`.
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

async fn handle(State(state): State<MockState>, RawQuery(query): RawQuery) -> Response {
    let query = query.unwrap_or_default();
    let current = state.inner.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state
        .inner
        .max_in_flight
        .fetch_max(current, Ordering::SeqCst);

    let q = QueryParams::parse(&query).get("q").map(str::to_owned);
    state.inner.log.lock().unwrap().push(Recorded {
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
            sonarr_rules_url: None,
            radarr_rules_url: None,
            clean_title_regex: DEFAULT_CLEAN_TITLE_REGEX.to_owned(),
            sonarr_indexer_format: DEFAULT_SONARR_FORMAT.to_owned(),
            radarr_indexer_format: DEFAULT_RADARR_FORMAT.to_owned(),
            min_count: 6,
            cache_expires: Duration::from_secs(60),
            sync_interval: Duration::ZERO,
            indexer_result_cache_expires: Duration::from_secs(15),
            rename_file: false,
            login_enabled: false,
            sync_enabled: false,
            request_timeout: Duration::from_secs(5),
            result_cache_max_entries: 100,
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
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
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
