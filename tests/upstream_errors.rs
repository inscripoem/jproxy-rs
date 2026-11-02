//! Upstream error handling: retry policy, bounded concurrency and the mapping
//! of upstream failures onto meaningful downstream Torznab statuses.

mod common;

use std::time::Duration;

use common::{EMPTY_FEED, MockResponse, TestEnv, get};
use jproxy_rs::models::Provider;
use jproxy_rs::upstream::{RetryPolicy, UpstreamClient};

const JC_ZHCN: &str = include_str!("../fixtures/torznab/jackie_zhcn.xml");

async fn seed_sonarr(env: &TestEnv) {
    env.exec(
        "INSERT INTO sonarr_title (id, tvdb_id, sno, main_title, title, clean_title, season_number, monitored) VALUES \
         (767110, 76711, 0, 'Jackie Chan Adventures', 'Jackie Chan Adventures', 'jackie chan adventures', -1, 1), \
         (767111, 76711, 1, 'Jackie Chan Adventures', 'Jackie Chan Adventures', 'jackie chan adventures', -1, 1); \
         INSERT INTO tmdb_title (id, tvdb_id, language, title) VALUES \
         (1, 76711, 'zh-CN', '成龙历险记'), \
         (2, 76711, 'zh-TW', '成龍小子');",
    )
    .await;
}

/// Upstream 429 is retried, then surfaced downstream as 429 with the original
/// `Retry-After` header preserved (instead of a generic 500).
#[tokio::test]
async fn upstream_429_is_retried_and_surfaced_with_retry_after() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    env.mock
        .fallback(MockResponse::status(429).with_retry_after("7"));

    let mut config = env.config();
    config.upstream_retry = RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(20),
    };
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 429);
    assert_eq!(response.header("retry-after"), Some("7"));
    // One initial attempt plus two retries.
    assert_eq!(env.mock.request_count(), 3);
}

/// Upstream 5xx is retried and mapped to 502 Bad Gateway, never 500.
#[tokio::test]
async fn upstream_5xx_is_retried_and_mapped_to_bad_gateway() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    env.mock.fallback(MockResponse::status(503));

    let mut config = env.config();
    config.upstream_retry = RetryPolicy {
        max_retries: 1,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(10),
    };
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 502);
    assert_eq!(env.mock.request_count(), 2);
}

/// A timeout is retried and mapped to 504 Gateway Timeout.
#[tokio::test]
async fn upstream_timeout_is_mapped_to_gateway_timeout() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    env.mock
        .fallback(MockResponse::ok(EMPTY_FEED).with_delay(Duration::from_millis(300)));

    let mut config = env.config();
    config.request_timeout = Duration::from_millis(50);
    config.upstream_retry = RetryPolicy {
        max_retries: 0,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(10),
    };
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 504);
}

/// A non-retryable 4xx is forwarded as-is after a single attempt.
#[tokio::test]
async fn upstream_4xx_is_not_retried() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    env.mock.fallback(MockResponse::status(404));

    let mut config = env.config();
    config.upstream_retry = RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(10),
    };
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 502);
    assert_eq!(env.mock.request_count(), 1);
}

/// A partial failure: the first (initial) attempt succeeds and a later
/// alternate-title attempt keeps failing. The proxy returns the partial page
/// with HTTP 200 instead of discarding results.
#[tokio::test]
async fn partial_indexer_failure_returns_partial_results() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    // First request: a usable page. Every later request: a retried 500.
    env.mock.script(vec![
        MockResponse::ok(JC_ZHCN),
        MockResponse::status(500),
        MockResponse::status(500),
        MockResponse::status(500),
    ]);

    let mut config = env.config();
    config.min_count = 20;
    config.upstream_retry = RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(10),
    };
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    assert_eq!(response.item_count(), 3);
    // 1 success + 3 attempts against the failing alternate.
    assert_eq!(env.mock.request_count(), 4);
}

/// Bounded concurrency: with a limit of 2 no more than two upstream requests
/// are ever in flight, even under a burst of six callers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_concurrency_is_bounded() {
    let env = TestEnv::start().await;
    env.mock.script(
        (0..12)
            .map(|_| MockResponse::ok(EMPTY_FEED).with_delay(Duration::from_millis(40)))
            .collect(),
    );

    let client = UpstreamClient::with_policy(
        Some(env.mock.base_url()),
        None,
        Duration::from_secs(5),
        RetryPolicy {
            max_retries: 0,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        },
        2,
    )
    .unwrap();

    let mut handles = Vec::new();
    for index in 0..6 {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            client
                .fetch(
                    Provider::Prowlarr,
                    "1",
                    &format!("t=search&q=burst{index}&limit=100"),
                )
                .await
        }));
    }
    for handle in handles {
        assert_eq!(handle.await.unwrap().unwrap().status, 200);
    }

    assert!(
        env.mock.max_concurrency() <= 2,
        "observed {} concurrent upstream requests",
        env.mock.max_concurrency()
    );
    assert_eq!(env.mock.max_concurrency(), 2);
    assert_eq!(env.mock.request_count(), 6);
}
