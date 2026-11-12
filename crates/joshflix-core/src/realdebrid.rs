//! Real-Debrid: account checks and torrent → direct stream resolution.
//!
//! Resolution follows Jedflix: add the magnet, select the one playable file,
//! wait until Real-Debrid has it (instant when cached, with live progress when
//! not), then unrestrict the link into a direct HTTPS URL the player streams.
//! The key never leaves the TV except to `api.real-debrid.com`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

use crate::error::{CoreError, Result};
use crate::http::Http;
use crate::ids::VideoRef;
use crate::sources::Source;

const SERVICE: &str = "Real-Debrid";
pub const DEFAULT_BASE: &str = "https://api.real-debrid.com/rest/1.0";
/// Where users create or copy their key.
pub const API_TOKEN_URL: &str = "real-debrid.com/apitoken";
/// Where users watch slow downloads.
pub const TORRENTS_URL: &str = "real-debrid.com/torrents";

/// A Real-Debrid API error with a message safe for the screen.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct RdError {
    pub kind: RdErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdErrorKind {
    BadKey,
    NotPremium,
    Blocked,
    Busy,
    TrafficExhausted,
    TorrentFailed,
    NoPlayableFile,
    Other,
}

impl RdError {
    fn new(kind: RdErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Map Real-Debrid's numeric `error_code` (see api.real-debrid.com).
    fn from_api(status: u16, code: Option<i64>, text: Option<&str>) -> Self {
        use RdErrorKind::*;
        match (status, code) {
            (_, Some(8 | 9 | 12 | 13 | 14)) | (401, _) => Self::new(
                BadKey,
                "Real-Debrid rejected the API key. Add it again in Settings → Real-Debrid.",
            ),
            (_, Some(20 | 24)) => Self::new(Blocked, "Real-Debrid can't serve this file. Pick another source."),
            (_, Some(35)) => Self::new(
                Blocked,
                "Real-Debrid blocked this file (infringing file). Pick another source.",
            ),
            (_, Some(21 | 34)) | (429, _) => Self::new(
                Busy,
                "Real-Debrid is busy (too many active downloads or requests). Try again in a moment.",
            ),
            (_, Some(36)) => Self::new(TrafficExhausted, "Your Real-Debrid traffic is used up for today."),
            (403, _) | (_, Some(10 | 11)) => Self::new(
                NotPremium,
                "Your Real-Debrid account needs an active premium subscription.",
            ),
            _ => Self::new(
                Other,
                format!(
                    "Real-Debrid error{}{}",
                    text.map(|t| format!(": {t}")).unwrap_or_default(),
                    code.map(|c| format!(" ({c})")).unwrap_or_default()
                ),
            ),
        }
    }
}

/// The signed-in Real-Debrid account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdUser {
    pub username: String,
    /// "premium" or "free".
    #[serde(rename = "type")]
    pub account_type: String,
    /// RFC 3339 premium expiry.
    #[serde(default)]
    pub expiration: Option<String>,
    /// Seconds of premium left.
    #[serde(default)]
    pub premium: i64,
}

impl RdUser {
    pub fn is_premium(&self) -> bool {
        self.account_type == "premium" && self.premium > 0
    }

    pub fn premium_days_left(&self) -> i64 {
        (self.premium.max(0) + 86_399) / 86_400
    }
}

/// Live state of a resolve, for the "Getting your stream" screen.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveProgress {
    Adding,
    /// Waiting for Real-Debrid to read the torrent's file list.
    ReadingFiles,
    /// Real-Debrid is downloading (not cached). `percent` 0–100, `speed` bytes/s.
    Downloading {
        percent: f32,
        speed: Option<u64>,
        seeders: Option<u32>,
        status: String,
    },
    Unrestricting,
}

/// A playable direct stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedStream {
    pub url: String,
    pub filename: String,
    pub size: u64,
    pub torrent_id: String,
}

/// Real-Debrid client for one API key.
#[derive(Clone)]
pub struct RealDebrid {
    http: Http,
    base: String,
    token: String,
}

