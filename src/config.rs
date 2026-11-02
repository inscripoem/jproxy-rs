use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::models::AppKind;
use crate::upstream::RetryPolicy;

/// Clean-title regular expression from the final upstream Liquibase migration.
pub const DEFAULT_CLEAN_TITLE_REGEX: &str =
    r#"(`|,|~|!|@|#|%|&|_|=|'|"|:|<|>|-|—|·|，|～|、|。|‘|’|“|”|？|！|：|（|）|【|】|《|》|♀|/)"#;
pub const DEFAULT_SONARR_FORMAT: &str =
    "{title} {season}{episode} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}";
pub const DEFAULT_RADARR_FORMAT: &str =
    "{title} {year} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}";
pub const DEFAULT_TMDB_URL: &str = "https://api.themoviedb.org";
pub const DEFAULT_RULE_LOCATION: &str =
    "https://raw.githubusercontent.com/LuckyPuppy514/jproxy/main/src/main/resources/rule";
pub const DEFAULT_RULE_LOCATION_BACKUP: &str =
    "https://github.rn.lckp.top/LuckyPuppy514/jproxy/main/src/main/resources/rule";
pub const DEFAULT_SONARR_LANGUAGE_1: &str = "zh-CN";
pub const DEFAULT_SONARR_LANGUAGE_2: &str = "zh-TW";
pub const DEFAULT_RULE_SYNC_AUTHORS: &str = "ALL";

/// The `system_config` keys this service is allowed to read.
///
/// Downloader credentials are read only to log in; values are held in memory
/// and are never logged. Environment variables take precedence over these rows.
/// `system_user` rows are never selected.
const SYSTEM_CONFIG_KEYS: &[&str] = &[
    "sonarrUrl",
    "sonarrApikey",
    "radarrUrl",
    "radarrApikey",
    "tmdbUrl",
    "tmdbApikey",
    "sonarrLanguage1",
    "sonarrLanguage2",
    "ruleSyncAuthors",
    "ruleLocation",
    "ruleLocationBackup",
    "qbittorrentUrl",
    "qbittorrentUsername",
    "qbittorrentPassword",
    "transmissionUrl",
    "transmissionUsername",
    "transmissionPassword",
];

#[derive(Clone, Default)]
pub struct SystemConfigValues {
    values: HashMap<String, String>,
}

impl SystemConfigValues {
    pub fn new(values: HashMap<String, String>) -> Self {
        Self { values }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

#[derive(Clone)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub database_path: PathBuf,
    pub prowlarr_url: Option<String>,
    pub jackett_url: Option<String>,
    pub sonarr_url: Option<String>,
    pub sonarr_api_key: Option<String>,
    pub radarr_url: Option<String>,
    pub radarr_api_key: Option<String>,
    pub tmdb_url: Option<String>,
    pub tmdb_api_key: Option<String>,
    pub sonarr_language_1: Option<String>,
    pub sonarr_language_2: Option<String>,
    pub rule_location: Option<String>,
    pub rule_location_backup: Option<String>,
    pub rule_sync_authors: Option<String>,
    /// Downloader endpoints and credentials. Credentials are runtime-only and
    /// must never be logged. A blank username or password is preserved because
    /// some downloader accounts intentionally have no password.
    pub qbittorrent_url: Option<String>,
    pub qbittorrent_username: Option<String>,
    pub qbittorrent_password: Option<String>,
    pub transmission_url: Option<String>,
    pub transmission_username: Option<String>,
    pub transmission_password: Option<String>,
    pub clean_title_regex: String,
    pub sonarr_indexer_format: String,
    pub radarr_indexer_format: String,
    /// Java defaults to 6 in application-prod.yml. Zero disables all retries.
    pub min_count: usize,
    /// Caffeine-compatible reference-data TTL. Zero expires immediately.
    pub cache_expires: Duration,
    /// Sync period. Zero disables the background task and on-demand sync.
    pub sync_interval: Duration,
    pub indexer_result_cache_expires: Duration,
    /// Java `time.downloader-login` runs every 30 minutes. Zero disables the
    /// scheduled re-login; the startup login still runs.
    pub downloader_login_interval: Duration,
    /// Java `time.sonarr-rename` runs every 30 seconds. Zero disables the task.
    pub sonarr_rename_interval: Duration,
    /// Java `time.radarr-rename` runs every minute. Zero disables the task.
    pub radarr_rename_interval: Duration,
    /// Java `time.sonarr-rename-fall-back`, in minutes.
    pub sonarr_rename_fallback: Duration,
    /// Java `time.radarr-rename-fall-back`, in minutes.
    pub radarr_rename_fallback: Duration,
    /// Java `rename.file`: also rename the files inside each torrent.
    pub rename_file: bool,
    /// Parsed for compatibility with the Java deployment. Phase 1 has no auth.
    pub login_enabled: bool,
    pub sync_enabled: bool,
    pub request_timeout: Duration,
    pub on_demand_sync_timeout: Duration,
    pub result_cache_max_entries: usize,
    pub cache_max_entries: usize,
    /// Maximum simultaneous in-flight upstream indexer requests.
    pub upstream_max_concurrency: usize,
    /// Retry policy for 408/429/5xx and transport failures.
    pub upstream_retry: RetryPolicy,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen_addr = env_string("LISTEN_ADDR")
            .unwrap_or_else(|| "0.0.0.0:8117".to_owned())
            .parse()
            .context("LISTEN_ADDR must be a socket address such as 0.0.0.0:8117")?;

        let database_path = env_string("DATABASE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./jproxy.db"));

