use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use tokio::sync::Semaphore;
use url::Url;

use crate::models::Provider;

/// Retry policy applied to upstream indexer requests.
///
/// Only 408, 429 and 5xx responses and transport failures (including timeouts)
/// are retried. Every other status is returned to the caller unchanged so the
/// proxy can forward it, which preserves the incoming Torznab query semantics.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(4),
        }
    }
}

#[derive(Clone)]
pub struct UpstreamClient {
    client: Client,
    prowlarr_url: Option<String>,
    jackett_url: Option<String>,
    policy: RetryPolicy,
    limiter: Arc<Semaphore>,
}

#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
    /// Raw `Retry-After` header, forwarded downstream for 429/503 responses.
    pub retry_after: Option<String>,
}

/// A failed upstream interaction with enough detail to map it to a meaningful
/// downstream Torznab response instead of a generic 500.
#[derive(Debug)]
pub enum UpstreamError {
    NotConfigured(Provider),
    InvalidIndexerId(String),
    Timeout,
    Transport(String),
    Status {
        status: u16,
        retry_after: Option<String>,
    },
}

impl UpstreamError {
    /// HTTP status to return to Sonarr/Radarr for this failure.
    ///
    /// `429` is preserved so the *arr client backs off, timeouts become `504`,
    /// and any other upstream failure (including 5xx) becomes `502`.
    pub fn downstream_status(&self) -> u16 {
        match self {
            Self::NotConfigured(_) => 503,
            Self::InvalidIndexerId(_) => 400,
            Self::Timeout => 504,
            Self::Transport(_) => 502,
            Self::Status { status, .. } if *status == 429 => 429,
            Self::Status { .. } => 502,
        }
    }

    pub fn retry_after(&self) -> Option<&str> {
        match self {
            Self::Status { retry_after, .. } => retry_after.as_deref(),
            _ => None,
        }
    }

    fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout | Self::Transport(_) => true,
            Self::Status { status, .. } => is_retryable_status(*status),
            Self::NotConfigured(_) | Self::InvalidIndexerId(_) => false,
        }
    }
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured(provider) => write!(formatter, "{provider} URL is not configured"),
            Self::InvalidIndexerId(message) => write!(formatter, "invalid indexer id: {message}"),
            Self::Timeout => write!(formatter, "upstream request timed out"),
            Self::Transport(message) => write!(formatter, "upstream transport error: {message}"),
            Self::Status {
                status,
                retry_after,
            } => match retry_after {
                Some(value) => write!(
                    formatter,
                    "upstream returned HTTP {status} (retry-after {value})"
                ),
                None => write!(formatter, "upstream returned HTTP {status}"),
            },
        }
    }
}

impl std::error::Error for UpstreamError {}

pub fn is_retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

impl UpstreamClient {
    pub fn new(
        prowlarr_url: Option<String>,
        jackett_url: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        Self::with_policy(
            prowlarr_url,
            jackett_url,
            timeout,
            RetryPolicy::default(),
            8,
        )
    }

    pub fn with_policy(
        prowlarr_url: Option<String>,
        jackett_url: Option<String>,
        timeout: Duration,
        policy: RetryPolicy,
        max_concurrency: usize,
    ) -> Result<Self> {
        for (name, base) in [
            ("PROWLARR_URL", prowlarr_url.as_deref()),
            ("JACKETT_URL", jackett_url.as_deref()),
        ] {
            if let Some(base) = base {
                validate_base_url(name, base)?;
            }
        }
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(concat!("jproxy-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build upstream HTTP client")?;
        Ok(Self {
            client,
            prowlarr_url: clean_base(prowlarr_url),
            jackett_url: clean_base(jackett_url),
            policy,
            limiter: Arc::new(Semaphore::new(max_concurrency.max(1))),
        })
    }

    pub fn has_provider(&self, provider: Provider) -> bool {
        self.base_url(provider).is_some()
    }

    pub fn http_client(&self) -> &Client {
        &self.client
    }

    pub fn base_url(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Prowlarr => self.prowlarr_url.as_deref(),
            Provider::Jackett => self.jackett_url.as_deref(),
        }
    }

    /// Fetch one indexer response, applying the configured concurrency bound and
    /// retry/backoff policy. The URL is rebuilt once per call, so retries reuse
    /// the exact same query string.
    pub async fn fetch(
        &self,
        provider: Provider,
        indexer_id: &str,
        raw_query: &str,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let base = self
            .base_url(provider)
            .ok_or(UpstreamError::NotConfigured(provider))?;
        let url = build_upstream_url(base, provider, indexer_id, raw_query)
            .map_err(|error| UpstreamError::InvalidIndexerId(error.to_string()))?;
        self.fetch_url(&url).await
    }