impl RealDebrid {
    pub fn new(http: Http, token: impl Into<String>) -> Self {
        Self::with_base(http, DEFAULT_BASE, token)
    }

    pub fn with_base(http: Http, base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            token: token.into().trim().to_owned(),
        }
    }

    /// Check the key and fetch the account.
    pub async fn user(&self) -> Result<RdUser> {
        self.get("/user").await
    }

    /// Resolve a torrent source into a direct stream, reporting progress.
    ///
    /// Returns [`CoreError::Cancelled`] when `cancel` flips to `true`.
    pub async fn resolve(
        &self,
        stream: &Source,
        video: &VideoRef,
        progress: &(dyn Fn(ResolveProgress) + Send + Sync),
        cancel: watch::Receiver<bool>,
    ) -> Result<ResolvedStream> {
        progress(ResolveProgress::Adding);
        let torrent_id = match self.find_existing(&stream.info_hash).await? {
            Some(id) => id,
            None => {
                let added: AddedTorrent = self
                    .post_form("/torrents/addMagnet", &[("magnet", stream.magnet())])
                    .await?;
                added.id
            }
        };

        progress(ResolveProgress::ReadingFiles);
        let mut info = self.torrent_info(&torrent_id).await?;
        let mut waited = 0;
        while info.status == "magnet_conversion" || (info.files.is_empty() && info.status != "waiting_files_selection") {
            check_cancel(&cancel)?;
            if is_failed(&info.status) {
                return Err(failed(&info.status));
            }
            if waited >= 60 {
                return Err(RdError::new(
                    RdErrorKind::TorrentFailed,
                    "Real-Debrid couldn't read this torrent's files. Pick another source.",
                )
                .into());
            }
            sleep_or_cancel(Duration::from_secs(1), &cancel).await?;
            waited += 1;
            info = self.torrent_info(&torrent_id).await?;
        }

        let file = pick_file(&info.files, stream, video).ok_or_else(|| {
            RdError::new(
                RdErrorKind::NoPlayableFile,
                "This source has no playable video for this title. Pick another source.",
            )
        })?;

        let already_selected = info.files.iter().any(|f| f.selected == 1);
        if info.status == "waiting_files_selection" || !already_selected {
            self.post_empty(
                &format!("/torrents/selectFiles/{torrent_id}"),
                &[("files", file.id.to_string())],
            )
            .await?;
        } else if !info.files.iter().any(|f| f.id == file.id && f.selected == 1) {
            // An earlier selection in the user's account doesn't include our file: start fresh.
            let added: AddedTorrent = self
                .post_form("/torrents/addMagnet", &[("magnet", stream.magnet())])
                .await?;
            return Box::pin(self.resolve_added(&added.id, file.id, progress, cancel)).await;
        }
        self.resolve_added(&torrent_id, file.id, progress, cancel).await
    }

    async fn resolve_added(
        &self,
        torrent_id: &str,
        file_id: u32,
        progress: &(dyn Fn(ResolveProgress) + Send + Sync),
        cancel: watch::Receiver<bool>,
    ) -> Result<ResolvedStream> {
        let mut info = self.torrent_info(torrent_id).await?;
        if info.status == "waiting_files_selection" {
            self.post_empty(
                &format!("/torrents/selectFiles/{torrent_id}"),
                &[("files", file_id.to_string())],
            )
            .await?;
            info = self.torrent_info(torrent_id).await?;
        }
        loop {
            check_cancel(&cancel)?;
            if info.status == "downloaded" && !info.links.is_empty() {
                break;
            }
            if is_failed(&info.status) {
                return Err(failed(&info.status));
            }
            progress(ResolveProgress::Downloading {
                percent: info.progress.clamp(0.0, 100.0),
                speed: info.speed,
                seeders: info.seeders,
                status: info.status.clone(),
            });
            sleep_or_cancel(Duration::from_millis(1500), &cancel).await?;
            info = self.torrent_info(torrent_id).await?;
        }

        // `links` follows the order of selected files.
        let selected: Vec<&RdFile> = info.files.iter().filter(|f| f.selected == 1).collect();
        let link_index = selected.iter().position(|f| f.id == file_id).unwrap_or(0);
        let link = info
            .links
            .get(link_index)
            .or_else(|| info.links.first())
            .cloned()
            .ok_or_else(|| RdError::new(RdErrorKind::TorrentFailed, "Real-Debrid returned no link."))?;

        progress(ResolveProgress::Unrestricting);
        let unrestricted: Unrestricted = self.post_form("/unrestrict/link", &[("link", link)]).await?;
        Ok(ResolvedStream {
            url: unrestricted.download,
            filename: unrestricted.filename,
            size: unrestricted.filesize,
            torrent_id: torrent_id.to_owned(),
        })
    }

    async fn find_existing(&self, info_hash: &str) -> Result<Option<String>> {
        let torrents: Vec<TorrentListItem> = self.get("/torrents?limit=100").await.unwrap_or_default();
        Ok(torrents
            .into_iter()
            .find(|t| t.hash.eq_ignore_ascii_case(info_hash) && !is_failed(&t.status))
            .map(|t| t.id))
    }

    async fn torrent_info(&self, id: &str) -> Result<TorrentInfo> {
        self.get(&format!("/torrents/info/{id}")).await
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .client()
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        read(response).await
    }

    async fn post_form<T: for<'de> Deserialize<'de>>(&self, path: &str, form: &[(&str, String)]) -> Result<T> {
        let response = self
            .http
            .client()
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .form(form)
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        read(response).await
    }

    async fn post_empty(&self, path: &str, form: &[(&str, String)]) -> Result<()> {
        let response = self
            .http
            .client()
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .form(form)
            .send()
            .await
            .map_err(|source| CoreError::Network { service: SERVICE, source })?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(api_error(response).await)
        }
    }
}

