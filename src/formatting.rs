use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use sqlx::SqlitePool;

use crate::db;
#[cfg(test)]
use crate::models::Rule;
use crate::models::{AppKind, TitleCandidate};
use crate::rules::{RuleSet, full_match};
use crate::text::{
    clean_title, execute_offset, is_english_clean_title, remove_all_tokens, replace_token,
    token_names,
};

static SONARR_SPECIAL_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"( season \d+| episode \d+| ep \d+| aka )").expect("static regex")
});
static RADARR_SPECIAL_WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"( aka )").expect("static regex"));

/// Reference data used to rewrite the `<title>` elements of a Torznab feed.
///
/// This mirrors the Java `executeFormatRule` path. It is deliberately kept
/// separate from search-query planning: a missing title rule leaves the feed
/// byte-for-byte unchanged.
#[derive(Debug, Clone)]
pub struct FormattingData {
    kind: AppKind,
    format: String,
    clean_regex: Regex,
    rules: RuleSet,
    candidates: Vec<TitleCandidate>,
}

impl FormattingData {
    pub async fn load(
        pool: &SqlitePool,
        kind: AppKind,
        format: &str,
        clean_regex: Regex,
    ) -> Result<Option<Self>> {
        let tokens = token_names(format);
        if !tokens.iter().any(|token| token == "title") {
            return Ok(None);
        }
        let rules = db::load_rules(pool, kind).await?;
        let rules = RuleSet::new(rules, &tokens);
        if rules.token_rules("title").is_empty() {
            return Ok(None);
        }
        let candidates = db::load_title_candidates(pool, kind).await?;
        Ok(Some(Self {
            kind,
            format: format.to_owned(),
            clean_regex,
            rules,
            candidates,
        }))
    }

    #[cfg(test)]
    pub fn from_parts(
        kind: AppKind,
        format: &str,
        clean_regex: Regex,
        rules: Vec<Rule>,
        candidates: Vec<TitleCandidate>,
    ) -> Self {
        let tokens = token_names(format);
        Self {
            kind,
            format: format.to_owned(),
            clean_regex,
            rules: RuleSet::new(rules, &tokens),
            candidates,
        }
    }

    pub fn format_item_title(&self, title: &str, description: Option<&str>) -> Option<String> {
        let text = title.replace(['\r', '\n'], " ");
        let mut format = self.format.clone();
        if !self.apply_title_rules(&text, &mut format) {
            return None;
        }
        if format.contains("{title}") {
            return None;
        }

        let combined = match description {
            Some(description) if !description.trim().is_empty() => {
                format!("{text} / {description}")
            }
            _ => text,
        };
        format = self.apply_token_rules(&combined, format);
        if self.kind == AppKind::Sonarr && format.contains("{episode}") {
            return Some(combined);
        }
        Some(remove_all_tokens(&format).trim().to_owned())
    }

    fn apply_title_rules(&self, text: &str, format: &mut String) -> bool {
        let clean_text = clean_title(text, &self.clean_regex);
        for prepared in self.rules.token_rules("title") {
            if prepared.contains_clean_title_placeholder() {
                for candidate in &self.candidates {
                    let clean_title_value = candidate
                        .clean_title
                        .clone()
                        .unwrap_or_else(|| clean_title(&candidate.title, &self.clean_regex));
                    if clean_title_value.is_empty() {
                        continue;
                    }
                    let clean_pattern = clean_title_value.replace(' ', ".?");
                    let expanded = prepared.rule.regex.replace("{cleanTitle}", &clean_pattern);
                    if !full_match(&expanded, &clean_text) {
                        continue;
                    }

                    if is_english_clean_title(&clean_pattern) {
                        let prefix = prepared
                            .rule
                            .regex
                            .replace("{cleanTitle}", &format!("[a-zA-Z]+ {clean_pattern}"));
                        let suffix = prepared
                            .rule
                            .regex
                            .replace("{cleanTitle}", &format!("{clean_pattern} [a-zA-Z]+"));
                        let boundary_text = match self.kind {
                            AppKind::Sonarr => SONARR_SPECIAL_WORDS
                                .replace_all(&clean_text, "/")
                                .into_owned(),
                            AppKind::Radarr => RADARR_SPECIAL_WORDS
                                .replace_all(&clean_text, "/")
                                .into_owned(),
                        };
                        if full_match(&prefix, &boundary_text)
                            || full_match(&suffix, &boundary_text)
                        {
                            continue;
                        }
                    }

                    if self.kind == AppKind::Radarr && !self.candidate_year_matches(candidate, text)
                    {
                        continue;
                    }

                    *format = replace_token("title", &candidate.main_title, format);
                    if self.kind == AppKind::Sonarr
                        && candidate.season_number != -1
                        && candidate.season_number != 1
                    {
                        *format = replace_token(
                            "season",
                            &format!("S{}", candidate.season_number),
                            format,
                        );
                    }
                    if self.kind == AppKind::Radarr
                        && let Some(year) = candidate.year
                    {
                        *format = replace_token("year", &year.to_string(), format);
                    }
                    return true;
                }
            } else if let Some(regex) = &prepared.regex
                && regex.is_match(text)
            {
                let value = regex
                    .replace_all(text, prepared.rule.replacement.as_str())
                    .into_owned();
                *format = replace_token("title", &value, format);
                return true;
            }
        }
        false
    }

