//! Unix-domain control channel and CLI integration tests.
//!
//! Every server, socket, database, and credential here is temporary and
//! synthetic. External Sonarr/Radarr/downloader endpoints are local mocks.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{MockHttp, MockHttpReply, MockResponse, TestEnv};
use jproxy_rs::app::AppState;
use jproxy_rs::config::Config;
use jproxy_rs::control::{self, ControlInfo, ControlPeer};
use reqwest::Client;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Harness {
    prepared: control::PreparedControl,
    info: Arc<ControlInfo>,
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl Harness {
    async fn start(config: Config) -> Self {
        let state = AppState::new(config.clone()).await.unwrap();
        let mut prepared = control::prepare(&config).await.unwrap();
        let info = prepared.info.clone();
        let listener = prepared.take_listener().unwrap();
        let (shutdown, wait) = oneshot::channel();
        let router = control::control_router(state, info.clone())
            .into_make_service_with_connect_info::<ControlPeer>();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = wait.await;
                })
                .await;
        });
        Self {
            prepared,
            info,
            shutdown: Some(shutdown),
            join,
        }
    }

    fn socket(&self) -> &Path {
        &self.info.socket_path
    }

    fn token(&self) -> &str {
        &self.info.token
    }

    fn token_file(&self) -> &Path {
        &self.info.token_file
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.join.await;
        self.prepared.cleanup();
    }
}

struct TcpHarness {
    addr: String,
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl TcpHarness {
    async fn start(config: Config) -> Self {
        let state = AppState::new(config).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (shutdown, wait) = oneshot::channel();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, jproxy_rs::app::router(state))
                .with_graceful_shutdown(async {
                    let _ = wait.await;
                })
                .await;
        });
        Self {
            addr,
            shutdown: Some(shutdown),
            join,
        }
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.join.await;
    }
}

fn uds_client(socket: &Path) -> Client {
    Client::builder()
        .unix_socket(socket.to_path_buf())
        .build()
        .unwrap()
}

fn url(path: &str) -> String {
    format!("http://localhost{path}")
}

