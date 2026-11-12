//! On-device storage: settings, My List and watch progress, in one redb file.
//!
//! Everything lives in the app's private data directory, which Android
//! sandboxes per app. Values are JSON so the schema can grow without
//! migrations: unknown fields are ignored and missing ones take defaults.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;

use crate::error::{CoreError, Result};
use crate::ids::{MediaKind, VideoRef};
use crate::release::StreamFilters;
use crate::sources::Source;

const SETTINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("settings");
const LIBRARY: TableDefinition<&str, &[u8]> = TableDefinition::new("library");
const PROGRESS: TableDefinition<&str, &[u8]> = TableDefinition::new("progress");
const SETTINGS_KEY: &str = "settings";

/// Progress below this fraction doesn't count as started.
pub const STARTED_FRACTION: f64 = 0.02;
/// Progress at or above this fraction counts as watched.
pub const WATCHED_FRACTION: f64 = 0.92;

/// Every user setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub onboarding_done: bool,
    pub rd_api_key: Option<String>,
    /// Cached from the last successful key check.
    pub rd_username: Option<String>,
    pub rd_premium_until: Option<String>,
    pub filters: StreamFilters,
    pub subtitles_enabled: bool,
    /// ISO 639-2 codes in preference order.
    pub subtitle_languages: Vec<String>,
    /// Subtitle text scale (1.0 = default).
    pub subtitle_scale: f32,
    pub night_mode: bool,
    pub frame_rate_matching: bool,
    pub autoplay_next: bool,
    /// Last update version the user dismissed.
    pub dismissed_update_code: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            onboarding_done: false,
            rd_api_key: None,
            rd_username: None,
            rd_premium_until: None,
            filters: StreamFilters::default(),
            subtitles_enabled: true,
            subtitle_languages: vec!["eng".to_owned()],
            subtitle_scale: 1.0,
            night_mode: false,
            frame_rate_matching: true,
            autoplay_next: true,
            dismissed_update_code: None,
        }
    }
}

impl Settings {
    pub fn has_rd_key(&self) -> bool {
        self.rd_api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
    }
}

/// A title saved to My List.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryItem {
    pub id: String,
    pub kind: MediaKind,
    pub name: String,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub logo: Option<String>,
    pub release_info: Option<String>,
    pub added_at: u64,
}

/// The source last used for a video, so "Resume" and the next episode can reuse it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastSource {
    pub stream: Source,
}

/// Watch progress for one video.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub video: VideoRef,
    pub name: String,
    pub episode_title: Option<String>,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub thumbnail: Option<String>,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub updated_at: u64,
    /// Set for the next episode after a finished one ("Up next" in Continue Watching).
    #[serde(default)]
    pub up_next: bool,
    #[serde(default)]
    pub last_source: Option<LastSource>,
}

impl Progress {
    pub fn fraction(&self) -> f64 {
        if self.duration_secs > 0.0 {
            (self.position_secs / self.duration_secs).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    pub fn is_watched(&self) -> bool {
        self.fraction() >= WATCHED_FRACTION
    }

    pub fn is_in_progress(&self) -> bool {
        let f = self.fraction();
        f >= STARTED_FRACTION && f < WATCHED_FRACTION
    }

    /// Position to resume from, or `None` to start at the beginning.
    pub fn resume_position(&self) -> Option<f64> {
        self.is_in_progress().then_some(self.position_secs)
    }
}

/// Handle to the database. Cheap to clone; safe across threads.
#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        txn.open_table(SETTINGS)?;
        txn.open_table(LIBRARY)?;
        txn.open_table(PROGRESS)?;
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    pub fn settings(&self) -> Result<Settings> {
        Ok(self.get(SETTINGS, SETTINGS_KEY)?.unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        self.put(SETTINGS, SETTINGS_KEY, settings)
    }

    /// Read-modify-write the settings atomically.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let txn = self.db.begin_write()?;
        let settings = {
            let mut table = txn.open_table(SETTINGS)?;
            let mut settings: Settings = match table.get(SETTINGS_KEY)? {
                Some(value) => decode(value.value())?,
                None => Settings::default(),
            };
            change(&mut settings);
            table.insert(SETTINGS_KEY, encode(&settings)?.as_slice())?;
            settings
        };
        txn.commit()?;
        Ok(settings)
    }

    // ---- My List ----

    pub fn library(&self) -> Result<Vec<LibraryItem>> {
        let mut items: Vec<LibraryItem> = self.all(LIBRARY)?;
        items.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        Ok(items)
    }

    pub fn in_library(&self, id: &str) -> Result<bool> {
        Ok(self.get::<LibraryItem>(LIBRARY, id)?.is_some())
    }

    pub fn add_to_library(&self, mut item: LibraryItem) -> Result<()> {
        item.added_at = now();
        self.put(LIBRARY, &item.id.clone(), &item)
    }

    pub fn remove_from_library(&self, id: &str) -> Result<()> {
        self.remove(LIBRARY, id)
    }

    // ---- progress ----

    pub fn progress(&self, video: &VideoRef) -> Result<Option<Progress>> {
        self.get(PROGRESS, &video.video_id())
    }

    pub fn all_progress(&self) -> Result<Vec<Progress>> {
        self.all(PROGRESS)
    }

    /// Progress records for every episode of a series.
    pub fn series_progress(&self, meta_id: &str) -> Result<Vec<Progress>> {
        Ok(self
            .all_progress()?
            .into_iter()
            .filter(|p| p.video.meta_id == meta_id)
            .collect())
    }

    pub fn save_progress(&self, mut progress: Progress) -> Result<()> {
        progress.updated_at = now();
        self.put(PROGRESS, &progress.video.video_id(), &progress)
    }

    pub fn remove_progress(&self, video: &VideoRef) -> Result<()> {
        self.remove(PROGRESS, &video.video_id())
    }

    /// Continue Watching: per title, the most recent unfinished video or "up next"
    /// episode, newest first.
    pub fn continue_watching(&self, limit: usize) -> Result<Vec<Progress>> {
        let mut latest: std::collections::HashMap<String, Progress> = std::collections::HashMap::new();
        for p in self.all_progress()? {
            let entry = latest.entry(p.video.meta_id.clone());
            match entry {
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    if p.updated_at > o.get().updated_at {
                        o.insert(p);
                    }
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(p);
                }
            }
        }
        let mut rows: Vec<Progress> = latest
            .into_values()
            .filter(|p| p.is_in_progress() || p.up_next)
            .collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        rows.truncate(limit);
        Ok(rows)
    }

