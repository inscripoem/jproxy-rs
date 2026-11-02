//! Behaviour parity harness.
//!
//! Each test drives the real router against Torznab XML fixtures served by an
//! in-process mock indexer. Expectations are derived from the Java jproxy
//! reference implementation (`IndexerFilter` + `SonarrIndexerServiceImpl` /
//! `RadarrIndexerServiceImpl`). Where the Rust rewrite deliberately differs the
//! test pins the Rust behaviour and the divergence is spelled out in a comment.

mod common;

use common::{EMPTY_FEED, MockResponse, TestEnv, get};

const JC_ZHCN: &str = include_str!("../fixtures/torznab/jackie_zhcn.xml");
const JC_ZHTW: &str = include_str!("../fixtures/torznab/jackie_zhtw.xml");
const FORMAT_SONARR: &str = include_str!("../fixtures/torznab/format_sonarr_items.xml");
const FORMAT_RADARR: &str = include_str!("../fixtures/torznab/format_radarr_items.xml");

/// Sonarr metadata mirroring the live schema: a main row (sno 0), a title-slug
/// row (sno 1), one alternate (sno 2) and two TMDB translations.
async fn seed_sonarr(env: &TestEnv) {
    env.exec(
        "INSERT INTO sonarr_title (id, tvdb_id, sno, main_title, title, clean_title, season_number, monitored) VALUES \
         (767110, 76711, 0, 'Jackie Chan Adventures', 'Jackie Chan Adventures', 'jackie chan adventures', -1, 1), \
         (767111, 76711, 1, 'Jackie Chan Adventures', 'Jackie Chan Adventures', 'jackie chan adventures', -1, 1), \
         (767112, 76711, 2, 'Jackie Chan Adventures', 'Jackie Chan Adventures 2002', 'jackie chan adventures 2002', 1, 1), \
         (2720740, 272074, 0, 'Kill la Kill', 'Kill la Kill', 'kill la kill', -1, 1), \
         (2720741, 272074, 1, 'Kill la Kill', 'Kill la Kill', 'kill la kill', -1, 1); \
         INSERT INTO tmdb_title (id, tvdb_id, language, title) VALUES \
         (1, 76711, 'zh-CN', '成龙历险记'), \
         (2, 76711, 'zh-TW', '成龍小子'), \
         (3, 272074, 'zh-CN', '斩服少女'), \
         (4, 272074, 'zh-TW', 'KILL la KILL');",
    )
    .await;
}

async fn seed_radarr(env: &TestEnv) {
    env.exec(
        "INSERT INTO radarr_title (id, tmdb_id, sno, main_title, title, clean_title, year, monitored) VALUES \
         (180, 18, 0, 'The Fifth Element', 'The Fifth Element', 'fifth element', 1997, 1), \
         (181, 18, 1, 'The Fifth Element', 'The Fifth Element', 'fifth element', 1997, 1), \
         (182, 18, 2, 'The Fifth Element', 'The Fifth Element', 'thefifthelement', 1997, 1), \
         (183, 18, 3, 'The Fifth Element', 'Le Cinquième Élément', 'le cinquième élément', 1997, 1), \
         (9990, 999, 0, '第五元素', '第五元素', '第五元素', 1997, 1);",
    )
    .await;
}

/// The upstream-bundled title rule plus two group rules with distinct
/// priorities, so priority ordering is observable.
async fn seed_rules(env: &TestEnv) {
    env.exec(
        "INSERT INTO sonarr_rule (id, token, priority, regex, replacement, offset, valid_status) VALUES \
         ('title-main', 'title', 999, '.*(\\b|\\s){cleanTitle}(\\b|\\s).*', '{mainTitle}', 0, 1), \
         ('group-late', 'group', 2000, '.*', 'LATE', 0, 1), \
         ('group-early', 'group', 100, '.*\\[(.+)\\].*', '$1', 0, 1); \
         INSERT INTO radarr_rule (id, token, priority, regex, replacement, offset, valid_status) VALUES \
         ('title-main', 'title', 999, '.*(\\b|\\s){cleanTitle}(\\b|\\s).*', '{mainTitle}', 0, 1);",
    )
    .await;
}