async fn control_get(client: &Client, token: Option<&str>, path: &str) -> (StatusCode, String) {
    let mut request = client.get(url(path));
    if let Some(token) = token {
        request = request.header("x-control-token", token);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

async fn control_post(
    client: &Client,
    token: &str,
    path: &str,
    body: &Value,
) -> (StatusCode, String) {
    let response = client
        .post(url(path))
        .header("x-control-token", token)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

async fn wait_job(client: &Client, token: &str, id: &str) -> Value {
    for _ in 0..200 {
        let (status, body) =
            control_get(client, Some(token), &format!("/api/control/jobs/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let view: Value = serde_json::from_str(&body).unwrap();
        if matches!(
            view.get("status").and_then(Value::as_str),
            Some("succeeded" | "failed" | "cancelled")
        ) {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} did not finish");
}

async fn start_job(client: &Client, token: &str, path: &str) -> (StatusCode, Value) {
    let response = client
        .post(url(path))
        .header("x-control-token", token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    (status, body)
}

// ---------------------------------------------------------------------------
// Authentication and transport separation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn control_token_authenticates_only_on_the_unix_socket() {
    let env = TestEnv::start().await;
    let harness = Harness::start(env.config()).await;
    let client = uds_client(harness.socket());

    let (status, body) = control_get(&client, Some(harness.token()), "/api/control/status").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _) = control_get(
        &client,
        Some("wrong-synthetic-token"),
        "/api/control/status",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = control_get(&client, None, "/api/control/status").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The Authorization form is accepted on the control channel too.
    let response = client
        .get(url("/api/control/status"))
        .header("authorization", format!("Bearer {}", harness.token()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The same token is not an admin bypass on the TCP/Web UI listener.
    let tcp = TcpHarness::start(env.config()).await;
    let tcp_client = Client::new();
    let response = tcp_client
        .get(format!("http://{}/api/control/status", tcp.addr))
        .header("x-control-token", harness.token())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = tcp_client
        .get(format!("http://{}/api/system/config/query", tcp.addr))
        .header("x-control-token", harness.token())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Public routes remain public.
    let response = tcp_client
        .get(format!("http://{}/health", tcp.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    tcp.stop().await;
    harness.stop().await;
}

#[tokio::test]
async fn control_token_reuses_admin_handlers_for_rule_crud() {
    let env = TestEnv::start().await;
    let harness = Harness::start(env.config()).await;
    let client = uds_client(harness.socket());

    let rule = json!({
        "token": "title",
        "priority": 900,
        "regex": ".*",
        "replacement": "x",
        "offset": 0,
        "example": "synthetic example",
        "remark": "synthetic remark",
        "author": "synthetic author"
    });
    let (status, body) =
        control_post(&client, harness.token(), "/api/sonarr/rule/save", &rule).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = control_get(
        &client,
        Some(harness.token()),
        "/api/sonarr/rule/query?current=1&pageSize=10",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(page["total"], 1);
    let id = page["list"][0]["id"].as_str().unwrap().to_owned();

    let (status, body) = control_post(
        &client,
        harness.token(),
        "/api/sonarr/rule/remove",
        &json!([id]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// Socket lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_socket_is_removed_and_a_live_socket_is_not() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(listener);
    assert!(socket.exists());

    let mut config = TestEnv::start().await.config();
    config.control_socket = socket.clone();
    let mut prepared = control::prepare(&config).await.unwrap();
    assert!(socket.exists());
    assert_eq!(prepared.info.token.len(), 43);

    // A second process cannot take the same socket while the lock is held.
    let error = control::prepare(&config).await.err().unwrap();
    assert!(error.to_string().contains("locked"));

    prepared.cleanup();
    assert!(!socket.exists());
    assert!(!prepared.info.runtime_file.exists());
    assert!(!prepared.info.token_file.exists());
}

// ---------------------------------------------------------------------------
// Status, reload, jobs, SSE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_and_reload_report_state_without_secrets() {
    let env = TestEnv::start().await;
    let harness = Harness::start(env.config()).await;
    let client = uds_client(harness.socket());

    let (status, body) = control_get(&client, Some(harness.token()), "/api/control/status").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        value["control"]["socket"],
        harness.socket().to_str().unwrap()
    );
    assert_eq!(value["control"]["peer_policy"], "same-uid-or-root");
    assert!(!body.contains(harness.token()));
    assert!(
        value["config"]["invalid_keys"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let (status, body) =
        control_post(&client, harness.token(), "/api/control/reload", &json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["reloaded"], true);

    harness.stop().await;
}

#[tokio::test]
async fn sync_job_reports_progress_and_finishes_over_sse() {
    let env = TestEnv::start().await;
    let mut config = env.config();
    config.sync_enabled = true;
    config.sync_interval = Duration::from_secs(60);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-sonarr-key".to_owned());
    env.mock.route_path(
        "/api/v3/series",
        MockResponse::json(
            serde_json::json!([{
                "id": 7,
                "tvdbId": 100,
                "title": "Main Show",
                "titleSlug": "main-show",
                "monitored": true,
                "alternateTitles": []
            }])
            .to_string(),
        ),
    );

    let harness = Harness::start(config).await;
    let client = uds_client(harness.socket());
    let (status, accepted) =
        start_job(&client, harness.token(), "/api/control/sync?kind=sonarr").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = accepted["id"].as_str().unwrap().to_owned();

    let response = client
        .get(url(&format!("/api/control/jobs/{id}/events")))
        .header("x-control-token", harness.token())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let events = response.text().await.unwrap();
    assert!(events.contains("event: log"), "{events}");
    assert!(events.contains("event: done"), "{events}");

    let view = wait_job(&client, harness.token(), &id).await;
    assert_eq!(view["status"], "succeeded");
    assert!(view["result"]["results"][0]["count"].as_u64().unwrap() >= 1);

    harness.stop().await;
}

#[tokio::test]
async fn jobs_can_be_cancelled() {
    let env = TestEnv::start().await;
    let mut config = env.config();
    config.sync_enabled = true;
    config.sync_interval = Duration::from_secs(60);
    config.sonarr_url = Some(env.mock.base_url());
    config.sonarr_api_key = Some("synthetic-sonarr-key".to_owned());
    env.mock.route_path(
        "/api/v3/series",
        MockResponse::json("[]").with_delay(Duration::from_secs(5)),
    );

    let harness = Harness::start(config).await;
    let client = uds_client(harness.socket());
    let (_, accepted) = start_job(&client, harness.token(), "/api/control/sync?kind=sonarr").await;
    let id = accepted["id"].as_str().unwrap().to_owned();

    let (status, body) = control_post(
        &client,
        harness.token(),
        &format!("/api/control/jobs/{id}/cancel"),
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let view = wait_job(&client, harness.token(), &id).await;
    assert_eq!(view["status"], "cancelled");
    assert_eq!(view["result"], Value::Null);

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// Rename dry run and downloader login
// ---------------------------------------------------------------------------

#[tokio::test]
async fn downloader_login_and_rename_dry_run_do_not_mutate_the_downloader() {
    let env = TestEnv::start().await;
    let http = MockHttp::start().await;
    http.route(
        "POST",
        "/api/v2/auth/login",
        MockHttpReply::ok("Ok.").with_header("set-cookie", "SID=synthetic-session"),
    );
    http.route(
        "GET",
        "/api/v3/history/since",
        MockHttpReply::json(
            serde_json::json!([{
                "sourceTitle": "Old.Name.S01E01.1080p.mkv",
                "downloadId": "ABCDEF",
                "data": { "downloadClient": "qBittorrent" }
            }])
            .to_string(),
        ),
    );

    let mut config = env.config();
    config.qbittorrent_url = Some(http.base_url());
    config.qbittorrent_username = Some("synthetic-user".to_owned());
    config.qbittorrent_password = Some("synthetic-pass".to_owned());
    config.sonarr_url = Some(http.base_url());
    config.sonarr_api_key = Some("synthetic-sonarr-key".to_owned());

    let harness = Harness::start(config).await;
    let client = uds_client(harness.socket());

    let (status, accepted) = start_job(
        &client,
        harness.token(),
        "/api/control/login?client=qbittorrent",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let view = wait_job(&client, harness.token(), accepted["id"].as_str().unwrap()).await;
    assert_eq!(view["status"], "succeeded");
    assert_eq!(view["result"]["logged_in"], true);

    let (status, accepted) = start_job(
        &client,
        harness.token(),
        "/api/control/rename?kind=sonarr&dry_run=true",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let view = wait_job(&client, harness.token(), accepted["id"].as_str().unwrap()).await;
    assert_eq!(view["status"], "succeeded");
    assert_eq!(view["result"]["dry_run"], true);
    assert_eq!(view["result"]["planned_torrents"], 1);
    assert_eq!(view["result"]["renamed_torrents"], 0);
    assert!(
        http.requests_for("POST", "/api/v2/torrents/rename")
            .is_empty(),
        "dry run must not rename the torrent"
    );
    assert!(
        http.requests_for("POST", "/api/v2/torrents/renameFile")
            .is_empty(),
        "dry run must not rename files"
    );

    harness.stop().await;
}

// ---------------------------------------------------------------------------
// CLI exit codes and command surface
// ---------------------------------------------------------------------------

fn cli(socket: &Path, token_file: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jproxy-rs"));
    command
        .arg("--socket")
        .arg(socket)
        .arg("--token-file")
        .arg(token_file)
        .args(args)
        .env_remove("CONTROL_SOCKET")
        .env_remove("CONTROL_RUNTIME_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn jproxy-rs CLI");
    if let Some(value) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(value.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

async fn cli_async(
    socket: PathBuf,
    token_file: PathBuf,
    args: Vec<String>,
    stdin: Option<String>,
) -> Output {
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        cli(&socket, &token_file, &args, stdin.as_deref())
    })
    .await
    .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

#[tokio::test]
async fn cli_reports_connection_auth_usage_and_server_exit_codes() {
    let env = TestEnv::start().await;
    let harness = Harness::start(env.config()).await;
    let socket = harness.socket().to_path_buf();
    let token_file = harness.token_file().to_path_buf();

    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec!["status".into()],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("version:"));

    // Wrong token -> auth error.
    let dir = tempdir().unwrap();
    let bad = dir.path().join("bad.token");
    std::fs::write(&bad, "wrong-synthetic-token\n").unwrap();
    std::fs::set_permissions(&bad, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let output = cli_async(socket.clone(), bad, vec!["status".into()], None).await;
    assert_eq!(code(&output), 4, "{}", stderr(&output));

    // Missing socket -> connection error.
    let missing = dir.path().join("missing.sock");
    let output = cli_async(missing, token_file.clone(), vec!["status".into()], None).await;
    assert_eq!(code(&output), 3, "{}", stderr(&output));

    // Missing argument -> usage error.
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec!["config".into(), "get".into()],
        None,
    )
    .await;
    assert_eq!(code(&output), 2, "{}", stderr(&output));

    // 404 from the server -> server error.
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec!["jobs".into(), "show".into(), "missing".into()],
        None,
    )
    .await;
    assert_eq!(code(&output), 5, "{}", stderr(&output));

    // Version mismatch is detected before the request.
    let runtime = harness.info.runtime_file.clone();
    std::fs::write(
        &runtime,
        serde_json::to_vec(&json!({
            "socket": socket,
            "pid": 1,
            "version": "9.9.9",
            "token_file": token_file,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(
        &runtime,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let output = cli_async(socket, token_file, vec!["status".into()], None).await;
    assert_eq!(code(&output), 6, "{}", stderr(&output));

    harness.stop().await;
}

#[tokio::test]
async fn cli_covers_the_admin_command_surface() {
    let env = TestEnv::start().await;
    env.exec(
        "INSERT INTO sonarr_title (id, tvdb_id, sno, main_title, title, clean_title, \
         season_number, monitored) VALUES (1, 100, 0, 'Main Show', 'Main Show', \
         'main show', -1, 1); \
         INSERT INTO tmdb_title (id, tvdb_id, tmdb_id, language, title, valid_status) \
         VALUES (5, 100, 200, 'zh-CN', 'Synthetic TMDB', 1)",
    )
    .await;
    let harness = Harness::start(env.config()).await;
    let socket = harness.socket().to_path_buf();
    let token_file = harness.token_file().to_path_buf();

    let commands: Vec<Vec<&str>> = vec![
        vec!["status"],
        vec!["healthcheck"],
        vec!["version"],
        vec!["config", "list"],
        vec!["rules", "sonarr", "list"],
        vec!["titles", "sonarr", "list"],
        vec!["examples", "sonarr", "list"],
        vec!["tmdb", "list"],
        vec!["cache", "list"],
        vec!["jobs", "list"],
        vec!["doctor"],
        vec!["sync", "--kind", "sonarr"],
        vec!["rename", "--kind", "radarr", "--dry-run"],
        vec!["login", "--client", "transmission"],
    ];
    for command in commands {
        let output = cli_async(
            socket.clone(),
            token_file.clone(),
            command.iter().map(|value| value.to_string()).collect(),
            None,
        )
        .await;
        assert_eq!(code(&output), 0, "{command:?} failed: {}", stderr(&output));
        assert!(
            !stdout(&output).trim().is_empty(),
            "{command:?} printed nothing"
        );
    }

    // Rule CRUD through the CLI with JSON on stdin, never argv.
    let rule = json!({
        "token": "title",
        "priority": 900,
        "regex": ".*",
        "replacement": "x",
        "offset": 0,
        "example": "synthetic example",
        "remark": "synthetic remark",
        "author": "synthetic author"
    });
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "rules".into(),
            "sonarr".into(),
            "add".into(),
            "--stdin".into(),
        ],
        Some(rule.to_string()),
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec!["rules".into(), "sonarr".into(), "list".into()],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("synthetic author"));

    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec!["rules".into(), "sonarr".into(), "remove".into()],
        None,
    )
    .await;
    assert_eq!(code(&output), 2, "destructive commands require --yes");

    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "examples".into(),
            "sonarr".into(),
            "add".into(),
            "--stdin".into(),
        ],
        Some("Synthetic Show S01E01\n".into()),
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    // Remaining rule actions.
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec!["rules".into(), "sonarr".into(), "list".into()],
        None,
    )
    .await;
    let rule_id = stdout(&output)
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .expect("rule id")
        .to_owned();
    for action in [
        vec!["rules", "sonarr", "get", rule_id.as_str()],
        vec!["rules", "sonarr", "disable", rule_id.as_str()],
        vec!["rules", "sonarr", "enable", rule_id.as_str()],
    ] {
        let output = cli_async(
            socket.clone(),
            token_file.clone(),
            action.iter().map(|value| value.to_string()).collect(),
            None,
        )
        .await;
        assert_eq!(code(&output), 0, "{action:?}: {}", stderr(&output));
    }
    let export_file = harness
        .info
        .runtime_file
        .with_file_name("rules-export.json");
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "rules".into(),
            "sonarr".into(),
            "export".into(),
            "--file".into(),
            export_file.display().to_string(),
        ],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "rules".into(),
            "sonarr".into(),
            "import".into(),
            "--file".into(),
            export_file.display().to_string(),
        ],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    // Config read/write over UDS.
    for action in [vec!["config", "get", "sonarrUrl"], vec!["config", "list"]] {
        let output = cli_async(
            socket.clone(),
            token_file.clone(),
            action.iter().map(|value| value.to_string()).collect(),
            None,
        )
        .await;
        assert_eq!(code(&output), 0, "{action:?}: {}", stderr(&output));
    }
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "config".into(),
            "set".into(),
            "cleanTitleRegex".into(),
            "--stdin".into(),
        ],
        Some("!".into()),
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    // Remaining title, example, TMDB, and cache actions.
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "titles".into(),
            "sonarr".into(),
            "remove".into(),
            "1".into(),
            "--yes".into(),
        ],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let hash = jproxy_rs::auth::md5_upper("Synthetic Show S01E01");
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "examples".into(),
            "sonarr".into(),
            "remove".into(),
            hash,
            "--yes".into(),
        ],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec!["tmdb".into(), "remove".into(), "5".into(), "--yes".into()],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "cache".into(),
            "clear".into(),
            "indexer_result".into(),
            "--yes".into(),
        ],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    // Job inspection/watch/cancel over the CLI.
    let output = cli_async(
        socket.clone(),
        token_file.clone(),
        vec![
            "sync".into(),
            "--kind".into(),
            "sonarr".into(),
            "--no-wait".into(),
            "--json".into(),
        ],
        None,
    )
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let accepted: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let job_id = accepted["id"].as_str().unwrap().to_owned();
    for action in [
        vec!["jobs", "show", job_id.as_str()],
        vec!["jobs", "watch", job_id.as_str()],
        vec!["jobs", "cancel", job_id.as_str()],
    ] {
        let output = cli_async(
            socket.clone(),
            token_file.clone(),
            action.iter().map(|value| value.to_string()).collect(),
            None,
        )
        .await;
        assert_eq!(code(&output), 0, "{action:?}: {}", stderr(&output));
    }

    harness.stop().await;
}

#[tokio::test]
async fn offline_config_remains_available_without_a_server() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("jproxy.db");
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_jproxy-rs"))
            .args(["config", "list"])
            .env("DATABASE_PATH", database)
            .env("CONTROL_SOCKET", dir.path().join("no-server.sock"))
            .env_remove("CONTROL_RUNTIME_FILE")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("offline database mode"));
    assert!(stdout(&output).contains("sonarrUrl="));
}