    async fn fetch_url(&self, url: &str) -> Result<UpstreamResponse, UpstreamError> {
        let mut attempt = 0u32;
        loop {
            let response = {
                let _permit = self
                    .limiter
                    .acquire()
                    .await
                    .expect("upstream semaphore is never closed");
                self.attempt(url).await
            };
            match response {
                Ok(response) => {
                    if is_retryable_status(response.status) && attempt < self.policy.max_retries {
                        let delay =
                            with_jitter(self.backoff(attempt, response.retry_after.as_deref()));
                        tracing::warn!(
                            status = response.status,
                            attempt = attempt + 1,
                            delay_ms = delay.as_millis() as u64,
                            "retrying upstream request"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Ok(response);
                }
                Err(error) if error.is_retryable() && attempt < self.policy.max_retries => {
                    let delay = with_jitter(self.backoff(attempt, None));
                    tracing::warn!(
                        error = %error,
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "retrying upstream request after transport failure"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn attempt(&self, url: &str) -> Result<UpstreamResponse, UpstreamError> {
        let response = self
            .client
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                "application/xml, text/xml, application/rss+xml, */*",
            )
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    UpstreamError::Timeout
                } else {
                    UpstreamError::Transport(error.without_url().to_string())
                }
            })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/xml; charset=utf-8")
            .to_owned();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .text()
            .await
            .map_err(|error| UpstreamError::Transport(error.without_url().to_string()))?;
        Ok(UpstreamResponse {
            status,
            content_type,
            body,
            retry_after,
        })
    }

    fn backoff(&self, attempt: u32, retry_after: Option<&str>) -> Duration {
        if let Some(seconds) = retry_after.and_then(|value| value.trim().parse::<u64>().ok()) {
            return Duration::from_secs(seconds).min(self.policy.max_delay);
        }
        let exponential = self
            .policy
            .base_delay
            .saturating_mul(1u32 << attempt.min(16));
        exponential.min(self.policy.max_delay)
    }
}

/// Add up to 25% jitter so retries from concurrent requests do not synchronise.
pub fn with_jitter(delay: Duration) -> Duration {
    if delay.is_zero() {
        return delay;
    }
    let span = delay.as_millis() as u64 / 4 + 1;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.subsec_nanos() as u64)
        .unwrap_or(0);
    delay + Duration::from_millis(nanos % span)
}

pub fn build_upstream_url(
    base: &str,
    provider: Provider,
    indexer_id: &str,
    raw_query: &str,
) -> Result<String> {
    validate_indexer_id(indexer_id)?;
    let base = base.trim_end_matches('/');
    let path = match provider {
        Provider::Prowlarr => format!("/{indexer_id}/api"),
        Provider::Jackett => {
            format!("/api/v2.0/indexers/{indexer_id}/results/torznab/api")
        }
    };
    let query = raw_query.trim_start_matches('?');
    if query.is_empty() {
        Ok(format!("{base}{path}"))
    } else {
        Ok(format!("{base}{path}?{query}"))
    }
}

pub fn validate_indexer_id(indexer_id: &str) -> Result<()> {
    if indexer_id.is_empty()
        || !indexer_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        bail!("indexer_id must contain only ASCII letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

fn validate_base_url(name: &str, base: &str) -> Result<()> {
    let url = Url::parse(base).with_context(|| format!("{name} must be a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("{name} must use http or https");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("{name} must not contain a query string or fragment");
    }
    Ok(())
}

fn clean_base(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prowlarr_path_and_raw_query_are_preserved() {
        let url = build_upstream_url(
            "http://prowlarr:9696/",
            Provider::Prowlarr,
            "12",
            "t=search&q=Show+%E7%AE%80&limit=100",
        )
        .unwrap();
        assert_eq!(
            url,
            "http://prowlarr:9696/12/api?t=search&q=Show+%E7%AE%80&limit=100"
        );
    }

    #[test]
    fn jackett_path_uses_the_real_torznab_indexer_endpoint() {
        let url = build_upstream_url(
            "http://jackett:9117",
            Provider::Jackett,
            "somesite",
            "apikey=x&q=Show",
        )
        .unwrap();
        assert_eq!(
            url,
            "http://jackett:9117/api/v2.0/indexers/somesite/results/torznab/api?apikey=x&q=Show"
        );
    }

    #[test]
    fn invalid_indexer_ids_are_rejected() {
        assert!(validate_indexer_id("../etc").is_err());
        assert!(validate_indexer_id("").is_err());
        assert!(validate_indexer_id("indexer-1_ok").is_ok());
    }

    #[test]
    fn retryable_statuses_cover_rate_limit_and_server_errors() {
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(200));
    }

    #[test]
    fn upstream_errors_map_to_meaningful_downstream_statuses() {
        assert_eq!(
            UpstreamError::Status {
                status: 429,
                retry_after: Some("30".to_owned())
            }
            .downstream_status(),
            429
        );
        assert_eq!(
            UpstreamError::Status {
                status: 500,
                retry_after: None
            }
            .downstream_status(),
            502
        );
        assert_eq!(UpstreamError::Timeout.downstream_status(), 504);
        assert_eq!(
            UpstreamError::NotConfigured(Provider::Prowlarr).downstream_status(),
            503
        );
    }

    #[test]
    fn backoff_grows_exponentially_and_respects_retry_after() {
        let client = UpstreamClient::with_policy(
            Some("http://localhost:1".to_owned()),
            None,
            Duration::from_secs(1),
            RetryPolicy {
                max_retries: 4,
                base_delay: Duration::from_millis(100),
                max_delay: Duration::from_secs(1),
            },
            1,
        )
        .unwrap();
        assert_eq!(client.backoff(0, None), Duration::from_millis(100));
        assert_eq!(client.backoff(1, None), Duration::from_millis(200));
        assert_eq!(client.backoff(2, None), Duration::from_millis(400));
        assert_eq!(client.backoff(4, None), Duration::from_secs(1));
        assert_eq!(client.backoff(0, Some("2")), Duration::from_secs(1));
    }
}
