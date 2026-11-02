use url::form_urlencoded;

use crate::models::{AppKind, Provider};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryParams {
    pairs: Vec<(String, String)>,
}

impl QueryParams {
    pub fn parse(raw: &str) -> Self {
        Self {
            pairs: form_urlencoded::parse(raw.trim_start_matches('?').as_bytes())
                .into_owned()
                .collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        let mut found = false;
        self.pairs.retain_mut(|(name, current)| {
            if name == key {
                if found {
                    return false;
                }
                found = true;
                *current = value.clone();
            }
            true
        });
        if !found {
            self.pairs.push((key.to_owned(), value));
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.pairs.retain(|(name, _)| name != key);
    }

    pub fn remove_any(&mut self, keys: &[&str]) {
        self.pairs
            .retain(|(name, _)| !keys.iter().any(|key| name == key));
    }

    pub fn to_query_string(&self) -> String {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(self.pairs.iter().map(|(key, value)| (key, value)))
            .finish()
    }

    /// Stable cache identity that never contains the Torznab API key.
    pub fn cache_key(&self, kind: AppKind, provider: Provider, indexer_id: &str) -> String {
        let mut pairs: Vec<(String, String)> = self
            .pairs
            .iter()
            .filter(|(key, _)| !key.eq_ignore_ascii_case("apikey"))
            .cloned()
            .collect();
        pairs.sort();
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        format!("{kind}/{provider}/{indexer_id}?{query}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_all_duplicates_with_one_value() {
        let mut params = QueryParams::parse("q=one&q=two&limit=10");
        assert_eq!(params.get("q"), Some("one"));
        params.set("q", "three");
        assert_eq!(params.to_query_string(), "q=three&limit=10");
    }

    #[test]
    fn cache_key_removes_api_key_and_is_order_independent() {
        let first = QueryParams::parse("q=Show&apikey=secret&t=search");
        let second = QueryParams::parse("t=search&q=Show");
        assert_eq!(
            first.cache_key(AppKind::Sonarr, Provider::Prowlarr, "1"),
            second.cache_key(AppKind::Sonarr, Provider::Prowlarr, "1")
        );
        assert!(
            !first
                .cache_key(AppKind::Sonarr, Provider::Prowlarr, "1")
                .contains("secret")
        );
    }

    #[test]
    fn season_and_episode_are_removed_only_when_requested() {
        let mut params = QueryParams::parse("q=Show&season=1&ep=2&year=2024");
        params.remove_any(&["season", "ep"]);
        assert_eq!(params.to_query_string(), "q=Show&year=2024");
    }
}
