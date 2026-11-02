use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppKind {
    Sonarr,
    Radarr,
}

impl AppKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sonarr => "sonarr",
            Self::Radarr => "radarr",
        }
    }

    pub fn title_table(self) -> &'static str {
        match self {
            Self::Sonarr => "sonarr_title",
            Self::Radarr => "radarr_title",
        }
    }

    pub fn rule_table(self) -> &'static str {
        match self {
            Self::Sonarr => "sonarr_rule",
            Self::Radarr => "radarr_rule",
        }
    }
}

impl fmt::Display for AppKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Prowlarr,
    Jackett,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prowlarr => "prowlarr",
            Self::Jackett => "jackett",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: String,
    pub token: String,
    pub priority: i64,
    pub regex: String,
    pub replacement: String,
    pub offset: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchedTitle {
    pub id: i64,
    pub tvdb_id: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub sno: i64,
    pub main_title: String,
    /// The query-derived title. Java overwrites the persisted `title` with the
    /// title extracted from the incoming Torznab query before using it.
    pub title: String,
    pub clean_title: String,
    pub season_number: i64,
    pub year: Option<i64>,
    pub monitored: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TitleCandidate {
    pub main_title: String,
    pub title: String,
    pub clean_title: Option<String>,
    pub season_number: i64,
    pub year: Option<i64>,
    pub monitored: bool,
}
