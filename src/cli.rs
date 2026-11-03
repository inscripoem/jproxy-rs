//! Agent and operator subcommands.
//!
//! These run without starting the HTTP server. Every command opens the
//! configured SQLite database directly, so it also works through
//! `docker exec` or SSH on a stopped service. Errors return a non-zero exit
//! status through `main`; credentials are never printed.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;

use crate::admin_db;
use crate::auth;
use crate::config::{self, Config, ConfigStore};
use crate::models::AppKind;
use crate::{db, probe, sync};

pub async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("admin") => admin_command(&args[1..]).await,
        Some("config") => config_command(&args[1..]).await,
        Some("sync") => sync_command(&args[1..]).await,
        Some("doctor") => doctor_command().await,
        Some("version") => version_command(&args[1..]),
        Some(other) => bail!("unknown subcommand: {other}"),
        None => bail!("missing subcommand"),
    }
}

fn version_command(args: &[String]) -> Result<()> {
    if let Some(extra) = args.first() {
        bail!("unexpected argument for version: {extra}");
    }
    println!("jproxy-rs {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

async fn open_pool() -> Result<SqlitePool> {
    let path = config::database_path_from_env();
    db::connect(&path)
        .await
        .with_context(|| format!("open database {}", path.display()))
}

async fn admin_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("set-password") => admin_set_password(&args[1..]).await,
        Some("reset-password") => admin_reset_password(&args[1..]).await,
        Some(other) => bail!("unknown admin command: {other}"),
        None => bail!("usage: jproxy-rs admin set-password|reset-password --stdin"),
    }
}

async fn admin_set_password(args: &[String]) -> Result<()> {
    let mut username = None;
    let mut stdin = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--username" => {
                username = Some(
                    args.get(index + 1)
                        .context("--username requires a value")?
                        .clone(),
                );
                index += 2;
            }
            "--stdin" => {
                stdin = true;
                index += 1;
            }
            other => bail!("unexpected argument: {other}"),
        }
    }
    if !stdin {
        bail!("admin set-password requires --stdin");
    }
    let explicit_username = username
        .or_else(|| std::env::var("ADMIN_USERNAME").ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let password = read_stdin_value()?;
    let hash = auth::stored_password_hash(&password);
    let pool = open_pool().await?;
    // With no explicit target, follow the account that already exists. This
    // keeps a migrated `inscripoem` account authoritative instead of adding a
    // second `jproxy` administrator.
    let username = match explicit_username {
        Some(username) => username,
        None => match admin_db::list_usernames(&pool).await?.as_slice() {
            [username] => username.clone(),
            _ => auth::DEFAULT_USERNAME.to_owned(),
        },
    };
    admin_db::set_user_password(&pool, &username, &hash).await?;
    println!("admin user {username}: password updated");
    Ok(())
}

async fn admin_reset_password(args: &[String]) -> Result<()> {
    let mut stdin = false;
    for argument in args {
        match argument.as_str() {
            "--stdin" => stdin = true,
            other => bail!("unexpected argument: {other}"),
        }
    }
    if !stdin {
        bail!("admin reset-password requires --stdin");
    }
    let password = read_stdin_value()?;
    let hash = auth::stored_password_hash(&password);
    let pool = open_pool().await?;
    let username = admin_db::reset_single_user_password(&pool, &hash).await?;
    println!("admin user {username}: password reset");
    Ok(())
}

/// Read one credential line from stdin. Trailing line endings are removed;
/// other whitespace is preserved because a password may contain it.
fn read_stdin_value() -> Result<String> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .context("read credential from stdin")?;
    let value = buffer.trim_end_matches(['\r', '\n']).to_owned();
    if value.is_empty() {
        bail!("stdin is empty");
    }
    Ok(value)
}

fn read_file_value(path: &PathBuf) -> Result<String> {
    let buffer = std::fs::read_to_string(path)
        .with_context(|| format!("read value from {}", path.display()))?;
    let value = buffer.trim_end_matches(['\r', '\n']).to_owned();
    if value.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(value)
}

async fn config_command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("get") => config_get(&args[1..]).await,
        Some("set") => config_set(&args[1..]).await,
        Some("list") => config_list(&args[1..]).await,
        Some(other) => bail!("unknown config command: {other}"),
        None => bail!("usage: jproxy-rs config get|set|list ..."),
    }
}