    // ---- generic helpers ----

    fn get<T: DeserializeOwned>(&self, table: TableDefinition<&str, &[u8]>, key: &str) -> Result<Option<T>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(table)?;
        match table.get(key)? {
            Some(value) => Ok(Some(decode(value.value())?)),
            None => Ok(None),
        }
    }

    fn all<T: DeserializeOwned>(&self, table: TableDefinition<&str, &[u8]>) -> Result<Vec<T>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(table)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            // Skip records an older/newer build can't read instead of failing the whole list.
            if let Ok(item) = decode(value.value()) {
                out.push(item);
            }
        }
        Ok(out)
    }

    fn put<T: Serialize>(&self, table: TableDefinition<&str, &[u8]>, key: &str, value: &T) -> Result<()> {
        let bytes = encode(value)?;
        let txn = self.db.begin_write()?;
        txn.open_table(table)?.insert(key, bytes.as_slice())?;
        txn.commit()?;
        Ok(())
    }

    fn remove(&self, table: TableDefinition<&str, &[u8]>, key: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(table)?.remove(key)?;
        txn.commit()?;
        Ok(())
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| CoreError::Storage(e.to_string()))
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| CoreError::Storage(e.to_string()))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Store, std::path::PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("joshflix-store-test-{}-{n}", std::process::id()));
        let store = Store::open(&dir.join("db.redb")).unwrap();
        (store, dir)
    }

    fn progress(video: VideoRef, pos: f64, dur: f64) -> Progress {
        Progress {
            video,
            name: "Show".into(),
            episode_title: None,
            poster: None,
            background: None,
            thumbnail: None,
            position_secs: pos,
            duration_secs: dur,
            updated_at: 0,
            up_next: false,
            last_source: None,
        }
    }

    #[test]
    fn settings_round_trip() {
        let (store, dir) = temp_store();
        assert!(!store.settings().unwrap().onboarding_done);
        let saved = store
            .update_settings(|s| {
                s.onboarding_done = true;
                s.rd_api_key = Some("KEY".into());
            })
            .unwrap();
        assert!(saved.has_rd_key());
        assert_eq!(store.settings().unwrap(), saved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn continue_watching_keeps_latest_per_title() {
        let (store, dir) = temp_store();
        store.save_progress(progress(VideoRef::episode("tt1", 1, 1), 1000.0, 1000.0)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        store.save_progress(progress(VideoRef::episode("tt1", 1, 2), 300.0, 1000.0)).unwrap();
        store.save_progress(progress(VideoRef::movie("tt2"), 5.0, 6000.0)).unwrap(); // not started
        std::thread::sleep(std::time::Duration::from_millis(3));
        let mut up_next = progress(VideoRef::episode("tt3", 2, 1), 0.0, 0.0);
        up_next.up_next = true;
        store.save_progress(up_next).unwrap();

        let rows = store.continue_watching(10).unwrap();
        let ids: Vec<String> = rows.iter().map(|p| p.video.video_id()).collect();
        assert_eq!(ids, ["tt3:2:1", "tt1:1:2"]);
        assert_eq!(rows[1].resume_position(), Some(300.0));
        assert!(store.progress(&VideoRef::episode("tt1", 1, 1)).unwrap().unwrap().is_watched());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn library_add_remove() {
        let (store, dir) = temp_store();
        let item = LibraryItem {
            id: "tt1".into(),
            kind: MediaKind::Movie,
            name: "Movie".into(),
            poster: None,
            background: None,
            logo: None,
            release_info: None,
            added_at: 0,
        };
        store.add_to_library(item).unwrap();
        assert!(store.in_library("tt1").unwrap());
        assert_eq!(store.library().unwrap().len(), 1);
        store.remove_from_library("tt1").unwrap();
        assert!(!store.in_library("tt1").unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
