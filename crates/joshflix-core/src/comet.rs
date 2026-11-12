//! Comet (ElfHosted's public instance): Real-Debrid-aware source search.
//!
//! The user's Real-Debrid key travels in Comet's config segment, the same way
//! Jedflix TV uses it. Comet checks which torrents are already cached on
//! Real-Debrid (⚡) and returns a playback URL per source that 302-redirects to
//! the direct Real-Debrid stream. Uncached sources (⬇️) are resolved on the TV
//! with [`crate::realdebrid`] so the user sees live download progress.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::http::Http;
use crate::ids::VideoRef;
use crate::release::ReleaseInfo;
use crate::sources::Source;

const SERVICE: &str = "Comet";
pub const DEFAULT_BASE: &str = "https://comet.elfhosted.com";

/// Sources plus any notices Comet attached (bad key, maintenance…).
#[derive(Debug, Clone, Default)]
pub struct CometResult {
    pub sources: Vec<Source>,
    /// Human-readable problems Comet reported instead of sources.
    pub notices: Vec<String>,
}

/// Comet client bound to one Real-Debrid key.
#[derive(Clone)]
pub struct Comet {
    http: Http,
    base: String,
    config: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CometConfig<'a> {
    debrid_service: &'a str,
    debrid_api_key: &'a str,
    cached_only: bool,
    remove_trash: bool,
    result_format: [&'a str; 1],
    max_results_per_resolution: u32,
    max_size: f32,
    deduplicate_streams: bool,
}

impl Comet {
    pub fn new(http: Http, rd_api_key: &str) -> Self {
        Self::with_base(http, DEFAULT_BASE, rd_api_key)
    }

    pub fn with_base(http: Http, base: impl Into<String>, rd_api_key: &str) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            config: encode_config(rd_api_key.trim()),
        }
    }

    /// Ranked-by-Comet sources for a movie or episode (Joshflix re-ranks them).
    pub async fn streams(&self, video: &VideoRef) -> Result<CometResult> {
        let url = format!(
            "{}/{}/stream/{}/{}.json",
            self.base,
            self.config,
            video.kind.as_str(),
            video.video_id()
        );
        let response: RawResponse = self.http.get_json(SERVICE, &url).await?;
        let mut result = CometResult::default();
        // Decode each stream on its own: one odd entry must not hide the rest.
        for raw in response.streams.into_iter().filter_map(|value| serde_json::from_value::<RawStream>(value).ok()) {
            match raw.into_source() {
                Parsed::Source(source) => result.sources.push(*source),
                Parsed::Notice(notice) => result.notices.push(notice),
                Parsed::Skip => {}
            }
        }
        Ok(result)
    }

    /// Follow a Comet playback URL to the direct Real-Debrid stream without
    /// downloading it. Comet answers errors with a short status video instead
    /// of a redirect; that becomes an error here.
    pub async fn resolve_playback(&self, playback_url: &str) -> Result<String> {
        let response = self
            .http
            .no_redirect()
            .get(playback_url)
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| CoreError::decode(SERVICE, "redirect without location"))?;
            let resolved = response
                .url()
                .join(location)
                .map_err(|e| CoreError::decode(SERVICE, e))?;
            return Ok(resolved.to_string());
        }
        Err(CoreError::Invalid(if status.is_success() {
            "Comet couldn't get this file from Real-Debrid.".to_owned()
        } else {
            format!("Comet returned an error ({status}).")
        }))
    }
}

/// The config path segment Comet URLs carry for `rd_api_key`. It embeds the
/// key, so logs must treat it as a secret.
pub fn config_segment(rd_api_key: &str) -> String {
    encode_config(rd_api_key.trim())
}

/// Standard-base64 config that's safe in a URL path segment. Comet decodes
/// standard base64, so URL-safe alphabets don't work, and `/` would split the
/// path. In ASCII JSON a `/` can only come from a `?` (0x3F) landing on the last
/// byte of a 3-byte group, so shifting the text with leading spaces removes it.
/// (`+` is legal in paths.)
fn encode_config(rd_api_key: &str) -> String {
    let config = CometConfig {
        debrid_service: "realdebrid",
        debrid_api_key: rd_api_key,
        cached_only: false,
        remove_trash: true,
        result_format: ["all"],
        max_results_per_resolution: 0,
        max_size: 0.0,
        deduplicate_streams: true,
    };
    let json = serde_json::to_string(&config).unwrap_or_default();
    let engine = base64::engine::general_purpose::STANDARD;
    for pad in 0..3 {
        let encoded = engine.encode(format!("{}{json}", " ".repeat(pad)));
        if !encoded.contains('/') {
            return encoded;
        }
    }
    // Several `?` in the key at clashing offsets: percent-encode as a last resort.
    engine.encode(json).replace('/', "%2F")
}

