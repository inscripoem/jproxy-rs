//! Agent CLI tests.
//!
//! All credentials are synthetic and every database is temporary.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::{MockResponse, TestEnv};
use jproxy_rs::auth;
use tempfile::tempdir;

fn run_cli(database: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    run_cli_with_env(database, args, stdin, &[])
}

fn run_cli_with_env(
    database: &Path,
    args: &[&str],
    stdin: Option<&str>,
    environment: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jproxy-rs"));
    command
        .args(args)
        .env("DATABASE_PATH", database)
        // Keep the CLI on its documented offline fallback even when a control
        // socket happens to exist on the build host.
        .env("CONTROL_SOCKET", database.with_file_name("no-control.sock"))
        .env_remove("CONTROL_RUNTIME_FILE")
        .env_remove("ADMIN_USERNAME")
        .env_remove("ADMIN_PASSWORD");
    for name in [
        "PROWLARR_URL",
        "JACKETT_URL",
        "SONARR_URL",
        "SONARR_API_KEY",
        "RADARR_URL",
        "RADARR_API_KEY",
        "TMDB_URL",
        "TMDB_API_KEY",
        "LOGIN_ENABLED",
        "SYNC_ENABLED",
        "SYNC_INTERVAL",
    ] {
        command.env_remove(name);
    }
    for (name, value) in environment {
        command.env(name, value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn jproxy-rs CLI");
    if let Some(value) = stdin {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(value.as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait for CLI")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_subcommand_reports_the_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_jproxy-rs"))
        .arg("version")
        .output()
        .expect("run version");
    assert!(output.status.success());
    assert_eq!(
        stdout(&output).trim(),
        format!("jproxy-rs {}", env!("CARGO_PKG_VERSION"))
    );
}

#[tokio::test]
async fn first_run_env_credentials_override_the_defaults() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    let output = run_cli_with_env(
        &database,
        &["config", "list"],
        None,
        &[
            ("ADMIN_USERNAME", "operator"),
            ("ADMIN_PASSWORD", "synthetic-env-password"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("synthetic-env-password"));

    let pool = jproxy_rs::db::connect(&database).await.unwrap();
    let (username, hash): (String, String) =
        sqlx::query_as("SELECT username, password FROM system_user")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(username, "operator");
    assert_eq!(hash, auth::stored_password_hash("synthetic-env-password"));
}

#[tokio::test]
async fn admin_set_and_reset_store_the_java_compatible_hash() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    let first = "synthetic-admin-password";
    let second = "synthetic-reset-password";

    let output = run_cli(
        &database,
        &["admin", "set-password", "--stdin"],
        Some(&format!("{first}\n")),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains(first));
    assert!(!stderr(&output).contains(first));

    let pool = jproxy_rs::db::connect(&database).await.unwrap();
    let (username, hash): (String, String) =
        sqlx::query_as("SELECT username, password FROM system_user")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(username, auth::DEFAULT_USERNAME);
    assert_eq!(hash, auth::stored_password_hash(first));

    let output = run_cli(
        &database,
        &["admin", "reset-password", "--stdin"],
        Some(&format!("{second}\n")),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains(second));

    let hash: String = sqlx::query_scalar("SELECT password FROM system_user")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hash, auth::stored_password_hash(second));
}

#[tokio::test]
async fn admin_reset_preserves_a_migrated_username() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    let pool = jproxy_rs::db::connect(&database).await.unwrap();
    sqlx::query("DELETE FROM system_user")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO system_user (username, password, role, valid_status) \
         VALUES ('inscripoem', 'MIGRATED-HASH-VALUE', 'admin', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    drop(pool);

    // `set` without --username targets the sole existing account.
    let output = run_cli(
        &database,
        &["admin", "set-password", "--stdin"],
        Some("synthetic-set-password\n"),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let pool = jproxy_rs::db::connect(&database).await.unwrap();
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT username, password FROM system_user")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec![(
            "inscripoem".to_owned(),
            auth::stored_password_hash("synthetic-set-password")
        )]
    );
    drop(pool);

    let output = run_cli(
        &database,
        &["admin", "reset-password", "--stdin"],
        Some("synthetic-migrated-password\n"),
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let pool = jproxy_rs::db::connect(&database).await.unwrap();
    let (username, hash): (String, String) =
        sqlx::query_as("SELECT username, password FROM system_user")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(username, "inscripoem");
    assert_eq!(
        hash,
        auth::stored_password_hash("synthetic-migrated-password")
    );
}

