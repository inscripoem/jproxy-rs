//! qBittorrent and Transmission session handling.
//!
//! This is a port of the Java `QbittorrentServiceImpl` and
//! `TransmissionServiceImpl` login/session behavior. Credentials live only in
//! memory, are never logged, and are used exclusively by the login and rename
//! tasks. Every request failure is returned to the caller so the rename task
//! can isolate it per history item.

use std::sync::Arc;
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderValue, SET_COOKIE};
use reqwest::{Client, Response};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::debug;

/// Java `Common.VIDEO_AND_SUBTITLE_EXTENSION_REGEX`.
pub static VIDEO_AND_SUBTITLE_EXTENSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(\.(mp4|avi|wmv|flv|mov|mkv|webm|mpg|mpeg|3gp|iso|ts|([-_a-zA-Z]{2,5}\.|)ass|([-_a-zA-Z]{2,5}\.|)srt|([-_a-zA-Z]{2,5}\.|)ssa|([-_a-zA-Z]{2,5}\.|)idx|([-_a-zA-Z]{2,5}\.|)sub))$",
    )
    .expect("static regex")
});

/// Java `Common.SUBTITLE_EXTENSION_REGEX`.
pub static SUBTITLE_EXTENSION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\.(ass|srt|ssa|idx|sub))$").expect("static regex"));

const TRANSMISSION_SESSION_HEADER: &str = "x-transmission-session-id";

#[derive(Debug, Clone, Default)]
struct QbittorrentSession {
    base_url: Option<String>,
    username: Option<String>,
    password: Option<String>,
    cookie: Option<String>,
    logged_in: bool,
}

/// qBittorrent client with the same session lifecycle as the Java singleton:
/// login stores the endpoint and credentials, scheduled re-login reuses them,
/// and every API call sends the captured cookie.
#[derive(Clone)]
pub struct QbittorrentClient {
    http: Client,
    session: Arc<Mutex<QbittorrentSession>>,
}

impl QbittorrentClient {
    pub fn new(http: Client) -> Self {
        Self {
            http,
            session: Arc::new(Mutex::new(QbittorrentSession::default())),
        }
    }

    /// Store the endpoint and credentials and start a fresh session.
    ///
    /// Mirrors Java `QbittorrentServiceImpl::login(String, String, String)`:
    /// the previous cookie is discarded before the request, and a non-blank
    /// `Set-Cookie` value is the only success signal.
    pub async fn login_with(&self, url: &str, username: &str, password: &str) -> bool {
        let base_url = url.trim().trim_end_matches('/');
        if base_url.is_empty() {
            return false;
        }
        {
            let mut session = self.session.lock().await;
            *session = QbittorrentSession {
                base_url: Some(base_url.to_owned()),
                username: Some(username.to_owned()),
                password: Some(password.to_owned()),
                cookie: None,
                logged_in: false,
            };
        }

        let endpoint = format!("{base_url}/api/v2/auth/login");
        let response = match self
            .http
            .post(endpoint)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body(&[("username", username), ("password", password)]))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                debug!(error = %error.without_url(), "qBittorrent login failed");
                return false;
            }
        };

        if !response.status().is_success() {
            debug!(status = %response.status(), "qBittorrent login failed");
            return false;
        }
        let cookie = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .find_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .filter(|cookie| !cookie.trim().is_empty());
        let Some(cookie) = cookie else {
            debug!("qBittorrent login returned no session cookie");
            return false;
        };

        let mut session = self.session.lock().await;
        session.cookie = Some(cookie);
        session.logged_in = true;
        true
    }

    /// Re-login with the stored endpoint and credentials (Java `login()`).
    pub async fn login(&self) -> bool {
        let credentials = {
            let session = self.session.lock().await;
            match (
                session.base_url.clone(),
                session.username.clone(),
                session.password.clone(),
            ) {
                (Some(url), Some(username), Some(password)) => Some((url, username, password)),
                _ => None,
            }
        };
        match credentials {
            Some((url, username, password)) => self.login_with(&url, &username, &password).await,
            None => false,
        }
    }

    pub async fn is_login(&self) -> bool {
        self.session.lock().await.logged_in
    }

    async fn endpoint_and_cookie(&self) -> Result<(String, HeaderValue)> {
        let session = self.session.lock().await;
        if !session.logged_in {
            bail!("qBittorrent is not logged in");
        }
        let base_url = session
            .base_url
            .clone()
            .ok_or_else(|| anyhow!("qBittorrent endpoint is not configured"))?;
        let cookie = session
            .cookie
            .clone()
            .filter(|cookie| !cookie.trim().is_empty())
            .ok_or_else(|| anyhow!("qBittorrent session cookie is missing"))?;
        let cookie = HeaderValue::from_str(&cookie)
            .context("qBittorrent session cookie is not a valid header value")?;
        Ok((base_url, cookie))
    }

    /// Java `files(String)`: the `name` field of every torrent file entry.
    pub async fn files(&self, hash: &str) -> Result<Vec<String>> {
        let (base_url, cookie) = self.endpoint_and_cookie().await?;
        let response = self
            .http
            .get(format!("{base_url}/api/v2/torrents/files"))
            .query(&[("hash", hash)])
            .header(COOKIE, cookie)
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        if !status.is_success() {
            bail!("qBittorrent files returned HTTP {status}");
        }
        let payload = response.json::<Value>().await.map_err(http_error)?;
        let entries = payload
            .as_array()
            .ok_or_else(|| anyhow!("qBittorrent files response must be a JSON array"))?;
        entries
            .iter()
            .map(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("qBittorrent file entry has no name"))
            })
            .collect()
    }

    /// Java `rename(String, String)`: rename the torrent itself.
    pub async fn rename(&self, hash: &str, name: &str) -> Result<bool> {
        let (base_url, cookie) = self.endpoint_and_cookie().await?;
        let response = self
            .http
            .post(format!("{base_url}/api/v2/torrents/rename"))
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body(&[("hash", hash), ("name", name)]))
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        if !status.is_success() {
            bail!("qBittorrent rename returned HTTP {status}");
        }
        Ok(true)
    }

    /// Java `renameFile(String, String, String)`: rename one file in a torrent.
    pub async fn rename_file(&self, hash: &str, old_path: &str, new_path: &str) -> Result<bool> {
        let (base_url, cookie) = self.endpoint_and_cookie().await?;
        let response = self
            .http
            .post(format!("{base_url}/api/v2/torrents/renameFile"))
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body(&[
                ("hash", hash),
                ("oldPath", old_path),
                ("newPath", new_path),
            ]))
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        if !status.is_success() {
            bail!("qBittorrent renameFile returned HTTP {status}");
        }
        Ok(true)
    }
}

