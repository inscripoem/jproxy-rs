use std::future::Future;

use tracing::warn;

use crate::models::{AppKind, MatchedTitle};
use crate::text::remove_year;
use crate::torznab::{merge_xml, trim_items};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchReason {
    Initial,
    AlternateTitle,
    MainLanguageFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchStep {
    pub q: String,
    pub clear_season_episode: bool,
    pub reason: SearchReason,
}

impl SearchStep {
    pub fn is_fallback(&self) -> bool {
        self.reason == SearchReason::MainLanguageFallback
    }
}

#[derive(Debug, Clone)]
pub struct SearchAttempt {
    pub step: SearchStep,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct SearchPage {
    pub xml: String,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct SearchRun {
    pub xml: Option<String>,
    pub count: usize,
    pub attempts: usize,
    pub used_fallback: bool,
}

/// Build the ordered search attempts used by the Java filter.
///
/// Sonarr tries TMDB alternate titles for the main/slug rows, then falls back
/// to the main title with season/episode removed. Radarr tries the title
/// without its year and then the main title. Duplicate query strings are
/// removed while preserving order.
pub fn build_search_steps(
    kind: AppKind,
    search_key: &str,
    lookup_title: &str,
    matched: Option<&MatchedTitle>,
    tmdb_titles: &[String],
    _min_count: usize,
) -> Vec<SearchStep> {
    // Java's `getSearchTitle` intentionally repeats the first TMDB title and
    // the clean title; do not deduplicate.
    let mut steps = vec![SearchStep {
        q: search_key.to_owned(),
        clear_season_episode: false,
        reason: SearchReason::Initial,
    }];

    let Some(matched) = matched else {
        return steps;
    };

    match kind {
        AppKind::Sonarr if matches!(matched.sno, 0 | 1) => {
            if let Some(first) = tmdb_titles.first() {
                for alternate in tmdb_titles {
                    steps.push(SearchStep {
                        q: replace_title(search_key, lookup_title, alternate),
                        clear_season_episode: false,
                        reason: SearchReason::AlternateTitle,
                    });
                }
                steps.push(SearchStep {
                    q: replace_title(search_key, lookup_title, first),
                    clear_season_episode: false,
                    reason: SearchReason::AlternateTitle,
                });
            }
        }
        AppKind::Radarr => {
            let without_year = remove_year(search_key);
            if without_year != search_key {
                steps.push(SearchStep {
                    q: without_year,
                    clear_season_episode: false,
                    reason: SearchReason::AlternateTitle,
                });
            }
            if matched.sno == 1 || matched.sno == 2 {
                // Java appends the sno 0 row's clean_title twice.
                for _ in 0..2 {
                    steps.push(SearchStep {
                        q: matched.clean_title.clone(),
                        clear_season_episode: false,
                        reason: SearchReason::AlternateTitle,
                    });
                }
            }
        }
        _ => {}
    }

    steps
}

fn replace_title(query: &str, base: &str, alternate: &str) -> String {
    if base.is_empty() {
        alternate.to_owned()
    } else {
        query.replace(base, alternate)
    }
}

/// Execute all search attempts until the result limit or the plan is
/// exhausted. Java only uses `MIN_COUNT` to strip season/episode from the last
/// attempt; it never stops early. Failed alternate attempts are ignored when a
/// previous page was returned; a failed first attempt is an error.
pub async fn execute_search<F, Fut, E>(
    steps: Vec<SearchStep>,
    min_count: usize,
    initial_offset: i64,
    initial_limit: i64,
    mut fetch: F,
) -> Result<SearchRun, E>
where
    F: FnMut(SearchAttempt) -> Fut,
    Fut: Future<Output = Result<SearchPage, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut remaining_limit = if initial_limit > 0 {
        initial_limit
    } else {
        100
    };
    let mut merged: Option<String> = None;
    let mut total_count = 0usize;
    let mut attempts = 0usize;
    let mut used_fallback = false;
    let step_count = steps.len();

    for (index, mut step) in steps.into_iter().enumerate() {
        if remaining_limit <= 0 {
            break;
        }

        // Java modifies only the last title when the earlier attempts produced
        // fewer than MIN_COUNT results. That is the only role of MIN_COUNT.
        if step_count > 2 && index + 1 == step_count && min_count > 0 && total_count < min_count {
            step.clear_season_episode = true;
            used_fallback = true;
        }

        let attempt = SearchAttempt {
            step,
            offset: if index == 0 { initial_offset } else { 0 },
            limit: remaining_limit,
        };
        let is_fallback = attempt.step.is_fallback();
        let page = match fetch(attempt).await {
            Ok(page) => page,
            Err(error) => {
                if merged.is_some() {
                    warn!(%error, "alternate indexer search failed; returning partial results");
                    break;
                }
                return Err(error);
            }
        };
        attempts += 1;
        used_fallback |= is_fallback;

        let mut page = page;
        if page.count as i64 > remaining_limit {
            page.xml = trim_items(&page.xml, remaining_limit as usize);
            page.count = remaining_limit as usize;
        }
        let count = page.count;
        if count > 0 || merged.is_none() {
            merged = match merged.take() {
                Some(previous) => Some(merge_xml(&previous, &page.xml)),
                None => Some(page.xml),
            };
        }
        total_count += count;
        remaining_limit -= count as i64;
    }

    Ok(SearchRun {
        xml: merged,
        count: total_count,
        attempts,
        used_fallback,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    fn matched(main_title: &str, sno: i64) -> MatchedTitle {
        MatchedTitle {
            id: 1,
            tvdb_id: Some(1),
            tmdb_id: None,
            sno,
            main_title: main_title.to_owned(),
            title: "Show".to_owned(),
            clean_title: main_title.to_lowercase(),
            season_number: -1,
            year: None,
            monitored: true,
        }
    }

    fn page(title: &str) -> String {
        format!(
            "<rss><channel><title>feed</title><item><title>{title}</title></item></channel></rss>"
        )
    }

    #[test]
    fn radarr_plan_uses_clean_title_twice_for_folder_rows() {
        // Java appends the sno 0 row's lowercase clean_title twice for
        // folder/slug rows (sno 1|2).
        let matched = matched("Oldboy", 1);
        let steps = build_search_steps(
            AppKind::Radarr,
            "Oldboy Folder 2024",
            "Oldboy Folder",
            Some(&matched),
            &[],
            6,
        );
        assert_eq!(
            steps.iter().map(|step| step.q.as_str()).collect::<Vec<_>>(),
            vec!["Oldboy Folder 2024", "Oldboy Folder", "oldboy", "oldboy"]
        );
        assert_eq!(steps[2].reason, SearchReason::AlternateTitle);
    }

    #[test]
    fn radarr_plan_has_no_extra_step_for_the_main_row() {
        let matched = matched("The Fifth Element", 0);
        let steps = build_search_steps(
            AppKind::Radarr,
            "The Fifth Element 1997",
            "The Fifth Element",
            Some(&matched),
            &[],
            6,
        );
        // Only the initial query and the year-stripped retry; the fallback
        // duplicate is removed.
        assert_eq!(
            steps.iter().map(|step| step.q.as_str()).collect::<Vec<_>>(),
            vec!["The Fifth Element 1997", "The Fifth Element"]
        );
    }

    #[test]
    fn sonarr_plan_repeats_first_tmdb_title() {
        let matched = matched("Main Show", 0);
        let steps = build_search_steps(
            AppKind::Sonarr,
            "Show S01E02",
            "Show",
            Some(&matched),
            &["主标题".to_owned()],
            6,
        );
        assert_eq!(steps[0].q, "Show S01E02");
        assert_eq!(steps[1].q, "主标题 S01E02");
        assert_eq!(steps[2].q, "主标题 S01E02");
        assert_eq!(steps[2].reason, SearchReason::AlternateTitle);
        assert!(!steps[2].clear_season_episode);
    }

    #[test]
    fn plan_is_full_even_when_min_count_is_zero() {
        let matched = matched("Main Show", 0);
        let steps = build_search_steps(
            AppKind::Sonarr,
            "Show 02",
            "Show",
            Some(&matched),
            &["Other".to_owned()],
            0,
        );
        assert_eq!(
            steps.iter().map(|step| step.q.as_str()).collect::<Vec<_>>(),
            vec!["Show 02", "Other 02", "Other 02"]
        );
    }

    #[tokio::test]
    async fn fallback_step_merges_pages() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let steps = vec![
            SearchStep {
                q: "Show 02".to_owned(),
                clear_season_episode: false,
                reason: SearchReason::Initial,
            },
            SearchStep {
                q: "Main Show".to_owned(),
                clear_season_episode: true,
                reason: SearchReason::MainLanguageFallback,
            },
        ];
        let calls_for_fetch = Rc::clone(&calls);
        let run = execute_search(steps, 2, 0, 10, move |attempt| {
            calls_for_fetch.borrow_mut().push(attempt.clone());
            async move {
                Ok::<_, std::convert::Infallible>(if attempt.step.is_fallback() {
                    SearchPage {
                        xml: page("fallback"),
                        count: 3,
                    }
                } else {
                    SearchPage {
                        xml: page("initial"),
                        count: 1,
                    }
                })
            }
        })
        .await
        .unwrap();

        assert_eq!(run.attempts, 2);
        assert_eq!(run.count, 4);
        assert!(run.used_fallback);
        let calls = calls.borrow();
        assert_eq!(calls[0].offset, 0);
        assert_eq!(calls[1].offset, 0);
        assert_eq!(calls[1].limit, 9);
        assert_eq!(crate::torznab::count_items(run.xml.as_deref().unwrap()), 2);
    }

    #[tokio::test]
    async fn walks_all_steps_after_min_count_reached() {
        let calls = Rc::new(RefCell::new(0usize));
        let counter = Rc::clone(&calls);
        let steps = vec![
            SearchStep {
                q: "one".to_owned(),
                clear_season_episode: false,
                reason: SearchReason::Initial,
            },
            SearchStep {
                q: "two".to_owned(),
                clear_season_episode: true,
                reason: SearchReason::MainLanguageFallback,
            },
        ];
        let run = execute_search(steps, 2, 0, 10, move |_| {
            *counter.borrow_mut() += 1;
            async move {
                Ok::<_, std::convert::Infallible>(SearchPage {
                    xml: page("initial"),
                    count: 2,
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(run.attempts, 2);
        assert_eq!(*calls.borrow(), 2);
        assert!(run.used_fallback);
        assert_eq!(run.count, 4);
    }

    #[tokio::test]
    async fn zero_min_count_still_walks_all_steps() {
        let calls = Rc::new(RefCell::new(0usize));
        let counter = Rc::clone(&calls);
        let steps = vec![
            SearchStep {
                q: "one".to_owned(),
                clear_season_episode: false,
                reason: SearchReason::Initial,
            },
            SearchStep {
                q: "two".to_owned(),
                clear_season_episode: true,
                reason: SearchReason::MainLanguageFallback,
            },
        ];
        let run = execute_search(steps, 0, 0, 10, move |_| {
            *counter.borrow_mut() += 1;
            async move {
                Ok::<_, std::convert::Infallible>(SearchPage {
                    xml: page("initial"),
                    count: 1,
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(run.attempts, 2);
        assert_eq!(*calls.borrow(), 2);
        assert_eq!(run.count, 2);
        assert!(run.used_fallback);
    }
}