        let cache_expires =
            Duration::from_secs(env_minutes("CACHE_EXPIRES", 4320)?.saturating_mul(60));
        let sync_interval =
            Duration::from_secs(env_minutes("SYNC_INTERVAL", 3)?.saturating_mul(60));
        let indexer_result_cache_expires = Duration::from_secs(
            env_minutes("INDEXER_RESULT_CACHE_EXPIRES", 15)?.saturating_mul(60),
        );
        let sync_enabled = env_bool("SYNC_ENABLED", true)? && !sync_interval.is_zero();

        Ok(Self {
            listen_addr,
            database_path,
            prowlarr_url: env_string("PROWLARR_URL"),
            jackett_url: env_string("JACKETT_URL"),
            sonarr_url: env_string("SONARR_URL"),
            sonarr_api_key: env_string("SONARR_API_KEY"),
            radarr_url: env_string("RADARR_URL"),
            radarr_api_key: env_string("RADARR_API_KEY"),
            tmdb_url: env_string("TMDB_URL"),
            tmdb_api_key: env_string("TMDB_API_KEY"),
            sonarr_language_1: env_string("SONARR_LANGUAGE_1"),
            sonarr_language_2: env_string("SONARR_LANGUAGE_2"),
            rule_location: env_raw("RULE_LOCATION").map(|value| value.trim().to_owned()),
            rule_location_backup: env_raw("RULE_LOCATION_BACKUP")
                .map(|value| value.trim().to_owned()),
            rule_sync_authors: env_raw("RULE_SYNC_AUTHORS").map(|value| value.trim().to_owned()),
            qbittorrent_url: env_string("QBITTORRENT_URL"),
            qbittorrent_username: env_secret("QBITTORRENT_USERNAME"),
            qbittorrent_password: env_secret("QBITTORRENT_PASSWORD"),
            transmission_url: env_string("TRANSMISSION_URL"),
            transmission_username: env_secret("TRANSMISSION_USERNAME"),
            transmission_password: env_secret("TRANSMISSION_PASSWORD"),
            clean_title_regex: env_string("CLEAN_TITLE_REGEX")
                .unwrap_or_else(|| DEFAULT_CLEAN_TITLE_REGEX.to_owned()),
            sonarr_indexer_format: env_string("SONARR_INDEXER_FORMAT")
                .unwrap_or_else(|| DEFAULT_SONARR_FORMAT.to_owned()),
            radarr_indexer_format: env_string("RADARR_INDEXER_FORMAT")
                .unwrap_or_else(|| DEFAULT_RADARR_FORMAT.to_owned()),
            min_count: env_usize("MIN_COUNT", 6)?,
            cache_expires,
            sync_interval,
            indexer_result_cache_expires,
            downloader_login_interval: Duration::from_secs(env_u64(
                "DOWNLOADER_LOGIN_INTERVAL_SECS",
                30 * 60,
            )?),
            sonarr_rename_interval: Duration::from_secs(env_u64(
                "SONARR_RENAME_INTERVAL_SECS",
                30,
            )?),
            radarr_rename_interval: Duration::from_secs(env_u64(
                "RADARR_RENAME_INTERVAL_SECS",
                60,
            )?),
            sonarr_rename_fallback: Duration::from_secs(
                env_minutes("SONARR_RENAME_FALLBACK_MINS", 30)?.saturating_mul(60),
            ),
            radarr_rename_fallback: Duration::from_secs(
                env_minutes("RADARR_RENAME_FALLBACK_MINS", 30)?.saturating_mul(60),
            ),
            rename_file: env_bool("RENAME_FILE", true)?,
            login_enabled: env_bool("LOGIN_ENABLED", false)?,
            sync_enabled,
            request_timeout: Duration::from_secs(env_u64("REQUEST_TIMEOUT_SECS", 30)?),
            on_demand_sync_timeout: Duration::from_secs(env_u64("ONDEMAND_SYNC_TIMEOUT_SECS", 30)?),
            result_cache_max_entries: env_usize("RESULT_CACHE_MAX_ENTRIES", 1000)?,
            cache_max_entries: env_usize("CACHE_MAX_ENTRIES", 1000)?,
            upstream_max_concurrency: env_usize("UPSTREAM_MAX_CONCURRENCY", 8)?.max(1),
            upstream_retry: RetryPolicy {
                max_retries: env_u32("UPSTREAM_MAX_RETRIES", 2)?,
                base_delay: Duration::from_millis(env_u64("UPSTREAM_RETRY_BASE_MS", 250)?),
                max_delay: Duration::from_millis(env_u64("UPSTREAM_RETRY_MAX_MS", 4000)?),
            },
        })
    }

    /// Fill values that were not supplied through the environment from the
    /// baseline `system_config` rows. Environment values always win.
    pub fn apply_system_config(&mut self, values: &SystemConfigValues) {
        fill_optional(&mut self.sonarr_url, values.get("sonarrUrl"));
        fill_optional(&mut self.sonarr_api_key, values.get("sonarrApikey"));
        fill_optional(&mut self.radarr_url, values.get("radarrUrl"));
        fill_optional(&mut self.radarr_api_key, values.get("radarrApikey"));
        fill_optional(&mut self.tmdb_url, values.get("tmdbUrl"));
        fill_optional(&mut self.tmdb_api_key, values.get("tmdbApikey"));
        fill_optional(&mut self.sonarr_language_1, values.get("sonarrLanguage1"));
        fill_optional(&mut self.sonarr_language_2, values.get("sonarrLanguage2"));
        if self.rule_sync_authors.is_none() {
            self.rule_sync_authors = values.get("ruleSyncAuthors").map(str::to_owned);
        }
        if self.rule_location.is_none() {
            self.rule_location = values.get("ruleLocation").map(str::to_owned);
        }
        if self.rule_location_backup.is_none() {
            self.rule_location_backup = values.get("ruleLocationBackup").map(str::to_owned);
        }
        fill_raw_optional(&mut self.qbittorrent_url, values.get("qbittorrentUrl"));
        fill_raw_optional(
            &mut self.qbittorrent_username,
            values.get("qbittorrentUsername"),
        );
        fill_raw_optional(
            &mut self.qbittorrent_password,
            values.get("qbittorrentPassword"),
        );
        fill_raw_optional(&mut self.transmission_url, values.get("transmissionUrl"));
        fill_raw_optional(
            &mut self.transmission_username,
            values.get("transmissionUsername"),
        );
        fill_raw_optional(
            &mut self.transmission_password,
            values.get("transmissionPassword"),
        );
    }

    pub fn system_config_keys() -> &'static [&'static str] {
        SYSTEM_CONFIG_KEYS
    }

    /// Whether enough values exist to attempt a qBittorrent login. A blank
    /// password is valid and is reported as configured.
    pub fn qbittorrent_configured(&self) -> bool {
        non_blank(self.qbittorrent_url.as_deref()).is_some()
            && self.qbittorrent_username.is_some()
            && self.qbittorrent_password.is_some()
    }

    pub fn transmission_configured(&self) -> bool {
        non_blank(self.transmission_url.as_deref()).is_some()
            && self.transmission_username.is_some()
            && self.transmission_password.is_some()
    }

    pub fn downloaders_configured(&self) -> bool {
        self.qbittorrent_configured() || self.transmission_configured()
    }

    pub fn tmdb_url(&self) -> &str {
        non_blank(self.tmdb_url.as_deref()).unwrap_or(DEFAULT_TMDB_URL)
    }

    pub fn sonarr_languages(&self) -> [String; 2] {
        [
            non_blank(self.sonarr_language_1.as_deref())
                .unwrap_or(DEFAULT_SONARR_LANGUAGE_1)
                .to_owned(),
            non_blank(self.sonarr_language_2.as_deref())
                .unwrap_or(DEFAULT_SONARR_LANGUAGE_2)
                .to_owned(),
        ]
    }

    pub fn rule_sync_authors(&self) -> &str {
        self.rule_sync_authors
            .as_deref()
            .unwrap_or(DEFAULT_RULE_SYNC_AUTHORS)
    }

    /// Primary and backup rule locations. An explicitly blank primary disables
    /// rule synchronization entirely.
    pub fn rule_locations(&self) -> Vec<String> {
        let primary = self
            .rule_location
            .clone()
            .unwrap_or_else(|| DEFAULT_RULE_LOCATION.to_owned());
        if primary.trim().is_empty() {
            return Vec::new();
        }
        let mut locations = vec![primary.clone()];
        let backup = self
            .rule_location_backup
            .clone()
            .unwrap_or_else(|| DEFAULT_RULE_LOCATION_BACKUP.to_owned());
        if !backup.trim().is_empty() && backup.trim() != primary.trim() {
            locations.push(backup);
        }
        locations
    }

    pub fn title_sync_configured(&self, kind: AppKind) -> bool {
        match kind {
            AppKind::Sonarr => self.sonarr_url.is_some() && self.sonarr_api_key.is_some(),
            AppKind::Radarr => self.radarr_url.is_some() && self.radarr_api_key.is_some(),
        }
    }

    /// A compact startup summary that never includes API keys, URLs, author
    /// lists, or rule locations.
    pub fn startup_summary(&self) -> String {
        format!(
            "listen={} database={} min_count={} sync_enabled={} sync_interval={}s cache_ttl={}s result_cache_ttl={}s cache_max_entries={} rename_file={} rename_enabled={} login_enabled={} sonarr_sync={} radarr_sync={} tmdb_sync={} rule_sync={} qbittorrent={} transmission={} login_interval={}s sonarr_rename_interval={}s radarr_rename_interval={}s sonarr_rename_fallback={}s radarr_rename_fallback={}s languages={},{} ondemand_timeout={}s upstream_concurrency={} upstream_retries={}",
            self.listen_addr,
            self.database_path.display(),
            self.min_count,
            self.sync_enabled,
            self.sync_interval.as_secs(),
            self.cache_expires.as_secs(),
            self.indexer_result_cache_expires.as_secs(),
            self.cache_max_entries,
            self.rename_file,
            !self.sonarr_rename_interval.is_zero() || !self.radarr_rename_interval.is_zero(),
            self.login_enabled,
            self.title_sync_configured(AppKind::Sonarr),
            self.title_sync_configured(AppKind::Radarr),
            self.tmdb_api_key.is_some(),
            !self.rule_locations().is_empty(),
            self.qbittorrent_configured(),
            self.transmission_configured(),
            self.downloader_login_interval.as_secs(),
            self.sonarr_rename_interval.as_secs(),
            self.radarr_rename_interval.as_secs(),
            self.sonarr_rename_fallback.as_secs(),
            self.radarr_rename_fallback.as_secs(),
            self.sonarr_languages()[0],
            self.sonarr_languages()[1],
            self.on_demand_sync_timeout.as_secs(),
            self.upstream_max_concurrency,
            self.upstream_retry.max_retries,
        )
    }
}

