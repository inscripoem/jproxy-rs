//! Rename-task tests against local mock Sonarr/Radarr/qBittorrent/Transmission
//! endpoints. No test contacts a real downloader or the network.

mod common;

use common::{MockHttp, MockHttpReply, TestEnv};
use jproxy_rs::app::AppState;
use jproxy_rs::rename::{login_once, radarr_rename_once, sonarr_rename_once};

const QB_USER: &str = "synthetic-qb-user";
const QB_PASSWORD: &str = "synthetic-qb-password";
const QB_COOKIE: &str = "SID=synthetic-session";
const TR_USER: &str = "synthetic-tr-user";
const TR_PASSWORD: &str = "synthetic-tr-password";
const TR_SESSION: &str = "synthetic-transmission-session";
const ARR_API_KEY: &str = "synthetic-arr-apikey";

struct RenameEnv {
    env: TestEnv,
    arr: MockHttp,
    qb: MockHttp,
    state: AppState,
}

async fn setup(rename_file: bool) -> RenameEnv {
    let env = TestEnv::start().await;
    let arr = MockHttp::start().await;
    let qb = MockHttp::start().await;

    qb.route(
        "POST",
        "/api/v2/auth/login",
        MockHttpReply::ok("Ok.").with_header("Set-Cookie", QB_COOKIE),
    );
    qb.route("POST", "/api/v2/torrents/rename", MockHttpReply::ok("Ok."));

    let mut config = env.config();
    config.sonarr_url = Some(arr.base_url());
    config.sonarr_api_key = Some(ARR_API_KEY.to_owned());
    config.radarr_url = Some(arr.base_url());
    config.radarr_api_key = Some(ARR_API_KEY.to_owned());
    config.qbittorrent_url = Some(qb.base_url());
    config.qbittorrent_username = Some(QB_USER.to_owned());
    config.qbittorrent_password = Some(QB_PASSWORD.to_owned());
    config.rename_file = rename_file;
    let state = AppState::new(config).await.unwrap();
    RenameEnv {
        env,
        arr,
        qb,
        state,
    }
}

async fn seed_sonarr_rules(env: &TestEnv) {
    env.exec(
        "INSERT INTO sonarr_rule \
         (id, token, priority, regex, replacement, offset, valid_status) VALUES \
         ('season-rule', 'season', 10, '.*', 'S01', 0, 1), \
         ('episode-rule', 'episode', 10, '.*E(\\d+).*', 'E$1', 0, 1)",
    )
    .await;
}

fn sonarr_history() -> MockHttpReply {
    MockHttpReply::json(
        r#"[
            {"sourceTitle":"Test Show S01E02 1080p","downloadId":"ABCDEF","data":{"downloadClient":"qBittorrent"}},
            {"sourceTitle":"Test Show S01E03 1080p","downloadId":"abcdef","data":{"downloadClient":"qBittorrent"}}
        ]"#,
    )
}

fn radarr_history() -> MockHttpReply {
    MockHttpReply::json(
        r#"[
            {"sourceTitle":"Movie Title 2024","downloadId":"RADARRHASH","data":{"downloadClient":"qBittorrent"}}
        ]"#,
    )
}

