use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::upstream::RetryPolicy;

/// Clean-title regular expression from the final upstream Liquibase migration.
pub const DEFAULT_CLEAN_TITLE_REGEX: &str =
    r#"(`|,|~|!|@|#|%|&|_|=|'|"|:|<|>|-|—|·|，|～|、|。|‘|’|“|”|？|！|：|（|）|【|】|《|》|♀|/)"#;
pub const DEFAULT_SONARR_FORMAT: &str =
    "{title} {season}{episode} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}";
pub const DEFAULT_RADARR_FORMAT: &str =
    "{title} {year} {language}{subtitle}{resolution}{quality}{dynamic_range}{group}";

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
    pub sonarr_rules_url: Option<String>,
    pub radarr_rules_url: Option<String>,
    pub clean_title_regex: String,
    pub sonarr_indexer_format: String,
    pub radarr_indexer_format: String,
    /// Java defaults to 6 in application-prod.yml. Zero disables all retries.
    pub min_count: usize,
    /// Caffeine-compatible reference-data TTL. Zero expires immediately.
    pub cache_expires: Duration,
    /// Sync period. Zero disables the background task.
    pub sync_interval: Duration,
    pub indexer_result_cache_expires: Duration,
    /// Parsed for compatibility with the Java deployment. Phase 1 has no renamer.
    pub rename_file: bool,
    /// Parsed for compatibility with the Java deployment. Phase 1 has no auth.
    pub login_enabled: bool,
    pub sync_enabled: bool,
    pub request_timeout: Duration,
    pub result_cache_max_entries: usize,
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
            sonarr_rules_url: env_string("SONARR_RULES_URL"),
            radarr_rules_url: env_string("RADARR_RULES_URL"),
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
            rename_file: env_bool("RENAME_FILE", true)?,
            login_enabled: env_bool("LOGIN_ENABLED", false)?,
            sync_enabled,
            request_timeout: Duration::from_secs(env_u64("REQUEST_TIMEOUT_SECS", 30)?),
            result_cache_max_entries: env_usize("RESULT_CACHE_MAX_ENTRIES", 1000)?,
            upstream_max_concurrency: env_usize("UPSTREAM_MAX_CONCURRENCY", 8)?.max(1),
            upstream_retry: RetryPolicy {
                max_retries: env_u32("UPSTREAM_MAX_RETRIES", 2)?,
                base_delay: Duration::from_millis(env_u64("UPSTREAM_RETRY_BASE_MS", 250)?),
                max_delay: Duration::from_millis(env_u64("UPSTREAM_RETRY_MAX_MS", 4000)?),
            },
        })
    }

    /// A compact startup summary that never includes API keys or upstream URLs.
    pub fn startup_summary(&self) -> String {
        format!(
            "listen={} database={} min_count={} sync_enabled={} sync_interval={}s cache_ttl={}s result_cache_ttl={}s rename_file={} login_enabled={} upstream_concurrency={} upstream_retries={}",
            self.listen_addr,
            self.database_path.display(),
            self.min_count,
            self.sync_enabled,
            self.sync_interval.as_secs(),
            self.cache_expires.as_secs(),
            self.indexer_result_cache_expires.as_secs(),
            self.rename_file,
            self.login_enabled,
            self.upstream_max_concurrency,
            self.upstream_retry.max_retries,
        )
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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