/// Sonarr: all TMDB alternate titles are searched in order, matching Java's
/// full walk over `searchTitleList`; `MIN_COUNT` no longer stops the loop.
///
/// Java `SonarrIndexerServiceImpl.getSearchTitle` builds
/// `[queryTitle, tmdb1, tmdb2, tmdb1]` and `IndexerFilter` replaces
/// `searchTitleList[0]` inside the query for each entry. The upstream queries
/// below are byte-identical to Java's:
///   `Jackie Chan Adventures`, `成龙历险记`, `成龍小子`, `成龙历险记`.
#[tokio::test]
async fn sonarr_tries_all_tmdb_alternates_in_order() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;

    env.mock
        .route("Jackie Chan Adventures", MockResponse::ok(EMPTY_FEED));
    env.mock.route("成龙历险记", MockResponse::ok(JC_ZHCN));
    env.mock.route("成龍小子", MockResponse::ok(JC_ZHTW));

    let mut config = env.config();
    config.min_count = 6;
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&season=1&ep=2&limit=100&apikey=SECRET",
    )
    .await;

    assert_eq!(response.status, 200);
    assert_eq!(
        env.mock.recorded_queries(),
        vec![
            "Jackie Chan Adventures",
            "成龙历险记",
            "成龍小子",
            "成龙历险记"
        ]
    );
    // All four upstream requests keep the incoming Torznab parameters; the
    // last step only strips them when the earlier titles missed MIN_COUNT.
    let full_queries = env.mock.recorded_full_queries();
    for query in &full_queries {
        assert!(query.contains("season=1"), "missing season in {query}");
        assert!(query.contains("ep=2"), "missing ep in {query}");
        assert!(
            query.contains("apikey=SECRET"),
            "query parametrisation lost in {query}"
        );
    }
    // 3 + 3 items for the two alternates, plus 3 for the repeated first
    // alternate.
    assert_eq!(response.item_count(), 9);
}

/// Sonarr: when the earlier titles produce fewer than `MIN_COUNT` results, the
/// repeated first TMDB title has season/episode stripped from the query.
#[tokio::test]
async fn sonarr_last_tmdb_title_clears_season_episode_when_below_min_count() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;

    env.mock
        .route("Jackie Chan Adventures", MockResponse::ok(EMPTY_FEED));
    env.mock.route("成龙历险记", MockResponse::ok(JC_ZHCN));
    env.mock.route("成龍小子", MockResponse::ok(JC_ZHTW));

    let mut config = env.config();
    config.min_count = 20;
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&season=1&ep=2&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    assert_eq!(
        env.mock.recorded_queries(),
        vec![
            "Jackie Chan Adventures",
            "成龙历险记",
            "成龍小子",
            "成龙历险记"
        ]
    );
    let full = env.mock.recorded_full_queries();
    let fallback = full.last().unwrap();
    assert!(
        !fallback.contains("season="),
        "season kept in fallback: {fallback}"
    );
    assert!(!fallback.contains("ep="), "ep kept in fallback: {fallback}");
    assert_eq!(response.item_count(), 9);
}

/// `MIN_COUNT=0` no longer short-circuits; Java still walks the whole title
/// list.
#[tokio::test]
async fn min_count_zero_still_walks_all_titles() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    env.mock
        .route("Jackie Chan Adventures", MockResponse::ok(JC_ZHCN));
    env.mock.route("成龙历险记", MockResponse::ok(EMPTY_FEED));
    env.mock.route("成龍小子", MockResponse::ok(EMPTY_FEED));

    let mut config = env.config();
    config.min_count = 0;
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    assert_eq!(
        env.mock.recorded_queries(),
        vec![
            "Jackie Chan Adventures",
            "成龙历险记",
            "成龍小子",
            "成龙历险记"
        ]
    );
    assert_eq!(response.item_count(), 3);
}

/// Radarr: the year suffix is stripped for the SQLite lookup and for the
/// second query. Java produces the same two queries for a sno-0 match.
#[tokio::test]
async fn radarr_strips_year_for_lookup_and_second_query() {
    let env = TestEnv::start().await;
    seed_radarr(&env).await;
    env.mock
        .route("The Fifth Element 1997", MockResponse::ok(EMPTY_FEED));
    env.mock
        .route("The Fifth Element", MockResponse::ok(EMPTY_FEED));

    let mut config = env.config();
    config.min_count = 6;
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/radarr/prowlarr/2/api?t=search&q=The+Fifth+Element+1997&year=1997&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    // Java: searchTitleList = ["The Fifth Element 1997", "The Fifth Element"].
    assert_eq!(
        env.mock.recorded_queries(),
        vec!["The Fifth Element 1997", "The Fifth Element"]
    );
    for query in env.mock.recorded_full_queries() {
        assert!(query.contains("year=1997"), "year param dropped in {query}");
    }
}

