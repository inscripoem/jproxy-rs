use std::collections::HashMap;

use regex::Regex;
use tracing::warn;

use crate::models::Rule;

#[derive(Debug, Clone)]
pub struct PreparedRule {
    pub rule: Rule,
    /// `None` for rules containing `{cleanTitle}`; those are compiled per
    /// candidate title by the formatter.
    pub regex: Option<Regex>,
}

impl PreparedRule {
    pub fn contains_clean_title_placeholder(&self) -> bool {
        self.rule.regex.contains("{cleanTitle}")
    }
}

#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    by_token: HashMap<String, Vec<PreparedRule>>,
}

impl RuleSet {
    pub fn new(mut rules: Vec<Rule>, tokens: &[String]) -> Self {
        rules.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut by_token: HashMap<String, Vec<PreparedRule>> = HashMap::new();
        for rule in rules {
            if !tokens.iter().any(|token| token == &rule.token) {
                continue;
            }
            let regex = if rule.regex.contains("{cleanTitle}") {
                None
            } else {
                match Regex::new(&rule.regex) {
                    Ok(regex) => Some(regex),
                    Err(error) => {
                        warn!(rule_id = %rule.id, %error, "ignoring rule with invalid regex");
                        continue;
                    }
                }
            };
            by_token
                .entry(rule.token.clone())
                .or_default()
                .push(PreparedRule { rule, regex });
        }
        for rules in by_token.values_mut() {
            rules.sort_by(|left, right| {
                left.rule
                    .priority
                    .cmp(&right.rule.priority)
                    .then_with(|| left.rule.id.cmp(&right.rule.id))
            });
        }
        Self { by_token }
    }

    pub fn token_rules(&self, token: &str) -> &[PreparedRule] {
        self.by_token.get(token).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }
}

/// Java uses `Matcher.matches()`, which requires the entire string to match.
pub fn full_match(pattern: &str, text: &str) -> bool {
    match Regex::new(&format!("^(?:{pattern})$")) {
        Ok(regex) => regex.is_match(text),
        Err(error) => {
            warn!(%pattern, %error, "ignoring rule with invalid expanded regex");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, token: &str, priority: i64, regex: &str) -> Rule {
        Rule {
            id: id.to_owned(),
            token: token.to_owned(),
            priority,
            regex: regex.to_owned(),
            replacement: id.to_owned(),
            offset: 0,
        }
    }

    #[test]
    fn rules_are_sorted_by_priority_then_id() {
        let rules = vec![
            rule("b", "title", 20, "x"),
            rule("a", "title", 10, "x"),
            rule("c", "title", 10, "y"),
        ];
        let set = RuleSet::new(rules, &["title".to_owned()]);
        let ids: Vec<&str> = set
            .token_rules("title")
            .iter()
            .map(|prepared| prepared.rule.id.as_str())
            .collect();
        assert_eq!(ids, ["a", "c", "b"]);
    }

    #[test]
    fn only_tokens_in_format_are_loaded() {
        let rules = vec![rule("a", "title", 10, "x"), rule("b", "group", 10, "y")];
        let set = RuleSet::new(rules, &["title".to_owned()]);
        assert_eq!(set.token_rules("title").len(), 1);
        assert!(set.token_rules("group").is_empty());
    }

    #[test]
    fn dynamic_clean_title_rules_are_not_precompiled() {
        let rules = vec![rule("a", "title", 10, ".*{cleanTitle}.*")];
        let set = RuleSet::new(rules, &["title".to_owned()]);
        assert!(set.token_rules("title")[0].regex.is_none());
    }
}
