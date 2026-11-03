use std::collections::HashSet;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;

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
    "sonarrIndexerFormat",
    "sonarrLanguage1",
    "sonarrLanguage2",
    "radarrUrl",
    "radarrApikey",
    "radarrIndexerFormat",
    "jackettUrl",
    "prowlarrUrl",
    "tmdbUrl",
    "tmdbApikey",
    "cleanTitleRegex",
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

/// One whitelisted `system_config` row, including its validity.
#[derive(Debug, Clone)]
pub struct ConfigRow {
    pub key: String,
    pub value: Option<String>,
    pub valid_status: i64,
}

/// Immutable view of the resolved configuration.
///
/// The snapshot is cheap to clone (three `Arc`s) and is replaced atomically by
/// [`ConfigStore::apply_rows`] after a successful configuration update. It
/// dereferences to [`Config`] so callers can keep using field access.
#[derive(Clone)]
pub struct ConfigSnapshot {
    config: Arc<Config>,
    invalid: Arc<HashSet<String>>,
    explicit: Arc<HashSet<String>>,
    /// Parsed clean-title regex. An invalid `system_config` value falls back to
    /// the built-in regex so proxy traffic keeps working.
    pub clean_title_regex: Arc<Regex>,
}

impl Deref for ConfigSnapshot {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

/// Failure modes for a Java `queryValueByKey`-style read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLookupError {
    /// The row exists but `validStatus` is `INVALID`.
    Invalid(String),
    /// No environment override and no usable row.
    Missing(String),
}

impl std::fmt::Display for ConfigLookupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(key) => write!(formatter, "system config is invalid: {key}"),
            Self::Missing(key) => write!(formatter, "system config is missing: {key}"),
        }
    }
}

impl std::error::Error for ConfigLookupError {}

impl ConfigSnapshot {
    /// Java `SystemConfigServiceImpl.queryValueByKey`.
    ///
    /// Environment values take precedence and are always returned even when the
    /// corresponding database row is invalid. Otherwise an invalid row is an
    /// error, exactly like Java's `SystemConfigException`.
    pub fn query_value_by_key(&self, key: &str) -> Result<&str, ConfigLookupError> {
        if self.explicit.contains(key) {
            return self
                .config
                .value_for_key(key)
                .ok_or_else(|| ConfigLookupError::Missing(key.to_owned()));
        }
        if self.invalid.contains(key) {
            return Err(ConfigLookupError::Invalid(key.to_owned()));
        }
        self.config
            .value_for_key(key)
            .ok_or_else(|| ConfigLookupError::Missing(key.to_owned()))
    }
}

/// Shared, refreshable configuration store.
///
/// The base configuration comes from the environment (and from explicit fields
/// in tests). Valid `system_config` rows fill only values that the environment
/// did not set. Replacing the snapshot is the equivalent of Java's
/// `@CacheEvict(SYSTEM_CONFIG)` after `updateSystemConfig`.
pub struct ConfigStore {
    base: Arc<Config>,
    explicit: Arc<HashSet<String>>,
    current: RwLock<ConfigSnapshot>,
}

impl ConfigStore {
    pub fn new(base: Config) -> Result<Self> {
        Regex::new(&base.clean_title_regex)
            .context("CLEAN_TITLE_REGEX is not a valid regular expression")?;
        let base = Arc::new(base);
        let explicit: Arc<HashSet<String>> = Arc::new(
            SYSTEM_CONFIG_KEYS
                .iter()
                .copied()
                .filter(|key| base.has_explicit_value(key))
                .map(str::to_owned)
                .collect(),
        );
        let clean_title_regex = Arc::new(compile_clean_regex(&base.clean_title_regex));
        let current = ConfigSnapshot {
            config: base.clone(),
            invalid: Arc::new(HashSet::new()),
            explicit: explicit.clone(),
            clean_title_regex,
        };
        Ok(Self {
            base,
            explicit,
            current: RwLock::new(current),
        })
    }

