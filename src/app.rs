use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::Serialize;
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;

use crate::auth::AuthState;
use crate::cache::{Expiry, TtlCache};
use crate::config::{Config, ConfigSnapshot, ConfigStore};
use crate::db;
use crate::downloader::Downloaders;
use crate::formatting::FormattingData;
use crate::models::{AppKind, MatchedTitle, Provider};
use crate::probe::Prober;
use crate::query::QueryParams;
use crate::search::{SearchPage, SearchRun, build_search_steps, execute_search};
use crate::sync::SyncState;
use crate::text::{remove_episode, remove_year};
use crate::torznab;
use crate::upstream::{UpstreamClient, UpstreamError, validate_indexer_id};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ConfigStore>,
    pub pool: SqlitePool,
    pub upstream: UpstreamClient,
    pub downloaders: Downloaders,
    pub prober: Prober,
    pub result_cache: Arc<TtlCache<Arc<CachedBody>>>,
    pub formatting_cache: Arc<TtlCache<Option<Arc<FormattingData>>>>,
    pub title_cache: Arc<TtlCache<TitleLookup>>,
    pub sync: Arc<SyncState>,
    pub auth: Arc<AuthState>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TitleLookup {
    pub matched: Option<MatchedTitle>,
    pub tmdb_titles: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CachedBody {
    pub body: Arc<String>,
    pub content_type: String,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self> {
        let pool = db::connect(&config.database_path).await?;
        let config_store = ConfigStore::new(config)?;
        match db::load_system_config_rows(&pool).await {
            Ok(rows) => config_store.apply_rows(&rows),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "system_config values unavailable; using environment and defaults"
                );
            }
        }
        let snapshot = config_store.snapshot();
        let upstream = UpstreamClient::with_policy(
            snapshot.prowlarr_url.clone(),
            snapshot.jackett_url.clone(),
            snapshot.request_timeout,
            snapshot.upstream_retry,
            snapshot.upstream_max_concurrency,
        )?;
        let downloaders = Downloaders::new(upstream.http_client().clone());
        let result_cache = Arc::new(TtlCache::with_expiry(
            snapshot.indexer_result_cache_expires,
            snapshot.result_cache_max_entries,
            Expiry::AfterCreate,
        ));
        let formatting_cache = Arc::new(TtlCache::with_expiry(
            snapshot.cache_expires,
            2,
            Expiry::AfterAccess,
        ));
        let title_cache = Arc::new(TtlCache::with_expiry(
            snapshot.cache_expires,
            snapshot.cache_max_entries,
            Expiry::AfterAccess,
        ));
        let auth = Arc::new(AuthState::new(
            snapshot.login_enabled,
            snapshot.token_expires,
        )?);
        Ok(Self {
            config: Arc::new(config_store),
            pool,
            upstream,
            downloaders,
            prober: Prober::new()?,
            result_cache,
            formatting_cache,
            title_cache,
            sync: Arc::new(SyncState::new()),
            auth,
        })
    }

    /// Current resolved configuration. The snapshot is replaced after a
    /// successful `system_config` update.
    pub fn snapshot(&self) -> ConfigSnapshot {
        self.config.snapshot()
    }

    /// Reload `system_config`, refresh provider URLs, and clear the caches that
    /// Java evicts on update.
    pub async fn refresh_config(&self) -> Result<()> {
        let rows = db::load_system_config_rows(&self.pool).await?;
        self.config.apply_rows(&rows);
        let snapshot = self.config.snapshot();
        self.upstream.update_urls(
            snapshot.prowlarr_url.as_deref(),
            snapshot.jackett_url.as_deref(),
        );
        self.sync.clear_all_gates().await;
        self.formatting_cache.clear().await;
        self.title_cache.clear().await;
        Ok(())
    }

    async fn formatting_data(&self, kind: AppKind) -> Result<Option<Arc<FormattingData>>> {
        let key = kind.as_str();
        if let Some(cached) = self.formatting_cache.get(key).await {
            return Ok(cached);
        }
        let snapshot = self.snapshot();
        let format_key = match kind {
            AppKind::Sonarr => "sonarrIndexerFormat",
            AppKind::Radarr => "radarrIndexerFormat",
        };
        let format = match snapshot.query_value_by_key(format_key) {
            Ok(format) => format.to_owned(),
            Err(error) => {
                // Java catches the exception in `executeFormatRule` and returns
                // the unmodified feed.
                tracing::debug!(%kind, %error, "indexer format unavailable; leaving XML unchanged");
                return Ok(None);
            }
        };
        if snapshot.query_value_by_key("cleanTitleRegex").is_err() {
            tracing::debug!(%kind, "clean-title regex invalid; leaving XML unchanged");
            return Ok(None);
        }
        let data = FormattingData::load(
            &self.pool,
            kind,
            &format,
            (*snapshot.clean_title_regex).clone(),
        )
        .await?;
        let data = data.map(Arc::new);
        self.formatting_cache
            .insert(key.to_owned(), data.clone())
            .await;
        Ok(data)
    }

    /// Resolve a query title, triggering Java-compatible on-demand sync on a
    /// miss. Results, including a synthetic miss, are cached under
    /// `CACHE_EXPIRES` and cleared by the next successful sync.
    async fn title_lookup(&self, kind: AppKind, title: &str) -> Result<TitleLookup> {
        let key = format!("{}:{title}", kind.as_str());
        if let Some(cached) = self.title_cache.get(&key).await {
            return Ok(cached);
        }

        let snapshot = self.snapshot();
        let mut matched =
            db::find_title(&self.pool, kind, title, &snapshot.clean_title_regex).await?;
        if matched.is_none() {
            matched = crate::sync::sync_and_get_title(self, kind, title).await;
        }

        let tmdb_titles = if kind == AppKind::Sonarr
            && matched
                .as_ref()
                .is_some_and(|matched| matches!(matched.sno, 0 | 1))
            && let Some(tvdb_id) = matched.as_ref().and_then(|matched| matched.tvdb_id)
        {
            match db::query_tmdb_titles(&self.pool, tvdb_id).await {
                Ok(titles) => titles,
                Err(error) => {
                    tracing::warn!(
                        %kind,
                        %error,
                        "TMDB title lookup failed; continuing without alternates"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let lookup = TitleLookup {
            matched,
            tmdb_titles,
        };
        // Java caches the complete search-title result, including the synthetic
        // title it uses when synchronization cannot find a match. The entry is
        // cleared by the next successful reference-data sync.
        self.title_cache.insert(key, lookup.clone()).await;
        Ok(lookup)
    }

    /// Clear caches whose Java counterparts are evicted by title/rule sync.
    pub async fn clear_reference_caches(&self) {
        self.formatting_cache.clear().await;
        self.title_cache.clear().await;
    }
}

pub fn router(state: AppState) -> Router {
    let trace = TraceLayer::new_for_http().make_span_with(|request: &Request<Body>| {
        tracing::info_span!(
            "http_request",
            method = %request.method(),
            path = %request.uri().path()
        )
    });

    Router::new()
        .route("/sonarr/prowlarr/{indexer_id}/api", get(sonarr_prowlarr))
        .route("/sonarr/jackett/{indexer_id}/api", get(sonarr_jackett))
        .route("/radarr/prowlarr/{indexer_id}/api", get(radarr_prowlarr))
        .route("/radarr/jackett/{indexer_id}/api", get(radarr_jackett))
        .route("/health", get(health))
        .merge(crate::admin::router(state.clone()))
        // Unknown reserved paths must not fall through to the SPA. The admin
        // router owns `/api/**`, including its unknown paths.
        .route("/sonarr/{*rest}", any(crate::admin::reserved_not_found))
        .route("/radarr/{*rest}", any(crate::admin::reserved_not_found))
        .fallback(static_fallback)
        .layer(trace)
        .with_state(state)
}

/// Serve the compiled Java admin UI with SPA fallback.
///
/// The fallback is reached only for paths that did not match a proxy, health,
/// admin API or reserved-prefix route, so the UI can never shadow those.
async fn static_fallback(State(state): State<AppState>, uri: Uri) -> Response {
    let path = uri.path();
    if path == "/health"
        || path == "/api"
        || path.starts_with("/api/")
        || path == "/sonarr"
        || path.starts_with("/sonarr/")
        || path == "/radarr"
        || path.starts_with("/radarr/")
    {
        return (StatusCode::NOT_FOUND, "").into_response();
    }

    let relative = path.trim_start_matches('/');
    if relative.is_empty() {
        return serve_index(&state).await;
    }
    if relative
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == ".." || part.contains('\\'))
    {
        return (StatusCode::BAD_REQUEST, "").into_response();
    }

    let candidate = state.snapshot().admin_ui_dir.join(relative);
    match tokio::fs::read(&candidate).await {
        Ok(bytes) => file_response(relative, bytes),
        Err(_) => serve_index(&state).await,
    }
}

async fn serve_index(state: &AppState) -> Response {
    let index = state.snapshot().admin_ui_dir.join("index.html");
    match tokio::fs::read(&index).await {
        Ok(bytes) => file_response("index.html", bytes),
        Err(_) => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

fn file_response(path: &str, bytes: Vec<u8>) -> Response {
    let content_type = match path.rsplit('.').next().unwrap_or_default() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "ico" => "image/x-icon",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    };
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

async fn sonarr_prowlarr(
    State(state): State<AppState>,
    Path(indexer_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    proxy_request(
        state,
        AppKind::Sonarr,
        Provider::Prowlarr,
        indexer_id,
        query,
    )
    .await
}

async fn sonarr_jackett(
    State(state): State<AppState>,
    Path(indexer_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    proxy_request(state, AppKind::Sonarr, Provider::Jackett, indexer_id, query).await
}

async fn radarr_prowlarr(
    State(state): State<AppState>,
    Path(indexer_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    proxy_request(
        state,
        AppKind::Radarr,
        Provider::Prowlarr,
        indexer_id,
        query,
    )
    .await
}

async fn radarr_jackett(
    State(state): State<AppState>,
    Path(indexer_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    proxy_request(state, AppKind::Radarr, Provider::Jackett, indexer_id, query).await
}

async fn proxy_request(
    state: AppState,
    kind: AppKind,
    provider: Provider,
    indexer_id: String,
    raw_query: Option<String>,
) -> Response {
    if let Err(error) = validate_indexer_id(&indexer_id) {
        return text_response(StatusCode::BAD_REQUEST, &error.to_string());
    }
    if !state.upstream.has_provider(provider) {
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{provider} URL is not configured"),
        );
    }

    let raw_query = raw_query.unwrap_or_default();
    let params = QueryParams::parse(&raw_query);
    let cache_key = params.cache_key(kind, provider, &indexer_id);
    if let Some(cached) = state.result_cache.get(&cache_key).await {
        return response_with_content_type(
            StatusCode::OK,
            &cached.content_type,
            cached.body.as_str(),
        );
    }

    let should_rewrite = params
        .get("q")
        .is_some_and(|query| !query.trim().is_empty());
    let payload = if should_rewrite {
        match search_request(&state, kind, provider, &indexer_id, &params).await {
            Ok(payload) => payload,
            Err(error) => return error.into_response(),
        }
    } else {
        match passthrough_request(&state, kind, provider, &indexer_id, &raw_query).await {
            Ok(payload) => payload,
            Err(error) => return error.into_response(),
        }
    };

    if payload.cacheable {
        state
            .result_cache
            .insert(
                cache_key,
                Arc::new(CachedBody {
                    body: Arc::new(payload.body.clone()),
                    content_type: payload.content_type.clone(),
                }),
            )
            .await;
    }
    response_with_headers(
        payload.status,
        &payload.content_type,
        &payload.body,
        payload.retry_after.as_deref(),
    )
}

struct ProxyPayload {
    status: StatusCode,
    content_type: String,
    body: String,
    cacheable: bool,
    retry_after: Option<String>,
}

struct ProxyError {
    status: StatusCode,
    message: &'static str,
    retry_after: Option<String>,
}

impl ProxyError {
    fn new(status: StatusCode, message: &'static str) -> Self {
        Self {
            status,
            message,
            retry_after: None,
        }
    }

    /// Map an upstream failure onto a meaningful downstream status instead of a
    /// generic 500/502: 429 stays 429 (with Retry-After), timeouts become 504,
    /// and other upstream failures become 502.
    fn from_upstream(error: &UpstreamError) -> Self {
        let status =
            StatusCode::from_u16(error.downstream_status()).unwrap_or(StatusCode::BAD_GATEWAY);
        let message = match error {
            UpstreamError::Timeout => "upstream indexer timed out",
            UpstreamError::Status { status: 429, .. } => "upstream indexer rate limited",
            UpstreamError::Status { .. } => "upstream indexer returned an error",
            UpstreamError::Transport(_) => "upstream indexer is unreachable",
            UpstreamError::NotConfigured(_) => "upstream URL is not configured",
            UpstreamError::InvalidIndexerId(_) => "invalid indexer id",
        };
        Self {
            status,
            message,
            retry_after: error.retry_after().map(str::to_owned),
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let mut response = text_response(self.status, self.message);
        if let Some(retry_after) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&retry_after)
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

async fn search_request(
    state: &AppState,
    kind: AppKind,
    provider: Provider,
    indexer_id: &str,
    params: &QueryParams,
) -> Result<ProxyPayload, ProxyError> {
    let original_query = params
        .get("q")
        .expect("search_request requires a non-empty q parameter");
    let search_key = strip_trailing_absolute_episode(original_query, original_query);
    // Java strips the episode suffix for Sonarr and the year suffix for Radarr
    // before looking the title up in SQLite; the upstream query keeps the
    // original text.
    let lookup_title = match kind {
        AppKind::Sonarr => remove_episode(&search_key),
        AppKind::Radarr => remove_year(&search_key),
    };

    let lookup = match state.title_lookup(kind, &lookup_title).await {
        Ok(lookup) => lookup,
        Err(error) => {
            tracing::error!(%kind, %error, "title lookup failed");
            return Err(ProxyError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database lookup failed",
            ));
        }
    };
    let matched = lookup.matched;
    let tmdb_titles = lookup.tmdb_titles;

    let snapshot = state.snapshot();
    let steps = build_search_steps(
        kind,
        &search_key,
        &lookup_title,
        matched.as_ref(),
        &tmdb_titles,
        snapshot.min_count,
    );
    let initial_offset = params
        .get("offset")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let initial_limit = params
        .get("limit")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(100);

    let upstream = state.upstream.clone();
    let upstream_indexer_id = indexer_id.to_owned();
    let base_params = params.clone();
    let result = execute_search(
        steps,
        snapshot.min_count,
        initial_offset,
        initial_limit,
        move |attempt| {
            let mut attempt_params = base_params.clone();
            attempt_params.set("q", &attempt.step.q);
            if attempt.step.clear_season_episode {
                attempt_params.remove_any(&["season", "ep"]);
            }
            attempt_params.set("offset", attempt.offset.to_string());
            attempt_params.set("limit", attempt.limit.to_string());
            let query = attempt_params.to_query_string();
            let upstream = upstream.clone();
            let indexer_id = upstream_indexer_id.clone();
            async move {
                let response = upstream.fetch(provider, &indexer_id, &query).await?;
                if !(200..300).contains(&response.status) {
                    return Err(UpstreamError::Status {
                        status: response.status,
                        retry_after: response.retry_after,
                    });
                }
                let count = torznab::count_items(&response.body);
                Ok(SearchPage {
                    xml: response.body,
                    count,
                })
            }
        },
    )
    .await;

    let SearchRun {
        xml,
        count: _,
        attempts: _,
        used_fallback: _,
    } = match result {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%kind, %provider, %error, "indexer search failed");
            return Err(ProxyError::from_upstream(&error));
        }
    };
    let Some(body) = xml else {
        return Err(ProxyError::new(
            StatusCode::BAD_GATEWAY,
            "upstream indexer returned no response",
        ));
    };
    let body = format_body(state, kind, body).await;
    Ok(ProxyPayload {
        status: StatusCode::OK,
        content_type: "application/xml; charset=utf-8".to_owned(),
        cacheable: body.contains("<channel>") && !body.is_empty(),
        body,
        retry_after: None,
    })
}

async fn passthrough_request(
    state: &AppState,
    kind: AppKind,
    provider: Provider,
    indexer_id: &str,
    raw_query: &str,
) -> Result<ProxyPayload, ProxyError> {
    let response = match state.upstream.fetch(provider, indexer_id, raw_query).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%kind, %provider, %error, "upstream passthrough failed");
            return Err(ProxyError::from_upstream(&error));
        }
    };
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = if status.is_success() {
        format_body(state, kind, response.body).await
    } else {
        response.body
    };
    Ok(ProxyPayload {
        status,
        content_type: response.content_type,
        cacheable: status == StatusCode::OK && body.contains("<channel>"),
        body,
        retry_after: response.retry_after,
    })
}

async fn format_body(state: &AppState, kind: AppKind, body: String) -> String {
    if !body.contains("<channel>") {
        return body;
    }
    let data = match state.formatting_data(kind).await {
        Ok(data) => data,
        Err(error) => {
            tracing::warn!(%kind, %error, "formatting data lookup failed; returning raw XML");
            return body;
        }
    };
    let Some(data) = data else {
        return body;
    };
    torznab::format_item_titles(&body, |title, description| {
        data.format_item_title(title, description)
    })
}

fn strip_trailing_absolute_episode(search_key: &str, fallback: &str) -> String {
    match search_key.strip_suffix(" 00") {
        Some(stripped) if !stripped.trim().is_empty() => stripped.to_owned(),
        _ => fallback.to_owned(),
    }
}

#[derive(Serialize)]
struct HealthBody {
    status: &'static str,
    version: &'static str,
    database: &'static str,
    result_cache_entries: usize,
}

async fn health(State(state): State<AppState>) -> Response {
    let cache_entries = state.result_cache.len().await;
    match db::healthcheck(&state.pool).await {
        Ok(()) => Json(HealthBody {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
            database: "ok",
            result_cache_entries: cache_entries,
        })
        .into_response(),
        Err(error) => {
            tracing::error!(%error, "health check database query failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(HealthBody {
                    status: "degraded",
                    version: env!("CARGO_PKG_VERSION"),
                    database: "error",
                    result_cache_entries: cache_entries,
                }),
            )
                .into_response()
        }
    }
}

fn response_with_content_type(status: StatusCode, content_type: &str, body: &str) -> Response {
    response_with_headers(status, content_type, body, None)
}

fn response_with_headers(
    status: StatusCode,
    content_type: &str,
    body: &str,
    retry_after: Option<&str>,
) -> Response {
    let mut response = (status, body.to_owned()).into_response();
    let value = HeaderValue::from_str(content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/xml; charset=utf-8"));
    response.headers_mut().insert(header::CONTENT_TYPE, value);
    if let Some(retry_after) = retry_after
        && let Ok(value) = HeaderValue::from_str(retry_after)
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn text_response(status: StatusCode, message: &str) -> Response {
    response_with_content_type(status, "text/plain; charset=utf-8", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::strip_season_episode;

    #[test]
    fn trailing_double_zero_is_removed_only_at_the_end() {
        assert_eq!(
            strip_trailing_absolute_episode("Show 00", "Show 00"),
            "Show"
        );
        assert_eq!(
            strip_trailing_absolute_episode("Show 00 01", "Show 00 01"),
            "Show 00 01"
        );
        assert_eq!(strip_trailing_absolute_episode("00", "00"), "00");
    }

    #[test]
    fn fallback_title_strips_season_episode_text() {
        assert_eq!(strip_season_episode("Main Show S01E02"), "Main Show");
    }
}
