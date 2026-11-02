use std::sync::LazyLock;

use regex::{Captures, Regex};

/// Port of the Java `FormatUtil` helpers that affect query/title matching.
static BRACKET_SEPARATORS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\]【】]").expect("static regex"));
static REGEX_SPECIAL_CHAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\$\(\)\*\+\.\?\^\{\}\|\\]").expect("static regex"));
static ARTICLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\b|\s)(a|an|the)\s").expect("static regex"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" +").expect("static regex"));
static SLASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/+").expect("static regex"));
static YEAR_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" (19|20)\d{2}$").expect("static regex"));
static EPISODE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" 0*(\d{1,3}|1[0-8]\d{2})$").expect("static regex"));
static SEASON_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" S\d+$").expect("static regex"));
static SEASON_EPISODE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" (S\d+ |)\d+$").expect("static regex"));
static EXPLICIT_SEASON_EPISODE_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s+(?:s\d{1,3}(?:e\d{1,4})?|\d{1,3}x\d{1,4}|e\d{1,4})$").expect("static regex")
});
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("static regex"));
static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([^}]+)\}").expect("static regex"));
static ENGLISH_CLEAN_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[ .?a-zA-Z]+$").expect("static regex"));

pub fn clean_title(title: &str, configured: &Regex) -> String {
    let mut value = BRACKET_SEPARATORS.replace_all(title, "/").into_owned();
    value = REGEX_SPECIAL_CHAR.replace_all(&value, " ").into_owned();
    value = configured.replace_all(&value, " ").into_owned();
    value = ARTICLE.replace_all(&value, " ").into_owned();
    if value.trim().is_empty() {
        value = title.to_owned();
    }
    value = SLASHES.replace_all(&value, "/").into_owned();
    value = SPACES.replace_all(&value, " ").into_owned();
    value.to_lowercase().trim().to_owned()
}

pub fn remove_year(title: &str) -> String {
    if title.trim().is_empty() {
        return String::new();
    }
    YEAR_SUFFIX.replace(title, "").into_owned()
}

pub fn remove_episode(title: &str) -> String {
    if title.trim().is_empty() {
        return String::new();
    }
    EPISODE_SUFFIX.replace(title, "").into_owned()
}

pub fn remove_season(title: &str) -> String {
    if title.trim().is_empty() {
        return String::new();
    }
    SEASON_SUFFIX.replace(title, "").into_owned()
}

pub fn remove_season_episode(title: &str) -> String {
    if title.trim().is_empty() {
        return String::new();
    }
    SEASON_EPISODE_SUFFIX.replace(title, "").into_owned()
}

/// Broader fallback than the Java regex: strips common explicit SxxExx forms
/// in addition to Java's `" Sxx <episode>"`/`" <episode>"` suffixes.
pub fn strip_season_episode(title: &str) -> String {
    let value = EXPLICIT_SEASON_EPISODE_SUFFIX.replace(title, "");
    remove_season_episode(&value).trim().to_owned()
}

pub fn replace_token(token: &str, value: &str, text: &str) -> String {
    text.replace(&format!("{{{token}}}"), value)
}

pub fn remove_all_tokens(text: &str) -> String {
    TOKEN.replace_all(text, "").into_owned()
}

pub fn token_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for capture in TOKEN.captures_iter(text) {
        let name = capture[1].to_owned();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

pub fn execute_offset(value: &str, offset: i64) -> String {
    if offset == 0 {
        return value.to_owned();
    }
    NUMBER
        .replace_all(value, |captures: &Captures<'_>| {
            match captures[0].parse::<i64>() {
                Ok(number) => number.saturating_add(offset).to_string(),
                Err(_) => captures[0].to_owned(),
            }
        })
        .into_owned()
}

pub fn is_english_clean_title(value: &str) -> bool {
    ENGLISH_CLEAN_TITLE.is_match(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex() -> Regex {
        Regex::new(crate::config::DEFAULT_CLEAN_TITLE_REGEX).unwrap()
    }

    #[test]
    fn clean_title_matches_java_shape() {
        assert_eq!(
            clean_title("[Group] The Show - 2024", &regex()),
            "group show 2024"
        );
        assert_eq!(clean_title("The  Show", &regex()), "show");
        assert_eq!(
            clean_title("【简中】葬送的芙莉莲", &regex()),
            "简中 葬送的芙莉莲"
        );
    }

    #[test]
    fn suffix_helpers_strip_expected_parts() {
        assert_eq!(remove_year("Movie 2024"), "Movie");
        assert_eq!(remove_episode("Show 007"), "Show");
        assert_eq!(remove_season("Show S03"), "Show");
        assert_eq!(remove_season_episode("Show S03 07"), "Show");
        assert_eq!(remove_season_episode("Show 07"), "Show");
        assert_eq!(strip_season_episode("Show S03E07"), "Show");
        assert_eq!(strip_season_episode("Show 3x07"), "Show");
    }

    #[test]
    fn offset_increments_every_number() {
        assert_eq!(execute_offset("S2E03", 1), "S3E4");
        assert_eq!(execute_offset("no digits", 5), "no digits");
    }

    #[test]
    fn tokens_are_extracted_once_in_order() {
        assert_eq!(
            token_names("{title} {season}{episode} {title}"),
            vec!["title", "season", "episode"]
        );
        assert_eq!(remove_all_tokens("a {title} b {year}"), "a  b ");
    }
}
