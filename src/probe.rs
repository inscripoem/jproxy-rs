//! Live configuration probes, ported from Java `CheckUtil`.
//!
//! Every probe uses a dedicated client with Java's connect/read timeouts and
//! never logs a URL, API key, username, or password. A failed probe is a
//! normal result, not an error: the caller records it in `validStatus`.

use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use reqwest::Client;

/// Java `CheckUtil.CONNECT_TIMEOUT`.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// Java `CheckUtil.READ_TIMEOUT`. `reqwest` applies this as the whole-request
/// timeout, which is bounded by the read timeout in practice.
pub const READ_TIMEOUT: Duration = Duration::from_secs(12);

/// Java `CheckUtil` result codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiCheck {
    /// `URL_APIKEY_FAIL = 0`.
    Fail,
    /// `ONLY_URL_OK = 1`.
    OnlyUrlOk,
    /// `URL_APIKEY_OK = 2`.
    UrlApiKeyOk,
}

impl ApiCheck {
    pub fn url_valid(self) -> bool {
        !matches!(self, Self::Fail)
    }

    pub fn api_key_valid(self) -> bool {
        matches!(self, Self::UrlApiKeyOk)
    }
}

#[derive(Clone)]
pub struct Prober {
    client: Client,
}

impl Prober {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(READ_TIMEOUT)
            .user_agent(concat!("jproxy-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build configuration probe client")?;
        Ok(Self { client })
    }

    /// Java `CheckUtil.checkSonarrOrRadarrApi`.
    pub async fn check_sonarr_or_radarr(
        &self,
        url: Option<&str>,
        api_key: Option<&str>,
    ) -> ApiCheck {
        let (Some(url), Some(api_key)) = (non_blank(url), api_key) else {
            return ApiCheck::Fail;
        };
        let endpoint = format!("{url}/api/v3/health");
        let response = self
            .client
            .get(endpoint)
            .query(&[("apikey", api_key)])
            .send()
            .await;
        self.classify_arr_response(response, "Unauthorized").await
    }

    /// Java `CheckUtil.checkTmdbApi`.
    pub async fn check_tmdb(&self, url: Option<&str>, api_key: Option<&str>) -> ApiCheck {
        let (Some(url), Some(api_key)) = (non_blank(url), api_key) else {
            return ApiCheck::Fail;
        };
        let endpoint = format!("{url}/3/movie/550");
        let response = self
            .client
            .get(endpoint)
            .query(&[("api_key", api_key)])
            .send()
            .await;
        self.classify_arr_response(response, "Invalid API key")
            .await
    }

    async fn classify_arr_response(
        &self,
        response: Result<reqwest::Response, reqwest::Error>,
        invalid_key_marker: &str,
    ) -> ApiCheck {
        match response {
            Ok(response) => {
                let status = response.status();
                if status.as_u16() == 401 {
                    return ApiCheck::OnlyUrlOk;
                }
                // Java's RestTemplate error handler deliberately treats 409 as
                // a non-error response.
                if !status.is_success() && status.as_u16() != 409 {
                    tracing::debug!(
                        status = status.as_u16(),
                        "configuration probe returned an error"
                    );
                    return ApiCheck::Fail;
                }
                match read_body(response).await {
                    Ok(body) if body.contains(invalid_key_marker) => ApiCheck::OnlyUrlOk,
                    Ok(_) => ApiCheck::UrlApiKeyOk,
                    Err(_) => ApiCheck::Fail,
                }
            }
            Err(error) => {
                tracing::debug!(
                    error = %error.without_url(),
                    "configuration probe request failed"
                );
                ApiCheck::Fail
            }
        }
    }

    /// Java `CheckUtil.checkUrl`.
    ///
    /// Any HTTP response counts as valid, including 4xx/5xx and redirect-loop
    /// errors. Only transport failures and blank/non-HTTP values are invalid.
    pub async fn check_url(&self, url: Option<&str>) -> bool {
        let Some(url) = non_blank(url).filter(|url| url.starts_with("http")) else {
            return false;
        };
        match self.client.get(url).send().await {
            Ok(_) => true,
            Err(error) if error.is_redirect() => true,
            Err(error) => {
                tracing::debug!(
                    error = %error.without_url(),
                    "configuration URL probe failed"
                );
                false
            }
        }
    }
}

async fn read_body(response: reqwest::Response) -> Result<String> {
    response
        .text()
        .await
        .map_err(|error| anyhow::anyhow!("read probe body: {}", error.without_url()))
}

/// Java `CheckUtil.checkSonarrIndexerFormat`.
pub fn check_sonarr_indexer_format(format: Option<&str>) -> bool {
    format.is_some_and(|format| {
        !format.trim().is_empty()
            && format.contains("{title}")
            && format.contains("{season}")
            && format.contains("{episode}")
    })
}

/// Java `CheckUtil.checkRadarrIndexerFormat`.
pub fn check_radarr_indexer_format(format: Option<&str>) -> bool {
    format.is_some_and(|format| {
        !format.trim().is_empty() && format.contains("{title}") && format.contains("{year}")
    })
}

/// Java `CheckUtil.checkRegex`.
pub fn check_regex(regex: Option<&str>) -> bool {
    regex
        .map(str::trim)
        .filter(|regex| !regex.is_empty())
        .is_some_and(|regex| Regex::new(regex).is_ok())
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_checks_require_the_java_tokens() {
        assert!(check_sonarr_indexer_format(Some(
            "{title} {season}{episode}"
        )));
        assert!(!check_sonarr_indexer_format(Some("{title} {season}")));
        assert!(!check_sonarr_indexer_format(Some("  ")));
        assert!(!check_sonarr_indexer_format(None));
        assert!(check_radarr_indexer_format(Some("{title} {year}")));
        assert!(!check_radarr_indexer_format(Some("{title}")));
        assert!(!check_radarr_indexer_format(None));
    }

    #[test]
    fn regex_check_rejects_blank_and_invalid_patterns() {
        assert!(check_regex(Some(".*")));
        assert!(!check_regex(Some("(")));
        assert!(!check_regex(Some("")));
        assert!(!check_regex(None));
    }

    #[test]
    fn api_check_codes_match_java() {
        assert!(!ApiCheck::Fail.url_valid());
        assert!(!ApiCheck::Fail.api_key_valid());
        assert!(ApiCheck::OnlyUrlOk.url_valid());
        assert!(!ApiCheck::OnlyUrlOk.api_key_valid());
        assert!(ApiCheck::UrlApiKeyOk.url_valid());
        assert!(ApiCheck::UrlApiKeyOk.api_key_valid());
    }
}