enum Parsed {
    Source(Box<Source>),
    Notice(String),
    Skip,
}

#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    streams: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawStream {
    name: String,
    description: String,
    title: String,
    url: Option<String>,
    #[serde(rename = "infoHash")]
    info_hash: Option<String>,
    #[serde(rename = "fileIdx")]
    file_idx: Option<u32>,
    #[serde(rename = "behaviorHints")]
    behavior_hints: RawHints,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawHints {
    #[serde(rename = "bingeGroup")]
    binge_group: Option<String>,
    filename: Option<String>,
    /// Comet sends -1 when it doesn't know the size.
    #[serde(rename = "videoSize")]
    video_size: Option<i64>,
}

impl RawStream {
    fn into_source(self) -> Parsed {
        let description = if self.description.is_empty() { self.title } else { self.description };
        // Notices look like "[❌] realdebrid" or "[⛔️] Comet" with no torrent behind them.
        if self.name.starts_with("[❌") || self.name.starts_with("[⛔") || self.name.starts_with("[⚠") {
            let message = description.lines().next().unwrap_or("Comet reported a problem.").trim();
            return Parsed::Notice(message.to_owned());
        }
        let url = self.url.filter(|u| !u.is_empty());
        // Comet has moved the hash around between versions: try every place it
        // has lived and keep the first that really is one. Current ElfHosted
        // playback URLs are `/playback/{config}/{encrypted}`, with no hash.
        let binge_hash = self.behavior_hints.binge_group.as_deref().and_then(|g| g.rsplit('|').next());
        let Some(info_hash) = [self.info_hash.as_deref(), binge_hash, url.as_deref().and_then(hash_from_playback_url)]
            .into_iter()
            .flatten()
            .find(|h| is_info_hash(h))
            .map(str::to_ascii_lowercase)
        else {
            return Parsed::Skip;
        };
        let cached = self.name.contains('⚡');
        let file_idx = self
            .file_idx
            .or_else(|| url.as_deref().and_then(file_index_from_playback_url));
        let release = ReleaseInfo::parse_comet(
            &self.name,
            &description,
            self.behavior_hints.filename.as_deref(),
            self.behavior_hints.video_size.and_then(|size| u64::try_from(size).ok()).filter(|&size| size > 0),
        );
        Parsed::Source(Box::new(Source {
            info_hash,
            file_idx,
            filename: self.behavior_hints.filename,
            binge_group: self.behavior_hints.binge_group,
            trackers: Vec::new(),
            cached,
            playback_url: url,
            release,
        }))
    }
}

/// `…/playback/{hash}/{entry}/{fileIndex|n}/{season}/{episode}?…`
fn playback_segments(url: &str) -> Option<Vec<&str>> {
    let path = url.split('?').next()?;
    let (_, rest) = path.split_once("/playback/")?;
    Some(rest.split('/').collect())
}