async fn read<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T> {
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|source| CoreError::Network { service: SERVICE, source })?;
    serde_json::from_slice(&bytes).map_err(|e| CoreError::decode(SERVICE, e))
}

async fn api_error(response: reqwest::Response) -> CoreError {
    let status = response.status().as_u16();
    let body: Option<ApiErrorBody> = response.json().await.ok();
    RdError::from_api(
        status,
        body.as_ref().and_then(|b| b.error_code),
        body.as_ref().and_then(|b| b.error.as_deref()),
    )
    .into()
}

fn check_cancel(cancel: &watch::Receiver<bool>) -> Result<()> {
    if *cancel.borrow() { Err(CoreError::Cancelled) } else { Ok(()) }
}

async fn sleep_or_cancel(duration: Duration, cancel: &watch::Receiver<bool>) -> Result<()> {
    let mut cancel = cancel.clone();
    tokio::select! {
        () = tokio::time::sleep(duration) => Ok(()),
        _ = cancel.wait_for(|c| *c) => Err(CoreError::Cancelled),
    }
}

fn is_failed(status: &str) -> bool {
    matches!(status, "error" | "magnet_error" | "virus" | "dead")
}

fn failed(status: &str) -> CoreError {
    let message = match status {
        "virus" => "Real-Debrid flagged this torrent as a virus. Pick another source.",
        "dead" => "This torrent is dead on Real-Debrid. Pick another source.",
        _ => "Real-Debrid couldn't download this torrent. Pick another source.",
    };
    RdError::new(RdErrorKind::TorrentFailed, message).into()
}

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "m4v", "mov", "wmv", "ts", "m2ts", "webm", "mpg", "mpeg"];

fn is_video(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    VIDEO_EXTENSIONS.iter().any(|ext| lower.ends_with(&format!(".{ext}")))
        && !lower.contains("sample")
        && !lower.contains("trailer")
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Choose the file to play: Torrentio's file index or name, then the episode
/// pattern, then the largest video.
fn pick_file<'a>(files: &'a [RdFile], stream: &Source, video: &VideoRef) -> Option<&'a RdFile> {
    let videos: Vec<&RdFile> = files.iter().filter(|f| is_video(&f.path)).collect();
    if videos.is_empty() {
        return None;
    }
    if let Some(name) = stream.filename.as_deref()
        && let Some(file) = videos.iter().find(|f| basename(&f.path) == name)
    {
        return Some(file);
    }
    // Real-Debrid file IDs are 1-based in torrent order, like Torrentio's fileIdx + 1.
    if let Some(idx) = stream.file_idx
        && let Some(file) = videos.iter().find(|f| f.id == idx + 1)
    {
        let matches_episode = match (video.season, video.episode) {
            (Some(s), Some(e)) => matches_episode(&file.path, s, e),
            _ => true,
        };
        if matches_episode {
            return Some(file);
        }
    }
    if let (Some(season), Some(episode)) = (video.season, video.episode) {
        return videos
            .iter()
            .filter(|f| matches_episode(&f.path, season, episode))
            .max_by_key(|f| f.bytes)
            .copied();
    }
    videos.into_iter().max_by_key(|f| f.bytes)
}

