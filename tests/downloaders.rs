//! Downloader session tests against local mock HTTP servers.
//!
//! Every credential here is synthetic. No test contacts a real downloader,
//! NAS service, or the network.

mod common;

use common::{MockHttp, MockHttpReply, TestEnv};
use jproxy_rs::app::AppState;

const QB_USER: &str = "synthetic-qb-user";
const QB_PASSWORD: &str = "synthetic-qb-password";
const QB_COOKIE: &str = "SID=synthetic-session";
const TR_USER: &str = "synthetic-tr-user";
const TR_PASSWORD: &str = "synthetic-tr-password";
const TR_SESSION: &str = "synthetic-transmission-session";

fn transmission_url(mock: &MockHttp) -> String {
    format!("{}/transmission/rpc", mock.base_url())
}

async fn state_with_downloaders(
    qb: Option<&MockHttp>,
    transmission: Option<&MockHttp>,
) -> AppState {
    let env = TestEnv::start().await;
    let mut config = env.config();
    if let Some(qb) = qb {
        config.qbittorrent_url = Some(qb.base_url());
        config.qbittorrent_username = Some(QB_USER.to_owned());
        config.qbittorrent_password = Some(QB_PASSWORD.to_owned());
    }
    if let Some(transmission) = transmission {
        config.transmission_url = Some(transmission_url(transmission));
        config.transmission_username = Some(TR_USER.to_owned());
        config.transmission_password = Some(TR_PASSWORD.to_owned());
    }
    AppState::new(config).await.unwrap()
}