#[tokio::test]
async fn sonarr_rename_renames_torrent_and_files() {
    let env = setup(true).await;
    seed_sonarr_rules(&env.env).await;
    env.arr
        .route("GET", "/api/v3/history/since", sonarr_history());
    env.qb.route(
        "GET",
        "/api/v2/torrents/files",
        MockHttpReply::json(
            r#"[{"name":"Old/Show.S01E02.1080p.mkv"},{"name":"Old/Show.S01E02.srt"}]"#,
        ),
    );
    env.qb.route(
        "POST",
        "/api/v2/torrents/renameFile",
        MockHttpReply::ok("Ok."),
    );

    let login = login_once(&env.state).await;
    assert!(login.qbittorrent);
    assert!(!login.transmission);

    let report = sonarr_rename_once(&env.state).await.unwrap();
    assert!(!report.skipped);
    assert_eq!(report.history_items, 2);
    assert_eq!(report.renamed_torrents, 1, "Sonarr deduplicates hashes");
    assert_eq!(report.renamed_files, 2);
    assert_eq!(report.errors, 0);

    let history = env.arr.requests_for("GET", "/api/v3/history/since");
    assert_eq!(history.len(), 1);
    assert!(history[0].query.contains("eventType=1"));
    assert!(history[0].query.contains("date="));
    assert!(history[0].query.contains("apikey=synthetic-arr-apikey"));

    let rename = env.qb.requests_for("POST", "/api/v2/torrents/rename");
    assert_eq!(rename.len(), 1);
    assert_eq!(rename[0].form_value("hash").as_deref(), Some("abcdef"));
    assert_eq!(
        rename[0].form_value("name").as_deref(),
        Some("Test Show S01E02 1080p")
    );

    let files = env.qb.requests_for("GET", "/api/v2/torrents/files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].header("cookie"), Some(QB_COOKIE));
    assert!(files[0].query.contains("hash=abcdef"));

    let renamed = env.qb.requests_for("POST", "/api/v2/torrents/renameFile");
    assert_eq!(renamed.len(), 2);
    assert_eq!(
        renamed[0].form_value("oldPath").as_deref(),
        Some("Old/Show.S01E02.1080p.mkv")
    );
    assert_eq!(
        renamed[0].form_value("newPath").as_deref(),
        Some("Test Show S01E02 1080p/S01E02.mkv")
    );
    assert_eq!(
        renamed[1].form_value("newPath").as_deref(),
        Some("Test Show S01E02 1080p/S01E02.1.srt")
    );
}

#[tokio::test]
async fn rename_file_false_only_renames_the_torrent() {
    let env = setup(false).await;
    env.arr
        .route("GET", "/api/v3/history/since", sonarr_history());
    env.qb.route(
        "GET",
        "/api/v2/torrents/files",
        MockHttpReply::json(r#"[{"name":"Old/Show.S01E02.mkv"}]"#),
    );
    env.qb.route(
        "POST",
        "/api/v2/torrents/renameFile",
        MockHttpReply::ok("Ok."),
    );

    assert!(login_once(&env.state).await.qbittorrent);
    let report = sonarr_rename_once(&env.state).await.unwrap();
    assert_eq!(report.renamed_torrents, 1);
    assert_eq!(report.renamed_files, 0);
    assert_eq!(
        env.qb.requests_for("POST", "/api/v2/torrents/rename").len(),
        1
    );
    assert!(
        env.qb
            .requests_for("GET", "/api/v2/torrents/files")
            .is_empty()
    );
    assert!(
        env.qb
            .requests_for("POST", "/api/v2/torrents/renameFile")
            .is_empty()
    );
}

#[tokio::test]
async fn rename_tasks_skip_without_a_logged_in_downloader() {
    let env = setup(true).await;
    env.arr
        .route("GET", "/api/v3/history/since", sonarr_history());

    let report = sonarr_rename_once(&env.state).await.unwrap();
    assert!(report.skipped);
    assert_eq!(report.history_items, 0);
    assert!(env.arr.request_count() == 0, "history must not be fetched");
}

#[tokio::test]
async fn one_failed_item_does_not_stop_the_remaining_history() {
    let env = setup(false).await;
    env.arr.route(
        "GET",
        "/api/v3/history/since",
        MockHttpReply::json(
            r#"[
                {"sourceTitle":"Broken Show S01E01","downloadId":"deadbeef","data":{"downloadClient":"qBittorrent"}},
                {"sourceTitle":"Working Show S01E02","downloadId":"cafebabe","data":{"downloadClient":"qBittorrent"}}
            ]"#,
        ),
    );
    env.qb.route_sequence(
        "POST",
        "/api/v2/torrents/rename",
        vec![
            MockHttpReply::status(404).with_body("Not Found"),
            MockHttpReply::ok("Ok."),
        ],
    );

    assert!(login_once(&env.state).await.qbittorrent);
    let report = sonarr_rename_once(&env.state).await.unwrap();
    assert_eq!(report.history_items, 2);
    assert_eq!(report.errors, 1);
    assert_eq!(report.renamed_torrents, 1);
    assert_eq!(
        env.qb.requests_for("POST", "/api/v2/torrents/rename").len(),
        2
    );
}

#[tokio::test]
async fn radarr_rename_renames_torrent_and_files() {
    let env = setup(true).await;
    env.arr
        .route("GET", "/api/v3/history/since", radarr_history());
    env.qb.route(
        "GET",
        "/api/v2/torrents/files",
        MockHttpReply::json(r#"[{"name":"Old/Movie.2024.mkv"},{"name":"Old/Movie.2024.srt"}]"#),
    );
    env.qb.route(
        "POST",
        "/api/v2/torrents/renameFile",
        MockHttpReply::ok("Ok."),
    );

    assert!(login_once(&env.state).await.qbittorrent);
    let report = radarr_rename_once(&env.state).await.unwrap();
    assert_eq!(report.history_items, 1);
    assert_eq!(report.renamed_torrents, 1);
    assert_eq!(report.renamed_files, 2);
    assert_eq!(report.errors, 0);

    let rename = env.qb.requests_for("POST", "/api/v2/torrents/rename");
    assert_eq!(rename[0].form_value("hash").as_deref(), Some("radarrhash"));
    assert_eq!(
        rename[0].form_value("name").as_deref(),
        Some("Movie Title 2024")
    );

    let renamed = env.qb.requests_for("POST", "/api/v2/torrents/renameFile");
    assert_eq!(
        renamed[0].form_value("newPath").as_deref(),
        Some("Movie Title 2024/Movie Title 2024.mkv")
    );
    assert_eq!(
        renamed[1].form_value("newPath").as_deref(),
        Some("Movie Title 2024/Movie Title 2024.1.srt")
    );
}

#[tokio::test]
async fn sonarr_history_uses_transmission_when_the_client_is_transmission() {
    let env = TestEnv::start().await;
    let arr = MockHttp::start().await;
    let transmission = MockHttp::start().await;
    arr.route(
        "GET",
        "/api/v3/history/since",
        MockHttpReply::json(
            r#"[
                {"sourceTitle":"Test Show S01E02 1080p","downloadId":"TRANSMISSIONHASH","data":{"downloadClient":"Transmission"}}
            ]"#,
        ),
    );
    transmission.route_sequence(
        "POST",
        "/transmission/rpc",
        vec![
            MockHttpReply::status(409).with_header("X-Transmission-Session-Id", TR_SESSION),
            MockHttpReply::json(r#"{"arguments":{"torrents":[{"name":"Old Show S01E02.mkv"}]}}"#),
            MockHttpReply::json(r#"{"result":"success"}"#),
        ],
    );

    let mut config = env.config();
    config.sonarr_url = Some(arr.base_url());
    config.sonarr_api_key = Some(ARR_API_KEY.to_owned());
    config.transmission_url = Some(format!("{}/transmission/rpc", transmission.base_url()));
    config.transmission_username = Some(TR_USER.to_owned());
    config.transmission_password = Some(TR_PASSWORD.to_owned());
    config.rename_file = true;
    let state = AppState::new(config).await.unwrap();

    let login = login_once(&state).await;
    assert!(!login.qbittorrent);
    assert!(login.transmission);

    let report = sonarr_rename_once(&state).await.unwrap();
    assert_eq!(report.renamed_torrents, 1);
    assert_eq!(report.renamed_files, 0);
    assert_eq!(report.errors, 0);

    let requests = transmission.requests_for("POST", "/transmission/rpc");
    assert_eq!(requests.len(), 3);
    let rename = &requests[2];
    assert!(rename.body.contains(r#""ids":"transmissionhash""#));
    assert!(rename.body.contains(r#""path":"Old Show S01E02.mkv""#));
    assert!(
        rename
            .body
            .contains(r#""name":"Test Show S01E02 1080p.mkv""#)
    );
}

#[tokio::test]
async fn rename_formatter_matches_java_token_rules() {
    let env = setup(true).await;
    seed_sonarr_rules(&env.env).await;
    let formatter = jproxy_rs::formatting::RenameFormatter::load(
        &env.state.pool,
        jproxy_rs::models::AppKind::Sonarr,
        &["season", "episode"],
    )
    .await
    .unwrap();
    assert_eq!(
        formatter.format("Test Show S01E02", "{season}", true, true),
        "S01"
    );
    assert_eq!(
        formatter.format("Old.S01E02.1080p", "S01{episode}", true, true),
        "S01E02"
    );
    assert_eq!(
        formatter.format("No episode here", "S01{episode}", true, true),
        "No episode here"
    );
}
