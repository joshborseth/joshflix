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
    /// Turn Audio Normalization on when playback starts.
    #[serde(alias = "night_mode")]
    pub audio_normalization: bool,
    pub frame_rate_matching: bool,
    pub autoplay_next: bool,
    /// Decode with MediaCodec copy-back instead of zero-copy (for TVs whose
    /// drivers stall on the fast path). `None` = automatic, which is copy-back.
    pub video_compat_mode: Option<bool>,
    /// Last update version the user dismissed.
    pub dismissed_update_code: Option<u64>,
    /// The audio language last picked in the player (mpv `alang`), kept for
    /// the next episode and title.
    pub audio_language: Option<String>,
    /// Launches that showed the "hold OK for more options" tip.
    pub hold_tips_shown: u8,
    /// Searches that led to a title, newest first (shown on an empty Search).
    pub recent_searches: Vec<String>,
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
            audio_normalization: false,
            frame_rate_matching: true,
            autoplay_next: true,
            video_compat_mode: None,
            dismissed_update_code: None,
            audio_language: None,
            hold_tips_shown: 0,
            recent_searches: Vec::new(),
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
    #[serde(default)]
    pub logo: Option<String>,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub updated_at: u64,
    /// Set for the next episode after a finished one ("Up next" in Continue Watching).
    #[serde(default)]
    pub up_next: bool,
    #[serde(default)]
    pub last_source: Option<LastSource>,
    /// Removed from Continue Watching; the resume point stays. Playing it
    /// again saves a fresh record without this.
    #[serde(default)]
    pub hidden: bool,
    /// Where it stood before "Mark as watched", restored by "Mark as unwatched".
    #[serde(default)]
    pub marked_from: Option<f64>,
    /// The episode that finishing (or marking) this one moved the show on
    /// to: an Up next it queued, or a started one it brought forward.
    #[serde(default)]
    pub queued: Option<VideoRef>,
    /// When `queued` was a started episode: its time before it was brought forward.
    #[serde(default)]
    pub queued_prev_at: Option<u64>,
    /// Its time before "Mark as watched" (restored by "Mark as unwatched").
    #[serde(default)]
    pub marked_prev_at: Option<u64>,
    /// Episodes a "Mark earlier episodes as watched" from here marked too.
    #[serde(default)]
    pub marked_earlier: Vec<VideoRef>,
}

/// An aired episode after the one marked watched (see [`Store::mark_watched`]).
#[derive(Debug, Clone)]
pub struct LaterEpisode {
    pub video: VideoRef,
    pub title: Option<String>,
    pub thumbnail: Option<String>,
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
    /// Continue Watching as last computed; cleared by every progress write
    /// (it reads every record, and Home asks for it on every visit).
    continue_cache: Arc<std::sync::Mutex<Option<Vec<Progress>>>>,
    /// Bumped by every progress write: a list computed across a write isn't cached.
    progress_generation: Arc<std::sync::atomic::AtomicU64>,
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
        Ok(Self { db: Arc::new(db), continue_cache: Arc::default(), progress_generation: Arc::default() })
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