#[tokio::test]
async fn config_commands_mask_secrets_unless_revealed() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    let secret = "synthetic-api-key-value";

    let output = run_cli(
        &database,
        &["config", "set", "sonarrApikey", "--stdin"],
        Some(secret),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains(secret));
    assert!(!stderr(&output).contains(secret));

    let output = run_cli(&database, &["config", "get", "sonarrApikey"], None);
    assert!(output.status.success());
    assert_eq!(stdout(&output).trim(), "********");
    assert!(!stdout(&output).contains(secret));

    let output = run_cli(
        &database,
        &["config", "get", "sonarrApikey", "--reveal"],
        None,
    );
    assert!(output.status.success());
    assert_eq!(stdout(&output).trim(), secret);

    let output = run_cli(&database, &["config", "list"], None);
    assert!(output.status.success());
    assert!(stdout(&output).contains("sonarrApikey=********"));
    assert!(!stdout(&output).contains(secret));

    // Non-secret keys remain readable without --reveal.
    let output = run_cli(
        &database,
        &["config", "set", "sonarrUrl", "--stdin"],
        Some("http://synthetic.invalid\n"),
    );
    assert!(output.status.success());
    let output = run_cli(&database, &["config", "get", "sonarrUrl"], None);
    assert_eq!(stdout(&output).trim(), "http://synthetic.invalid");
}

#[tokio::test]
async fn config_set_reads_from_a_file_and_rejects_unknown_keys() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    let value_file = dir.path().join("value.txt");
    std::fs::write(&value_file, "synthetic-file-value\n").unwrap();

    let output = run_cli(
        &database,
        &[
            "config",
            "set",
            "tmdbApikey",
            "--file",
            value_file.to_str().unwrap(),
        ],
        None,
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let output = run_cli(&database, &["config", "get", "tmdbApikey"], None);
    assert_eq!(stdout(&output).trim(), "********");

    let output = run_cli(
        &database,
        &["config", "set", "notAWhitelistedKey", "--stdin"],
        Some("synthetic"),
    );
    assert!(!output.status.success());
}

#[tokio::test]
async fn sync_runs_one_pass_against_the_configured_upstream() {
    let env = TestEnv::start().await;
    let series = serde_json::json!([{
        "id": 7,
        "tvdbId": 100,
        "title": "Main Show",
        "titleSlug": "main-show",
        "monitored": true,
        "alternateTitles": []
    }])
    .to_string();
    env.mock
        .route_path("/api/v3/series", MockResponse::json(series));

    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    let sonarr_url = env.mock.base_url();
    // The mock runs on this runtime, so the blocking child wait must not
    // starve it.
    let database_for_cli = database.clone();
    let output = tokio::task::spawn_blocking(move || {
        run_cli_with_env(
            &database_for_cli,
            &["sync", "--kind", "sonarr"],
            None,
            &[
                ("SONARR_URL", &sonarr_url),
                ("SONARR_API_KEY", "synthetic-sonarr-key"),
                ("SYNC_ENABLED", "true"),
                ("SYNC_INTERVAL", "3"),
            ],
        )
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("sonarr: "), "{}", stdout(&output));

    let pool = jproxy_rs::db::connect(&database).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sonarr_title")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(count > 0);
}

#[tokio::test]
async fn doctor_reports_configuration_and_reachability_without_secrets() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");

    let output = run_cli(&database, &["doctor"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    assert!(report.contains("database: ok"));
    assert!(report.contains("config: ok"));
    assert!(report.contains("upstream: not configured"));

    // A configured but unreachable Prowlarr is an explicit failure.
    let output = run_cli(
        &database,
        &["config", "set", "prowlarrUrl", "--stdin"],
        Some("http://127.0.0.1:1\n"),
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let output = run_cli(&database, &["doctor"], None);
    assert!(!output.status.success());
    assert!(stdout(&output).contains("upstream prowlarr: unreachable"));
    assert!(!stdout(&output).contains("127.0.0.1"));
}

#[test]
fn malformed_commands_exit_non_zero() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");

    for args in [
        vec!["admin", "set-password"],
        vec!["admin", "reset-password"],
        vec!["config", "get"],
        vec!["sync", "--kind", "bogus"],
    ] {
        let output = run_cli(&database, &args, None);
        assert!(!output.status.success(), "{args:?} must fail");
        assert!(
            !stderr(&output).is_empty(),
            "{args:?} must explain the failure"
        );
    }
}
