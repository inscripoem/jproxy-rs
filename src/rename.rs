//! Sonarr/Radarr history rename tasks.
//!
//! Port of the Java `DownloaderLoginTask`, `SonarrRenameTask`, and
//! `RadarrRenameTask`. The service fetches download history since a fallback
//! window, renames the torrent through the logged-in downloader, and, when
//! `RENAME_FILE` is enabled, renames the files inside a qBittorrent torrent.
//! Media files are never deleted or moved outside the downloader.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::app::AppState;
use crate::downloader::VIDEO_AND_SUBTITLE_EXTENSION_REGEX;
use crate::formatting::RenameFormatter;
use crate::models::AppKind;
use crate::schedule;

static EPISODE_IN_FILE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:S\d+|\b|\s)E\d+$").expect("static regex"));
static SUBTITLE_EXTENSION_FULL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\.(ass|srt|ssa|idx|sub))$").expect("static regex"));

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoginReport {
    pub qbittorrent: bool,
    pub transmission: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RenameReport {
    /// No downloader was logged in; the task returned before fetching history.
    pub skipped: bool,
    pub history_items: usize,
    pub renamed_torrents: usize,
    pub renamed_files: usize,
    pub errors: usize,
}

#[derive(Debug, Deserialize)]
struct HistoryItem {
    #[serde(rename = "sourceTitle", default)]
    source_title: Option<String>,
    #[serde(rename = "downloadId", default)]
    download_id: Option<String>,
    #[serde(default)]
    data: Option<HistoryData>,
}

#[derive(Debug, Deserialize)]
struct HistoryData {
    #[serde(rename = "torrentInfoHash", default)]
    torrent_info_hash: Option<String>,
    #[serde(rename = "downloadClient", default)]
    download_client: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ItemOutcome {
    torrent_renamed: bool,
    files_renamed: usize,
}

/// Start the login and rename schedules.
///
/// Java cron cadence from `application-prod.yml`:
/// downloader login every 30 minutes at `:00` (plus one startup run from
/// `CommandLineRunner`), Sonarr rename at seconds `0`/`30` of every minute, and
/// Radarr rename at second `0` of every minute. Cron tasks do not fire at
/// startup, so the first rename tick is the next wall-clock boundary.
pub fn spawn(state: AppState) {
    let login_state = state.clone();
    tokio::spawn(async move {
        let report = login_once(&login_state).await;
        debug!(?report, "initial downloader login pass complete");
        loop {
            schedule::sleep_until_next(schedule::DOWNLOADER_LOGIN).await;
            let report = login_once(&login_state).await;
            debug!(?report, "scheduled downloader login pass complete");
        }
    });

    spawn_rename(state.clone(), AppKind::Sonarr);
    spawn_rename(state, AppKind::Radarr);
}

fn spawn_rename(state: AppState, kind: AppKind) {
    let schedule = match kind {
        AppKind::Sonarr => schedule::SONARR_RENAME,
        AppKind::Radarr => schedule::RADARR_RENAME,
    };
    tokio::spawn(async move {
        loop {
            schedule::sleep_until_next(schedule).await;
            match rename_once(&state, kind).await {
                Ok(report) => debug!(%kind, ?report, "rename pass complete"),
                Err(error) => warn!(%kind, %error, "rename pass failed"),
            }
        }
    });
}

/// Log in to both configured downloaders. Failures are non-fatal and are
/// retried by the next scheduled pass. Invalid `system_config` rows are treated
/// like Java's caught `SystemConfigException`: that downloader is skipped.
pub async fn login_once(state: &AppState) -> LoginReport {
    let mut report = LoginReport::default();
    let snapshot = state.snapshot();

    if let Some((url, username, password)) = downloader_credentials(
        &snapshot,
        "qbittorrentUrl",
        "qbittorrentUsername",
        "qbittorrentPassword",
    ) {
        report.qbittorrent = state
            .downloaders
            .qbittorrent
            .login_with(&url, &username, &password)
            .await;
        if report.qbittorrent {
            info!("qBittorrent logged in");
        } else {
            debug!("qBittorrent login failed");
        }
    }

    if let Some((url, username, password)) = downloader_credentials(
        &snapshot,
        "transmissionUrl",
        "transmissionUsername",
        "transmissionPassword",
    ) {
        report.transmission = state
            .downloaders
            .transmission
            .login_with(&url, &username, &password)
            .await;
        if report.transmission {
            info!("Transmission logged in");
        } else {
            debug!("Transmission login failed");
        }
    }

    report
}

fn downloader_credentials(
    snapshot: &crate::config::ConfigSnapshot,
    url_key: &str,
    username_key: &str,
    password_key: &str,
) -> Option<(String, String, String)> {
    let read = |key: &str| match snapshot.query_value_by_key(key) {
        Ok(value) => Some(value.to_owned()),
        Err(error) => {
            debug!(%key, %error, "downloader configuration unavailable");
            None
        }
    };
    let url = read(url_key)?;
    if url.trim().is_empty() {
        return None;
    }
    Some((
        url.trim().to_owned(),
        read(username_key)?,
        read(password_key)?,
    ))
}

pub async fn sonarr_rename_once(state: &AppState) -> Result<RenameReport> {
    rename_once(state, AppKind::Sonarr).await
}

pub async fn radarr_rename_once(state: &AppState) -> Result<RenameReport> {
    rename_once(state, AppKind::Radarr).await
}

async fn rename_once(state: &AppState, kind: AppKind) -> Result<RenameReport> {
    let mut report = RenameReport::default();
    if !state.downloaders.any_logged_in().await {
        report.skipped = true;
        return Ok(report);
    }

    let history = match fetch_history(state, kind).await? {
        Some(history) => history,
        None => {
            report.skipped = true;
            return Ok(report);
        }
    };
    report.history_items = history.len();

    let formatter = match kind {
        AppKind::Sonarr => Some(
            RenameFormatter::load(&state.pool, kind, &["season", "episode"])
                .await
                .context("load Sonarr rename rules")?,
        ),
        AppKind::Radarr => None,
    };

    let mut seen_hashes = HashSet::new();
    for item in history {
        let Some(hash) = history_hash(&item) else {
            continue;
        };
        // Java deduplicates Sonarr history by torrent hash; Radarr processes
        // every record.
        if kind == AppKind::Sonarr && !seen_hashes.insert(hash.clone()) {
            continue;
        }
        let Some(source_title) = item
            .source_title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
        else {
            debug!(%kind, %hash, "history item has no source title");
            report.errors += 1;
            continue;
        };

        match process_item(
            state,
            kind,
            &hash,
            source_title,
            item.data.as_ref(),
            formatter.as_ref(),
        )
        .await
        {
            Ok(outcome) => {
                if outcome.torrent_renamed {
                    report.renamed_torrents += 1;
                }
                report.renamed_files += outcome.files_renamed;
            }
            Err(error) => {
                report.errors += 1;
                warn!(
                    %kind,
                    %hash,
                    error = %error,
                    "download rename failed for one history item"
                );
            }
        }
    }

    Ok(report)
}

fn history_hash(item: &HistoryItem) -> Option<String> {
    match item.download_id.as_deref().map(str::trim) {
        Some(download_id) if !download_id.is_empty() => Some(download_id.to_lowercase()),
        _ => item
            .data
            .as_ref()
            .and_then(|data| data.torrent_info_hash.as_deref())
            .map(str::to_owned),
    }
}

async fn process_item(
    state: &AppState,
    kind: AppKind,
    hash: &str,
    source_title: &str,
    data: Option<&HistoryData>,
    formatter: Option<&RenameFormatter>,
) -> Result<ItemOutcome> {
    let mut outcome = ItemOutcome::default();
    if download_client_is_transmission(data) {
        outcome.torrent_renamed = state
            .downloaders
            .transmission
            .rename(hash, source_title)
            .await?;
        if outcome.torrent_renamed {
            info!(%hash, %source_title, "Transmission torrent renamed");
        }
        return Ok(outcome);
    }

    outcome.torrent_renamed = state
        .downloaders
        .qbittorrent
        .rename(hash, source_title)
        .await?;
    if !outcome.torrent_renamed {
        debug!(%hash, %source_title, "qBittorrent torrent rename was not applied");
        return Ok(outcome);
    }
    info!(%hash, %source_title, "qBittorrent torrent renamed");

    if !state.snapshot().rename_file {
        debug!("file rename disabled");
        return Ok(outcome);
    }
    outcome.files_renamed = match kind {
        AppKind::Sonarr => {
            rename_sonarr_files(
                &state.downloaders,
                hash,
                source_title,
                formatter.expect("Sonarr rename formatter"),
            )
            .await?
        }
        AppKind::Radarr => rename_radarr_files(&state.downloaders, hash, source_title).await?,
    };
    Ok(outcome)
}

fn download_client_is_transmission(data: Option<&HistoryData>) -> bool {
    data.and_then(|data| data.download_client.as_deref())
        .is_some_and(|client| client.eq_ignore_ascii_case("Transmission"))
}

async fn rename_sonarr_files(
    downloaders: &crate::downloader::Downloaders,
    hash: &str,
    source_title: &str,
    formatter: &RenameFormatter,
) -> Result<usize> {
    let mut subtitle_no = 1;
    let mut renamed = 0;
    let mut new_name_format = formatter.format(source_title, "{season}", true, true);
    new_name_format.push_str("{episode}");

    for old_path in downloaders.qbittorrent.files(hash).await? {
        let last_index = path_split_index(&old_path);
        if last_index > 0 && source_title == &old_path[..last_index - 1] {
            debug!(%old_path, "qBittorrent files are already renamed");
            break;
        }
        let old_file_name = &old_path[last_index..];
        let mut new_file_name = old_file_name.to_owned();

        if let Some(captures) = VIDEO_AND_SUBTITLE_EXTENSION_REGEX.captures(old_file_name)
            && let Some(extension) = captures.get(1)
        {
            let candidate = VIDEO_AND_SUBTITLE_EXTENSION_REGEX
                .replace(old_file_name, "")
                .into_owned();
            let candidate = formatter
                .format(&candidate, &new_name_format, true, true)
                .trim()
                .to_owned();
            if candidate.is_empty() || !EPISODE_IN_FILE_NAME.is_match(&candidate) {
                new_file_name = old_file_name.to_owned();
            } else {
                let extension = numbered_extension(extension.as_str(), &mut subtitle_no);
                let suffix = source_title
                    .find(" [")
                    .map(|index| &source_title[index..])
                    .unwrap_or("");
                new_file_name = format!("{candidate}{suffix}{extension}");
            }
        }

        let new_path = renamed_path(&old_path, last_index, source_title, &new_file_name);
        downloaders
            .qbittorrent
            .rename_file(hash, &old_path, &new_path)
            .await?;
        renamed += 1;
        info!(%old_file_name, %new_file_name, "qBittorrent file renamed");
    }
    Ok(renamed)
}

async fn rename_radarr_files(
    downloaders: &crate::downloader::Downloaders,
    hash: &str,
    source_title: &str,
) -> Result<usize> {
    let mut subtitle_no = 1;
    let mut renamed = 0;

    for old_path in downloaders.qbittorrent.files(hash).await? {
        let last_index = path_split_index(&old_path);
        if last_index > 0 && source_title == &old_path[..last_index - 1] {
            debug!(%old_path, "qBittorrent files are already renamed");
            break;
        }
        let old_file_name = &old_path[last_index..];
        let mut new_file_name = old_file_name.to_owned();

        if let Some(captures) = VIDEO_AND_SUBTITLE_EXTENSION_REGEX.captures(old_file_name)
            && let Some(extension) = captures.get(1)
        {
            let extension = numbered_extension(extension.as_str(), &mut subtitle_no);
            let suffix = source_title
                .find(" [")
                .map(|index| &source_title[index..])
                .unwrap_or("");
            new_file_name = format!("{source_title}{suffix}{extension}");
        }

        let new_path = renamed_path(&old_path, last_index, source_title, &new_file_name);
        downloaders
            .qbittorrent
            .rename_file(hash, &old_path, &new_path)
            .await?;
        renamed += 1;
        info!(%old_file_name, %new_file_name, "qBittorrent file renamed");
    }
    Ok(renamed)
}

fn path_split_index(path: &str) -> usize {
    path.rfind('/').map(|index| index + 1).unwrap_or(0)
}

fn numbered_extension(extension: &str, subtitle_no: &mut usize) -> String {
    if SUBTITLE_EXTENSION_FULL.is_match(extension) {
        let value = format!(".{}{extension}", *subtitle_no);
        *subtitle_no += 1;
        value
    } else {
        extension.to_owned()
    }
}

fn renamed_path(
    old_path: &str,
    last_index: usize,
    source_title: &str,
    new_file_name: &str,
) -> String {
    match old_path.find('/') {
        Some(index) if index + 1 == last_index => {
            format!("{source_title}/{new_file_name}")
        }
        Some(index) => {
            format!(
                "{source_title}{}{new_file_name}",
                &old_path[index..last_index]
            )
        }
        None => format!("{source_title}/{new_file_name}"),
    }
}

fn history_since(minutes: u64) -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = elapsed.as_millis().min(i64::MAX as u128) as i64;
    format_utc_timestamp(millis - (minutes as i64).saturating_mul(60_000))
}

fn format_utc_timestamp(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let millis_part = millis.rem_euclid(1000) as u32;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis_part:03}Z")
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

/// Fetch history, or `None` when the arr configuration is invalid or missing.
/// Java logs the caught `SystemConfigException` at debug level and returns.
async fn fetch_history(state: &AppState, kind: AppKind) -> Result<Option<Vec<HistoryItem>>> {
    let snapshot = state.snapshot();
    let (url_key, api_key_key, fallback) = match kind {
        AppKind::Sonarr => ("sonarrUrl", "sonarrApikey", snapshot.sonarr_rename_fallback),
        AppKind::Radarr => ("radarrUrl", "radarrApikey", snapshot.radarr_rename_fallback),
    };
    let (base, api_key) = match (
        snapshot.query_value_by_key(url_key),
        snapshot.query_value_by_key(api_key_key),
    ) {
        (Ok(base), Ok(api_key)) if !base.trim().is_empty() && !api_key.trim().is_empty() => {
            (base.trim().to_owned(), api_key.trim().to_owned())
        }
        (url, key) => {
            if let Err(error) = url {
                debug!(%kind, %error, "rename skipped");
            }
            if let Err(error) = key {
                debug!(%kind, %error, "rename skipped");
            }
            return Ok(None);
        }
    };
    let base = base.trim_end_matches('/');
    let date = history_since(fallback.as_secs() / 60);
    let response = state
        .upstream
        .http_client()
        .get(format!("{base}/api/v3/history/since"))
        .query(&[
            ("eventType", "1"),
            ("date", date.as_str()),
            ("apikey", api_key.as_str()),
        ])
        .send()
        .await
        .map_err(http_error)?;
    let status = response.status();
    if !status.is_success() {
        bail!("{kind} history API returned HTTP {status}");
    }
    let payload = response.json::<Value>().await.map_err(http_error)?;
    if payload.is_null() {
        return Ok(Some(Vec::new()));
    }
    serde_json::from_value(payload)
        .context("parse history response")
        .map(Some)
}

fn http_error(error: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!("HTTP request failed: {}", error.without_url())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_timestamp_matches_java_date_format() {
        assert_eq!(format_utc_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_utc_timestamp(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(format_utc_timestamp(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(
            format_utc_timestamp(951_782_400_000),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn rename_paths_follow_java_layout() {
        assert_eq!(
            renamed_path("Show.S01E02.mkv", 0, "New Show", "New.Show.S01E02.mkv"),
            "New Show/New.Show.S01E02.mkv"
        );
        assert_eq!(
            renamed_path("Old/Show.S01E02.mkv", 4, "New Show", "New.Show.S01E02.mkv"),
            "New Show/New.Show.S01E02.mkv"
        );
        assert_eq!(
            renamed_path(
                "Old/Season 1/Show.S01E02.mkv",
                13,
                "New Show",
                "New.Show.S01E02.mkv"
            ),
            "New Show/Season 1/New.Show.S01E02.mkv"
        );
    }

    #[test]
    fn subtitle_extensions_are_numbered_like_java() {
        let mut subtitle_no = 1;
        assert_eq!(numbered_extension(".mkv", &mut subtitle_no), ".mkv");
        assert_eq!(subtitle_no, 1);
        assert_eq!(numbered_extension(".srt", &mut subtitle_no), ".1.srt");
        assert_eq!(
            numbered_extension(".zh-CN.srt", &mut subtitle_no),
            ".zh-CN.srt"
        );
        assert_eq!(subtitle_no, 2);
    }

    #[test]
    fn episode_name_check_matches_java_full_match() {
        assert!(EPISODE_IN_FILE_NAME.is_match("S01E02"));
        assert!(EPISODE_IN_FILE_NAME.is_match("E12"));
        assert!(!EPISODE_IN_FILE_NAME.is_match("Show S01E02"));
        assert!(!EPISODE_IN_FILE_NAME.is_match("Show.S01E02.1080p"));
    }

    #[test]
    fn history_hash_prefers_download_id_and_lowercases_it() {
        let item = HistoryItem {
            source_title: Some("Show".to_owned()),
            download_id: Some("ABC123".to_owned()),
            data: Some(HistoryData {
                torrent_info_hash: Some("DEF456".to_owned()),
                download_client: Some("qBittorrent".to_owned()),
            }),
        };
        assert_eq!(history_hash(&item), Some("abc123".to_owned()));

        let item = HistoryItem {
            source_title: Some("Show".to_owned()),
            download_id: Some(String::new()),
            data: Some(HistoryData {
                torrent_info_hash: Some("DEF456".to_owned()),
                download_client: None,
            }),
        };
        assert_eq!(history_hash(&item), Some("DEF456".to_owned()));
    }

    #[test]
    fn download_client_comparison_is_case_insensitive() {
        let data = HistoryData {
            torrent_info_hash: None,
            download_client: Some("transmission".to_owned()),
        };
        assert!(download_client_is_transmission(Some(&data)));
        let data = HistoryData {
            torrent_info_hash: None,
            download_client: Some("qBittorrent".to_owned()),
        };
        assert!(!download_client_is_transmission(Some(&data)));
        assert!(!download_client_is_transmission(None));
    }

    #[test]
    fn subtitle_regex_remains_unanchored_for_transmission() {
        use crate::downloader::SUBTITLE_EXTENSION_REGEX;
        assert!(SUBTITLE_EXTENSION_REGEX.is_match(".srt"));
        assert!(SUBTITLE_EXTENSION_REGEX.is_match(".zh-CN.srt"));
    }
}