#[tokio::test]
async fn qbittorrent_login_captures_cookie_and_uses_it_for_api_calls() {
    let mock = MockHttp::start().await;
    mock.route(
        "POST",
        "/api/v2/auth/login",
        MockHttpReply::ok("Ok.").with_header("Set-Cookie", QB_COOKIE),
    );
    mock.route(
        "GET",
        "/api/v2/torrents/files",
        MockHttpReply::json(r#"[{"name":"Old/Show.S01E02.mkv"},{"name":"Old/Show.S01E02.srt"}]"#),
    );
    mock.route("POST", "/api/v2/torrents/rename", MockHttpReply::ok("Ok."));
    mock.route(
        "POST",
        "/api/v2/torrents/renameFile",
        MockHttpReply::ok("Ok."),
    );

    let state = state_with_downloaders(Some(&mock), None).await;
    assert!(
        state
            .downloaders
            .qbittorrent
            .login_with(&mock.base_url(), QB_USER, QB_PASSWORD)
            .await
    );
    assert!(state.downloaders.qbittorrent.is_login().await);

    let login = mock.requests_for("POST", "/api/v2/auth/login");
    assert_eq!(login.len(), 1);
    assert_eq!(login[0].form_value("username").as_deref(), Some(QB_USER));
    assert_eq!(
        login[0].form_value("password").as_deref(),
        Some(QB_PASSWORD)
    );

    let files = state.downloaders.qbittorrent.files("abcdef").await.unwrap();
    assert_eq!(
        files,
        vec![
            "Old/Show.S01E02.mkv".to_owned(),
            "Old/Show.S01E02.srt".to_owned()
        ]
    );
    let files_request = mock.requests_for("GET", "/api/v2/torrents/files");
    assert_eq!(files_request[0].header("cookie"), Some(QB_COOKIE));
    assert!(files_request[0].query.contains("hash=abcdef"));

    assert!(
        state
            .downloaders
            .qbittorrent
            .rename("abcdef", "New Show S01E02")
            .await
            .unwrap()
    );
    assert!(
        state
            .downloaders
            .qbittorrent
            .rename_file("abcdef", "Old/Show.mkv", "New/Show.mkv")
            .await
            .unwrap()
    );

    let rename = mock.requests_for("POST", "/api/v2/torrents/rename");
    assert_eq!(rename[0].form_value("hash").as_deref(), Some("abcdef"));
    assert_eq!(
        rename[0].form_value("name").as_deref(),
        Some("New Show S01E02")
    );
    assert_eq!(rename[0].header("cookie"), Some(QB_COOKIE));

    let rename_file = mock.requests_for("POST", "/api/v2/torrents/renameFile");
    assert_eq!(
        rename_file[0].form_value("oldPath").as_deref(),
        Some("Old/Show.mkv")
    );
    assert_eq!(
        rename_file[0].form_value("newPath").as_deref(),
        Some("New/Show.mkv")
    );
}

#[tokio::test]
async fn qbittorrent_failed_relogin_is_non_fatal_and_the_next_attempt_succeeds() {
    let mock = MockHttp::start().await;
    mock.route_sequence(
        "POST",
        "/api/v2/auth/login",
        vec![
            MockHttpReply::ok("Ok.").with_header("Set-Cookie", QB_COOKIE),
            MockHttpReply::status(500),
            MockHttpReply::ok("Ok.").with_header("Set-Cookie", "SID=second-session"),
        ],
    );

    let state = state_with_downloaders(Some(&mock), None).await;
    assert!(
        state
            .downloaders
            .qbittorrent
            .login_with(&mock.base_url(), QB_USER, QB_PASSWORD)
            .await
    );

    assert!(!state.downloaders.qbittorrent.login().await);
    assert!(!state.downloaders.qbittorrent.is_login().await);
    assert!(state.downloaders.qbittorrent.login().await);
    assert!(state.downloaders.qbittorrent.is_login().await);
    assert_eq!(mock.requests_for("POST", "/api/v2/auth/login").len(), 3);
}

#[tokio::test]
async fn transmission_login_captures_session_and_renames_path() {
    let mock = MockHttp::start().await;
    mock.route_sequence(
        "POST",
        "/transmission/rpc",
        vec![
            MockHttpReply::status(409).with_header("X-Transmission-Session-Id", TR_SESSION),
            MockHttpReply::json(r#"{"arguments":{"torrents":[{"name":"Old Show S01E02.mkv"}]}}"#),
            MockHttpReply::json(r#"{"result":"success"}"#),
        ],
    );

    let state = state_with_downloaders(None, Some(&mock)).await;
    assert!(
        state
            .downloaders
            .transmission
            .login_with(&transmission_url(&mock), TR_USER, TR_PASSWORD)
            .await
    );
    assert!(state.downloaders.transmission.is_login().await);

    let login = mock.requests_for("POST", "/transmission/rpc");
    assert!(
        login[0]
            .header("authorization")
            .is_some_and(|value| value.starts_with("Basic "))
    );
    assert!(login[0].body.contains("session-get"));

    assert!(
        state
            .downloaders
            .transmission
            .rename("abcdef", "New: Show S01E02")
            .await
            .unwrap()
    );

    let requests = mock.requests_for("POST", "/transmission/rpc");
    assert_eq!(requests.len(), 3);
    let torrent_get = &requests[1];
    assert_eq!(
        torrent_get.header("x-transmission-session-id"),
        Some(TR_SESSION)
    );
    assert!(torrent_get.body.contains("torrent-get"));

    let rename = &requests[2];
    assert_eq!(rename.header("x-transmission-session-id"), Some(TR_SESSION));
    assert!(rename.body.contains("torrent-rename-path"));
    assert!(rename.body.contains(r#""path":"Old Show S01E02.mkv""#));
    assert!(rename.body.contains(r#""name":"New_ Show S01E02.mkv""#));
    assert!(!rename.body.contains("New: Show"));
}

#[tokio::test]
async fn transmission_relogins_after_409_and_retries_once() {
    let mock = MockHttp::start().await;
    mock.route_sequence(
        "POST",
        "/transmission/rpc",
        vec![
            MockHttpReply::status(409).with_header("X-Transmission-Session-Id", "session-1"),
            MockHttpReply::status(409).with_header("X-Transmission-Session-Id", "session-1"),
            MockHttpReply::status(409).with_header("X-Transmission-Session-Id", "session-2"),
            MockHttpReply::json(r#"{"arguments":{"torrents":[{"name":"Old Show.mkv"}]}}"#),
            MockHttpReply::json(r#"{"result":"success"}"#),
        ],
    );

    let state = state_with_downloaders(None, Some(&mock)).await;
    assert!(
        state
            .downloaders
            .transmission
            .login_with(&transmission_url(&mock), TR_USER, TR_PASSWORD)
            .await
    );
    assert!(
        state
            .downloaders
            .transmission
            .rename("abcdef", "New Show")
            .await
            .unwrap()
    );

    let requests = mock.requests_for("POST", "/transmission/rpc");
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests[3].header("x-transmission-session-id"),
        Some("session-2")
    );
    assert!(requests[4].body.contains("New Show.mkv"));
}

#[tokio::test]
async fn transmission_noop_rename_sends_no_rename_rpc() {
    let mock = MockHttp::start().await;
    mock.route_sequence(
        "POST",
        "/transmission/rpc",
        vec![
            MockHttpReply::status(409).with_header("X-Transmission-Session-Id", TR_SESSION),
            MockHttpReply::json(r#"{"arguments":{"torrents":[{"name":"New Show.mkv"}]}}"#),
        ],
    );

    let state = state_with_downloaders(None, Some(&mock)).await;
    assert!(
        state
            .downloaders
            .transmission
            .login_with(&transmission_url(&mock), TR_USER, TR_PASSWORD)
            .await
    );
    assert!(
        !state
            .downloaders
            .transmission
            .rename("abcdef", "New Show")
            .await
            .unwrap()
    );
    assert_eq!(mock.requests_for("POST", "/transmission/rpc").len(), 2);
}