fn masked(value: &str) -> &str {
    if value.is_empty() { "" } else { "********" }
}

async fn config_get(args: &[String]) -> Result<()> {
    let mut key = None;
    let mut reveal = false;
    for argument in args {
        match argument.as_str() {
            "--reveal" => reveal = true,
            other if other.starts_with("--") => bail!("unexpected argument: {other}"),
            other if key.is_none() => key = Some(other.to_owned()),
            other => bail!("unexpected argument: {other}"),
        }
    }
    let key = key.context("config get requires a key")?;
    let pool = open_pool().await?;
    let row = admin_db::get_system_config(&pool, &key)
        .await?
        .with_context(|| format!("system_config key not found: {key}"))?;
    let value = row.value.unwrap_or_default();
    if !reveal && config::is_secret_system_config_key(&key) {
        println!("{}", masked(&value));
    } else {
        println!("{value}");
    }
    Ok(())
}

async fn config_set(args: &[String]) -> Result<()> {
    let mut key = None;
    let mut stdin = false;
    let mut file = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--stdin" => {
                stdin = true;
                index += 1;
            }
            "--file" => {
                file = Some(PathBuf::from(
                    args.get(index + 1).context("--file requires a path")?,
                ));
                index += 2;
            }
            other if other.starts_with("--") => bail!("unexpected argument: {other}"),
            other if key.is_none() => {
                key = Some(other.to_owned());
                index += 1;
            }
            other => bail!("unexpected argument: {other}"),
        }
    }
    let key = key.context("config set requires a key")?;
    if stdin == file.is_some() {
        bail!("config set requires exactly one of --stdin or --file <path>");
    }
    if !Config::system_config_keys().contains(&key.as_str()) {
        bail!("unknown system_config key: {key}");
    }
    let value = match file {
        Some(path) => read_file_value(&path)?,
        None => read_stdin_value()?,
    };
    let pool = open_pool().await?;
    admin_db::set_system_config_value(&pool, &key, &value).await?;
    println!("system_config {key}: updated");
    Ok(())
}

async fn config_list(args: &[String]) -> Result<()> {
    let mut reveal = false;
    for argument in args {
        match argument.as_str() {
            "--reveal" => reveal = true,
            other => bail!("unexpected argument: {other}"),
        }
    }
    let pool = open_pool().await?;
    for row in admin_db::query_system_config(&pool).await? {
        let key = row.key.unwrap_or_default();
        let value = row.value.unwrap_or_default();
        if !reveal && config::is_secret_system_config_key(&key) {
            println!("{key}={}", masked(&value));
        } else {
            println!("{key}={value}");
        }
    }
    Ok(())
}

async fn sync_command(args: &[String]) -> Result<()> {
    let mut kind = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--kind" => {
                kind = Some(
                    args.get(index + 1)
                        .context("--kind requires a value")?
                        .clone(),
                );
                index += 2;
            }
            other => bail!("unexpected argument: {other}"),
        }
    }
    let kind = kind.context("sync requires --kind sonarr|radarr|tmdb|rules|all")?;
    if !matches!(
        kind.as_str(),
        "sonarr" | "radarr" | "tmdb" | "rules" | "all"
    ) {
        bail!("--kind must be one of sonarr, radarr, tmdb, rules, all");
    }

    let mut config = Config::from_env()?;
    // A manual pass is an explicit request; it should not be suppressed by
    // SYNC_ENABLED=false or a zero interval intended for the daemon.
    config.sync_enabled = true;
    if config.sync_interval.is_zero() {
        config.sync_interval = Duration::from_secs(60);
    }
    let state = crate::app::AppState::new(config).await?;

    let kinds: Vec<&str> = match kind.as_str() {
        "all" => vec!["sonarr", "radarr", "tmdb", "rules"],
        other => vec![other],
    };
    for requested in kinds {
        match requested {
            "sonarr" => {
                let count = sync::sync_titles_once(&state, AppKind::Sonarr)
                    .await
                    .context("sync Sonarr titles")?;
                report_sync("sonarr", count);
            }
            "radarr" => {
                let count = sync::sync_titles_once(&state, AppKind::Radarr)
                    .await
                    .context("sync Radarr titles")?;
                report_sync("radarr", count);
            }
            "tmdb" => {
                let count = sync::sync_tmdb_once(&state)
                    .await
                    .context("sync TMDB titles")?;
                report_sync("tmdb", count);
            }
            "rules" => {
                for app in [AppKind::Sonarr, AppKind::Radarr] {
                    let count = sync::sync_rules_once(&state, app)
                        .await
                        .with_context(|| format!("sync {app} rules"))?;
                    report_sync(&format!("{app} rules"), count);
                }
            }
            _ => unreachable!("validated --kind"),
        }
    }
    Ok(())
}