#[derive(Debug, Clone, Default)]
struct TransmissionSession {
    base_url: Option<String>,
    username: Option<String>,
    password: Option<String>,
    session_id: Option<String>,
    logged_in: bool,
}

/// Transmission client using the JSON-RPC `session-get` handshake. The session
/// id is captured from the `409 Conflict` response and refreshed transparently
/// when a call is rejected with another 409, matching the Java implementation.
#[derive(Clone)]
pub struct TransmissionClient {
    http: Client,
    session: Arc<Mutex<TransmissionSession>>,
}

impl TransmissionClient {
    pub fn new(http: Client) -> Self {
        Self {
            http,
            session: Arc::new(Mutex::new(TransmissionSession::default())),
        }
    }

    pub async fn login_with(&self, url: &str, username: &str, password: &str) -> bool {
        let base_url = url.trim().trim_end_matches('/');
        if base_url.is_empty() {
            return false;
        }
        {
            let mut session = self.session.lock().await;
            *session = TransmissionSession {
                base_url: Some(base_url.to_owned()),
                username: Some(username.to_owned()),
                password: Some(password.to_owned()),
                session_id: None,
                logged_in: false,
            };
        }

        let response = match self
            .http
            .post(base_url)
            .basic_auth(username, Some(password))
            .json(&json!({
                "method": "session-get",
                "arguments": {"fields": ["version"]}
            }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                debug!(error = %error.without_url(), "Transmission login failed");
                return false;
            }
        };

        if response.status() != StatusCode::CONFLICT {
            debug!(status = %response.status(), "Transmission login failed");
            return false;
        }
        let session_id = response
            .headers()
            .get(TRANSMISSION_SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty());
        let Some(session_id) = session_id else {
            debug!("Transmission login returned no session id");
            return false;
        };

        let mut session = self.session.lock().await;
        session.session_id = Some(session_id);
        session.logged_in = true;
        true
    }

    pub async fn login(&self) -> bool {
        let credentials = {
            let session = self.session.lock().await;
            match (
                session.base_url.clone(),
                session.username.clone(),
                session.password.clone(),
            ) {
                (Some(url), Some(username), Some(password)) => Some((url, username, password)),
                _ => None,
            }
        };
        match credentials {
            Some((url, username, password)) => self.login_with(&url, &username, &password).await,
            None => false,
        }
    }

    pub async fn is_login(&self) -> bool {
        self.session.lock().await.logged_in
    }

    async fn connection(&self) -> Result<(String, String, String, String)> {
        let session = self.session.lock().await;
        if !session.logged_in {
            bail!("Transmission is not logged in");
        }
        Ok((
            session
                .base_url
                .clone()
                .ok_or_else(|| anyhow!("Transmission endpoint is not configured"))?,
            session
                .username
                .clone()
                .ok_or_else(|| anyhow!("Transmission username is not configured"))?,
            session
                .password
                .clone()
                .ok_or_else(|| anyhow!("Transmission password is not configured"))?,
            session
                .session_id
                .clone()
                .ok_or_else(|| anyhow!("Transmission session id is missing"))?,
        ))
    }

    async fn send(
        &self,
        base_url: &str,
        username: &str,
        password: &str,
        session_id: &str,
        payload: &Value,
    ) -> Result<Response> {
        let header = HeaderValue::from_str(session_id)
            .context("Transmission session id is not a valid header value")?;
        self.http
            .post(base_url)
            .basic_auth(username, Some(password))
            .header(TRANSMISSION_SESSION_HEADER, header)
            .json(payload)
            .send()
            .await
            .map_err(http_error)
    }