    fn candidate_year_matches(&self, candidate: &TitleCandidate, text: &str) -> bool {
        let Some(year) = candidate.year else {
            return true;
        };
        for prepared in self.rules.token_rules("year") {
            let Some(regex) = &prepared.regex else {
                continue;
            };
            if regex.is_match(text) {
                let candidate_text = regex
                    .replace_all(text, prepared.rule.replacement.as_str())
                    .into_owned();
                return candidate_text == year.to_string();
            }
        }
        true
    }

    fn apply_token_rules(&self, text: &str, mut format: String) -> String {
        for token in token_names(&self.format) {
            if token == "title" {
                continue;
            }
            for prepared in self.rules.token_rules(&token) {
                let Some(regex) = &prepared.regex else {
                    continue;
                };
                if regex.is_match(text) {
                    let value = regex
                        .replace_all(text, prepared.rule.replacement.as_str())
                        .into_owned();
                    let value = execute_offset(&value, prepared.rule.offset);
                    format = replace_token(&token, &value, &format);
                    break;
                }
            }
        }
        format
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Rule;

    fn clean_regex() -> Regex {
        Regex::new(crate::config::DEFAULT_CLEAN_TITLE_REGEX).unwrap()
    }

    fn rule(id: &str, token: &str, priority: i64, regex: &str, replacement: &str) -> Rule {
        Rule {
            id: id.to_owned(),
            token: token.to_owned(),
            priority,
            regex: regex.to_owned(),
            replacement: replacement.to_owned(),
            offset: 0,
        }
    }

    fn candidate(main: &str, title: &str, clean: &str) -> TitleCandidate {
        TitleCandidate {
            main_title: main.to_owned(),
            title: title.to_owned(),
            clean_title: Some(clean.to_owned()),
            season_number: -1,
            year: Some(2024),
            monitored: true,
        }
    }

    #[test]
    fn direct_title_rules_apply_in_priority_order() {
        let rules = vec![
            rule("later", "title", 20, ".*", "Later"),
            rule("first", "title", 10, "Bad", "Good"),
        ];
        let data =
            FormattingData::from_parts(AppKind::Radarr, "{title}", clean_regex(), rules, vec![]);
        assert_eq!(
            data.format_item_title("Bad Release 2024", None),
            Some("Good Release 2024".to_owned())
        );
    }

    #[test]
    fn clean_title_rule_selects_main_title() {
        let rules = vec![rule(
            "primary",
            "title",
            999,
            r".*(\b|\s){cleanTitle}(\b|\s).*",
            "{mainTitle}",
        )];
        let data = FormattingData::from_parts(
            AppKind::Radarr,
            "{title}",
            clean_regex(),
            rules,
            vec![candidate("Main Movie", "Main.Movie.2024", "main movie")],
        );
        assert_eq!(
            data.format_item_title("Main.Movie.2024.1080p", None),
            Some("Main Movie".to_owned())
        );
    }

    #[test]
    fn token_rules_use_priority_and_description() {
        let title_rule = rule(
            "primary",
            "title",
            999,
            r".*(\b|\s){cleanTitle}(\b|\s).*",
            "{mainTitle}",
        );
        let group_rule = rule("group", "group", 100, r".*\[(.+)\].*", "$1");
        let data = FormattingData::from_parts(
            AppKind::Radarr,
            "{title} {group}",
            clean_regex(),
            vec![title_rule, group_rule],
            vec![candidate("Main Movie", "Main.Movie", "main movie")],
        );
        assert_eq!(
            data.format_item_title("Main.Movie.2024 [SubsPlease]", None),
            Some("Main Movie SubsPlease".to_owned())
        );
        assert_eq!(
            data.format_item_title("Main.Movie.2024", Some("[SubsPlease]")),
            Some("Main Movie SubsPlease".to_owned())
        );
    }

    #[test]
    fn unmatched_title_returns_none_so_xml_is_preserved() {
        let rules = vec![rule(
            "primary",
            "title",
            999,
            r".*(\b|\s){cleanTitle}(\b|\s).*",
            "{mainTitle}",
        )];
        let data = FormattingData::from_parts(
            AppKind::Radarr,
            "{title}",
            clean_regex(),
            rules,
            vec![candidate("Main Movie", "Main Movie", "main movie")],
        );
        assert_eq!(data.format_item_title("Another Release 2024", None), None);
    }
}