/// Anime with an absolute episode number: the episode is stripped for the
/// lookup but the alternates keep the `05` suffix.
///
/// The first three upstream queries match Java exactly:
///   `Kill la Kill 05`, `斩服少女 05`, `KILL la KILL 05`.
#[tokio::test]
async fn anime_absolute_episode_keeps_suffix_in_alternates() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    for q in [
        "Kill la Kill 05",
        "斩服少女 05",
        "KILL la KILL 05",
        "Kill la Kill",
    ] {
        env.mock.route(q, MockResponse::ok(EMPTY_FEED));
    }

    let mut config = env.config();
    config.min_count = 20;
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/3/api?t=tvsearch&q=Kill+la+Kill+05&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    let queries = env.mock.recorded_queries();
    assert!(
        queries.len() >= 3,
        "expected at least 3 queries, got {queries:?}"
    );
    assert_eq!(
        &queries[..3],
        &["Kill la Kill 05", "斩服少女 05", "KILL la KILL 05"]
    );
}

/// Formatting rules: the priority-ordered title rule selects the row's
/// `main_title` and the higher-priority group rule wins.
#[tokio::test]
async fn sonarr_formatting_rewrites_item_titles_like_java() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    seed_rules(&env).await;
    env.mock
        .route("Jackie Chan Adventures", MockResponse::ok(FORMAT_SONARR));

    let mut config = env.config();
    config.min_count = 0;
    // Only title and group tokens, so the result is deterministic.
    config.sonarr_indexer_format = "{title} {group}".to_owned();
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    let titles = response.item_titles();
    // {title} -> mainTitle, {group} -> first capture of the priority-100 rule
    // (the priority-2000 `.*` rule must not win).
    assert_eq!(titles[0], "Jackie Chan Adventures SubsPlease");
    // A title with no matching candidate is left byte-for-byte untouched.
    assert_eq!(titles[1], "Completely Unrelated Show S01E01 1080p");
    assert!(response.body.contains("<title>Fixture Indexer</title>"));
}

/// When the configured format still contains an unresolved `{episode}` token
/// after the token rules run, Java returns the raw `title / description`
/// concatenation instead of a cleaned title. This pins that behaviour.
#[tokio::test]
async fn sonarr_unresolved_episode_token_returns_combined_text() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    seed_rules(&env).await;
    env.mock
        .route("Jackie Chan Adventures", MockResponse::ok(FORMAT_SONARR));

    let mut config = env.config();
    config.min_count = 0;
    // Default format keeps {episode}; no episode rule is seeded.
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Jackie+Chan+Adventures&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    assert_eq!(
        response.item_titles()[0],
        "Jackie Chan Adventures S01E05 [SubsPlease] 1080p / Group release / 1080p"
    );
}

/// Formatting rules for Radarr: Chinese `{cleanTitle}` selection plus the year
/// token.
#[tokio::test]
async fn radarr_formatting_rewrites_item_titles_like_java() {
    let env = TestEnv::start().await;
    seed_radarr(&env).await;
    seed_rules(&env).await;
    env.mock.route("第五元素", MockResponse::ok(FORMAT_RADARR));

    let mut config = env.config();
    config.min_count = 0;
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/radarr/prowlarr/2/api?t=search&q=%E7%AC%AC%E4%BA%94%E5%85%83%E7%B4%A0&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    let titles = response.item_titles();
    assert_eq!(titles[0], "第五元素 1997");
    assert_eq!(titles[1], "Unrelated.Movie.2019.1080p.BluRay.x264");
}

/// A title that does not exist in SQLite still proxies the first query once.
#[tokio::test]
async fn unknown_title_still_proxies_the_query() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    env.mock.fallback(MockResponse::ok(JC_ZHCN));

    let mut config = env.config();
    config.min_count = 6;
    let router = env.router_with(config).await;

    let response = get(
        &router,
        "/sonarr/prowlarr/1/api?t=tvsearch&q=Totally+Unknown+Show+S01E01&limit=100",
    )
    .await;

    assert_eq!(response.status, 200);
    let queries = env.mock.recorded_queries();
    assert_eq!(
        queries.first().map(String::as_str),
        Some("Totally Unknown Show S01E01")
    );
}

/// Requests without a `q` (capabilities) are proxied once, unmodified.
#[tokio::test]
async fn capabilities_request_is_passed_through() {
    let env = TestEnv::start().await;
    seed_sonarr(&env).await;
    env.mock.fallback(MockResponse::ok(
        "<?xml version=\"1.0\"?><caps><server version=\"1.0\"/></caps>",
    ));

    let router = env.router().await;
    let response = get(&router, "/sonarr/prowlarr/1/api?t=caps").await;

    assert_eq!(response.status, 200);
    assert_eq!(env.mock.request_count(), 1);
    assert_eq!(env.mock.recorded_full_queries(), vec!["t=caps"]);
    assert!(response.body.contains("<caps>"));
}