fn is_info_hash(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Only the older `/playback/{hash}/…` layout carries the hash.
fn hash_from_playback_url(url: &str) -> Option<&str> {
    playback_segments(url)?.first().copied().filter(|s| is_info_hash(s))
}

/// The file index only exists in the older layout, after the hash.
fn file_index_from_playback_url(url: &str) -> Option<u32> {
    let segments = playback_segments(url)?;
    if !is_info_hash(segments.first()?) {
        return None;
    }
    segments.get(2)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;
    use crate::ids::MediaKind;

    #[test]
    fn config_is_path_safe_and_decodes() {
        for key in ["ABC", "Z".repeat(52).as_str(), "a?b>c~d", "?x"] {
            let encoded = encode_config(key);
            assert!(!encoded.contains('/'), "{encoded}");
            let decoded = base64::engine::general_purpose::STANDARD.decode(&encoded).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
            assert_eq!(value["debridService"], "realdebrid");
            assert_eq!(value["debridApiKey"], key);
        }
    }

    const SAMPLE: &str = r#"{"streams":[
      {"name":"[RD⚡] Comet 2160p","description":"📄 Dune Part Two 2024 2160p UHD BluRay DV REMUX HDR10 HEVC TrueHD 7 1 Atmos-UnKn0wn.mkv\n📹 hevc • DV • HDR • 10bit | 🔊 Atmos • TrueHD • 7.1\n⭐ BluRay REMUX | 🏷️ UnKn0wn\n💾 63.2 GB 🔎 StremThru","behaviorHints":{"bingeGroup":"comet|realdebrid|cc404febe5fb876f1970acd4e64db3ac1d01fc90","filename":"Dune.mkv","videoSize":67907452799},"url":"https://comet.example/CFG/playback/cc404febe5fb876f1970acd4e64db3ac1d01fc90/0/3/n/n?torrent_name=x"},
      {"name":"[RD⬇️] Comet 1080p","description":"📄 Dune.Part.Two.2024.1080p.WEB-DL.x264-FLUX\n👤 900 💾 6 GB 🔎 TorrentGalaxy","behaviorHints":{"bingeGroup":"comet|realdebrid|1111111111111111111111111111111111111111"},"url":"https://comet.example/CFG/playback/1111111111111111111111111111111111111111/0/n/n/n"},
      {"name":"[❌] realdebrid","description":"realdebrid: Invalid API key.\nPlease check your configuration.","url":"https://comet.example/playback/xyz"}
    ]}"#;

    #[test]
    fn parses_sources_and_notices() {
        let raw: RawResponse = serde_json::from_str(SAMPLE).unwrap();
        let parsed: Vec<Parsed> = raw.streams.into_iter().map(|v| serde_json::from_value::<RawStream>(v).unwrap().into_source()).collect();
        let Parsed::Source(cached) = &parsed[0] else { panic!("expected source") };
        assert!(cached.cached);
        assert_eq!(cached.info_hash, "cc404febe5fb876f1970acd4e64db3ac1d01fc90");
        assert_eq!(cached.file_idx, Some(3));
        assert_eq!(cached.release.resolution, Some(crate::release::Resolution::R2160));
        assert_eq!(cached.release.size_bytes, Some(63_200_000_000));
        let Parsed::Source(uncached) = &parsed[1] else { panic!("expected source") };
        assert!(!uncached.cached);
        assert_eq!(uncached.file_idx, None);
        assert_eq!(uncached.release.seeders, Some(900));
        let Parsed::Notice(notice) = &parsed[2] else { panic!("expected notice") };
        assert_eq!(notice, "realdebrid: Invalid API key.");
    }

    /// Current ElfHosted layout: `/playback/{config}/{encrypted…}` with the
    /// hash only in `bingeGroup`. Every source used to be dropped here.
    #[test]
    fn parses_current_elfhosted_playback_urls() {
        let raw: RawResponse = serde_json::from_str(r#"{"streams":[
          {"name":"[RD⚡] Comet 2160p","description":"📄 The.Shawshank.Redemption.1994.2160p.UHD.Remux.DV.HDR.HEVC.DTS-HD.MA.5.1-OzONE.mkv\n👤 1 💾 53.8 GB 🔎 TheRARBG",
           "behaviorHints":{"bingeGroup":"comet|realdebrid|58B0E410B2588CFC4C5298F59C12EC93A8A123CE","filename":"Shawshank.mkv","videoSize":-1},
           "url":"https://comet.example/playback/T_RNsdrmkDAqAOKm/BXLgdBhGP-W2kMgd/12345/sgzo6oTH6w0qWSdO"}
        ]}"#).unwrap();
        let stream: RawStream = serde_json::from_value(raw.streams.into_iter().next().unwrap()).unwrap();
        let Parsed::Source(source) = stream.into_source() else { panic!("expected source") };
        assert_eq!(source.info_hash, "58b0e410b2588cfc4c5298f59c12ec93a8a123ce");
        assert_eq!(source.file_idx, None, "no file index in the encrypted layout");
        assert!(source.cached);
        assert!(source.playback_url.is_some());
    }

    async fn fake_streams() -> impl IntoResponse {
        ([(header::CONTENT_TYPE, "application/json")], SAMPLE)
    }

    async fn fake_redirect() -> impl IntoResponse {
        (StatusCode::FOUND, [(header::LOCATION, "https://download.real-debrid.com/d/ABC/Dune.mkv")])
    }

    async fn fake_status_video() -> impl IntoResponse {
        ([(header::CONTENT_TYPE, "video/mp4")], vec![0u8; 16])
    }

    #[tokio::test]
    async fn fetches_and_resolves() {
        let app = Router::new()
            .route("/{cfg}/stream/movie/{id}", get(fake_streams))
            .route("/ok/playback/{hash}", get(fake_redirect))
            .route("/bad/playback/{hash}", get(fake_status_video));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");
        let comet = Comet::with_base(Http::new().unwrap(), &base, "KEY");

        let result = comet.streams(&VideoRef::movie("tt15239678")).await.unwrap();
        assert_eq!(result.sources.len(), 2);
        assert_eq!(result.notices.len(), 1);

        let direct = comet.resolve_playback(&format!("{base}/ok/playback/abc")).await.unwrap();
        assert_eq!(direct, "https://download.real-debrid.com/d/ABC/Dune.mkv");
        assert!(comet.resolve_playback(&format!("{base}/bad/playback/abc")).await.is_err());
        let _ = MediaKind::Movie;
    }
}