fn fill_optional(target: &mut Option<String>, value: Option<&str>) {
    if target.is_none() {
        *target = value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }
}

/// Fill a credential without trimming. A present but empty `system_config`
/// value is preserved because a blank downloader password is valid.
fn fill_raw_optional(target: &mut Option<String>, value: Option<&str>) {
    if target.is_none()
        && let Some(value) = value
    {
        *target = Some(value.to_owned());
    }
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn env_string(name: &str) -> Option<String> {
    env_raw(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Credential lookup that preserves an explicitly empty value. An unset
/// variable still means "not configured".
fn env_secret(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Raw environment lookup. Unlike `env_string`, an explicitly empty value is
/// preserved so rule synchronization can be disabled with `RULE_LOCATION=`.
fn env_raw(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    let Some(value) = env_string(name) else {
        return Ok(default);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("{name} must be true/false, 1/0, yes/no, or on/off"),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    match env_string(name) {
        Some(value) => value
            .parse()
            .with_context(|| format!("{name} must be a non-negative integer")),
        None => Ok(default),
    }
}

fn env_usize(name: &str, default: usize) -> Result<usize> {
    let value = env_u64(name, default as u64)?;
    usize::try_from(value).with_context(|| format!("{name} is too large"))
}

fn env_u32(name: &str, default: u32) -> Result<u32> {
    let value = env_u64(name, default as u64)?;
    u32::try_from(value).with_context(|| format!("{name} is too large"))
}

fn env_minutes(name: &str, default: u64) -> Result<u64> {
    env_u64(name, default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            database_path: PathBuf::from(":memory:"),
            prowlarr_url: None,
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
            sync_interval: Duration::from_secs(60),
            indexer_result_cache_expires: Duration::from_secs(15),
            downloader_login_interval: Duration::from_secs(1800),
            sonarr_rename_interval: Duration::from_secs(30),
            radarr_rename_interval: Duration::from_secs(60),
            sonarr_rename_fallback: Duration::from_secs(1800),
            radarr_rename_fallback: Duration::from_secs(1800),
            rename_file: false,
            login_enabled: false,
            sync_enabled: true,
            request_timeout: Duration::from_secs(5),
            on_demand_sync_timeout: Duration::from_secs(5),
            result_cache_max_entries: 100,
            cache_max_entries: 100,
            upstream_max_concurrency: 8,
            upstream_retry: RetryPolicy::default(),
        }
    }

    #[test]
    fn system_config_fills_only_unset_values() {
        let mut config = test_config();
        config.sonarr_url = Some("http://env.invalid".to_owned());
        let values = SystemConfigValues::new(HashMap::from([
            ("sonarrUrl".to_owned(), "http://db.invalid".to_owned()),
            ("sonarrApikey".to_owned(), "db-key".to_owned()),
            ("sonarrLanguage1".to_owned(), "ja-JP".to_owned()),
            ("ruleSyncAuthors".to_owned(), "alice,bob".to_owned()),
            ("ruleLocation".to_owned(), "http://rules.invalid".to_owned()),
            (
                "qbittorrentUrl".to_owned(),
                "http://qbittorrent.invalid".to_owned(),
            ),
            ("qbittorrentUsername".to_owned(), "db-user".to_owned()),
            ("qbittorrentPassword".to_owned(), "db-pass".to_owned()),
            (
                "transmissionUrl".to_owned(),
                "http://transmission.invalid".to_owned(),
            ),
            ("transmissionUsername".to_owned(), "db-user".to_owned()),
            ("transmissionPassword".to_owned(), "db-pass".to_owned()),
        ]));
        config.apply_system_config(&values);

        assert_eq!(config.sonarr_url.as_deref(), Some("http://env.invalid"));
        assert_eq!(config.sonarr_api_key.as_deref(), Some("db-key"));
        assert_eq!(config.sonarr_languages()[0], "ja-JP");
        assert_eq!(config.sonarr_languages()[1], "zh-TW");
        assert_eq!(config.rule_sync_authors(), "alice,bob");
        assert_eq!(
            config.qbittorrent_password.as_deref(),
            Some("db-pass"),
            "downloader credentials are loaded from system_config"
        );
        assert!(config.qbittorrent_configured());
        assert!(config.transmission_configured());
        assert_eq!(
            config.rule_locations(),
            vec![
                "http://rules.invalid".to_owned(),
                DEFAULT_RULE_LOCATION_BACKUP.to_owned()
            ]
        );
    }

    #[test]
    fn blank_rule_configuration_disables_rule_sync() {
        let mut config = test_config();
        config.rule_location = Some(String::new());
        assert!(config.rule_locations().is_empty());

        let mut config = test_config();
        config.rule_sync_authors = Some(String::new());
        assert_eq!(config.rule_sync_authors(), "");
    }

    #[test]
    fn startup_summary_never_contains_secret_values() {
        let mut config = test_config();
        config.sonarr_url = Some("http://sonarr.invalid".to_owned());
        config.sonarr_api_key = Some("super-secret".to_owned());
        config.tmdb_api_key = Some("tmdb-secret".to_owned());
        config.rule_location = Some("http://rules.invalid".to_owned());
        config.rule_sync_authors = Some("private-author".to_owned());
        config.qbittorrent_url = Some("http://qbittorrent.invalid".to_owned());
        config.qbittorrent_username = Some("qb-user".to_owned());
        config.qbittorrent_password = Some("qb-password".to_owned());
        config.transmission_url = Some("http://transmission.invalid".to_owned());
        config.transmission_username = Some("tr-user".to_owned());
        config.transmission_password = Some("tr-password".to_owned());
        let summary = config.startup_summary();
        for secret in [
            "super-secret",
            "tmdb-secret",
            "sonarr.invalid",
            "rules.invalid",
            "private-author",
            "qbittorrent.invalid",
            "transmission.invalid",
            "qb-user",
            "qb-password",
            "tr-user",
            "tr-password",
        ] {
            assert!(!summary.contains(secret), "summary leaked {secret}");
        }
    }
}