    /// Send a JSON-RPC call, repeating it once after a 409 re-login.
    pub async fn call(&self, payload: Value) -> Result<Value> {
        let (base_url, username, password, session_id) = self.connection().await?;
        let mut response = self
            .send(&base_url, &username, &password, &session_id, &payload)
            .await?;
        if response.status() == StatusCode::CONFLICT {
            if !self.login().await {
                bail!("Transmission session refresh failed");
            }
            let (base_url, username, password, session_id) = self.connection().await?;
            response = self
                .send(&base_url, &username, &password, &session_id, &payload)
                .await?;
        }
        let status = response.status();
        if !status.is_success() {
            bail!("Transmission RPC returned HTTP {status}");
        }
        response.json::<Value>().await.map_err(http_error)
    }

    async fn torrent_name(&self, hash: &str) -> Result<Option<String>> {
        let payload = json!({
            "method": "torrent-get",
            "arguments": {"ids": hash, "fields": ["name"]}
        });
        let response = self.call(payload).await?;
        Ok(response
            .get("arguments")
            .and_then(|arguments| arguments.get("torrents"))
            .and_then(Value::as_array)
            .and_then(|torrents| torrents.first())
            .and_then(|torrent| torrent.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Java `rename(String, String)`.
    ///
    /// Colons become underscores, the existing file extension is preserved,
    /// and a no-op rename reports `false` without an RPC call.
    pub async fn rename(&self, hash: &str, name: &str) -> Result<bool> {
        let name = name.replace(':', "_");
        let Some(old_name) = self.torrent_name(hash).await? else {
            debug!(%hash, "Transmission torrent name unavailable");
            return Ok(false);
        };
        if old_name.trim().is_empty() || old_name.eq_ignore_ascii_case(hash) {
            debug!(%hash, %old_name, "Transmission torrent is not ready to rename");
            return Ok(false);
        }

        let mut new_name = name;
        if let Some(captures) = VIDEO_AND_SUBTITLE_EXTENSION_REGEX.captures(&old_name)
            && let Some(extension) = captures.get(1)
        {
            new_name.push_str(extension.as_str());
        }
        if old_name == new_name {
            debug!(%new_name, "Transmission torrent is already renamed");
            return Ok(false);
        }

        let payload = json!({
            "method": "torrent-rename-path",
            "arguments": {"ids": hash, "path": old_name, "name": new_name}
        });
        self.call(payload).await?;
        Ok(true)
    }

    /// Transmission exposes no file-level rename in the Java implementation.
    pub async fn files(&self, _hash: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    pub async fn rename_file(&self, _hash: &str, _old_path: &str, _new_path: &str) -> Result<bool> {
        Ok(false)
    }
}

/// Both downloader clients, shared by the login and rename tasks.
#[derive(Clone)]
pub struct Downloaders {
    pub qbittorrent: QbittorrentClient,
    pub transmission: TransmissionClient,
}

impl Downloaders {
    pub fn new(http: Client) -> Self {
        Self {
            qbittorrent: QbittorrentClient::new(http.clone()),
            transmission: TransmissionClient::new(http),
        }
    }

    /// Build downloader clients with a bounded request timeout.
    ///
    /// Java's RestTemplate waits far longer than a rename task should; this
    /// keeps login and rename calls bounded by `DOWNLOADER_REQUEST_TIMEOUT_SECS`.
    pub fn with_timeout(timeout: std::time::Duration) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(concat!("jproxy-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build downloader HTTP client")?;
        Ok(Self::new(client))
    }

    pub async fn any_logged_in(&self) -> bool {
        self.qbittorrent.is_login().await || self.transmission.is_login().await
    }
}

fn http_error(error: reqwest::Error) -> anyhow::Error {
    anyhow!("HTTP request failed: {}", error.without_url())
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(pairs.iter().copied());
    serializer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_regex_matches_java_shapes() {
        assert_eq!(
            VIDEO_AND_SUBTITLE_EXTENSION_REGEX
                .captures("Show.S01E02.1080p.mkv")
                .and_then(|captures| captures.get(1))
                .map(|value| value.as_str()),
            Some(".mkv")
        );
        assert_eq!(
            VIDEO_AND_SUBTITLE_EXTENSION_REGEX
                .captures("Show.S01E02.zh-CN.srt")
                .and_then(|captures| captures.get(1))
                .map(|value| value.as_str()),
            Some(".zh-CN.srt")
        );
        assert!(
            VIDEO_AND_SUBTITLE_EXTENSION_REGEX
                .captures("Show.S01E02")
                .is_none()
        );
        assert!(SUBTITLE_EXTENSION_REGEX.is_match(".srt"));
        // Java compiles the same regex for `String.matches`, which requires a
        // full match; the raw regex itself is deliberately unanchored.
        assert!(SUBTITLE_EXTENSION_REGEX.is_match(".zh-CN.srt"));
    }
}