    /// Progress records for every episode of a series: one key range
    /// (`tt123`, `tt123:1:2`…), not a scan of every record.
    pub fn series_progress(&self, meta_id: &str) -> Result<Vec<Progress>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROGRESS)?;
        table_series(&table, meta_id)
    }

    pub fn save_progress(&self, mut progress: Progress) -> Result<()> {
        progress.updated_at = now();
        self.put(PROGRESS, &progress.video.video_id(), &progress)?;
        self.progress_changed();
        Ok(())
    }

    /// Save a playback's progress and, when it finished, move the show on to
    /// `next` (queued as Up next, or brought forward if already started), in
    /// one transaction. The record remembers what it moved.
    pub fn save_progress_and_queue(&self, mut progress: Progress, later: &[LaterEpisode]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PROGRESS)?;
            let at = now();
            progress.updated_at = at;
            progress.queued = None;
            progress.queued_prev_at = None;
            if !later.is_empty() {
                let series = table_series(&table, &progress.video.meta_id)?;
                let before = series.iter().find(|p| p.video == progress.video);
                // A later save of the same finish (the credits keep saving):
                // the show already moved on; restamp it, keeping its old time.
                let moved_before = before.and_then(|b| {
                    let queued = b.queued.clone()?;
                    let moved = series.iter().find(|p| p.video == queued && p.updated_at == b.updated_at + 1)?;
                    Some((queued, b.queued_prev_at, moved.clone()))
                });
                if let Some((queued, prev, moved)) = moved_before {
                    let moved = Progress { updated_at: at + 1, ..moved };
                    table.insert(queued.video_id().as_str(), encode(&moved)?.as_slice())?;
                    (progress.queued, progress.queued_prev_at) = (Some(queued), prev);
                } else {
                    (progress.queued, progress.queued_prev_at) = move_on(&mut table, &series, at, later, &progress, false)?;
                }
            }
            table.insert(progress.video.video_id().as_str(), encode(&progress)?.as_slice())?;
        }
        txn.commit()?;
        self.progress_changed();
        Ok(())
    }

    /// "Mark as watched", in one transaction. `template` names the video and
    /// carries its titles and art; `earlier` (optional) are episodes before
    /// it to mark too ("Mark earlier episodes as watched"); `later` lists the
    /// aired episodes after it, in order (empty for a movie).
    ///
    /// An episode at or past the viewer's place moves the show on: to the
    /// first later episode not yet watched, brought forward if started or
    /// queued as Up next. An older one keeps the show's place. The record
    /// remembers its earlier position and time and what it moved, so
    /// [`Store::unmark_watched`] can undo it. A hidden show stays hidden.
    pub fn mark_watched(&self, template: Progress, earlier: &[Progress], later: &[LaterEpisode]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PROGRESS)?;
            let series = table_series(&table, &template.video.meta_id)?;
            let hidden = series.iter().max_by_key(|p| p.updated_at).is_some_and(|p| p.hidden);
            // Earlier episodes become history: watched, but not the place.
            for before in earlier {
                let record = series.iter().find(|p| p.video == before.video);
                if record.is_some_and(Progress::is_watched) {
                    continue;
                }
                let mut done = watched_record(before.clone(), record, hidden);
                done.updated_at = record.map_or(1, |r| r.updated_at);
                table.insert(done.video.video_id().as_str(), encode(&done)?.as_slice())?;
            }
            let existing = series.iter().find(|p| p.video == template.video);
            let mine = existing.filter(|e| !e.up_next).map_or(0, |e| e.updated_at);
            // The viewer's place: the newest started or finished episode.
            let place = series
                .iter()
                .filter(|p| p.video != template.video && !p.up_next && (p.is_in_progress() || p.is_watched()))
                .max_by_key(|p| p.updated_at);
            let at_place = existing.is_some_and(|e| e.up_next)
                || place.is_none_or(|p| p.updated_at <= mine || comes_after(&template.video, &p.video));
            let mut watched = watched_record(template, existing, hidden);
            // One "Mark as unwatched" on this episode undoes the whole catch-up.
            watched.marked_earlier = earlier
                .iter()
                .filter(|b| !series.iter().any(|p| p.video == b.video && p.is_watched()))
                .map(|b| b.video.clone())
                .collect();
            if at_place {
                let at = now();
                watched.updated_at = at;
                (watched.queued, watched.queued_prev_at) = move_on(&mut table, &series, at, later, &watched, hidden)?;
            } else {
                watched.updated_at = existing.map_or(1, |e| e.updated_at);
            }
            table.insert(watched.video.video_id().as_str(), encode(&watched)?.as_slice())?;
        }
        txn.commit()?;
        self.progress_changed();
        Ok(())
    }

    /// "Mark as unwatched": undo what watching (or marking) it did, as far as
    /// nothing happened since. The Up next it queued goes and an episode it
    /// brought forward gets its old time back, unless played meanwhile; it
    /// returns to its earlier position and time (or to no record). Whether
    /// the show is off Continue Watching stays as it is now.
    pub fn unmark_watched(&self, video: &VideoRef) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PROGRESS)?;
            let series = table_series(&table, &video.meta_id)?;
            let Some(record) = series.iter().find(|p| p.video == *video).cloned() else {
                drop(table);
                txn.commit()?;
                return Ok(());
            };
            let hidden_now = series.iter().max_by_key(|p| p.updated_at).is_some_and(|p| p.hidden);
            if let Some(queued) = &record.queued
                && let Some(moved) = series.iter().find(|p| p.video == *queued)
                // Untouched since it was moved (a playback would have restamped it).
                && moved.updated_at == record.updated_at + 1
            {
                match record.queued_prev_at {
                    Some(prev) => {
                        let restored = Progress { updated_at: prev, up_next: false, ..moved.clone() };
                        table.insert(queued.video_id().as_str(), encode(&restored)?.as_slice())?;
                    }
                    None if moved.up_next => {
                        table.remove(queued.video_id().as_str())?;
                    }
                    None => {}
                }
            }
            // A catch-up ("Mark earlier episodes as watched") is undone with it.
            for earlier in &record.marked_earlier {
                let Some(done) = series.iter().find(|p| p.video == *earlier && p.is_watched()) else { continue };
                match done.marked_from.filter(|at| *at > 0.0) {
                    Some(at) => {
                        let restored = Progress {
                            position_secs: at,
                            updated_at: done.marked_prev_at.unwrap_or(done.updated_at),
                            marked_from: None,
                            marked_prev_at: None,
                            marked_earlier: Vec::new(),
                            ..done.clone()
                        };
                        table.insert(earlier.video_id().as_str(), encode(&restored)?.as_slice())?;
                    }
                    None => {
                        table.remove(earlier.video_id().as_str())?;
                    }
                }
            }
            match record.marked_from.filter(|at| *at > 0.0) {
                Some(at) => {
                    let restored = Progress {
                        position_secs: at,
                        updated_at: record.marked_prev_at.unwrap_or(record.updated_at),
                        marked_from: None,
                        marked_prev_at: None,
                        marked_earlier: Vec::new(),
                        queued: None,
                        queued_prev_at: None,
                        ..record.clone()
                    };
                    table.insert(video.video_id().as_str(), encode(&restored)?.as_slice())?;
                }
                None => {
                    table.remove(video.video_id().as_str())?;
                }
            }
            if let Some(mut newest) = table_series(&table, &video.meta_id)?.into_iter().max_by_key(|p| p.updated_at)
                && newest.hidden != hidden_now
            {
                newest.hidden = hidden_now;
                table.insert(newest.video.video_id().as_str(), encode(&newest)?.as_slice())?;
            }
        }
        txn.commit()?;
        self.progress_changed();
        Ok(())
    }

    /// Take a title off Continue Watching, keeping its resume point.
    pub fn hide_from_continue(&self, meta_id: &str) -> Result<()> {
        let Some(mut latest) = self.series_progress(meta_id)?.into_iter().max_by_key(|p| p.updated_at) else {
            return Ok(());
        };
        latest.hidden = true;
        self.put(PROGRESS, &latest.video.video_id(), &latest)?;
        self.progress_changed();
        Ok(())
    }

    pub fn remove_progress(&self, video: &VideoRef) -> Result<()> {
        self.remove(PROGRESS, &video.video_id())?;
        self.progress_changed();
        Ok(())
    }

    fn progress_changed(&self) {
        self.progress_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut cache) = self.continue_cache.lock() {
            *cache = None;
        }
    }

    /// Continue Watching: per title, the most recent unfinished video or "up next"
    /// episode, newest first.
    pub fn continue_watching(&self, limit: usize) -> Result<Vec<Progress>> {
        if let Ok(cache) = self.continue_cache.lock()
            && let Some(rows) = cache.as_ref()
        {
            return Ok(rows.iter().take(limit).cloned().collect());
        }
        let generation = self.progress_generation.load(std::sync::atomic::Ordering::SeqCst);
        let rows = self.compute_continue_watching()?;
        if let Ok(mut cache) = self.continue_cache.lock()
            && self.progress_generation.load(std::sync::atomic::Ordering::SeqCst) == generation
        {
            *cache = Some(rows.clone());
        }
        Ok(rows.into_iter().take(limit).collect())
    }

    fn compute_continue_watching(&self) -> Result<Vec<Progress>> {
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
            .filter(|p| !p.hidden && (p.is_in_progress() || p.up_next))
            .collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        rows.truncate(50);
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

/// Every progress record of one title: the key `tt123` (a movie) and the
/// keys from `tt123:` on (episodes). Not one range: `tt1234` sorts between
/// `tt123` and `tt123:`.
fn table_series(table: &impl ReadableTable<&'static str, &'static [u8]>, meta_id: &str) -> Result<Vec<Progress>> {
    let mut out = Vec::new();
    if let Some(value) = table.get(meta_id)?
        && let Ok(p) = decode::<Progress>(value.value())
    {
        out.push(p);
    }
    let prefix = format!("{meta_id}:");
    for entry in table.range(prefix.as_str()..)? {
        let (key, value) = entry?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        if let Ok(p) = decode::<Progress>(value.value()) {
            out.push(p);
        }
    }
    Ok(out)
}

/// `video` comes after `other` in episode order (regular seasons only).
fn comes_after(video: &VideoRef, other: &VideoRef) -> bool {
    match (video.season, video.episode, other.season, other.episode) {
        (Some(s), Some(e), Some(os), Some(oe)) if s > 0 && os > 0 => (s, e) > (os, oe),
        _ => false,
    }
}

/// The watched version of `template`, remembering where it stood.
fn watched_record(template: Progress, existing: Option<&Progress>, hidden: bool) -> Progress {
    let duration = existing.map_or(0.0, |e| e.duration_secs).max(template.duration_secs).max(1.0);
    let real = existing.filter(|e| !e.up_next);
    Progress {
        position_secs: duration,
        duration_secs: duration,
        up_next: false,
        hidden,
        marked_from: real.filter(|e| e.is_in_progress()).map(|e| e.position_secs),
        marked_prev_at: real.map(|e| e.updated_at),
        last_source: existing.and_then(|e| e.last_source.clone()).or(template.last_source.clone()),
        queued: None,
        queued_prev_at: None,
        ..template
    }
}

/// Move a show on after `finished` (stamped `at`): the first of `later` not
/// yet watched is brought forward (started) or queued as Up next, stamped
/// `at + 1`. Returns what moved and, if brought forward, its old time.
fn move_on(
    table: &mut redb::Table<&'static str, &'static [u8]>,
    series: &[Progress],
    at: u64,
    later: &[LaterEpisode],
    finished: &Progress,
    hidden: bool,
) -> Result<(Option<VideoRef>, Option<u64>)> {
    for next in later {
        let record = series.iter().find(|p| p.video == next.video);
        if record.is_some_and(Progress::is_watched) {
            continue;
        }
        let (moved, prev) = match record {
            Some(started) if !started.up_next && started.is_in_progress() => {
                (Progress { updated_at: at + 1, hidden, ..started.clone() }, Some(started.updated_at))
            }
            // A few seconds of it: queued as Up next, keeping its source;
            // its old time lets "Mark as unwatched" put it back.
            Some(barely) if !barely.up_next => (
                Progress { updated_at: at + 1, up_next: true, hidden, ..barely.clone() },
                Some(barely.updated_at),
            ),
            // Nothing, or an Up next: queue it fresh.
            _ => (
                Progress {
                    video: next.video.clone(),
                    episode_title: next.title.clone(),
                    thumbnail: next.thumbnail.clone(),
                    position_secs: 0.0,
                    duration_secs: 0.0,
                    updated_at: at + 1,
                    up_next: true,
                    last_source: None,
                    hidden,
                    marked_from: None,
                    marked_prev_at: None,
                    marked_earlier: Vec::new(),
                    queued: None,
                    queued_prev_at: None,
                    ..finished.clone()
                },
                None,
            ),
        };
        table.insert(moved.video.video_id().as_str(), encode(&moved)?.as_slice())?;
        return Ok((Some(next.video.clone()), prev));
    }
    Ok((None, None))
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
            logo: None,
            position_secs: pos,
            duration_secs: dur,
            updated_at: 0,
            up_next: false,
            last_source: None,
            hidden: false,
            marked_from: None,
            queued: None,
            queued_prev_at: None,
            marked_prev_at: None,
            marked_earlier: Vec::new(),
        }
    }

    fn later(meta: &str, episodes: &[(u32, u32)]) -> Vec<LaterEpisode> {
        episodes
            .iter()
            .map(|(s, e)| LaterEpisode { video: VideoRef::episode(meta, *s, *e), title: None, thumbnail: None })
            .collect()
    }

    fn cw_ids(store: &Store) -> Vec<String> {
        store.continue_watching(10).unwrap().iter().map(|p| p.video.video_id()).collect()
    }

    fn pause() {
        std::thread::sleep(std::time::Duration::from_millis(3));
    }

    #[test]
    fn marking_the_current_episode_moves_on_and_unmarking_undoes_it() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(4), 1000.0, 1000.0)).unwrap();
        pause();
        store.save_progress(progress(e(5), 400.0, 1000.0)).unwrap();
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6), (1, 7)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"], "E6 queued as Up next");
        assert!(store.progress(&e(6)).unwrap().unwrap().up_next);
        store.unmark_watched(&e(5)).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:5"]);
        assert_eq!(store.progress(&e(5)).unwrap().unwrap().position_secs, 400.0, "resume point restored");
        assert!(store.progress(&e(6)).unwrap().is_none(), "the Up next it queued is gone");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn marking_an_older_episode_keeps_the_place_and_unmarking_keeps_other_up_next() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        for n in 1..=5 {
            store.save_progress_and_queue(progress(e(n), 1000.0, 1000.0), &later("tt1", &[(1, n + 1)])).unwrap();
            pause();
        }
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        // E2 is old: unmarking it must not touch E6's Up next.
        store.unmark_watched(&e(2)).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        // An untouched older episode marked and unmarked: place unchanged.
        store.mark_watched(progress(e(2), 0.0, 0.0), &[], &later("tt1", &[(1, 3)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        store.unmark_watched(&e(2)).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn marking_brings_a_started_next_episode_forward_and_back() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(6), 500.0, 1000.0)).unwrap(); // started earlier
        pause();
        store.save_progress(progress(e(5), 300.0, 1000.0)).unwrap(); // rewatching E5 now
        let before = store.progress(&e(6)).unwrap().unwrap().updated_at;
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        assert_eq!(store.progress(&e(6)).unwrap().unwrap().position_secs, 500.0, "started one kept as it is");
        store.unmark_watched(&e(5)).unwrap();
        assert_eq!(store.progress(&e(6)).unwrap().unwrap().updated_at, before);
        assert_eq!(cw_ids(&store), ["tt1:1:5"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_barely_started_next_episode_becomes_up_next() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(3), 10.0, 2700.0)).unwrap(); // 30 s isn't started
        pause();
        store.mark_watched(progress(e(2), 0.0, 0.0), &[], &later("tt1", &[(1, 3)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:3"]);
        assert!(store.progress(&e(3)).unwrap().unwrap().up_next);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hidden_shows_stay_hidden_through_mark_and_unmark() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(5), 400.0, 1000.0)).unwrap();
        store.hide_from_continue("tt1").unwrap();
        assert!(cw_ids(&store).is_empty());
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6)])).unwrap();
        assert!(cw_ids(&store).is_empty());
        store.unmark_watched(&e(5)).unwrap();
        assert!(cw_ids(&store).is_empty());
        assert_eq!(store.progress(&e(5)).unwrap().unwrap().position_secs, 400.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unmark_leaves_an_episode_played_since_alone() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(6), 500.0, 1000.0)).unwrap();
        pause();
        store.save_progress(progress(e(5), 300.0, 1000.0)).unwrap();
        pause();
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6)])).unwrap();
        pause();
        store.save_progress(progress(e(6), 700.0, 1000.0)).unwrap(); // played E6 since
        store.unmark_watched(&e(5)).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"], "E6, just watched, stays the place");
        assert_eq!(store.progress(&e(5)).unwrap().unwrap().position_secs, 300.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unmark_keeps_the_current_hidden_state() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(5), 400.0, 1000.0)).unwrap();
        pause();
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6)])).unwrap();
        store.hide_from_continue("tt1").unwrap(); // removed after marking
        store.unmark_watched(&e(5)).unwrap();
        assert!(cw_ids(&store).is_empty(), "still removed");
        // Hidden, marked, then played: the show is back, and unmarking keeps it.
        let (store, dir2) = temp_store();
        store.save_progress(progress(e(5), 400.0, 1000.0)).unwrap();
        store.hide_from_continue("tt1").unwrap();
        pause();
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6)])).unwrap();
        pause();
        store.save_progress(progress(e(6), 300.0, 1000.0)).unwrap();
        store.unmark_watched(&e(5)).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(dir2).unwrap();
    }

    #[test]
    fn unmark_restores_a_movies_place_in_the_row() {
        let (store, dir) = temp_store();
        store.save_progress(progress(VideoRef::movie("tt8"), 600.0, 6000.0)).unwrap();
        pause();
        store.save_progress(progress(VideoRef::movie("tt9"), 600.0, 6000.0)).unwrap();
        assert_eq!(cw_ids(&store), ["tt9", "tt8"]);
        store.mark_watched(progress(VideoRef::movie("tt8"), 0.0, 0.0), &[], &[]).unwrap();
        store.unmark_watched(&VideoRef::movie("tt8")).unwrap();
        assert_eq!(cw_ids(&store), ["tt9", "tt8"], "back where it was, not first");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn marking_a_later_episode_moves_the_show_on() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress_and_queue(progress(e(1), 1000.0, 1000.0), &later("tt1", &[(1, 2)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:2"]);
        pause();
        // Watched E2–E5 elsewhere: marking E5 goes on to E6.
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn marking_earlier_episodes_catches_up() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        let earlier: Vec<Progress> = (1..5).map(|n| progress(e(n), 0.0, 0.0)).collect();
        store.mark_watched(progress(e(5), 0.0, 0.0), &earlier, &later("tt1", &[(1, 6)])).unwrap();
        for n in 1..=5 {
            assert!(store.progress(&e(n)).unwrap().unwrap().is_watched(), "E{n}");
        }
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finishing_an_episode_moves_on_past_a_barely_started_or_brings_a_started_one() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(6), 20.0, 2700.0)).unwrap(); // opened by mistake
        pause();
        store.save_progress_and_queue(progress(e(5), 2700.0, 2700.0), &later("tt1", &[(1, 6)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        assert!(store.progress(&e(6)).unwrap().unwrap().up_next);
        let (store, dir2) = temp_store();
        store.save_progress(progress(e(6), 1300.0, 2700.0)).unwrap(); // half watched
        pause();
        store.save_progress_and_queue(progress(e(5), 2700.0, 2700.0), &later("tt1", &[(1, 6)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:6"]);
        assert_eq!(store.progress(&e(6)).unwrap().unwrap().position_secs, 1300.0);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(dir2).unwrap();
    }

    #[test]
    fn credits_saves_keep_the_old_time_for_undo() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(6), 1300.0, 2700.0)).unwrap();
        let before = store.progress(&e(6)).unwrap().unwrap().updated_at;
        pause();
        for pos in [2500.0, 2600.0, 2700.0] {
            store.save_progress_and_queue(progress(e(5), pos, 2700.0), &later("tt1", &[(1, 6)])).unwrap();
            pause();
        }
        store.unmark_watched(&e(5)).unwrap();
        assert_eq!(store.progress(&e(6)).unwrap().unwrap().updated_at, before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn marking_earlier_episodes_keeps_a_later_place_and_undoes_in_one_go() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(8), 400.0, 1000.0)).unwrap();
        pause();
        let earlier: Vec<Progress> = (1..3).map(|n| progress(e(n), 0.0, 0.0)).collect();
        store.mark_watched(progress(e(3), 0.0, 0.0), &earlier, &later("tt1", &[(1, 4)])).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:8"], "E8 stays the place");
        store.unmark_watched(&e(3)).unwrap();
        for n in 1..=3 {
            assert!(store.progress(&e(n)).unwrap().is_none(), "E{n} unmarked too");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unmarking_restores_a_barely_started_next_episode() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        store.save_progress(progress(e(6), 20.0, 2700.0)).unwrap();
        pause();
        store.save_progress(progress(e(5), 400.0, 2700.0)).unwrap();
        store.mark_watched(progress(e(5), 0.0, 0.0), &[], &later("tt1", &[(1, 6)])).unwrap();
        store.unmark_watched(&e(5)).unwrap();
        let six = store.progress(&e(6)).unwrap().unwrap();
        assert!(!six.up_next);
        assert_eq!(six.position_secs, 20.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finishing_a_rewatch_keeps_the_show_in_continue_watching() {
        let (store, dir) = temp_store();
        let e = |n| VideoRef::episode("tt1", 1, n);
        for n in 3..=7 {
            store.save_progress(progress(e(n), 1000.0, 1000.0)).unwrap();
        }
        store.save_progress(progress(e(8), 400.0, 1000.0)).unwrap();
        pause();
        let after_two: Vec<(u32, u32)> = (3..=10).map(|n| (1, n)).collect();
        store.save_progress_and_queue(progress(e(2), 1000.0, 1000.0), &later("tt1", &after_two)).unwrap();
        assert_eq!(cw_ids(&store), ["tt1:1:8"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn series_progress_reads_only_that_title() {
        let (store, dir) = temp_store();
        store.save_progress(progress(VideoRef::episode("tt1", 1, 1), 10.0, 100.0)).unwrap();
        store.save_progress(progress(VideoRef::episode("tt12", 1, 1), 10.0, 100.0)).unwrap();
        store.save_progress(progress(VideoRef::movie("tt1"), 10.0, 100.0)).unwrap();
        let ids: Vec<String> = store.series_progress("tt1").unwrap().iter().map(|p| p.video.video_id()).collect();
        assert_eq!(ids, ["tt1", "tt1:1:1"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_movie_marked_from_any_row_keeps_its_resume_point() {
        let (store, dir) = temp_store();
        let m = VideoRef::movie("tt9");
        store.save_progress(progress(m.clone(), 2400.0, 6000.0)).unwrap();
        store.mark_watched(progress(m.clone(), 0.0, 0.0), &[], &[]).unwrap();
        assert!(store.progress(&m).unwrap().unwrap().is_watched());
        store.unmark_watched(&m).unwrap();
        assert_eq!(store.progress(&m).unwrap().unwrap().position_secs, 2400.0);
        std::fs::remove_dir_all(dir).unwrap();
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
    fn night_mode_setting_carries_over_to_audio_normalization() {
        let old: Settings = serde_json::from_str(r#"{"night_mode":true}"#).unwrap();
        assert!(old.audio_normalization);
        let saved = serde_json::to_string(&old).unwrap();
        assert!(saved.contains("\"audio_normalization\":true") && !saved.contains("night_mode"));
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
