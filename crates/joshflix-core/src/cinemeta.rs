//! Cinemeta: catalogs, search and metadata (keyless, IMDb IDs).

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::Result;
use crate::http::Http;
use crate::ids::{MediaKind, is_imdb_id};

const SERVICE: &str = "Cinemeta";
pub const DEFAULT_BASE: &str = "https://v3-cinemeta.strem.io";

/// Genres Cinemeta accepts for the `top` and `imdbRating` catalogs.
pub const GENRES: &[&str] = &[
    "Action",
    "Adventure",
    "Animation",
    "Biography",
    "Comedy",
    "Crime",
    "Documentary",
    "Drama",
    "Family",
    "Fantasy",
    "History",
    "Horror",
    "Mystery",
    "Romance",
    "Sci-Fi",
    "Sport",
    "Thriller",
    "War",
    "Western",
];

/// The catalogs Cinemeta serves for both movies and series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CatalogId {
    /// "Popular".
    Top,
    /// "New": requires a year as the genre.
    Year,
    /// "Featured" / top rated.
    ImdbRating,
}

impl CatalogId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Year => "year",
            Self::ImdbRating => "imdbRating",
        }
    }
}

/// A catalog page request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CatalogQuery {
    pub kind: MediaKind,
    pub catalog: CatalogId,
    pub genre: Option<String>,
    pub search: Option<String>,
    pub skip: u32,
}

impl CatalogQuery {
    pub fn new(kind: MediaKind, catalog: CatalogId) -> Self {
        Self {
            kind,
            catalog,
            genre: None,
            search: None,
            skip: 0,
        }
    }

    pub fn genre(mut self, genre: impl Into<String>) -> Self {
        self.genre = Some(genre.into());
        self
    }

    pub fn search(kind: MediaKind, query: impl Into<String>) -> Self {
        Self {
            kind,
            catalog: CatalogId::Top,
            genre: None,
            search: Some(query.into()),
            skip: 0,
        }
    }

    pub fn skip(mut self, skip: u32) -> Self {
        self.skip = skip;
        self
    }

    /// Path relative to the Cinemeta base, Stremio addon-protocol style.
    pub fn path(&self) -> String {
        let mut extras = Vec::new();
        if let Some(search) = self.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            extras.push(format!("search={}", encode_component(search)));
        }
        if let Some(genre) = &self.genre {
            extras.push(format!("genre={}", encode_component(genre)));
        }
        if self.skip > 0 {
            extras.push(format!("skip={}", self.skip));
        }
        let base = format!("/catalog/{}/{}", self.kind.as_str(), self.catalog.as_str());
        if extras.is_empty() {
            format!("{base}.json")
        } else {
            format!("{base}/{}.json", extras.join("&"))
        }
    }
}

/// A title as it appears in rows and grids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaPreview {
    pub id: String,
    pub kind: MediaKind,
    pub name: String,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub logo: Option<String>,
    pub description: Option<String>,
    /// "2024" or "2008–2013".
    pub release_info: Option<String>,
    pub imdb_rating: Option<String>,
    pub genres: Vec<String>,
    pub runtime: Option<String>,
}

/// One episode of a series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    /// Stremio video ID `tt123:1:2`.
    pub id: String,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    pub overview: Option<String>,
    pub thumbnail: Option<String>,
    /// RFC 3339 air date.
    pub released: Option<String>,
}

/// Full details for a title.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaDetail {
    pub preview: MetaPreview,
    pub cast: Vec<String>,
    pub director: Vec<String>,
    /// "Ended", "Continuing"…
    pub status: Option<String>,
    /// YouTube IDs.
    pub trailers: Vec<String>,
    /// Sorted by season then episode; season 0 (specials) last.
    pub episodes: Vec<Episode>,
}

impl MetaDetail {
    /// Seasons in display order (specials last).
    pub fn seasons(&self) -> Vec<u32> {
        let mut seasons: Vec<u32> = self.episodes.iter().map(|e| e.season).collect();
        seasons.dedup();
        seasons
    }

    pub fn episodes_in(&self, season: u32) -> impl Iterator<Item = &Episode> {
        self.episodes.iter().filter(move |e| e.season == season)
    }

    /// The episode after `(season, episode)` in display order, skipping specials.
    pub fn next_episode(&self, season: u32, episode: u32) -> Option<&Episode> {
        let index = self
            .episodes
            .iter()
            .position(|e| e.season == season && e.episode == episode)?;
        self.episodes[index + 1..].iter().find(|e| e.season != 0)
    }