    pub fn snapshot(&self) -> ConfigSnapshot {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replace the resolved configuration with `base` plus all valid rows.
    ///
    /// Invalid rows are recorded so `query_value_by_key` can fail the way Java
    /// does. They are never applied to the resolved values.
    pub fn apply_rows(&self, rows: &[ConfigRow]) {
        let mut next = (*self.base).clone();
        let mut invalid = HashSet::new();
        for row in rows {
            if row.valid_status == 1 {
                if !self.explicit.contains(&row.key) {
                    next.apply_key(&row.key, row.value.as_deref());
                }
            } else {
                invalid.insert(row.key.clone());
            }
        }
        let clean_title_regex = Arc::new(compile_clean_regex(&next.clean_title_regex));
        let snapshot = ConfigSnapshot {
            config: Arc::new(next),
            invalid: Arc::new(invalid),
            explicit: self.explicit.clone(),
            clean_title_regex,
        };
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
    }
}

fn compile_clean_regex(source: &str) -> Regex {
    Regex::new(source).unwrap_or_else(|_| {
        Regex::new(DEFAULT_CLEAN_TITLE_REGEX).expect("built-in clean-title regex is valid")
    })
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
    /// Java `time.sonarr-rename-fall-back`, in minutes.
    pub sonarr_rename_fallback: Duration,
    /// Java `time.radarr-rename-fall-back`, in minutes.
    pub radarr_rename_fallback: Duration,
    /// Java `rename.file`: also rename the files inside each torrent.
    pub rename_file: bool,
    /// Java `time.token-expires`, in minutes.
    pub token_expires: Duration,
    /// GitHub releases endpoint used by `/api/system/config/version`. Java
    /// hard-codes this URL; the override keeps tests on a local mock server.
    pub version_check_url: String,
    /// Directory containing the compiled Java admin UI (`admin_dist`).
    pub admin_ui_dir: PathBuf,
    /// Parsed for compatibility with the Java deployment.
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
            sonarr_rename_fallback: Duration::from_secs(
                env_minutes("SONARR_RENAME_FALLBACK_MINS", 30)?.saturating_mul(60),
            ),
            radarr_rename_fallback: Duration::from_secs(
                env_minutes("RADARR_RENAME_FALLBACK_MINS", 30)?.saturating_mul(60),
            ),
            rename_file: env_bool("RENAME_FILE", true)?,
            token_expires: Duration::from_secs(
                env_minutes("TOKEN_EXPIRES", 10080)?.saturating_mul(60),
            ),
            version_check_url: env_string("VERSION_CHECK_URL").unwrap_or_else(|| {
                "https://api.github.com/repos/LuckyPuppy514/jproxy/releases/latest".to_owned()
            }),
            admin_ui_dir: env_string("ADMIN_UI_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("admin_dist")),
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

    /// Apply one valid `system_config` row to this configuration. Environment
    /// values are handled by the caller (they always win).
    fn apply_key(&mut self, key: &str, value: Option<&str>) {
        match key {
            "sonarrUrl" => self.sonarr_url = optional_trimmed(value),
            "sonarrApikey" => self.sonarr_api_key = optional_trimmed(value),
            "radarrUrl" => self.radarr_url = optional_trimmed(value),
            "radarrApikey" => self.radarr_api_key = optional_trimmed(value),
            "tmdbUrl" => self.tmdb_url = optional_trimmed(value),
            "tmdbApikey" => self.tmdb_api_key = optional_trimmed(value),
            "sonarrLanguage1" => self.sonarr_language_1 = optional_trimmed(value),
            "sonarrLanguage2" => self.sonarr_language_2 = optional_trimmed(value),
            "prowlarrUrl" => self.prowlarr_url = optional_trimmed(value),
            "jackettUrl" => self.jackett_url = optional_trimmed(value),
            "sonarrIndexerFormat" => {
                if let Some(value) = non_blank_owned(value) {
                    self.sonarr_indexer_format = value;
                }
            }
            "radarrIndexerFormat" => {
                if let Some(value) = non_blank_owned(value) {
                    self.radarr_indexer_format = value;
                }
            }
            "cleanTitleRegex" => {
                if let Some(value) = non_blank_owned(value) {
                    self.clean_title_regex = value;
                }
            }
            "ruleSyncAuthors" => self.rule_sync_authors = value.map(str::to_owned),
            "ruleLocation" => self.rule_location = value.map(str::to_owned),
            "ruleLocationBackup" => self.rule_location_backup = value.map(str::to_owned),
            "qbittorrentUrl" => self.qbittorrent_url = optional_trimmed(value),
            "qbittorrentUsername" => self.qbittorrent_username = value.map(str::to_owned),
            "qbittorrentPassword" => self.qbittorrent_password = value.map(str::to_owned),
            "transmissionUrl" => self.transmission_url = optional_trimmed(value),
            "transmissionUsername" => self.transmission_username = value.map(str::to_owned),
            "transmissionPassword" => self.transmission_password = value.map(str::to_owned),
            _ => {}
        }
    }

    /// Current value for a whitelisted key, regardless of where it came from.
    pub fn value_for_key(&self, key: &str) -> Option<&str> {
        match key {
            "sonarrUrl" => self.sonarr_url.as_deref(),
            "sonarrApikey" => self.sonarr_api_key.as_deref(),
            "radarrUrl" => self.radarr_url.as_deref(),
            "radarrApikey" => self.radarr_api_key.as_deref(),
            "tmdbUrl" => self.tmdb_url.as_deref().or(Some(DEFAULT_TMDB_URL)),
            "tmdbApikey" => self.tmdb_api_key.as_deref(),
            "sonarrLanguage1" => self
                .sonarr_language_1
                .as_deref()
                .or(Some(DEFAULT_SONARR_LANGUAGE_1)),
            "sonarrLanguage2" => self
                .sonarr_language_2
                .as_deref()
                .or(Some(DEFAULT_SONARR_LANGUAGE_2)),
            "prowlarrUrl" => self.prowlarr_url.as_deref(),
            "jackettUrl" => self.jackett_url.as_deref(),
            "sonarrIndexerFormat" => Some(&self.sonarr_indexer_format),
            "radarrIndexerFormat" => Some(&self.radarr_indexer_format),
            "cleanTitleRegex" => Some(&self.clean_title_regex),
            "ruleSyncAuthors" => self
                .rule_sync_authors
                .as_deref()
                .or(Some(DEFAULT_RULE_SYNC_AUTHORS)),
            "ruleLocation" => self.rule_location.as_deref(),
            "ruleLocationBackup" => self.rule_location_backup.as_deref(),
            "qbittorrentUrl" => self.qbittorrent_url.as_deref(),
            "qbittorrentUsername" => self.qbittorrent_username.as_deref(),
            "qbittorrentPassword" => self.qbittorrent_password.as_deref(),
            "transmissionUrl" => self.transmission_url.as_deref(),
            "transmissionUsername" => self.transmission_username.as_deref(),
            "transmissionPassword" => self.transmission_password.as_deref(),
            _ => None,
        }
    }

    /// Whether the environment (or a test) supplied this key explicitly. Such
    /// values take precedence over `system_config`, including invalid rows.
    pub fn has_explicit_value(&self, key: &str) -> bool {
        if environment_override_present(key) {
            return true;
        }
        match key {
            "sonarrUrl" => self.sonarr_url.is_some(),
            "sonarrApikey" => self.sonarr_api_key.is_some(),
            "radarrUrl" => self.radarr_url.is_some(),
            "radarrApikey" => self.radarr_api_key.is_some(),
            "tmdbUrl" => self.tmdb_url.is_some(),
            "tmdbApikey" => self.tmdb_api_key.is_some(),
            "sonarrLanguage1" => self.sonarr_language_1.is_some(),
            "sonarrLanguage2" => self.sonarr_language_2.is_some(),
            "prowlarrUrl" => self.prowlarr_url.is_some(),
            "jackettUrl" => self.jackett_url.is_some(),
            "sonarrIndexerFormat" => self.sonarr_indexer_format != DEFAULT_SONARR_FORMAT,
            "radarrIndexerFormat" => self.radarr_indexer_format != DEFAULT_RADARR_FORMAT,
            "cleanTitleRegex" => self.clean_title_regex != DEFAULT_CLEAN_TITLE_REGEX,
            "ruleSyncAuthors" => self.rule_sync_authors.is_some(),
            "ruleLocation" => self.rule_location.is_some(),
            "ruleLocationBackup" => self.rule_location_backup.is_some(),
            "qbittorrentUrl" => self.qbittorrent_url.is_some(),
            "qbittorrentUsername" => self.qbittorrent_username.is_some(),
            "qbittorrentPassword" => self.qbittorrent_password.is_some(),
            "transmissionUrl" => self.transmission_url.is_some(),
            "transmissionUsername" => self.transmission_username.is_some(),
            "transmissionPassword" => self.transmission_password.is_some(),
            _ => false,
        }
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
            "listen={} database={} min_count={} sync_enabled={} sync_interval={}s cache_ttl={}s result_cache_ttl={}s cache_max_entries={} rename_file={} login_enabled={} sonarr_sync={} radarr_sync={} tmdb_sync={} rule_sync={} qbittorrent={} transmission={} sonarr_rename_fallback={}s radarr_rename_fallback={}s languages={},{} ondemand_timeout={}s upstream_concurrency={} upstream_retries={} schedule=java-cron",
            self.listen_addr,
            self.database_path.display(),
            self.min_count,
            self.sync_enabled,
            self.sync_interval.as_secs(),
            self.cache_expires.as_secs(),
            self.indexer_result_cache_expires.as_secs(),
            self.cache_max_entries,
            self.rename_file,
            self.login_enabled,
            self.title_sync_configured(AppKind::Sonarr),
            self.title_sync_configured(AppKind::Radarr),
            self.tmdb_api_key.is_some(),
            !self.rule_locations().is_empty(),
            self.qbittorrent_configured(),
            self.transmission_configured(),
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

/// Environment variable for a whitelisted key. Used to keep precedence exact
/// even when an environment value happens to equal the built-in default.
fn environment_override_present(key: &str) -> bool {
    let name = match key {
        "sonarrUrl" => "SONARR_URL",
        "sonarrApikey" => "SONARR_API_KEY",
        "radarrUrl" => "RADARR_URL",
        "radarrApikey" => "RADARR_API_KEY",
        "tmdbUrl" => "TMDB_URL",
        "tmdbApikey" => "TMDB_API_KEY",
        "sonarrLanguage1" => "SONARR_LANGUAGE_1",
        "sonarrLanguage2" => "SONARR_LANGUAGE_2",
        "prowlarrUrl" => "PROWLARR_URL",
        "jackettUrl" => "JACKETT_URL",
        "sonarrIndexerFormat" => "SONARR_INDEXER_FORMAT",
        "radarrIndexerFormat" => "RADARR_INDEXER_FORMAT",
        "cleanTitleRegex" => "CLEAN_TITLE_REGEX",
        "ruleSyncAuthors" => "RULE_SYNC_AUTHORS",
        "ruleLocation" => "RULE_LOCATION",
        "ruleLocationBackup" => "RULE_LOCATION_BACKUP",
        "qbittorrentUrl" => "QBITTORRENT_URL",
        "qbittorrentUsername" => "QBITTORRENT_USERNAME",
        "qbittorrentPassword" => "QBITTORRENT_PASSWORD",
        "transmissionUrl" => "TRANSMISSION_URL",
        "transmissionUsername" => "TRANSMISSION_USERNAME",
        "transmissionPassword" => "TRANSMISSION_PASSWORD",
        _ => return false,
    };
    std::env::var_os(name).is_some()
}

fn optional_trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn non_blank_owned(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
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
            sonarr_rename_fallback: Duration::from_secs(1800),
            radarr_rename_fallback: Duration::from_secs(1800),
            rename_file: false,
            token_expires: Duration::from_secs(10080 * 60),
            version_check_url: "http://127.0.0.1:1/releases/latest".to_owned(),
            admin_ui_dir: PathBuf::from("admin_dist"),
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

    fn row(key: &str, value: &str, valid_status: i64) -> ConfigRow {
        ConfigRow {
            key: key.to_owned(),
            value: Some(value.to_owned()),
            valid_status,
        }
    }

    #[test]
    fn valid_rows_fill_only_values_without_an_environment_override() {
        let mut config = test_config();
        config.sonarr_url = Some("http://env.invalid".to_owned());
        let store = ConfigStore::new(config).unwrap();
        store.apply_rows(&[
            row("sonarrUrl", "http://db.invalid", 1),
            row("sonarrApikey", "db-key", 1),
            row("sonarrLanguage1", "ja-JP", 1),
            row("ruleSyncAuthors", "alice,bob", 1),
            row("ruleLocation", "http://rules.invalid", 1),
            row("qbittorrentUrl", "http://qbittorrent.invalid", 1),
            row("qbittorrentUsername", "db-user", 1),
            row("qbittorrentPassword", "db-pass", 1),
            row("transmissionUrl", "http://transmission.invalid", 1),
            row("transmissionUsername", "db-user", 1),
            row("transmissionPassword", "db-pass", 1),
            row("sonarrIndexerFormat", "{title} {season} {episode}", 1),
            row("cleanTitleRegex", "!", 1),
        ]);

        let snapshot = store.snapshot();
        assert_eq!(snapshot.sonarr_url.as_deref(), Some("http://env.invalid"));
        assert_eq!(snapshot.sonarr_api_key.as_deref(), Some("db-key"));
        assert_eq!(snapshot.sonarr_languages()[0], "ja-JP");
        assert_eq!(snapshot.sonarr_languages()[1], "zh-TW");
        assert_eq!(snapshot.rule_sync_authors(), "alice,bob");
        assert_eq!(snapshot.sonarr_indexer_format, "{title} {season} {episode}");
        assert_eq!(snapshot.clean_title_regex.as_str(), "!");
        assert_eq!(
            snapshot.qbittorrent_password.as_deref(),
            Some("db-pass"),
            "downloader credentials are loaded from system_config"
        );
        assert!(snapshot.qbittorrent_configured());
        assert!(snapshot.transmission_configured());
        assert_eq!(
            snapshot.rule_locations(),
            vec![
                "http://rules.invalid".to_owned(),
                DEFAULT_RULE_LOCATION_BACKUP.to_owned()
            ]
        );
        assert_eq!(
            snapshot.query_value_by_key("sonarrUrl").unwrap(),
            "http://env.invalid"
        );
    }

    #[test]
    fn invalid_rows_are_not_applied_and_strict_reads_fail() {
        let mut config = test_config();
        config.sonarr_url = Some("http://env.invalid".to_owned());
        let store = ConfigStore::new(config).unwrap();
        store.apply_rows(&[
            row("sonarrUrl", "http://db.invalid", 0),
            row("radarrUrl", "http://radarr.invalid", 0),
            row("radarrApikey", "radarr-key", 0),
        ]);
        let snapshot = store.snapshot();
        // The environment override still wins over the invalid row.
        assert_eq!(snapshot.sonarr_url.as_deref(), Some("http://env.invalid"));
        assert_eq!(
            snapshot.query_value_by_key("sonarrUrl").unwrap(),
            "http://env.invalid"
        );
        assert_eq!(snapshot.radarr_url, None);
        assert_eq!(
            snapshot.query_value_by_key("radarrUrl"),
            Err(ConfigLookupError::Invalid("radarrUrl".to_owned()))
        );
        assert_eq!(
            snapshot.query_value_by_key("radarrApikey"),
            Err(ConfigLookupError::Invalid("radarrApikey".to_owned()))
        );
    }

    #[test]
    fn applying_new_rows_replaces_the_live_snapshot() {
        let store = ConfigStore::new(test_config()).unwrap();
        store.apply_rows(&[row("radarrUrl", "http://first.invalid", 1)]);
        assert_eq!(
            store.snapshot().radarr_url.as_deref(),
            Some("http://first.invalid")
        );

        store.apply_rows(&[row("radarrUrl", "http://second.invalid", 1)]);
        assert_eq!(
            store.snapshot().radarr_url.as_deref(),
            Some("http://second.invalid")
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