/// True when the path names `season`/`episode` (S03E04, 3x04, "Season 3 … Episode 04").
pub fn matches_episode(path: &str, season: u32, episode: u32) -> bool {
    let lower = path.to_ascii_lowercase();
    let patterns = [
        format!("s{season:02}e{episode:02}"),
        format!("s{season}e{episode:02}"),
        format!("{season}x{episode:02}"),
        format!("s{season:02}.e{episode:02}"),
        format!("s{season:02} e{episode:02}"),
    ];
    if patterns.iter().any(|p| {
        lower.match_indices(p.as_str()).any(|(i, _)| {
            // Make sure "s01e01" doesn't match "s01e010".
            !lower[i + p.len()..].starts_with(|c: char| c.is_ascii_digit())
        })
    }) {
        return true;
    }
    let base = basename(&lower);
    let has_season = lower.contains(&format!("season {season}")) || lower.contains(&format!("season {season:02}"));
    has_season
        && (base.contains(&format!("episode {episode:02}"))
            || base.contains(&format!("episode {episode} "))
            || base.contains(&format!("e{episode:02}")))
}

// ---- wire types ----

#[derive(Deserialize)]
struct ApiErrorBody {
    error: Option<String>,
    error_code: Option<i64>,
}

#[derive(Deserialize)]
struct AddedTorrent {
    id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TorrentListItem {
    id: String,
    hash: String,
    status: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct TorrentInfo {
    status: String,
    progress: f32,
    speed: Option<u64>,
    seeders: Option<u32>,
    files: Vec<RdFile>,
    links: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct RdFile {
    id: u32,
    path: String,
    bytes: u64,
    selected: u8,
}

#[derive(Deserialize)]
struct Unrestricted {
    download: String,
    #[serde(default)]
    filename: String,
    #[serde(default)]
    filesize: u64,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};
    use serde_json::{Value, json};

    use super::*;
    use crate::release::ReleaseInfo;

    #[derive(Default)]
    struct Fake {
        polls: u32,
        selected: Option<String>,
    }

    type Shared = Arc<Mutex<Fake>>;

    fn authorized(headers: &HeaderMap) -> bool {
        headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer GOODKEY")
    }

    async fn user(headers: HeaderMap) -> (StatusCode, Json<Value>) {
        if authorized(&headers) {
            (StatusCode::OK, Json(json!({"username":"josh","type":"premium","premium":864000,"expiration":"2027-01-01T00:00:00.000Z"})))
        } else {
            (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad_token","error_code":8})))
        }
    }

    async fn list() -> Json<Value> {
        Json(json!([]))
    }

    async fn add(Form(form): Form<Vec<(String, String)>>) -> (StatusCode, Json<Value>) {
        assert!(form[0].1.starts_with("magnet:?xt=urn:btih:"));
        (StatusCode::CREATED, Json(json!({"id":"T1","uri":"x"})))
    }

    async fn info(State(state): State<Shared>, Path(id): Path<String>) -> Json<Value> {
        assert_eq!(id, "T1");
        let mut s = state.lock().unwrap();
        let files = json!([
            {"id":1,"path":"/Show/S03E03.mkv","bytes":100,"selected": 0},
            {"id":2,"path":"/Show/Show.S03E04.1080p.mkv","bytes":200,"selected": if s.selected.is_some() {1} else {0}},
            {"id":3,"path":"/Show/sample.mkv","bytes":5,"selected":0}
        ]);
        if s.selected.is_none() {
            return Json(json!({"status":"waiting_files_selection","progress":0,"files":files,"links":[]}));
        }
        s.polls += 1;
        if s.polls < 2 {
            Json(json!({"status":"downloading","progress":42.5,"speed":1000,"seeders":9,"files":files,"links":[]}))
        } else {
            Json(json!({"status":"downloaded","progress":100,"files":files,"links":["https://real-debrid.com/d/ABC"]}))
        }
    }

    async fn select(State(state): State<Shared>, Path(_id): Path<String>, Form(form): Form<Vec<(String, String)>>) -> StatusCode {
        state.lock().unwrap().selected = Some(form[0].1.clone());
        StatusCode::NO_CONTENT
    }

    async fn unrestrict(Form(form): Form<Vec<(String, String)>>) -> Json<Value> {
        assert_eq!(form[0].1, "https://real-debrid.com/d/ABC");
        Json(json!({"download":"https://cdn.example/file.mkv","filename":"Show.S03E04.1080p.mkv","filesize":200}))
    }

    async fn spawn_fake() -> (String, Shared) {
        let state: Shared = Arc::default();
        let app = Router::new()
            .route("/user", get(user))
            .route("/torrents", get(list))
            .route("/torrents/addMagnet", post(add))
            .route("/torrents/info/{id}", get(info))
            .route("/torrents/selectFiles/{id}", post(select))
            .route("/unrestrict/link", post(unrestrict))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), state)
    }

    #[tokio::test(start_paused = false)]
    async fn checks_keys() {
        let (base, _) = spawn_fake().await;
        let http = Http::new().unwrap();
        let user = RealDebrid::with_base(http.clone(), &base, "GOODKEY").user().await.unwrap();
        assert_eq!(user.username, "josh");
        assert!(user.is_premium());
        assert_eq!(user.premium_days_left(), 10);
        let error = RealDebrid::with_base(http, &base, "BAD").user().await.unwrap_err();
        match error {
            CoreError::RealDebrid(e) => assert_eq!(e.kind, RdErrorKind::BadKey),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolves_an_episode_with_progress() {
        let (base, state) = spawn_fake().await;
        let rd = RealDebrid::with_base(Http::new().unwrap(), &base, "GOODKEY");
        let stream = Source {
            info_hash: "a".repeat(40),
            file_idx: Some(0), // points at S03E03: must be rejected for S03E04
            filename: None,
            binge_group: None,
            trackers: vec![],
            cached: false,
            playback_url: None,
            release: ReleaseInfo::default(),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let (_tx, cancel) = watch::channel(false);
        let resolved = rd
            .resolve(
                &stream,
                &VideoRef::episode("tt1", 3, 4),
                &move |p| seen2.lock().unwrap().push(p),
                cancel,
            )
            .await
            .unwrap();
        assert_eq!(resolved.url, "https://cdn.example/file.mkv");
        assert_eq!(state.lock().unwrap().selected.as_deref(), Some("2"));
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|p| matches!(p, ResolveProgress::Downloading { percent, .. } if (*percent - 42.5).abs() < 0.01)));
        assert_eq!(seen.last(), Some(&ResolveProgress::Unrestricting));
    }

    #[test]
    fn matches_episode_names() {
        assert!(matches_episode("Show.S03E04.mkv", 3, 4));
        assert!(matches_episode("show 3x04.mkv", 3, 4));
        assert!(!matches_episode("Show.S03E040.mkv", 3, 4));
        assert!(matches_episode("Season 3/Breaking Bad Season 3 Episode 04 - Green Light.avi", 3, 4));
        assert!(!matches_episode("Show.S03E05.mkv", 3, 4));
    }

    #[test]
    fn maps_error_codes() {
        assert_eq!(RdError::from_api(451, Some(35), None).kind, RdErrorKind::Blocked);
        assert_eq!(RdError::from_api(401, None, None).kind, RdErrorKind::BadKey);
        assert_eq!(RdError::from_api(503, Some(21), None).kind, RdErrorKind::Busy);
    }
}