fn report_sync(label: &str, count: std::option::Option<usize>) {
    match count {
        Some(count) => println!("{label}: {count}"),
        None => println!("{label}: skipped (sync disabled or not configured)"),
    }
}

async fn doctor_command() -> Result<()> {
    let config = Config::from_env()?;
    let store = ConfigStore::new(config.clone())?;
    let path = config.database_path.clone();
    let pool = db::connect(&path)
        .await
        .with_context(|| format!("open database {}", path.display()))?;

    db::healthcheck(&pool).await?;
    println!("database: ok");

    let rows = db::load_system_config_rows(&pool).await?;
    // The Java seed stores unconfigured URLs and credentials as invalid empty
    // rows. Those are "not configured", not failures.
    let invalid: Vec<&str> = rows
        .iter()
        .filter(|row| row.valid_status != 1)
        .filter(|row| {
            row.value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        })
        .map(|row| row.key.as_str())
        .collect();
    store.apply_rows(&rows);
    let snapshot = store.snapshot();

    let mut problems = 0;
    if invalid.is_empty() {
        println!("config: ok");
    } else {
        println!("config: invalid rows: {}", invalid.join(", "));
        problems += 1;
    }
    for (name, ok) in [
        (
            "sonarrIndexerFormat",
            probe::check_sonarr_indexer_format(Some(&snapshot.sonarr_indexer_format)),
        ),
        (
            "radarrIndexerFormat",
            probe::check_radarr_indexer_format(Some(&snapshot.radarr_indexer_format)),
        ),
        (
            "cleanTitleRegex",
            snapshot
                .query_value_by_key("cleanTitleRegex")
                .is_ok_and(|value| probe::check_regex(Some(value))),
        ),
    ] {
        if ok {
            println!("config {name}: ok");
        } else {
            println!("config {name}: invalid");
            problems += 1;
        }
    }

    let prober =
        probe::Prober::with_timeouts(snapshot.probe_connect_timeout, snapshot.probe_read_timeout)?;
    let mut checks = 0;

    if let Some(url) = snapshot
        .prowlarr_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        checks += 1;
        if prober.check_url(Some(url)).await {
            println!("upstream prowlarr: ok");
        } else {
            println!("upstream prowlarr: unreachable");
            problems += 1;
        }
    }
    if let Some(url) = snapshot
        .jackett_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        checks += 1;
        if prober.check_url(Some(url)).await {
            println!("upstream jackett: ok");
        } else {
            println!("upstream jackett: unreachable");
            problems += 1;
        }
    }
    for (name, url, key) in [
        (
            "sonarr",
            snapshot.sonarr_url.as_deref(),
            snapshot.sonarr_api_key.as_deref(),
        ),
        (
            "radarr",
            snapshot.radarr_url.as_deref(),
            snapshot.radarr_api_key.as_deref(),
        ),
    ] {
        if let (Some(url), Some(key)) = (url, key) {
            checks += 1;
            if prober
                .check_sonarr_or_radarr(Some(url), Some(key))
                .await
                .url_valid()
            {
                println!("upstream {name}: ok");
            } else {
                println!("upstream {name}: unreachable or rejected");
                problems += 1;
            }
        }
    }
    if let Some(key) = snapshot.tmdb_api_key.as_deref() {
        checks += 1;
        if prober
            .check_tmdb(snapshot.tmdb_url.as_deref(), Some(key))
            .await
            .url_valid()
        {
            println!("upstream tmdb: ok");
        } else {
            println!("upstream tmdb: unreachable or rejected");
            problems += 1;
        }
    }
    if checks == 0 {
        println!("upstream: not configured");
    }

    if problems > 0 {
        bail!("doctor found {problems} problem(s)");
    }
    Ok(())
}