    /// True when `(season, episode)` is the last episode of its season.
    pub fn is_season_finale(&self, season: u32, episode: u32) -> bool {
        self.episodes_in(season).map(|e| e.episode).max() == Some(episode)
    }

    /// True when `(season, episode)` is the last regular episode of the series.
    pub fn is_series_finale(&self, season: u32, episode: u32) -> bool {
        self.next_episode(season, episode).is_none()
    }
}

/// Cinemeta client.
#[derive(Clone)]
pub struct Cinemeta {
    http: Http,
    base: String,
}

impl Cinemeta {
    pub fn new(http: Http) -> Self {
        Self::with_base(http, DEFAULT_BASE)
    }

    pub fn with_base(http: Http, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    /// One catalog page (Cinemeta pages hold up to 50–100 items).
    pub async fn catalog(&self, query: &CatalogQuery) -> Result<Vec<MetaPreview>> {
        let url = format!("{}{}", self.base, query.path());
        let page: RawCatalog = self.http.get_json(SERVICE, &url).await?;
        let mut seen = std::collections::HashSet::new();
        Ok(page
            .metas
            .into_iter()
            .filter_map(|raw| raw.into_preview(query.kind))
            .filter(|meta| seen.insert(meta.id.clone()))
            .collect())
    }

    /// Search movies or series by title.
    pub async fn search(&self, kind: MediaKind, query: &str) -> Result<Vec<MetaPreview>> {
        self.catalog(&CatalogQuery::search(kind, query)).await
    }

    /// Full details, including episodes for series.
    pub async fn meta(&self, kind: MediaKind, id: &str) -> Result<MetaDetail> {
        let url = format!("{}/meta/{}/{}.json", self.base, kind.as_str(), encode_component(id));
        let raw: RawMetaResponse = self.http.get_json(SERVICE, &url).await?;
        let raw = raw
            .meta
            .ok_or_else(|| crate::CoreError::decode(SERVICE, "missing meta"))?;
        raw.into_detail(kind)
            .ok_or_else(|| crate::CoreError::decode(SERVICE, "meta without id or name"))
    }
}

/// Swap metahub image sizes (`small`/`medium`/`large`).
pub fn metahub_size(url: &str, size: &str) -> String {
    for from in ["/small/", "/medium/", "/large/"] {
        if url.contains("images.metahub.space") && url.contains(from) {
            return url.replacen(from, &format!("/{size}/"), 1);
        }
    }
    url.to_owned()
}

fn encode_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

// ---- raw wire types (lenient) ----

#[derive(Deserialize)]
struct RawCatalog {
    #[serde(default)]
    metas: Vec<RawMeta>,
}

#[derive(Deserialize)]
struct RawMetaResponse {
    meta: Option<RawMeta>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawMeta {
    id: Option<String>,
    imdb_id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    poster: Option<String>,
    background: Option<String>,
    logo: Option<String>,
    description: Option<String>,
    #[serde(rename = "releaseInfo", deserialize_with = "lenient_string")]
    release_info: Option<String>,
    #[serde(deserialize_with = "lenient_string")]
    year: Option<String>,
    #[serde(rename = "imdbRating", deserialize_with = "lenient_string")]
    imdb_rating: Option<String>,
    #[serde(deserialize_with = "lenient_strings")]
    genres: Vec<String>,
    #[serde(deserialize_with = "lenient_strings")]
    genre: Vec<String>,
    #[serde(deserialize_with = "lenient_string")]
    runtime: Option<String>,
    #[serde(deserialize_with = "lenient_strings")]
    cast: Vec<String>,
    #[serde(deserialize_with = "lenient_strings")]
    director: Vec<String>,
    status: Option<String>,
    trailers: Vec<RawTrailer>,
    videos: Vec<RawVideo>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTrailer {
    source: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawVideo {
    id: Option<String>,
    season: Option<u32>,
    episode: Option<u32>,
    number: Option<u32>,
    name: Option<String>,
    title: Option<String>,
    overview: Option<String>,
    description: Option<String>,
    thumbnail: Option<String>,
    released: Option<String>,
    #[serde(rename = "firstAired")]
    first_aired: Option<String>,
}

impl RawMeta {
    fn into_preview(self, fallback: MediaKind) -> Option<MetaPreview> {
        let id = self.id.or(self.imdb_id).filter(|id| !id.is_empty())?;
        let name = self.name.filter(|n| !n.trim().is_empty())?;
        let kind = self
            .kind
            .as_deref()
            .and_then(MediaKind::parse)
            .unwrap_or(fallback);
        let genres = if self.genres.is_empty() { self.genre } else { self.genres };
        Some(MetaPreview {
            poster: self.poster.filter(|p| !p.is_empty()).or_else(|| {
                is_imdb_id(&id).then(|| format!("https://images.metahub.space/poster/small/{id}/img"))
            }),
            background: self.background.filter(|b| !b.is_empty()),
            logo: self.logo.filter(|l| !l.is_empty()),
            description: self.description.filter(|d| !d.trim().is_empty()),
            release_info: self.release_info.or(self.year),
            imdb_rating: self.imdb_rating.filter(|r| !r.is_empty() && r != "0"),
            runtime: self.runtime.filter(|r| !r.is_empty()),
            genres,
            id,
            kind,
            name,
        })
    }

    fn into_detail(mut self, fallback: MediaKind) -> Option<MetaDetail> {
        let cast = std::mem::take(&mut self.cast);
        let director = std::mem::take(&mut self.director);
        let status = self.status.take();
        let trailers = std::mem::take(&mut self.trailers)
            .into_iter()
            .filter_map(|t| t.source)
            .collect();
        let videos = std::mem::take(&mut self.videos);
        let preview = self.into_preview(fallback)?;
        let mut episodes: Vec<Episode> = videos
            .into_iter()
            .filter_map(|v| {
                let season = v.season?;
                let episode = v.episode.or(v.number)?;
                Some(Episode {
                    id: v
                        .id
                        .unwrap_or_else(|| format!("{}:{season}:{episode}", preview.id)),
                    season,
                    episode,
                    title: v
                        .name
                        .or(v.title)
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| format!("Episode {episode}")),
                    overview: v.overview.or(v.description).filter(|o| !o.trim().is_empty()),
                    thumbnail: v.thumbnail.filter(|t| !t.is_empty()),
                    released: v.released.or(v.first_aired),
                })
            })
            .collect();
        episodes.sort_by_key(|e| (e.season == 0, e.season, e.episode));
        episodes.dedup_by(|a, b| a.season == b.season && a.episode == b.episode);
        Some(MetaDetail {
            preview,
            cast,
            director,
            status,
            trailers,
            episodes,
        })
    }
}

fn lenient_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn lenient_strings<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) if !s.trim().is_empty() => Some(s),
                _ => None,
            })
            .collect(),
        serde_json::Value::String(s) if !s.trim().is_empty() => vec![s],
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_catalog_paths() {
        assert_eq!(
            CatalogQuery::new(MediaKind::Movie, CatalogId::Top).path(),
            "/catalog/movie/top.json"
        );
        assert_eq!(
            CatalogQuery::new(MediaKind::Series, CatalogId::Top)
                .genre("Sci-Fi")
                .skip(100)
                .path(),
            "/catalog/series/top/genre=Sci-Fi&skip=100.json"
        );
        assert_eq!(
            CatalogQuery::search(MediaKind::Movie, "the office & co").path(),
            "/catalog/movie/top/search=the%20office%20%26%20co.json"
        );
    }

    #[test]
    fn parses_lenient_series_meta() {
        let json = r#"{"meta":{"id":"tt1","type":"series","name":"Show","imdbRating":8.5,
            "genre":["Drama"],"releaseInfo":"2008–2013",
            "videos":[
              {"id":"tt1:2:1","season":2,"number":1,"name":"B"},
              {"season":0,"episode":1,"name":"Special"},
              {"id":"tt1:1:2","season":1,"episode":2,"title":"A2"},
              {"id":"tt1:1:1","season":1,"episode":1,"name":""}
            ]}}"#;
        let raw: RawMetaResponse = serde_json::from_str(json).unwrap();
        let detail = raw.meta.unwrap().into_detail(MediaKind::Movie).unwrap();
        assert_eq!(detail.preview.kind, MediaKind::Series);
        assert_eq!(detail.preview.imdb_rating.as_deref(), Some("8.5"));
        assert_eq!(detail.preview.genres, vec!["Drama"]);
        assert_eq!(detail.seasons(), vec![1, 2, 0]);
        assert_eq!(detail.episodes[0].title, "Episode 1");
        assert_eq!(detail.next_episode(1, 2).unwrap().id, "tt1:2:1");
        assert!(detail.next_episode(2, 1).is_none());
        assert!(detail.is_season_finale(1, 2));
        assert!(detail.is_series_finale(2, 1));
        assert!(detail.preview.poster.unwrap().contains("tt1"));
    }

    #[test]
    fn swaps_metahub_sizes() {
        assert_eq!(
            metahub_size("https://images.metahub.space/poster/small/tt1/img", "medium"),
            "https://images.metahub.space/poster/medium/tt1/img"
        );
        assert_eq!(metahub_size("https://x/y.jpg", "large"), "https://x/y.jpg");
    }
}
