//! Identifiers for titles and playable videos.

use serde::{Deserialize, Serialize};

/// Movie or series. Serialized with Stremio's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Movie,
    Series,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "movie" => Some(Self::Movie),
            "series" => Some(Self::Series),
            _ => None,
        }
    }
}

/// One playable thing: a movie, or one episode of a series.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VideoRef {
    pub kind: MediaKind,
    /// IMDb ID of the title, e.g. `tt0903747`.
    pub meta_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
}

impl VideoRef {
    pub fn movie(meta_id: impl Into<String>) -> Self {
        Self {
            kind: MediaKind::Movie,
            meta_id: meta_id.into(),
            season: None,
            episode: None,
        }
    }

    pub fn episode(meta_id: impl Into<String>, season: u32, episode: u32) -> Self {
        Self {
            kind: MediaKind::Series,
            meta_id: meta_id.into(),
            season: Some(season),
            episode: Some(episode),
        }
    }

    /// Stremio video ID: `tt123` for movies, `tt123:1:2` for episodes.
    pub fn video_id(&self) -> String {
        match (self.season, self.episode) {
            (Some(s), Some(e)) => format!("{}:{s}:{e}", self.meta_id),
            _ => self.meta_id.clone(),
        }
    }

    /// Parse a Stremio video ID back into a reference.
    pub fn from_video_id(kind: MediaKind, video_id: &str) -> Option<Self> {
        let mut parts = video_id.split(':');
        let meta_id = parts.next().filter(|id| !id.is_empty())?.to_owned();
        match (parts.next(), parts.next(), parts.next()) {
            (None, _, _) => Some(Self {
                kind,
                meta_id,
                season: None,
                episode: None,
            }),
            (Some(s), Some(e), None) => Some(Self {
                kind,
                meta_id,
                season: Some(s.parse().ok()?),
                episode: Some(e.parse().ok()?),
            }),
            _ => None,
        }
    }

    /// `S03E04` style label, or `None` for movies.
    pub fn episode_label(&self) -> Option<String> {
        Some(format!("S{:02}E{:02}", self.season?, self.episode?))
    }
}

/// True for IMDb IDs (`tt` followed by digits).
pub fn is_imdb_id(id: &str) -> bool {
    id.len() > 2 && id.starts_with("tt") && id[2..].bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_video_ids() {
        let ep = VideoRef::episode("tt0903747", 3, 4);
        assert_eq!(ep.video_id(), "tt0903747:3:4");
        assert_eq!(VideoRef::from_video_id(MediaKind::Series, "tt0903747:3:4"), Some(ep));
        let movie = VideoRef::movie("tt15239678");
        assert_eq!(VideoRef::from_video_id(MediaKind::Movie, "tt15239678"), Some(movie));
        assert_eq!(VideoRef::from_video_id(MediaKind::Series, "tt1:x:2"), None);
        assert_eq!(VideoRef::episode("tt1", 3, 4).episode_label().as_deref(), Some("S03E04"));
        assert!(is_imdb_id("tt0903747"));
        assert!(!is_imdb_id("kitsu:1"));
    }
}
